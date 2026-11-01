// Test-only controller: loaded exclusively by the private GNOME Shell lab.
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const XML = `<node><interface name="org.cua.SwitchLab"><method name="Setup"><arg type="s" direction="in"/><arg type="s" direction="out"/></method><method name="Observe"><arg type="s" direction="out"/></method></interface></node>`;

export default class SwitchLab extends Extension {
    enable() {
        this._object = Gio.DBusExportedObject.wrapJSObject(XML, this);
        this._object.export(Gio.DBus.session, '/org/cua/SwitchLab');
        this._name = Gio.bus_own_name_on_connection(Gio.DBus.session, 'org.cua.SwitchLab', Gio.BusNameOwnerFlags.NONE, null, null);
        this._denying = false;
        this._signal = Main.overview.connect('showing', () => {
            if (!this._denying) return;
            // Lose Shell focus while the driver is waiting for positive proof.
            // An implementation which types on a timer leaks into this entry.
            this._idle = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 10, () => {
                this._idle = 0;
                Main.overview.hide();
                const client = this._window('cua-lab-cover');
                if (client) Main.activateWindow(client);
                return GLib.SOURCE_REMOVE;
            });
        });
    }

    _window(title) {
        return global.get_window_actors().map(a => a.meta_window).find(w => w.get_title() === title);
    }

    Observe() {
        const target = this._window('cua-lab-probe');
        const focused = global.display.focus_window;
        return JSON.stringify({target: target?.get_stable_sequence(), pid: target?.get_pid(),
            minimized: target?.minimized, workspace: target?.get_workspace()?.index(),
            active_workspace: global.workspace_manager.get_active_workspace_index(),
            overview: Main.overview.visible, focus: focused?.get_title(), focus_pid: focused?.get_pid(), post_proof_hit: this._postProofHit === true});
    }

    Setup(mode) {
        const target = this._window('cua-lab-probe');
        const cover = this._window('cua-lab-cover');
        if (!target || !cover) throw new Error('missing private GTK fixtures');
        this._denying = mode === 'refusal';
        this._postProofHit = false;
        const runtime = Main.extensionManager.lookup('winrects@cua')?.stateObj?._runtime;
        if (!runtime) throw new Error('missing private helper runtime');
        if (!this._originalFocusSearch) this._originalFocusSearch = runtime.FocusSwitchSearch;
        runtime.FocusSwitchSearch = (...args) => {
            if (mode === 'refusal') {
                Main.overview.hide();
                Main.activateWindow(cover);
                global.stage.set_key_focus(null);
                return JSON.stringify({...JSON.parse(runtime.GetSwitchState()), code: 'shell_not_ready'});
            }
            const raw = this._originalFocusSearch.apply(runtime, args);
            const proof = JSON.parse(raw);
            if (mode === 'post_proof' && !this._postProofHit && !proof.code && proof.shell_search_has_focus) {
                // Test-only deterministic rendezvous: the real helper has already
                // proven focus, but the driver has not yet received that proof.
                this._postProofHit = true;
                Main.overview.hide();
                target.unminimize();
                Main.activateWindow(target);
                global.stage.set_key_focus(null);
            }
            return raw;
        };
        Main.overview.hide();
        if (global.workspace_manager.n_workspaces < 2)
            global.workspace_manager.append_new_workspace(false, global.get_current_time());
        target.change_workspace_by_index(0, false);
        cover.change_workspace_by_index(0, false);
        Main.activateWindow(cover);
        if (mode === 'minimized' || mode === 'keyboard' || mode === 'refusal' || mode === 'post_proof') target.minimize();
        if (mode === 'workspace') target.change_workspace_by_index(1, false);
        if (mode === 'overview') Main.overview.show();
        return this.Observe();
    }

    disable() {
        this._denying = false;
        const runtime = Main.extensionManager.lookup('winrects@cua')?.stateObj?._runtime;
        if (runtime && this._originalFocusSearch) runtime.FocusSwitchSearch = this._originalFocusSearch;
        if (this._idle) GLib.source_remove(this._idle);
        if (this._signal) Main.overview.disconnect(this._signal);
        this._object?.unexport();
        if (this._name) Gio.bus_unown_name(this._name);
    }
}
