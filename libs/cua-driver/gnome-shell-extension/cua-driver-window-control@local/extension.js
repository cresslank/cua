import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const DBUS_NAME = 'org.trycua.Driver.GnomeShell';
const DBUS_PATH = '/org/trycua/Driver/GnomeShell';
const DBUS_IFACE = 'org.trycua.Driver.GnomeShell';

const IFACE_XML = `
<node>
  <interface name="${DBUS_IFACE}">
    <method name="ListWindowsJson">
      <arg type="s" name="json" direction="out"/>
    </method>
    <method name="GetFocusedWindowJson">
      <arg type="s" name="json" direction="out"/>
    </method>
    <method name="ActivateWindow">
      <arg type="u" name="id" direction="in"/>
      <arg type="b" name="ok" direction="out"/>
    </method>
    <method name="MoveResizeWindow">
      <arg type="u" name="id" direction="in"/>
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
      <arg type="i" name="width" direction="in"/>
      <arg type="i" name="height" direction="in"/>
      <arg type="b" name="ok" direction="out"/>
    </method>
    <method name="MoveWindowToWorkspace">
      <arg type="u" name="id" direction="in"/>
      <arg type="i" name="workspace_index" direction="in"/>
      <arg type="b" name="ok" direction="out"/>
    </method>
    <method name="SwitchWorkspace">
      <arg type="i" name="workspace_index" direction="in"/>
      <arg type="b" name="ok" direction="out"/>
    </method>
  </interface>
</node>`;

function _callString(obj, methodName) {
    try {
        if (obj && typeof obj[methodName] === 'function') {
            const value = obj[methodName]();
            return value === null || value === undefined ? '' : String(value);
        }
    } catch (_e) {
        return '';
    }
    return '';
}

function _callNumber(obj, methodName, fallback = 0) {
    try {
        if (obj && typeof obj[methodName] === 'function') {
            const value = Number(obj[methodName]());
            return Number.isFinite(value) ? value : fallback;
        }
    } catch (_e) {
        return fallback;
    }
    return fallback;
}

function _frameRect(win) {
    try {
        const rect = win.get_frame_rect();
        return {
            x: Number(rect.x) || 0,
            y: Number(rect.y) || 0,
            width: Number(rect.width) || 0,
            height: Number(rect.height) || 0,
        };
    } catch (_e) {
        return {x: 0, y: 0, width: 0, height: 0};
    }
}

function _workspaceIndex(win) {
    try {
        const workspace = win.get_workspace();
        if (workspace && typeof workspace.index === 'function')
            return Number(workspace.index());
    } catch (_e) {
    }
    return -1;
}

function _windowType(win) {
    try {
        const type = win.get_window_type();
        for (const [name, value] of Object.entries(Meta.WindowType)) {
            if (value === type)
                return name.toLowerCase();
        }
    } catch (_e) {
    }
    return '';
}

function _isSkippable(win) {
    try {
        if (!win)
            return true;
        if (typeof win.is_override_redirect === 'function' && win.is_override_redirect())
            return true;
        const type = win.get_window_type?.();
        if (type !== undefined) {
            // Keep normal dialogs and normal windows. Skip desktop/dock/dropdown
            // shell surfaces that are not useful automation targets.
            if (![Meta.WindowType.NORMAL, Meta.WindowType.DIALOG, Meta.WindowType.MODAL_DIALOG].includes(type))
                return true;
        }
    } catch (_e) {
        return true;
    }
    return false;
}

export default class CuaDriverWindowControlExtension extends Extension {
    enable() {
        this._nextId = 1;
        this._idByWindow = new Map();
        this._windowById = new Map();
        this._dbus = Gio.DBusExportedObject.wrapJSObject(IFACE_XML, this);
        this._dbus.export(Gio.DBus.session, DBUS_PATH);
        this._busOwnerId = Gio.bus_own_name(
            Gio.BusType.SESSION,
            DBUS_NAME,
            Gio.BusNameOwnerFlags.REPLACE,
            null,
            null,
            null,
        );
    }

