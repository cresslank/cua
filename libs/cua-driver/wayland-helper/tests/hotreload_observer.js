// Lab-only peer, copied only into an owned, headless private Shell.
// No production test hooks: inspect existing instances; park only the external
// GI screenshot provider AFTER its real implementation has produced PNG bytes.
import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Shell from 'gi://Shell';
import System from 'system';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const XML = `<node><interface name="org.cua.HotReload.Observer">
<method name="Run"><arg type="s" direction="in"/><arg type="s" direction="out"/></method>
</interface></node>`;

function aliveSource(id) {
    return id > 0 && !!GLib.MainContext.default().find_source_by_id(id);
}
function handlerLive(object, id) {
    try { return GObject.signal_handler_is_connected(object, id); }
    catch (_) { return false; }
}
function actorInfo(actor) {
    try { return {parent: !!actor.get_parent(), visible: actor.visible, mapped: actor.mapped}; }
    catch (_) { return {parent: false, visible: false, mapped: false}; }
}
function sourceIds(runtime) {
    const ids = new Set();
    for (const [key, value] of Object.entries(runtime)) {
        if (key === '_sources' && (value instanceof Set || value instanceof Map)) {
            for (const id of value.keys()) if (Number.isInteger(id)) ids.add(id);
        }
        if (/Id$/.test(key) && /[Ss]ource|[Tt]imer|[Rr]eaper|[Ii]dle|[Pp]oll|[Tt]imeout|[Cc]ursorUpdate/.test(key)
            && Number.isInteger(value) && value > 0) ids.add(value);
    }
    if (runtime._foreground?.timeoutId) ids.add(runtime._foreground.timeoutId);
    for (const record of runtime._cursors?.values() ?? []) if (record.frameId) ids.add(record.frameId);
    return [...ids];
}

