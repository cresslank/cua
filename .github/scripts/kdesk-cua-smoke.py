#!/usr/bin/env python3
"""Public MCP plus decoded HTTP/protobuf OTLP; no desktop or live user state."""
import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import tempfile
import threading
import time

PRIVATE = 'KDESK-SMOKE-PRIVATE-CREDENTIAL-RESULT'

def require(ok, message):
    if not ok:
        raise ValueError(message)

def varint(data, offset):
    value = shift = 0
    while offset < len(data) and shift < 70:
        byte = data[offset]
        offset += 1
        value |= (byte & 127) << shift
        if not byte & 128:
            return value, offset
        shift += 7
    raise ValueError('invalid protobuf varint')

def fields(data):
    result = {}
    offset = 0
    while offset < len(data):
        key, offset = varint(data, offset)
        field, wire = key >> 3, key & 7
        require(field > 0, 'invalid protobuf field')
        if wire == 0:
            value, offset = varint(data, offset)
        elif wire in (1, 5):
            size = 8 if wire == 1 else 4
            value = int.from_bytes(data[offset:offset + size], 'little')
            offset += size
        elif wire == 2:
            size, offset = varint(data, offset)
            require(offset + size <= len(data), 'truncated protobuf')
            value = data[offset:offset + size]
            offset += size
        else:
            raise ValueError('unsupported protobuf wire type')
        result.setdefault(field, []).append(value)
    require(offset == len(data), 'truncated fixed protobuf field')
    return result

def attributes(messages):
    result = {}
    for message in messages:
        kv = fields(message)
        key = kv[1][0].decode()
        require(key not in result, 'duplicate OTLP attribute')
        value = fields(kv[2][0])
        require(set(value) in ({1}, {3}), 'unexpected OTLP attribute type')
        result[key] = value[1][0].decode() if 1 in value else value[3][0]
    return result

def decode(requests, source_sha):
    records = []
    resources = {'service.name', 'service.version', 'service.instance.id', 'host.arch', 'os.type', 'cua.build.git_sha'}
    allowed = {'cua.tool.name', 'cua.tool.outcome', 'cua.tool.duration_ns', 'error.type', 'cua.tool.parent_span_id'}
    for headers, body in requests:
        require(PRIVATE.lower() not in str(headers).lower() and PRIVATE.encode() not in body, 'private content exported')
        require('authorization' not in headers and headers.get('content-type') == 'application/x-protobuf', 'credential/content-type leak')
        for resource_bytes in fields(body).get(1, []):
            resource = fields(resource_bytes)
            attrs = attributes(fields(resource[1][0]).get(1, []))
            require(set(attrs) == resources and attrs['cua.build.git_sha'] == source_sha, 'resource privacy/source mismatch')
            for scope_bytes in resource.get(2, []):
                scope = fields(scope_bytes)
                require(fields(scope[1][0])[1][0] == b'cua-driver.tool-events', 'unexpected instrumentation scope')
                for record_bytes in scope.get(2, []):
                    record = fields(record_bytes)
                    require(record.get(12) == [b'cua.tool.completed'] and 5 not in record, 'event/body privacy mismatch')
                    attrs = attributes(record.get(6, []))
                    require(set(attrs) <= allowed, 'unexpected tool attributes')
                    require(attrs['cua.tool.name'] == 'get_config' and attrs['cua.tool.outcome'] == 'success', 'wrong dispatch event')
                    require(isinstance(attrs['cua.tool.duration_ns'], int) and attrs['cua.tool.duration_ns'] > 0, 'missing timing')
                    require(record.get(1, [0])[0] > 0 and len(record[9][0]) == 16 and any(record[9][0]) and len(record[10][0]) == 8 and any(record[10][0]), 'missing timing/correlation')
                    records.append(attrs)
    return records

class Collector:
    def __init__(self):
        self.requests = []
        owner = self
        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                size = int(self.headers.get('content-length', '-1'))
                if not (0 <= size <= 1024 * 1024) or self.path != '/v1/logs':
                    self.send_error(400)
                    return
                owner.requests.append((dict((k.lower(), v) for k, v in self.headers.items()), self.rfile.read(size)))
                self.send_response(200)
                self.send_header('Content-Length', '0')
                self.end_headers()
            def log_message(self, *args):
                pass
        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.endpoint = f'http://127.0.0.1:{self.server.server_port}/v1/logs'
    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

def rpc(process, message):
    process.stdin.write(json.dumps(message) + '\n')
    process.stdin.flush()
    require(bool(select.select([process.stdout], [], [], 10)[0]), 'MCP response deadline')
    response = json.loads(process.stdout.readline())
    require(response.get('id') == message['id'] and 'error' not in response, 'MCP protocol failure')
    return response['result']

