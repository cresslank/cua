// Stable manager-facing bootstrap. Changing this file still requires a login.
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Shell from 'gi://Shell';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const ABI = 1;
const DEADLINE_MS = 5000;
const MAX_FILES = 64;
const MAX_BYTES = 2 * 1024 * 1024;
const SHA = /^[a-f0-9]{64}$/;
const PATH = /^[A-Za-z0-9_-]+(?:\/[A-Za-z0-9_-]+)*\.[A-Za-z0-9]+$/;
const XML = `<node><interface name="org.cua.WinRects.Loader">
<method name="GetStatus"><arg type="s" direction="out"/></method>
<method name="ReloadSelected"><arg type="s" direction="out"/></method>
</interface></node>`;
// Repeated imports of the implementation must not re-promisify GI methods.
Gio._promisify(Shell.Screenshot, 'composite_to_stream');

class ReloadBusy extends Error {}

function check(condition, message) {
    if (!condition) throw new Error(message);
}
function exactKeys(value, keys) {
    return value && typeof value === 'object' && !Array.isArray(value)
        && Object.keys(value).sort().join(',') === keys.sort().join(',');
}
function info(file, type) {
    const value = file.query_info('standard::type,standard::size,unix::uid,unix::mode',
        Gio.FileQueryInfoFlags.NOFOLLOW_SYMLINKS, null);
    check(value.get_file_type() === type, 'unexpected file type');
    check(value.get_attribute_uint32('unix::uid') === new Gio.Credentials().get_unix_user(),
        'unexpected file owner');
    check((value.get_attribute_uint32('unix::mode') & 0o022) === 0, 'writable by another user');
    return value;
}
function read(file, limit) {
    const before = info(file, Gio.FileType.REGULAR);
    check(before.get_size() <= limit, 'file exceeds loader budget');
    const [ok, bytes] = file.load_contents(null);
    check(ok && bytes.length <= limit && bytes.length === before.get_size(), 'file changed or unreadable');
    return bytes;
}
function text(bytes) { return new TextDecoder('utf-8', {fatal: true}).decode(bytes); }
function digest(bytes) { return GLib.compute_checksum_for_data(GLib.ChecksumType.SHA256, bytes); }

export default class WinRectsLoader extends Extension {
    enable() {
        // Shell calls disable synchronously, even during an async import. Never
        // return a Promise here; own both late-result fencing and all failures.
        this._wanted = true;
        this._generation = (this._generation ?? 0) + 1;
        this._state = 'loading';
        if (!this._unsafe) this._error = '';
        try {
            this._control = Gio.DBusExportedObject.wrapJSObject(XML, this);
            this._control.export(Gio.DBus.session, '/org/cua/WinRects/Loader');
            this._name = Gio.bus_own_name_on_connection(Gio.DBus.session,
                'org.cua.WinRects.Loader', Gio.BusNameOwnerFlags.NONE, null, null);
            this._start().catch(error => this._fail(error));
        } catch (error) { this._fail(error); }
    }

    GetStatus() {
        return JSON.stringify({loader_abi: ABI, selected_digest: this._selected ?? null,
            loaded_digest: this._loaded ?? null, state: this._state,
            epoch: this._runtime?._epoch ?? null, last_error: this._error ?? '',
            generation: this._generation, quarantined: Boolean(this._quarantine)});
    }

