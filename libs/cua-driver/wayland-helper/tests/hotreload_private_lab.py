#!/usr/bin/env python3
"""Opt-in real GNOME stable-loader qualification, with no host desktop clients.

Needs GNOME Shell, system Python/PyGObject/GTK3, and dbus-daemon. All buses,
settings, extension bytes, receiver windows and evidence are private/run-owned.
No Cargo, driver, portal consent, host display, installed extension or host bus.
"""
import argparse
import concurrent.futures
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import secrets
import selectors
import signal
import subprocess
import sys
import time
import traceback
from xml.sax.saxutils import escape

import gi
from gi.repository import Gio, GLib

HERE = Path(__file__).resolve().parent
BUS = 'org.cua.WinRects'
OBJECT = '/org/cua/WinRects'
LOADER = BUS + '.Loader'
CASES = ('idle', 'capture', 'foreground', 'cursor', 'disconnect', 'faults', 'control', 'driver', 'memory', 'timeout', 'uncertain')


def sha(data):
    return hashlib.sha256(data).hexdigest()


def require(value, message):
    if not value:
        raise AssertionError(message)


def process_inventory():
    """Read-only preflight; ordinary installed driver daemons are not test jobs."""
    rows = []
    blockers = []
    for entry in Path('/proc').iterdir():
        if not entry.name.isdigit():
            continue
        try:
            comm = (entry / 'comm').read_text().strip()
            cmd = (entry / 'cmdline').read_bytes().replace(b'\0', b' ').decode(errors='replace')
        except (OSError, PermissionError):
            continue
        if comm in ('cargo', 'rustc') or ('cua-driver' in comm and '/target/' in cmd):
            blockers.append({'pid': int(entry.name), 'comm': comm, 'command': cmd})
        elif 'cua-driver' in comm:
            rows.append({'pid': int(entry.name), 'comm': comm,
                         'classification': 'installed driver; lab uses no driver or per-UID driver lock'})
        elif comm == 'gnome-shell' and '--headless' in cmd:
            blockers.append({'pid': int(entry.name), 'comm': comm, 'command': cmd})
    require(not blockers, f'Unrelated native/Cargo/private-Shell jobs running: {blockers}')
    return rows