def dispatch(binary, source_sha, portal_input, endpoint, mode):
    with tempfile.TemporaryDirectory(prefix='kdesk-mcp-') as tmp:
        home = Path(tmp)
        env = {'PATH': '/usr/bin:/bin', 'HOME': str(home), 'USERPROFILE': str(home),
               'DO_NOT_TRACK': '1', 'CUA_DRIVER_TELEMETRY_ENABLED': 'false',
               'CUA_DRIVER_RS_UPDATE_CHECK': 'false', 'RUST_LOG': 'error',
               'OTEL_EXPORTER_OTLP_LOGS_PROTOCOL': 'http/protobuf',
               'OTEL_EXPORTER_OTLP_LOGS_ENDPOINT': endpoint,
               'OTEL_RESOURCE_ATTRIBUTES': 'private=' + PRIVATE,
               'OTEL_EXPORTER_OTLP_LOGS_HEADERS': 'authorization=' + PRIVATE}
        for name in ('CONFIG', 'DATA', 'CACHE', 'STATE', 'RUNTIME'):
            directory = home / name.lower()
            directory.mkdir(mode=0o700)
            env['XDG_' + name + ('_DIR' if name == 'RUNTIME' else '_HOME')] = str(directory)
        if mode != 'no-opt-in':
            env['OTEL_LOGS_EXPORTER'] = 'otlp'
        if mode == 'sdk-disabled':
            env['OTEL_SDK_DISABLED'] = 'true'
        sock = str(home / 'runtime' / 'fixture.sock')
        pidfile = home / 'runtime' / 'fixture.pid'
        log = (home / 'process.log').open('w+')
        daemon = subprocess.Popen([str(binary), 'serve', '--socket', sock, '--pid-file', str(pidfile), '--no-overlay', '--no-automation-prompts'], env=env, stdout=log, stderr=log)
        proxy = None
        try:
            deadline = time.monotonic() + 15
            while not Path(sock).exists():
                require(daemon.poll() is None and time.monotonic() < deadline, 'fixture daemon failed to start')
                time.sleep(0.02)
            require(pidfile.is_file() and int(pidfile.read_text()) == daemon.pid, 'private pid file mismatch')
            proxy = subprocess.Popen([str(binary), 'mcp', '--socket', sock, '--no-overlay'], env=env,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True)
            rpc(proxy, {'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {'protocolVersion': '2024-11-05', 'capabilities': {}, 'clientInfo': {'name': 'kdesk-package-smoke', 'version': '1'}}})
            result = rpc(proxy, {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/call', 'params': {'name': 'get_config', 'arguments': {}}})
            require(not result.get('isError'), 'get_config failed')
            config = result['structuredContent']
            require(config['source_sha'] == source_sha and config['build_features']['portal_input'] is portal_input, 'source/feature diagnostic mismatch')
            proxy.stdin.close()
            require(proxy.wait(timeout=10) == 0, 'MCP shutdown failed')
            # Graceful daemon shutdown flushes the real carried exporter. This
            # private shutdown is not a tool invocation or a default-socket stop.
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(5)
                client.connect(sock)
                client.sendall(b'{"method":"shutdown"}\n')
                client.recv(65536)
            require(daemon.wait(timeout=10) == 0, 'daemon shutdown failed')
            return config
        finally:
            for child in (proxy, daemon):
                if child is not None and child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()
            log.close()

def smoke(binary, source_sha, portal_input=True):
    collector = Collector()
    try:
        dispatch(binary, source_sha, portal_input, collector.endpoint, 'on')
        records = decode(collector.requests, source_sha)
        require(len(records) >= 1, 'no actual get_config OTLP export')
        count = len(collector.requests)
        for mode in ('no-opt-in', 'sdk-disabled'):
            dispatch(binary, source_sha, portal_input, collector.endpoint, mode)
            require(len(collector.requests) == count, mode + ' exported without consent')
        # Keep a bound, non-listening socket to make the outage deterministic.
        with socket.socket() as unavailable:
            unavailable.bind(('127.0.0.1', 0))
            endpoint = f'http://127.0.0.1:{unavailable.getsockname()[1]}/v1/logs'
            dispatch(binary, source_sha, portal_input, endpoint, 'outage')
        return {'schema': 1, 'status': 'passed', 'source_sha': source_sha,
                'portal_input': portal_input, 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                'positive_records': len(records), 'cases': ['opt-in', 'no-opt-in', 'sdk-disabled', 'outage', 'privacy', 'source-feature']}
    finally:
        collector.close()

if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary', type=Path, required=True)
    p.add_argument('--source-sha', required=True)
    p.add_argument('--portal-input', choices=['true', 'false'], default='true')
    p.add_argument('--output', type=Path, required=True)
    args = p.parse_args()
    args.output.write_text(json.dumps(smoke(args.binary.resolve(), args.source_sha, args.portal_input == 'true'), sort_keys=True) + '\n')