export default class Observer extends Extension {
    enable() {
        globalThis.__cuaHotreloadObserver = this;
        this.records = [];
        this.capture = {armed: false, entered: false, nativeCompleted: false, released: false, bytes: 0};
        this.object = Gio.DBusExportedObject.wrapJSObject(XML, this);
        this.object.export(Gio.DBus.session, '/org/cua/HotReload/Observer');
        this.name = Gio.bus_own_name_on_connection(Gio.DBus.session,
            'org.cua.HotReload.Observer', Gio.BusNameOwnerFlags.NONE, null, null);
        this.overviewTimer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 100, () => {
            Main.overview.hide();
            return GLib.SOURCE_CONTINUE;
        });
    }
    runtime() {
        const root = Main.extensionManager.lookup('winrects@cua')?.stateObj;
        return root?._runtime ?? null;
    }
    watch(runtime = this.runtime()) {
        if (!runtime) throw new Error('No active real WinRects runtime');
        let record = this.records.find(r => r.runtime === runtime);
        if (!record) {
            record = {runtime, epoch: runtime._epoch, sources: new Set(), signals: [], actors: []};
            this.records.push(record);
        }
        for (const id of sourceIds(runtime)) record.sources.add(id);
        const signals = [...(runtime._signals ?? [])];
        for (const cursor of runtime._cursors?.values() ?? []) {
            signals.push(...(cursor.targetSignals ?? []));
            for (const actor of [cursor.actor, cursor.badge]) {
                if (!actor || record.actors.some(a => a.actor === actor)) continue;
                const ref = {actor, destroyed: false};
                actor.connect('destroy', () => { ref.destroyed = true; });
                record.actors.push(ref);
            }
        }
        for (const [object, id] of signals) {
            if (!record.signals.some(s => s.object === object && s.id === id))
                record.signals.push({object, id});
        }
        return record.epoch;
    }
    status() {
        const extension = Main.extensionManager.lookup('winrects@cua');
        const runtime = this.runtime();
        return {
            shellState: extension?.state, shellError: extension?.error ?? null,
            runtimeKeys: runtime ? Object.keys(runtime) : [],
            runtimeEpoch: runtime?._epoch ?? null,
            modalCount: Main.modalCount,
            pointer: global.get_pointer(),
            sessionLocked: Main.sessionMode.isLocked,
            overviewVisible: Main.overview.visible,
            focus: global.display.focus_window?.get_stable_sequence() ?? null,
            capture: {...this.capture},
            importStarted: this.importStarted ?? false,
            importConstructed: this.importConstructed ?? false,
            windows: global.get_window_actors().map(actor => ({
                id: actor.meta_window?.get_stable_sequence(),
                title: actor.meta_window?.get_title(), ...actorInfo(actor),
            })),
            records: this.records.map(r => ({
                epoch: r.epoch, current: r.runtime === runtime,
                sourceIds: [...r.sources], liveSources: [...r.sources].filter(aliveSource),
                handlerCount: r.signals.length,
                liveHandlers: r.signals.filter(s => handlerLive(s.object, s.id)).map(s => s.id),
                actors: r.actors.map(a => ({destroyed: a.destroyed,
                    ...(a.destroyed ? {parent: false, visible: false, mapped: false} : actorInfo(a.actor))})),
                exported: !!r.runtime._impl, nameId: r.runtime._nameId ?? 0,
                nameOwnerSignalId: r.runtime._nameOwnerSignalId ?? 0,
                cursorCount: r.runtime._cursors?.size ?? 0,
                foreground: r.runtime._foreground?.transaction ?? null,
                retiring: r.runtime._retiring ?? null,
                pendingCount: r.runtime._pending?.size ?? null,
            })),
        };
    }
    armCapture() {
        if (this.originalComposite) throw new Error('Capture rendezvous already installed');
        this.capture = {armed: true, entered: false, nativeCompleted: false, released: false, bytes: 0};
        this.originalComposite = Shell.Screenshot.composite_to_stream;
        const original = this.originalComposite;
        const observer = this;
        Shell.Screenshot.composite_to_stream = async function (...args) {
            observer.capture.entered = true;
            const result = await original.apply(this, args);
            observer.capture.nativeCompleted = true;
            observer.capture.bytes = args.at(-1).get_data_size();
            await new Promise(resolve => { observer.release = resolve; });
            observer.capture.released = true;
            return result;
        };
    }
    releaseCapture() {
        if (this.originalComposite) Shell.Screenshot.composite_to_stream = this.originalComposite;
        this.originalComposite = null;
        this.release?.();
        this.release = null;
    }
    Run(json) {
        const request = JSON.parse(json);
        switch (request.op) {
        case 'status': return JSON.stringify(this.status());
        case 'watch': this.watch(); return JSON.stringify(this.status());
        case 'gc': System.gc(); return '{}';
        case 'arm-capture': this.armCapture(); return '{}';
        case 'release-capture': this.releaseCapture(); return '{}';
        case 'activate': {
            const window = global.get_window_actors().map(a => a.meta_window)
                .find(w => w?.get_stable_sequence() === request.id);
            if (!window) throw new Error('Unknown fixture window');
            window.unminimize();
            window.activate(global.get_current_time());
            return '{}';
        }
        case 'place': {
            for (const actor of global.get_window_actors()) {
                const window = actor.meta_window;
                if (window?.get_title() === 'Hotreload A') window.move_frame(false, 80, 100);
                if (window?.get_title() === 'Hotreload B') window.move_frame(false, 730, 100);
            }
            return '{}';
        }
        case 'poke': global.display.emit('restacked'); return '{}';
        case 'click': {
            this.pointer ??= Clutter.get_default_backend().get_default_seat()
                .create_virtual_device(Clutter.InputDeviceType.POINTER_DEVICE);
            const now = GLib.get_monotonic_time();
            Clutter.get_default_backend().get_default_seat().warp_pointer(request.x, request.y);
            this.pointer.notify_absolute_motion(now, request.x, request.y);
            const press = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 100, () => {
                this.pointer.notify_button(GLib.get_monotonic_time(), 1, Clutter.ButtonState.PRESSED);
                const release = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 80, () => {
                    this.pointer.notify_button(GLib.get_monotonic_time(), 1, Clutter.ButtonState.RELEASED);
                    this.clickSources.delete(release);
                    return GLib.SOURCE_REMOVE;
                });
                this.clickSources.add(release);
                this.clickSources.delete(press);
                return GLib.SOURCE_REMOVE;
            });
            this.clickSources ??= new Set();
            this.clickSources.add(press);
            return '{}';
        }
        default: throw new Error('Unknown lab observer operation');
        }
    }
    disable() {
        delete globalThis.__cuaHotreloadObserver;
        this.releaseCapture();
        if (this.overviewTimer) GLib.source_remove(this.overviewTimer);
        for (const id of this.clickSources ?? []) GLib.source_remove(id);
        this.object?.unexport();
        if (this.name) Gio.bus_unown_name(this.name);
        this.pointer = null;
    }
}