    ReloadSelectedAsync(_parameters, invocation) {
        if (!this._allowed()) {
            invocation.return_dbus_error('org.cua.WinRects.Loader.Unavailable', 'enabled, unlocked user session required');
            return;
        }
        if (this._busy) {
            invocation.return_dbus_error('org.cua.WinRects.Loader.Busy', 'reload already in progress');
            return;
        }
        if (this._unsafe) {
            invocation.return_dbus_error('org.cua.WinRects.Loader.Unavailable', 'retirement unresolved; logout required');
            return;
        }
        const blocked = this._runtime?.reloadBlockedReason();
        if (blocked) {
            this._error = blocked;
            invocation.return_dbus_error('org.cua.WinRects.Loader.Busy', blocked);
            return;
        }
        // Validate before retiring the known-good runtime. No caller path or URI.
        let selection;
        try { selection = this._selection(); }
        catch (error) {
            this._error = String(error).slice(0, 512);
            invocation.return_dbus_error('org.cua.WinRects.Loader.InvalidSelection', this._error);
            return;
        }
        this._start(selection, true).then(() => {
            invocation.return_value(new GLib.Variant('(s)', [this.GetStatus()]));
        }, error => {
            invocation.return_dbus_error(error instanceof ReloadBusy
                ? 'org.cua.WinRects.Loader.Busy' : 'org.cua.WinRects.Loader.Unavailable',
            String(error).slice(0, 512));
        });
    }

    _allowed() {
        return this._wanted && Main.sessionMode.currentMode === 'user'
            && !Main.sessionMode.isLocked;
    }

    _selection() {
        info(this.dir, Gio.FileType.DIRECTORY);
        const selected = JSON.parse(text(read(this.dir.get_child('selected.json'), 4096)));
        check(exactKeys(selected, ['loader_abi', 'selected', 'previous']) && selected.loader_abi === ABI
            && typeof selected.selected === 'string' && SHA.test(selected.selected)
            && (selected.previous === null || (typeof selected.previous === 'string' && SHA.test(selected.previous))),
            'invalid selector');
        const impl = this.dir.get_child('impl');
        info(impl, Gio.FileType.DIRECTORY);
        const directory = impl.get_child(selected.selected);
        info(directory, Gio.FileType.DIRECTORY);
        const bytes = read(directory.get_child('manifest.json'), 32768);
        check(digest(bytes) === selected.selected, 'closure manifest digest mismatch');
        const manifest = JSON.parse(text(bytes));
        check(exactKeys(manifest, ['loader_abi', 'entrypoint', 'helper_api', 'protocol', 'files'])
            && manifest.loader_abi === ABI && manifest.entrypoint === 'main.js'
            && Number.isSafeInteger(manifest.helper_api) && manifest.helper_api >= 16
            && manifest.protocol === 4 && Array.isArray(manifest.files)
            && manifest.files.length > 0 && manifest.files.length <= MAX_FILES, 'invalid manifest');
        let total = 0;
        const paths = new Set();
        for (const item of manifest.files) {
            check(exactKeys(item, ['path', 'size', 'sha256']) && typeof item.path === 'string'
                && PATH.test(item.path) && item.path.length <= 240 && item.path.split('/').length <= 8
                && item.path !== 'manifest.json' && !paths.has(item.path)
                && Number.isSafeInteger(item.size) && item.size >= 0 && item.size <= MAX_BYTES
                && typeof item.sha256 === 'string' && SHA.test(item.sha256), 'invalid closure file');
            total += item.size;
            check(total <= MAX_BYTES, 'closure exceeds loader budget');
            paths.add(item.path);
            let file = directory;
            const segments = item.path.split('/');
            for (const part of segments.slice(0, -1)) {
                file = file.get_child(part);
                info(file, Gio.FileType.DIRECTORY);
            }
            file = file.get_child(segments.at(-1));
            const content = read(file, item.size);
            check(content.length === item.size && digest(content) === item.sha256, 'closure file digest mismatch');
        }
        check(paths.has('main.js'), 'missing entrypoint');
        // Complete inventory: unmanifested files could be imported transitively.
        const walk = (dir, prefix = '') => {
            const entries = dir.enumerate_children('standard::name,standard::type',
                Gio.FileQueryInfoFlags.NOFOLLOW_SYMLINKS, null);
            try {
                let entry;
                while ((entry = entries.next_file(null))) {
                    const path = prefix + entry.get_name();
                    if (entry.get_file_type() === Gio.FileType.DIRECTORY) {
                        check([...paths].some(p => p.startsWith(path + '/')), 'unexpected closure directory');
                        walk(dir.get_child(entry.get_name()), path + '/');
                    } else {
                        check(entry.get_file_type() === Gio.FileType.REGULAR
                            && (path === 'manifest.json' || paths.has(path)), 'unexpected closure entry');
                    }
                }
            } finally { entries.close(null); }
        };
        walk(directory);
        return {digest: selected.selected, directory};
    }