    disable() {
        if (this._dbus) {
            try {
                this._dbus.unexport();
            } catch (_e) {
            }
            this._dbus = null;
        }
        if (this._busOwnerId) {
            Gio.bus_unown_name(this._busOwnerId);
            this._busOwnerId = 0;
        }
        this._idByWindow = null;
        this._windowById = null;
    }

    _currentTime() {
        try {
            return global.get_current_time();
        } catch (_e) {
            return GLib.get_monotonic_time() & 0xffffffff;
        }
    }

    _assignId(win) {
        let id = this._idByWindow.get(win);
        if (!id) {
            id = this._nextId++;
            this._idByWindow.set(win, id);
        }
        return id;
    }

    _refreshWindows() {
        const records = [];
        const live = new Map();
        const actors = global.get_window_actors?.() || [];
        const focusWindow = global.display?.focus_window || null;

        for (const actor of actors) {
            const win = actor?.meta_window;
            if (_isSkippable(win))
                continue;

            const id = this._assignId(win);
            live.set(id, win);
            const rect = _frameRect(win);
            const appId = _callString(win, 'get_gtk_application_id') ||
                _callString(win, 'get_sandboxed_app_id') ||
                _callString(win, 'get_wm_class_instance') ||
                _callString(win, 'get_wm_class');
            const wmClass = _callString(win, 'get_wm_class');
            const pid = _callNumber(win, 'get_pid', 0);
            const minimized = Boolean(win.minimized);
            const visible = !minimized && actor.visible !== false;

            records.push({
                backend: 'gnome',
                id,
                title: _callString(win, 'get_title'),
                app_id: appId,
                wm_class: wmClass,
                pid,
                workspace: _workspaceIndex(win),
                focused: win === focusWindow || Boolean(win.has_focus?.()),
                minimized,
                visible,
                window_type: _windowType(win),
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            });
        }

        this._windowById = live;
        return records;
    }

    _windowForId(id) {
        this._refreshWindows();
        return this._windowById.get(Number(id)) || null;
    }

    ListWindowsJson() {
        return JSON.stringify(this._refreshWindows());
    }

    GetFocusedWindowJson() {
        const focused = global.display?.focus_window || null;
        const records = this._refreshWindows();
        if (!focused)
            return '{}';
        const id = this._idByWindow.get(focused);
        return JSON.stringify(records.find(record => record.id === id) || {});
    }

    ActivateWindow(id) {
        const win = this._windowForId(id);
        if (!win)
            return false;
        try {
            win.activate(this._currentTime());
            return true;
        } catch (_e) {
            return false;
        }
    }

    MoveResizeWindow(id, x, y, width, height) {
        const win = this._windowForId(id);
        if (!win)
            return false;
        try {
            const w = Math.max(1, Number(width) || 1);
            const h = Math.max(1, Number(height) || 1);
            if (typeof win.move_resize_frame === 'function') {
                win.move_resize_frame(true, Number(x) || 0, Number(y) || 0, w, h);
                return true;
            }
            if (typeof win.move_frame === 'function') {
                win.move_frame(true, Number(x) || 0, Number(y) || 0);
                return true;
            }
        } catch (_e) {
        }
        return false;
    }

    MoveWindowToWorkspace(id, workspaceIndex) {
        const win = this._windowForId(id);
        if (!win)
            return false;
        try {
            const workspace = global.workspace_manager.get_workspace_by_index(Number(workspaceIndex));
            if (!workspace)
                return false;
            win.change_workspace(workspace);
            return true;
        } catch (_e) {
            return false;
        }
    }

    SwitchWorkspace(workspaceIndex) {
        try {
            const workspace = global.workspace_manager.get_workspace_by_index(Number(workspaceIndex));
            if (!workspace)
                return false;
            workspace.activate(this._currentTime());
            return true;
        } catch (_e) {
            return false;
        }
    }
}
