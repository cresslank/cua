"""Wire decoder/collector safety. Real packaged execution remains a VM gate."""
import importlib.util
from pathlib import Path
import urllib.request

import pytest

PATH = Path(__file__).resolve().parents[1] / 'kdesk-cua-smoke.py'
spec = importlib.util.spec_from_file_location('kdesk_smoke', PATH)
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)
SHA = 'a' * 40

def vint(n):
    data = bytearray()
    while n > 127:
        data.append((n & 127) | 128)
        n >>= 7
    data.append(n)
    return bytes(data)

def field(n, value):
    if isinstance(value, int):
        return vint(n << 3) + vint(value)
    if isinstance(value, str):
        value = value.encode()
    return vint((n << 3) | 2) + vint(len(value)) + value

def attr(key, value):
    return field(1, key) + field(2, field(3 if isinstance(value, int) else 1, value))

def wire(extra_tool=None, extra_resource=None, source=SHA, body=None, event='cua.tool.completed', duration=100, trace=b't' * 16):
    resource = {'service.name': 'cua-driver', 'service.version': '0.52.8', 'service.instance.id': 'fixture',
                'host.arch': 'x86_64', 'os.type': 'linux', 'cua.build.git_sha': source}
    resource.update(extra_resource or {})
    tool = {'cua.tool.name': 'get_config', 'cua.tool.outcome': 'success', 'cua.tool.duration_ns': duration}
    tool.update(extra_tool or {})
    record = field(1, 1000) + field(9, trace) + field(10, b's' * 8) + field(12, event)
    record += b''.join(field(6, attr(k, v)) for k, v in tool.items())
    if body is not None:
        record += field(5, field(1, body))
    scope = field(1, field(1, 'cua-driver.tool-events')) + field(2, record)
    resource_logs = field(1, b''.join(field(1, attr(k, v)) for k, v in resource.items())) + field(2, scope)
    return field(1, resource_logs)

HEADERS = {'content-type': 'application/x-protobuf'}

def test_real_loopback_http_protobuf_collector():
    collector = smoke.Collector()
    try:
        request = urllib.request.Request(collector.endpoint, data=wire(), headers=HEADERS, method='POST')
        with urllib.request.urlopen(request, timeout=5) as response:
            assert response.status == 200
        assert smoke.decode(collector.requests, SHA)[0]['cua.tool.name'] == 'get_config'
    finally:
        collector.close()

@pytest.mark.parametrize('fault', ['arguments', 'result', 'resource', 'credentials', 'source', 'event', 'duration', 'correlation'])
def test_privacy_source_and_dispatch_refusals(fault):
    headers = dict(HEADERS)
    kwargs = {}
    if fault == 'arguments':
        kwargs['extra_tool'] = {'arguments': smoke.PRIVATE}
    elif fault == 'result':
        kwargs['body'] = smoke.PRIVATE
    elif fault == 'resource':
        kwargs['extra_resource'] = {'arbitrary': smoke.PRIVATE}
    elif fault == 'credentials':
        headers['authorization'] = 'secret'
    elif fault == 'source':
        kwargs['source'] = 'b' * 40
    elif fault == 'event':
        kwargs['event'] = 'other'
    elif fault == 'duration':
        kwargs['duration'] = 0
    else:
        kwargs['trace'] = b'\0' * 16
    with pytest.raises(ValueError):
        smoke.decode([(headers, wire(**kwargs))], SHA)

@pytest.mark.parametrize('data', [b'\x80', b'\x0a\x7fshort', b'\x0f'])
def test_malformed_protobuf_refuses(data):
    with pytest.raises(ValueError):
        smoke.fields(data)