    _fail(error) {
        this._error = String(error).slice(0, 512);
        this._state = this._wanted ? 'error' : 'disabled';
        // Keep GNOME ACTIVE: ERROR cannot recover via public toggles on Shell 50.
        console.warn(`WinRects loader: ${this._error}`);
    }

    async _rawInputGuard() {
        // Share the driver's canonical per-effective-UID flock, NOT XDG or HOME.
        // A child locks a dup of our open file description and exits; this stream
        // retains the actual flock across every retirement/import await. No
        // subprocess probes/reopens the pathname and no Shell input is emitted.
        const uid = new Gio.Credentials().get_unix_user();
        const parent = Gio.File.new_for_path(`/run/user/${uid}`);
        check((info(parent, Gio.FileType.DIRECTORY).get_attribute_uint32('unix::mode') & 0o777) === 0o700,
            'raw_input_guard: invalid canonical runtime directory');
        const file = parent.get_child('cua-driver-gnome-raw-input.lock');
        if (!file.query_exists(null)) {
            try { file.create_readwrite(Gio.FileCreateFlags.PRIVATE, null).close(null); }
            catch (error) { if (!error.matches(Gio.IOErrorEnum, Gio.IOErrorEnum.EXISTS)) throw error; }
        }
        info(file, Gio.FileType.REGULAR);
        const attributes = 'standard::type,unix::uid,unix::mode,unix::nlink,unix::device,unix::inode';
        const stream = file.open_readwrite(null);
        let process;
        let completed = false;
        try {
            const opened = stream.query_info(attributes, null);
            const path = file.query_info(attributes, Gio.FileQueryInfoFlags.NOFOLLOW_SYMLINKS, null);
            check(opened.get_file_type() === Gio.FileType.REGULAR && path.get_file_type() === Gio.FileType.REGULAR
                && opened.get_attribute_uint32('unix::uid') === uid
                && (opened.get_attribute_uint32('unix::mode') & 0o777) === 0o600
                && opened.get_attribute_uint32('unix::nlink') === 1
                && opened.get_attribute_uint32('unix::device') === path.get_attribute_uint32('unix::device')
                && opened.get_attribute_uint64('unix::inode') === path.get_attribute_uint64('unix::inode'),
                'raw_input_guard: invalid canonical lock inode');
            const flock = GLib.find_program_in_path('flock');
            check(flock, 'raw_input_guard: util-linux flock is unavailable');
            const launcher = new Gio.SubprocessLauncher({flags:
                Gio.SubprocessFlags.STDOUT_SILENCE | Gio.SubprocessFlags.STDERR_SILENCE});
            const fds = new Gio.UnixFDList();
            fds.append(stream.get_output_stream().get_fd());
            launcher.take_fd(fds.get(0), 3);
            for (const fd of fds.steal_fds()) GLib.close(fd);
            try { process = launcher.spawnv([flock, '-x', '-n', '-E', '75', '3']); }
            finally { launcher.close(); }
            const wait = new Promise((resolve, reject) => process.wait_async(null, (child, result) => {
                try { child.wait_finish(result); completed = true; resolve(); } catch (error) { reject(error); }
            }));
            await this._bounded(wait, 'raw_input_guard', 1000);
            if (process.get_if_exited() && process.get_exit_status() === 75)
                throw new ReloadBusy('raw_input_window_open: idle reload required');
            check(process.get_successful(), 'raw_input_guard: flock failed');
            return stream;
        } catch (error) {
            if (process && !completed) process.force_exit();
            stream.close(null);
            throw error;
        }
    }

    async _bounded(promise, label, budget = DEADLINE_MS) {
        let id;
        try {
            return await Promise.race([promise, new Promise((_, reject) => {
                id = GLib.timeout_add(GLib.PRIORITY_DEFAULT, budget, () => {
                    id = 0;
                    reject(new Error(`${label} timed out`));
                    return GLib.SOURCE_REMOVE;
                });
            })]);
        } finally { if (id) GLib.source_remove(id); }
    }