class Lab:
    def __init__(self, args):
        self.args = args
        scratch = args.scratch_root.resolve(strict=True)
        require(scratch.is_dir() and scratch != Path('/tmp') and not scratch.is_relative_to('/tmp'),
                'scratch-root must be an existing owned directory outside system /tmp')
        require(scratch.stat().st_uid == os.getuid(), 'scratch-root is not owned by this UID')
        self.token = 'hr-' + secrets.token_hex(4)
        self.root = scratch / self.token
        self.root.mkdir(mode=0o700)
        for directory in ('home', 'run', 'config', 'data', 'cache', 'state', 'services', 'out'):
            (self.root / directory).mkdir(mode=0o700)
        # UNIX sockets have a small pathname budget, including the NUL byte.
        require(len(os.fsencode(str(self.root / 'run/sysbus'))) < 104, 'scratch path too long for UNIX sockets')
        self.out = self.root / 'out'
        self.evidence = (self.out / 'results.jsonl').open('w', buffering=1)
        user = pwd.getpwuid(os.getuid()).pw_name
        self.env = dict(PATH='/usr/bin:/bin', HOME=str(self.root / 'home'), USER=user, LOGNAME=user,
                        LANG='C.UTF-8', LC_ALL='C.UTF-8', XDG_RUNTIME_DIR=str(self.root / 'run'),
                        XDG_CONFIG_HOME=str(self.root / 'config'), XDG_DATA_HOME=str(self.root / 'data'),
                        XDG_CACHE_HOME=str(self.root / 'cache'), XDG_STATE_HOME=str(self.root / 'state'),
                        TMPDIR=str(self.root / 'run'), DBUS_SESSION_BUS_ADDRESS=f'unix:path={self.root}/run/bus',
                        DBUS_SYSTEM_BUS_ADDRESS=f'unix:path={self.root}/run/sysbus', XDG_SESSION_TYPE='wayland',
                        XDG_CURRENT_DESKTOP='GNOME', WAYLAND_DISPLAY='wl-hotreload',
                        CUA_HOTRELOAD_RUN=self.token, CUA_HOTRELOAD_ROOT=str(self.root),
                        GNOME_SHELL_DISABLE_TELEMETRY='1', NO_AT_BRIDGE='1', GTK_A11Y='none',
                        GDK_BACKEND='wayland', GIO_USE_VFS='local', PYTHONDONTWRITEBYTECODE='1')
        self.processes = []
        self.connection = None
        self.shell = None
        self.pool = concurrent.futures.ThreadPoolExecutor(max_workers=3)
        self.ext = self.root / 'data/gnome-shell/extensions/winrects@cua'
        self.versions = {}
        self.epochs = set()
        self.selected = None
        self.cell = 'setup'

    def emit(self, event, **fields):
        row = dict(event=event, cell=self.cell, monotonic=time.monotonic(), **fields)
        self.evidence.write(json.dumps(row, sort_keys=True) + '\n')
        print(json.dumps({k: v for k, v in row.items() if k in
                         ('event', 'cell', 'root', 'evidence', 'error', 'pid', 'index', 'rss_delta_kib')}), flush=True)
        return row

    def start(self, argv, name):
        output = (self.out / f'{name}.log').open('w')
        process = subprocess.Popen(argv, env=self.env, stdin=subprocess.DEVNULL,
                                   stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        self.processes.append((process, output, name))
        return process

    def connect(self):
        return Gio.DBusConnection.new_for_address_sync(
            self.env['DBUS_SESSION_BUS_ADDRESS'],
            Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
            None, None)

    def call(self, destination, path, interface, method, signature=None, values=(), connection=None, timeout=10000):
        parameters = GLib.Variant(signature, values) if signature else None
        active = connection or self.connection
        assert active is not None, 'private bus is not connected'
        return active.call_sync(destination, path, interface, method, parameters,
                                None, Gio.DBusCallFlags.NONE, timeout, None).unpack()

    def helper(self, method, signature=None, values=(), connection=None):
        result = self.call(BUS, OBJECT, BUS, method, signature, values, connection)
        if len(result) == 1 and isinstance(result[0], str):
            return json.loads(result[0])
        return result

    def loader(self, method='GetStatus', connection=None):
        return json.loads(self.call(LOADER, OBJECT + '/Loader', LOADER, method,
                                    connection=connection, timeout=20000)[0])

    def observe(self, op='status', **values):
        return json.loads(self.call('org.cua.HotReload.Observer', '/org/cua/HotReload/Observer',
                                    'org.cua.HotReload.Observer', 'Run', '(s)',
                                    (json.dumps(dict(op=op, **values)),))[0])

    def owner(self, name):
        return self.call('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus',
                         'NameHasOwner', '(s)', (name,))[0]

    def wait(self, description, predicate, timeout=10):
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            if self.shell is not None and self.shell.poll() is not None:
                raise AssertionError(f'Shell exited {self.shell.returncode}: {description}')
            try:
                last = predicate()
                if last:
                    return last
            except GLib.Error as error:
                last = str(error)
            time.sleep(0.04)
        raise AssertionError(f'timeout: {description}; last={last}')

    def background(self, fn):
        def invoke():
            connection = self.connect()
            try:
                return fn(connection)
            finally:
                connection.close_sync(None)
        return self.pool.submit(invoke)

    def install(self, label, failure=None):
        files = {'main.js': self.main_bytes, 'policy.js': self.policy_bytes + f'\n// private qualification closure: {label}\n'.encode()}
        if failure == 'import':
            files['main.js'] = b"throw new Error('hotreload_deliberate_import');\n" + files['main.js']
        elif failure == 'enable':
            files['runtime.js'] = files['main.js']
            files['main.js'] = ("import Runtime from './runtime.js';\n"
                                "export default class extends Runtime { enable() { super.enable(); "
                                "this._createCursorRecord({key:'partial-enable', connectionOwner:':partial-enable'}); "
                                "globalThis.__cuaHotreloadObserver.watch(this); "
                                "throw new Error('hotreload_deliberate_enable'); } }\n").encode()
        elif failure == 'uncertain-retirement':
            files['runtime.js'] = files['main.js']
            files['main.js'] = ("import Runtime from './runtime.js';\n"
                                "export default class extends Runtime { stopAdmission() { super.stopAdmission(); "
                                "this._foreground = {transaction:'cua-fg-uncertain-fixture',state:'quarantined'}; } }\n").encode()
        elif failure == 'slow-import':
            files['runtime.js'] = files['main.js']
            files['main.js'] = ("import GLib from 'gi://GLib';\nimport Runtime from './runtime.js';\n"
                                "globalThis.__cuaHotreloadObserver.importStarted = true;\n"
                                "await new Promise(resolve => GLib.timeout_add(GLib.PRIORITY_DEFAULT, 1500, () => {resolve(); return GLib.SOURCE_REMOVE;}));\n"
                                "export default class extends Runtime { constructor(context) { "
                                "super(context); globalThis.__cuaHotreloadObserver.importConstructed = true; } }\n").encode()
        manifest = dict(loader_abi=1, entrypoint='main.js', helper_api=18, protocol=4,
                        files=[dict(path=name, size=len(data), sha256=sha(data)) for name, data in sorted(files.items())])
        manifest_bytes = (json.dumps(manifest, sort_keys=True, separators=(',', ':')) + '\n').encode()
        digest = sha(manifest_bytes)
        target = self.ext / 'impl' / digest
        target.mkdir(parents=True)
        for name, data in files.items():
            (target / name).write_bytes(data)
        (target / 'manifest.json').write_bytes(manifest_bytes)
        self.versions[label] = digest
        self.emit('closure_staged', label=label, digest=digest, manifest=manifest,
                  manifest_sha256=sha(manifest_bytes), real_runtime_sha256=sha(self.main_bytes))
        return digest

    def select(self, label):
        selection = dict(loader_abi=1, selected=self.versions[label], previous=self.selected)
        next_path = self.ext / 'selected.next'
        next_path.write_text(json.dumps(selection) + '\n')
        next_path.replace(self.ext / 'selected.json')
        self.selected = selection['selected']
        require(json.loads((self.ext / 'selected.json').read_text()) == selection, 'selector publication mismatch')
        self.emit('selected', label=label, selection=selection)

    def ready(self, label, fresh=True):
        digest = self.versions[label]
        status = self.wait('real helper ready ' + label,
                           lambda: (s if (s := self.loader()).get('state') == 'ready'
                                    and s.get('loaded_digest') == digest and self.owner(BUS) else False), 15)
        caps = self.helper('GetCapabilities')
        version = self.helper('GetVersion')[0]
        require(version == 18 and caps['protocol_version'] == 4, f'helper compatibility mismatch: {version}, {caps}')
        require(status['selected_digest'] == digest and status['loader_abi'] == 1, f'loader provenance mismatch: {status}')
        require(status['epoch'] == caps['epoch'], 'loader/helper epoch disagree')
        require(not fresh or caps['epoch'] not in self.epochs, 'runtime epoch reused')
        self.epochs.add(caps['epoch'])
        require(self.observe()['shellState'] == 1, 'stable bootstrap not ACTIVE')
        self.observe('watch')
        assert self.shell is not None
        self.emit('ready', label=label, loader=status, capabilities=caps, shell_pid=self.shell.pid)
        return caps

    def clean_retired(self):
        state = self.observe()
        require(not state['modalCount'] and not state['sessionLocked'], f'unexpected private Shell native dialog/lock: {state}')
        require(all(window['title'] in ('Hotreload A', 'Hotreload B') for window in state['windows']),
                f'unexpected private native window; refusing further actions: {state["windows"]}')
        for record in state['records']:
            if record['current']:
                continue
            require(not record['liveSources'], f'retired runtime retains native GLib sources: {record}')
            require(not record['liveHandlers'], f'retired runtime retains GObject signals: {record}')
            require(all(a['destroyed'] and not a['parent'] for a in record['actors']), f'retired cursor actor retained: {record}')
            require(not record['exported'] and not record['nameId'] and not record['nameOwnerSignalId'],
                    f'retired runtime retains D-Bus resources: {record}')
            require(not record['cursorCount'] and not record['foreground'], f'retired runtime owns state: {record}')
        self.emit('retired_resources_verified', observer=state)
        return state

    def raw_input_held(self):
        path = Path(f'/run/user/{os.geteuid()}/cua-driver-gnome-raw-input.lock')
        with path.open('r+b') as file:
            try:
                fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return False
            except BlockingIOError:
                return True

    def switch(self, label):
        if self.observe()['runtimeEpoch']:
            self.observe('watch')
        self.select(label)
        result = self.loader('ReloadSelected')
        require(result['loaded_digest'] == self.versions[label], f'reload returned wrong selection: {result}')
        require(not self.raw_input_held(), 'successful reload leaked the canonical raw-input lease')
        self.emit('raw_input_guard_released')
        caps = self.ready(label)
        self.clean_retired()
        return caps

    def refusal(self, method, signature, values, text):
        try:
            self.helper(method, signature, values)
        except GLib.Error as error:
            require(text in str(error), f'{method}: wrong refusal; expected {text!r}, got {error}')
            self.emit('refusal_verified', method=method, expected=text, error=str(error))
            return str(error)
        raise AssertionError(f'{method}: unexpectedly succeeded, expected {text}')

    def receiver(self):
        return json.loads((self.out / 'receiver.json').read_text())

    def fixture(self, title='Hotreload A'):
        records = [r for r in self.helper('GetRects') if r['title'] == title]
        require(len(records) == 1, f'expected exactly one real fixture {title}: {records}')
        return records[0]

    def focus(self, record):
        self.observe('activate', id=record['id'])
        self.wait('focus fixture', lambda: self.observe()['focus'] == record['id'])
        self.wait('painted fixture', lambda: (r if (r := self.fixture(record['title'])) and r['capture_current'] else False))
        return self.fixture(record['title'])

    def no_late_input(self, baseline):
        self.observe('poke')
        time.sleep(0.35)
        after = self.receiver()
        require((after['clicks'], after['keys']) == (baseline['clicks'], baseline['keys']),
                f'late receiver input after retirement: before={baseline}, after={after}')
        self.clean_retired()
        self.emit('no_late_receiver_input', before=baseline, after=after)

    def guard(self):
        for key in ('HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_RUNTIME_DIR',
                    'XDG_CACHE_HOME', 'XDG_STATE_HOME', 'TMPDIR'):
            require(Path(self.env[key]).resolve().is_relative_to(self.root), f'non-private {key}')
        require('DISPLAY' not in self.env and 'AT_SPI_BUS_ADDRESS' not in self.env, 'host display/a11y inherited')
        for key in ('DBUS_SESSION_BUS_ADDRESS', 'DBUS_SYSTEM_BUS_ADDRESS'):
            require(self.env[key].startswith(f'unix:path={self.root}/run/'), f'non-private {key}')
        self.call('ca.desrt.dconf', '/ca/desrt/dconf', 'org.freedesktop.DBus.Peer', 'Ping')
        pid = self.call('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus',
                        'GetConnectionUnixProcessID', '(s)', ('ca.desrt.dconf',))[0]
        actual = dict(item.split('=', 1) for item in Path(f'/proc/{pid}/environ').read_bytes().decode().split('\0') if '=' in item)
        for key in ('HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_RUNTIME_DIR', 'CUA_HOTRELOAD_RUN'):
            require(actual.get(key) == self.env[key], f'dconf activation environment escaped: {key}')
        self.emit('dconf_guard_verified_before_settings', pid=pid, environment={key: actual.get(key) for key in self.env})

    def setup(self):
        self.emit('run_started', root=str(self.root), evidence=str(self.out),
                  process_inventory=process_inventory(), no_cargo=True, no_driver=not self.args.driver)
        self.ext.mkdir(parents=True)
        source = self.args.helper.resolve(strict=True)
        require((source / 'impl/main.js').is_file(), 'product not ready: missing impl/main.js')
        self.main_bytes = (source / 'impl/main.js').read_bytes()
        self.policy_bytes = (source / 'impl/policy.js').read_bytes()
        self.root_hashes = {}
        for path in source.iterdir():
            if path.is_file() and (path.suffix == '.js' or path.name == 'metadata.json'):
                require(not path.is_symlink(), f'product root symlink: {path}')
                data = path.read_bytes()
                (self.ext / path.name).write_bytes(data)
                self.root_hashes[path.name] = sha(data)
        self.source_hashes = {'impl/main.js': sha(self.main_bytes), 'impl/policy.js': sha(self.policy_bytes), **self.root_hashes}
        if self.args.mutation:
            mutation = json.loads(self.args.mutation.read_text())
            require(mutation['path'] in ('impl/main.js', 'extension.js'), 'only copied runtime/bootstrap mutations are supported')
            old = mutation['old'].encode()
            original = self.main_bytes if mutation['path'] == 'impl/main.js' else (self.ext / 'extension.js').read_bytes()
            require(original.count(old) == 1, 'mutation old-text match is not unique')
            changed = original.replace(old, mutation['new'].encode(), 1)
            if mutation['path'] == 'impl/main.js':
                self.main_bytes = changed
            else:
                (self.ext / 'extension.js').write_bytes(changed)
                self.root_hashes['extension.js'] = sha(changed)
                self.source_hashes['extension.js'] = sha(changed)
            self.emit('scratch_only_mutation', mutation=mutation, original_sha256=sha(original),
                      mutated_sha256=sha(changed))
        observer = self.ext.parent / 'hotreload-observer@lab'
        observer.mkdir()
        (observer / 'extension.js').write_bytes((HERE / 'hotreload_observer.js').read_bytes())
        shell_version = subprocess.check_output(['gnome-shell', '--version'], text=True).strip()
        match = re.search(r'\b(\d+)\.', shell_version)
        assert match is not None, f'unrecognized GNOME version: {shell_version}'
        major = match.group(1)
        (observer / 'metadata.json').write_text(json.dumps(dict(uuid=observer.name, name='Private hotreload observer',
                                                               description='Test-only', **{'shell-version': [major]})))
        for label, failure in [('v1', None), ('v2', None), ('bad-import', 'import'), ('bad-enable', 'enable')]:
            self.install(label, failure)
        if 'memory' in self.args.cases:
            for index in range(20):
                self.install(f'memory-{index:02}')
        self.select('v1')
        (self.root / 'services/ca.desrt.dconf.service').write_text(
            '[D-BUS Service]\nName=ca.desrt.dconf\nExec=/usr/libexec/dconf-service\n')
        for name in ('session', 'system'):
            address = escape(self.env[f'DBUS_{name.upper()}_BUS_ADDRESS'])
            services = f'<servicedir>{escape(str(self.root / "services"))}</servicedir>' if name == 'session' else ''
            config = ('<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN" '
                      '"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">\n'
                      f'<busconfig><type>{name}</type><listen>{address}</listen><auth>EXTERNAL</auth>'
                      '<policy context="default"><allow send_destination="*" eavesdrop="true"/>'
                      f'<allow eavesdrop="true"/><allow own="*"/></policy>{services}</busconfig>\n')
            config_path = self.root / f'{name}.conf'
            config_path.write_text(config)
            self.start(['dbus-daemon', '--nofork', '--nopidfile', '--config-file=' + str(config_path)], name + '-bus')
        self.wait('private bus sockets', lambda: (self.root / 'run/bus').is_socket() and (self.root / 'run/sysbus').is_socket())
        self.connection = self.connect()
        self.guard()  # Mandatory BEFORE gsettings setters, including dconf activation.
        for schema, key, value in [
            ('org.gnome.shell', 'enabled-extensions', "['hotreload-observer@lab']"),
            ('org.gnome.shell', 'disable-user-extensions', 'false'),
            ('org.gnome.shell', 'welcome-dialog-last-shown-version', '999'),
            ('org.gnome.desktop.interface', 'enable-animations', 'false'),
            ('org.gnome.desktop.session', 'idle-delay', '0'),
        ]:
            subprocess.run(['gsettings', 'set', schema, key, value], env=self.env, check=True, timeout=5, capture_output=True)
        require((self.root / 'config/dconf/user').is_file(), 'private dconf output missing')
        (self.out / 'private-environment.json').write_text(json.dumps(self.env, indent=2) + '\n')
        (self.out / 'source-manifest.json').write_text(json.dumps(dict(helper=str(source), files=self.source_hashes,
            harness={path.name: sha(path.read_bytes()) for path in HERE.glob('hotreload*') if path.is_file()},
            shell_version=shell_version), indent=2) + '\n')
        self.shell = self.start(['gnome-shell', '--headless', '--wayland', '--no-x11', '--virtual-monitor', '1280x800',
                                 '--wayland-display', 'wl-hotreload'], 'shell')
        self.wait('observer bus', lambda: self.owner('org.cua.HotReload.Observer'), 40)
        subprocess.run(['gnome-extensions', 'enable', 'winrects@cua'], env=self.env, check=True, timeout=8, capture_output=True)
        self.wait('stable loader bus', lambda: self.owner(LOADER), 15)
        self.ready('v1')
        self.start(['/usr/bin/python3', '-I', '-B', str(HERE / 'hotreload_receiver.py')], 'receiver')
        self.wait('receiver output', lambda: (self.out / 'receiver.json').is_file())
        self.wait('two real GTK windows', lambda: {'Hotreload A', 'Hotreload B'}.issubset(
            {r['title'] for r in self.helper('GetRects')}))
        time.sleep(2.5)  # Startup overview is closed by the independent observer.
        self.observe('place')
        time.sleep(0.3)
        self.focus(self.fixture())
        stat = Path(f'/proc/{self.shell.pid}/stat').read_text()
        self.emit('private_shell_ready', pid=self.shell.pid, shell_stat=stat, loader=self.loader(), receiver=self.receiver())

    def run(self):
        require(len(set(self.args.cases)) == len(self.args.cases), 'duplicate scenario selection')
        for case in self.args.cases:
            self.cell = case
            getattr(self, 'case_' + case)()
            self.emit('cell_passed')
        self.cell = 'final'
        for name, digest in self.root_hashes.items():
            require(sha((self.ext / name).read_bytes()) == digest, f'stable root changed: {name}')
        if self.observe()['runtimeEpoch']:
            self.observe('watch')
        subprocess.run(['gnome-extensions', 'disable', 'winrects@cua'], env=self.env,
                       check=True, timeout=8, capture_output=True)
        self.wait('disabled helper and stable-loader bus names released', lambda: not self.owner(BUS) and not self.owner(LOADER))
        if 'uncertain' in self.args.cases:
            current = next(record for record in self.observe()['records'] if record['current'])
            require(not current['liveSources'] and not current['liveHandlers'] and not current['nameId']
                    and not current['exported'] and all(actor['destroyed'] for actor in current['actors']),
                    f'quarantined runtime retained live resources: {current}')
            self.emit('uncertainty_tombstone_retained_after_disable', observer=self.observe())
        else:
            self.wait('disabled real runtime retired', lambda: not self.observe()['runtimeEpoch'])
        self.clean_retired()
        assert self.shell is not None
        self.emit('all_tests_passed', cases=self.args.cases, shell_pid=self.shell.pid,
                  epochs=sorted(self.epochs), root_hashes=self.root_hashes, observer=self.clean_retired())

    def case_idle(self):
        before = self.focus(self.fixture())
        proof = self.helper('CaptureTarget', '(s)', (before['target_id'],))
        require(proof['target'] == before['target_id'] and proof['png_base64'].startswith('iVBOR'), 'real capture did not produce PNG')
        old_epoch = before['helper_epoch']
        caps = self.switch('v2')
        require(caps['epoch'] != old_epoch, 'idle swap reused epoch')
        require((self.ext / 'impl' / self.versions['v1'] / 'main.js').read_bytes() ==
                (self.ext / 'impl' / self.versions['v2'] / 'main.js').read_bytes(), 'v1/v2 runtime differs unexpectedly')
        self.refusal('CaptureTarget', '(s)', (before['target_id'],), 'stale_target')
        self.refusal('BeginForeground', '(ss)', ('cua-fg-hotreload-stale-0001', before['target_id']), 'stale_target')
        # Native positive receiver control proves the receiver is not inert.
        current = self.focus(self.fixture())
        baseline = self.receiver()
        self.observe('click', x=current['x'] + current['w'] // 2, y=current['y'] + current['h'] // 2)
        time.sleep(0.2)
        self.emit('native_receiver_attempt', target=current, observer=self.observe(), receiver=self.receiver())
        self.wait('real receiver click', lambda: self.receiver()['clicks'] > baseline['clicks'])
        self.emit('native_receiver_positive', receiver=self.receiver())
        self.no_late_input(self.receiver())

    def case_capture(self):
        before = self.focus(self.fixture())
        self.observe('watch')
        self.observe('arm-capture')
        capture = self.background(lambda connection: self.helper('CaptureTarget', '(s)', (before['target_id'],), connection))
        reload = None
        try:
            self.wait('real GI composite completes behind rendezvous', lambda: self.observe()['capture']['nativeCompleted'])
            held = self.observe()
            require(held['capture']['bytes'] > 0 and not capture.done(), 'capture was not parked after real PNG production')
            self.select('v1' if self.loader()['loaded_digest'] == self.versions['v2'] else 'v2')
            reload = self.background(lambda connection: self.loader('ReloadSelected', connection))
            self.wait('retirement entered or successor prematurely completed',
                      lambda: self.loader()['state'] != 'ready' or reload.done())
            time.sleep(0.1)
            require(not reload.done(), 'successor loaded while real capture continuation was unresolved')
            require(self.raw_input_held(), 'retirement wait did not exclude new canonical raw-input windows')
            self.emit('capture_retirement_rendezvous', observer=held, loader=self.loader(), raw_input_held=self.raw_input_held())
            self.observe('release-capture')
            try:
                result = capture.result(timeout=10)
            except GLib.Error as error:
                require('retir' in str(error).lower() or 'stale' in str(error).lower(), f'wrong retired capture refusal: {error}')
                self.emit('retired_capture_refused', error=str(error), native_png_bytes=held['capture']['bytes'])
            else:
                raise AssertionError(f'old capture published success after retirement: target={result.get("target")}')
            result = reload.result(timeout=15)
            label = next(label for label in ('v1', 'v2') if self.versions[label] == result['loaded_digest'])
            self.ready(label)
            require(self.observe()['capture']['released'], 'real source continuation did not resume')
            self.clean_retired()
            self.no_late_input(self.receiver())
        finally:
            self.observe('release-capture')
            # Never strand the original provider or future on a failed assertion.
            concurrent.futures.wait([future for future in (capture, reload) if future], timeout=22)

    def case_foreground(self):
        target = self.fixture('Hotreload A')
        self.focus(self.fixture('Hotreload B'))
        epoch = self.loader()['epoch']
        transaction = 'cua-fg-hotreload-open-0002'
        begun = self.helper('BeginForeground', '(ss)', (transaction, target['target_id']))
        require(begun['activated'], f'foreground failed: {begun}')
        require(self.observe()['focus'] == target['id'], 'real target not focused')
        next_label = 'v2' if self.selected == self.versions['v1'] else 'v1'
        self.select(next_label)
        try:
            self.loader('ReloadSelected')
        except GLib.Error as error:
            require('org.cua.WinRects.Loader.Busy' in str(error) and 'foreground_transaction_open' in str(error),
                    f'wrong open-transaction refusal: {error}')
            self.emit('maintenance_busy_refused', kind='foreground', error=str(error), epoch=epoch)
        else:
            raise AssertionError('reload admitted an open foreground transaction')
        require(self.loader()['epoch'] == epoch and self.loader()['state'] == 'ready', 'busy reload changed runtime')
        require(self.observe()['focus'] == target['id'], 'reload aborted/restored an open foreground transaction')
        require(self.helper('QueryForeground', '(s)', (transaction,))['terminal'] is False,
                'reload terminalized the foreground transaction')
        # NO native input was submitted in this focus-only fixture. Commit is
        # not taken as a consumption acknowledgement for any native batch.
        require(self.helper('CommitForeground', '(s)', (transaction,))['committed'], 'focus-only fixture did not close')
        canonical = Path(f'/run/user/{os.geteuid()}/cua-driver-gnome-raw-input.lock')
        descriptor = os.open(canonical, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        with os.fdopen(descriptor, 'r+b') as lease:
            require(os.fstat(lease.fileno()).st_uid == os.geteuid()
                    and os.fstat(lease.fileno()).st_mode & 0o777 == 0o600, 'invalid canonical raw-input fixture')
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
            try:
                self.loader('ReloadSelected')
            except GLib.Error as error:
                require('org.cua.WinRects.Loader.Busy' in str(error) and 'raw_input_window_open' in str(error),
                        f'wrong raw-window refusal: {error}')
                self.emit('maintenance_busy_refused', kind='raw-input', error=str(error), epoch=epoch)
            else:
                raise AssertionError('reload admitted an open canonical raw-input window')
            require(self.loader()['epoch'] == epoch and self.observe()['focus'] == target['id'],
                    'raw-window refusal changed runtime or focus')
        self.switch(next_label)
        self.refusal('CaptureTarget', '(s)', (target['target_id'],), 'stale_target')
        self.no_late_input(self.receiver())
        self.emit('foreground_and_raw_window_busy_verified', old_epoch=epoch, loader=self.loader())

    def cursor(self, connection=None, owner='hotreload-cursor'):
        target = self.focus(self.fixture())
        self.helper('MoveCursorFor', '(ssii)', (owner, target['target_id'], target['x'] + 80, target['y'] + 80), connection)
        self.helper('ClickPulseFor', '(ssii)', (owner, target['target_id'], target['x'] + 80, target['y'] + 80), connection)
        state = self.observe('watch')
        current = next(record for record in state['records'] if record['current'])
        require(current['actors'] and any(a['visible'] for a in current['actors']), f'no real visible cursor actor: {state}')
        require(current['liveSources'], f'cursor has no real GLib sources: {state}')
        return state

    def case_cursor(self):
        before = self.cursor()
        baseline = self.receiver()
        self.switch('v2' if self.selected == self.versions['v1'] else 'v1')
        self.no_late_input(baseline)
        self.emit('active_cursor_retired', before=before, after=self.observe())

    def case_disconnect(self):
        connection = self.connect()
        try:
            self.cursor(connection, 'disconnect-cursor')
            unique_name = connection.get_unique_name()
        finally:
            connection.close_sync(None)
        self.wait('disconnected client cursor removed', lambda: all(not r['cursorCount'] for r in self.observe()['records'] if r['current']))
        self.emit('client_disconnect_cleanup', unique_name=unique_name, observer=self.observe())
        self.switch('v2' if self.selected == self.versions['v1'] else 'v1')
        self.no_late_input(self.receiver())

    def case_faults(self):
        for label, expected in [('bad-import', 'hotreload_deliberate_import'), ('bad-enable', 'hotreload_deliberate_enable')]:
            self.observe('watch')
            previous_epoch = self.loader()['epoch']
            self.select(label)
            # Error is reported in stable-loader status, not Shell ERROR state.
            try:
                returned = self.loader('ReloadSelected')
            except GLib.Error as error:
                returned = {'dbus_error': str(error)}
            status = self.wait('contained activation failure', lambda: (s if (s := self.loader()).get('state') == 'error' else False))
            require(expected in str(status['last_error']), f'wrong candidate error: {status}')
            require(self.observe()['shellState'] == 1, 'failure poisoned stable GNOME extension state')
            require(not self.owner(BUS), 'failed candidate still owns helper bus name')
            self.clean_retired()
            self.emit('fault_contained', label=label, status=status, reload_reply=returned)
            self.switch('v1')
            require(self.loader()['epoch'] != previous_epoch, 'rollback reused runtime epoch')
            self.emit('rollback_verified', failed=label, loader=self.loader())

    def case_driver(self):
        driver = self.args.driver.resolve(strict=True)
        self.start(['/usr/libexec/at-spi-bus-launcher', '--launch-immediately'], 'a11y-launcher')
        self.wait('owned accessibility bus', lambda: self.owner('org.a11y.Bus'))
        address = self.call('org.a11y.Bus', '/org/a11y/bus', 'org.a11y.Bus', 'GetAddress')[0]
        self.env['AT_SPI_BUS_ADDRESS'] = address
        self.start(['/usr/libexec/at-spi2-registryd'], 'a11y-registry')
        self.env.update(CUA_DRIVER_RS_TELEMETRY_ENABLED='false', CUA_DRIVER_RS_UPDATE_CHECK='false')
        log = (self.out / 'driver.log').open('w')
        process = subprocess.Popen([str(driver), 'mcp'], env=self.env, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=log, start_new_session=True)
        self.processes.append((process, log, 'driver'))
        stdin, stdout = process.stdin, process.stdout
        assert stdin is not None and stdout is not None
        selector = selectors.DefaultSelector()
        selector.register(stdout, selectors.EVENT_READ)
        sequence = 0
        buffered = b''
        def rpc(method, params=None, notify=False):
            nonlocal sequence, buffered
            sequence += 1
            request: dict[str, object] = dict(jsonrpc='2.0', method=method)
            if params is not None: request['params'] = params
            if not notify: request['id'] = sequence
            stdin.write((json.dumps(request) + '\n').encode())
            stdin.flush()
            if notify: return None
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                while b'\n' in buffered:
                    raw, buffered = buffered.split(b'\n', 1)
                    value = json.loads(raw)
                    if value.get('id') == sequence: return value
                require(selector.select(max(0, deadline - time.monotonic())), 'real MCP response deadline')
                data = os.read(stdout.fileno(), 65536)
                require(data, 'real driver closed stdout')
                buffered += data
            raise AssertionError('real MCP response deadline')
        def tool(name, arguments):
            response = rpc('tools/call', dict(name=name, arguments=arguments))
            assert response is not None
            require('result' in response, f'real driver RPC error: {response}')
            return response['result']
        try:
            rpc('initialize', dict(protocolVersion='2025-06-18', capabilities={},
                clientInfo=dict(name='private-hotreload-lab', version='1')))
            rpc('notifications/initialized', notify=True)
            before = self.focus(self.fixture())
            windows = tool('list_windows', {})
            require(not windows.get('isError'), f'driver enumeration failed: {windows}')
            target = next(w for w in windows['structuredContent']['windows'] if w['title'] == 'Hotreload A')
            first = tool('get_window_state', dict(pid=target['pid'], window_id=target['window_id'], max_elements=0))
            require(not first.get('isError'), f'driver exact capture failed: {first}')
            self.observe('watch')
            self.switch('v2' if self.selected == self.versions['v1'] else 'v1')
            stale = tool('get_window_state', dict(pid=target['pid'], window_id=target['window_id'], max_elements=0))
            require(stale.get('isError'), 'real driver replayed stale pre-reload window target')
            rediscovered = tool('list_windows', {})
            new = next(w for w in rediscovered['structuredContent']['windows'] if w['title'] == 'Hotreload A')
            require(new['window_id'] != target['window_id'], 'driver public target identity survived epoch change')
            fresh = tool('get_window_state', dict(pid=new['pid'], window_id=new['window_id'], max_elements=0))
            require(not fresh.get('isError'), f'driver did not rediscover after epoch change: {fresh}')
            self.emit('real_driver_epoch_rediscovery', driver=str(driver), driver_sha256=sha(driver.read_bytes()),
                      pid=process.pid, old_target=target, new_target=new, stale_refusal=stale,
                      before_epoch=before['helper_epoch'], after_epoch=self.loader()['epoch'])
            stdin.close()
            require(process.wait(timeout=15) == 0, 'real MCP client did not disconnect cleanly')
            self.wait('driver disconnect cursors reaped', lambda: all(not r['cursorCount'] for r in self.observe()['records'] if r['current']))
            self.no_late_input(self.receiver())
            self.emit('real_driver_disconnect_verified', pid=process.pid, observer=self.observe())
        finally:
            selector.close()
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=15)

    def case_control(self):
        old_epoch = self.loader()['epoch']
        self.install('validation')
        self.select('validation')
        directory = self.ext / 'impl' / self.versions['validation']
        member = directory / 'main.js'
        original = member.read_bytes()
        for kind in ('digest', 'unlisted', 'symlink', 'selector'):
            if kind == 'digest':
                member.write_bytes(bytes([original[0] ^ 1]) + original[1:])
            elif kind == 'unlisted':
                (directory / 'unexpected.js').write_text('throw new Error("must not import");')
            elif kind == 'symlink':
                member.unlink()
                member.symlink_to(directory / 'policy.js')
            else:
                (self.ext / 'selected.json').write_text(json.dumps(dict(loader_abi=1, selected='../main.js', previous=None)))
            try:
                try:
                    self.loader('ReloadSelected')
                except GLib.Error as error:
                    require('org.cua.WinRects.Loader.InvalidSelection' in str(error), f'wrong selection refusal: {error}')
                    self.emit('invalid_selection_refused', kind=kind, error=str(error))
                else:
                    raise AssertionError(f'invalid {kind} selection admitted')
                require(self.loader()['epoch'] == old_epoch and self.loader()['state'] == 'ready', 'invalid selection retired healthy helper')
            finally:
                if member.is_symlink(): member.unlink()
                member.write_bytes(original)
                (directory / 'unexpected.js').unlink(missing_ok=True)
                self.select('validation')
        self.install('slow', 'slow-import')
        self.select('slow')
        reload = self.background(lambda connection: self.loader('ReloadSelected', connection))
        self.wait('slow import really evaluating', lambda: self.observe()['importStarted'])
        try:
            self.loader('ReloadSelected')
        except GLib.Error as error:
            require('org.cua.WinRects.Loader.Busy' in str(error), f'wrong concurrent reload refusal: {error}')
            self.emit('concurrent_reload_refused', error=str(error))
        else:
            raise AssertionError('concurrent reload admitted')
        subprocess.run(['gnome-extensions', 'disable', 'winrects@cua'], env=self.env, check=True, capture_output=True, timeout=8)
        self.select('v1')
        subprocess.run(['gnome-extensions', 'enable', 'winrects@cua'], env=self.env, check=True, capture_output=True, timeout=8)
        reload.result(timeout=12)
        self.ready('v1')
        require(not self.observe()['importConstructed'], 'late import constructed retired implementation')
        self.clean_retired()
        self.emit('late_import_generation_fenced', observer=self.observe())

    def case_uncertain(self):
        self.install('uncertain', 'uncertain-retirement')
        self.switch('uncertain')
        epoch = self.loader()['epoch']
        self.select('v2')
        result = self.loader('ReloadSelected')
        require(result['state'] == 'error' and 'transaction appeared during drain' in result['last_error'],
                f'uncertain retirement admitted a successor: {result}')
        require(result['epoch'] == epoch and not self.owner(BUS) and self.raw_input_held(),
                'uncertain retirement failed to retain unavailable quarantine')
        require(result['quarantined'], 'uncertain retirement missing quarantine status')
        self.emit('uncertain_retirement_quarantined', loader=result, observer=self.observe(), raw_input_held=self.raw_input_held(),
                  fixture='lab-derived implementation declares uncertainty after closing admission; no native batch submitted')

    def case_timeout(self):
        before = self.focus(self.fixture())
        self.observe('watch')
        self.observe('arm-capture')
        capture = self.background(lambda connection: self.helper('CaptureTarget', '(s)', (before['target_id'],), connection))
        try:
            self.wait('timeout capture parked after real PNG', lambda: self.observe()['capture']['nativeCompleted'])
            self.select('v2')
            result = self.loader('ReloadSelected')
            require(result['state'] == 'error' and 'retirement timed out' in result['last_error'], f'wrong retirement timeout: {result}')
            require(not self.owner(BUS), 'timed-out retirement kept helper available')
            require(result['epoch'] == before['helper_epoch'], 'timed-out retirement admitted successor')
            self.observe('release-capture')
            try: capture.result(timeout=10)
            except GLib.Error as error:
                require('org.cua.WinRects.Retired' in str(error), f'wrong timeout capture refusal: {error}')
            else: raise AssertionError('timed-out retired capture published success')
            time.sleep(0.2)
            self.select('v1')
            try: self.loader('ReloadSelected')
            except GLib.Error as error:
                require('org.cua.WinRects.Loader.Unavailable' in str(error), f'wrong poisoned refusal: {error}')
            else: raise AssertionError('late retirement completion reopened successor admission')
            require(not self.owner(BUS) and self.loader()['epoch'] == before['helper_epoch'], 'retirement poison recovered silently')
            require(self.raw_input_held(), 'retirement poison released the actual canonical raw-input lease')
            subprocess.run(['gnome-extensions', 'disable', 'winrects@cua'], env=self.env, check=True, capture_output=True, timeout=8)
            subprocess.run(['gnome-extensions', 'enable', 'winrects@cua'], env=self.env, check=True, capture_output=True, timeout=8)
            self.wait('poison survives GNOME toggle', lambda: self.owner(LOADER) and self.loader()['state'] == 'error')
            require(not self.owner(BUS) and self.raw_input_held() and self.loader()['quarantined'],
                    'GNOME toggle reopened quarantined helper or released its actual lease')
            self.emit('retirement_timeout_poison_verified', loader=self.loader(), observer=self.observe(), raw_input_held=self.raw_input_held())
        finally:
            self.observe('release-capture')
            concurrent.futures.wait([capture], timeout=12)

    def rss(self):
        values = {}
        assert self.shell is not None
        for line in Path(f'/proc/{self.shell.pid}/status').read_text().splitlines():
            if line.startswith(('VmRSS:', 'VmSize:', 'RssAnon:', 'VmHWM:')):
                key, value = line.split(':', 1)
                values[key] = int(value.split()[0])
        return values

    def case_memory(self):
        self.observe('gc')
        time.sleep(0.2)
        baseline = self.rss()
        samples = []
        for index in range(20):
            label = f'memory-{index:02}'
            self.switch(label)
            target = self.focus(self.fixture())
            # Exercise actual large helper code on every distinct imported closure.
            capture = self.helper('CaptureTargetPreview', '(su)', (target['target_id'], 128))
            require(capture['png_base64'].startswith('iVBOR'), 'memory closure did not execute real capture')
            self.observe('gc')
            time.sleep(0.05)
            sample = dict(index=index + 1, digest=self.versions[label], **self.rss())
            samples.append(sample)
            self.emit('real_closure_memory_sample', **sample)
        final = self.rss()
        self.emit('real_closure_memory_summary', distinct_real_closures=20, baseline=baseline, final=final,
                  rss_delta_kib=final['VmRSS'] - baseline['VmRSS'], samples=samples,
                  limitation='whole-process RSS, renderer/allocator noise and retained observer refs; ES modules remain cached')

    def marked_pids(self):
        pids = []
        for entry in Path('/proc').iterdir():
            if entry.name.isdigit():
                try:
                    if ('CUA_HOTRELOAD_RUN=' + self.token).encode() in (entry / 'environ').read_bytes().split(b'\0'):
                        pids.append(int(entry.name))
                except OSError:
                    pass
        return pids

    def cleanup(self):
        self.cell = 'cleanup'
        if self.connection is not None:
            try:
                if self.shell is not None and self.shell.poll() is None:
                    self.observe('release-capture')
            except GLib.Error:
                pass
        # Stop owned peers before joining futures, so a failed provider cannot hang teardown.
        for process, _, _ in reversed(self.processes):
            if process.poll() is None:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline and self.marked_pids():
            time.sleep(0.1)
        for pid in self.marked_pids():
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        for process, output, _ in self.processes:
            process.wait(timeout=4)
            output.close()
        self.pool.shutdown(wait=True, cancel_futures=True)
        if self.connection is not None and not self.connection.is_closed():
            self.connection.close_sync(None)
        time.sleep(0.1)
        remaining = self.marked_pids()
        mounts = [line for line in Path('/proc/mounts').read_text().splitlines() if str(self.root) in line]
        self.emit('cleanup_verified', remaining_marked_pids=remaining, matching_mounts=mounts,
                  children=[dict(name=name, pid=process.pid, rc=process.returncode) for process, _, name in self.processes])
        require(not remaining and not mounts, 'owned process/mount cleanup incomplete')
        self.evidence.close()
        print('EVIDENCE=' + str(self.out), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--allow-private-shell', action='store_true', help='explicitly opt in to owned native GNOME windows/input')
    parser.add_argument('--helper', type=Path, default=HERE.parent / 'winrects@cua')
    parser.add_argument('--scratch-root', type=Path, required=True, help='existing short owned scratch directory (not /tmp)')
    parser.add_argument('--qualification-lock', type=Path, required=True, help='coordinated flock shared with native/Cargo workers')
    parser.add_argument('--driver', type=Path, help='candidate binary for the real MCP epoch/disconnect cell')
    parser.add_argument('--cases', nargs='+', choices=CASES)
    parser.add_argument('--mutation', type=Path, help='JSON exact-text mutation of copied runtime/bootstrap only; never edits source')
    args = parser.parse_args()
    args.cases = args.cases or [case for case in CASES if case != 'uncertain' and (case != 'driver' or args.driver)]
    require('driver' not in args.cases or args.driver, 'driver cell requires --driver')
    if not args.allow_private_shell:
        parser.error('native lab is opt-in: pass --allow-private-shell only after candidate/worker coordination')
    require(args.qualification_lock.is_absolute(), 'qualification lock must be absolute')
    require(args.scratch_root.is_absolute(), 'scratch root must be absolute')
    # Held only for THIS lab, including its owned-process cleanup. Never an outer campaign lock.
    with args.qualification_lock.open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        process_inventory()
        os.nice(max(0, 19 - os.getpriority(os.PRIO_PROCESS, 0)))
        lab = Lab(args)
        status = 1
        try:
            lab.setup()
            lab.run()
            status = 0
        except BaseException as error:
            lab.emit('run_failed', error=str(error), traceback=traceback.format_exc())
        finally:
            lab.cleanup()
        return status


if __name__ == '__main__':
    sys.exit(main())