    async _retire() {
        const runtime = this._runtime;
        if (!runtime) return;
        this._state = 'retiring';
        try { await this._bounded(runtime.retire(), 'retirement'); }
        catch (error) {
            this._unsafe = true; // Even a late completion must not open admission.
            throw error;
        }
        this._runtime = null;
        this._loaded = null;
    }

    _start(selection = null, maintenance = false) {
        if (this._busy) return this._operation;
        this._busy = true;
        const generation = ++this._generation;
        let guard;
        let retiring = false;
        this._operation = (async () => {
            try {
                check(!this._unsafe, this._error || 'retirement unresolved; logout required');
                selection ??= this._selection();
                if (maintenance || this._runtime || this._activatedOnce) {
                    const blocked = this._runtime?.reloadBlockedReason();
                    if (blocked) throw new ReloadBusy(blocked);
                    guard = await this._rawInputGuard();
                    if (generation !== this._generation || !this._allowed()) return;
                    // A Begin could arrive during guard acquisition. No abort or
                    // restore makes that transaction safe to replace.
                    const appeared = this._runtime?.reloadBlockedReason();
                    if (appeared) throw new ReloadBusy(appeared);
                }
                this._selected = selection.digest;
                retiring = true;
                await this._retire();
                if (generation !== this._generation || !this._allowed()) return;
                this._state = 'loading';
                const module = await this._bounded(import(selection.directory.get_child('main.js').get_uri()), 'import');
                if (generation !== this._generation || !this._allowed()) return;
                check(typeof module.default === 'function', 'implementation factory missing');
                const runtime = new module.default({root: this, implementationDir: selection.directory,
                    digest: selection.digest});
                this._runtime = runtime; // Retire a partially enabled instance too.
                check(typeof runtime.retire === 'function' && typeof runtime.stopAdmission === 'function'
                    && typeof runtime.reloadBlockedReason === 'function', 'implementation lifecycle ABI missing');
                const enabled = runtime.enable();
                check(!(enabled instanceof Promise) && typeof runtime._epoch === 'string'
                    && runtime._epoch.length > 0 && runtime._epoch.length <= 128,
                    'implementation enable must be synchronous with a bounded epoch');
                this._loaded = selection.digest;
                this._activatedOnce = true;
                this._error = '';
                this._state = 'ready';
            } catch (error) {
                if (!retiring && error instanceof ReloadBusy) {
                    this._error = String(error).slice(0, 512);
                    this._state = this._runtime && !this._runtime._retiring ? 'ready' : 'error';
                    throw error;
                }
                if (!retiring && maintenance) {
                    this._unsafe = true;
                    this._runtime?.stopAdmission();
                    this._fail(error);
                    throw error;
                }
                if (!this._unsafe) {
                    try { await this._retire(); }
                    catch (retirement) { error = new Error(`${error}; ${retirement}`); }
                }
                this._fail(error);
            } finally {
                // An uncertain retirement retains the actual canonical flock,
                // not a reopened path, until Shell exits. Never certify it idle.
                if (guard && this._unsafe) this._quarantine = guard;
                else guard?.close(null);
                this._busy = false;
                if (generation !== this._generation && this._wanted && !this._unsafe)
                    this._start().catch(error => this._fail(error));
            }
        })();
        return this._operation;
    }

    disable() {
        this._wanted = false;
        ++this._generation;
        // Stop admission synchronously; await terminal cleanup only in our chain.
        this._runtime?.stopAdmission();
        this._control?.unexport();
        this._control = null;
        if (this._name) Gio.bus_unown_name(this._name);
        this._name = 0;
        if (!this._busy) {
            this._busy = true;
            this._operation = this._retire().catch(error => this._fail(error)).finally(() => {
                this._busy = false;
                if (this._wanted && !this._unsafe) this._start().catch(error => this._fail(error));
                else this._state = 'disabled';
            });
        }
    }
}
