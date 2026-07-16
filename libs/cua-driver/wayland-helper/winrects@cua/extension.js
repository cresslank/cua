import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Shell from 'gi://Shell';
import St from 'gi://St';
import Clutter from 'gi://Clutter';
import Cairo from 'cairo';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import {
    captureAreaIsSafe,
    captureContextIsSafe,
    targetIsPainted,
    targetTokenMatches,
} from './policy.js';

Gio._promisify(Shell.Screenshot.prototype, 'screenshot_area');

const PROTOCOL_VERSION = 2;
const FOREGROUND_TIMEOUT_MS = 30_000;

const IFACE = `<node><interface name="org.cua.WinRects">
<method name="GetCapabilities"><arg type="s" direction="out" name="json"/></method>
<method name="GetRects"><arg type="s" direction="out" name="json"/></method>
<method name="CaptureTarget"><arg type="s" direction="in" name="target"/><arg type="s" direction="out" name="png_base64"/></method>
<method name="BeginForeground"><arg type="s" direction="in" name="target"/><arg type="s" direction="out" name="json"/></method>
<method name="EndForeground"><arg type="s" direction="in" name="transaction"/><arg type="s" direction="out" name="json"/></method>
<method name="CommitForeground"><arg type="s" direction="in" name="transaction"/><arg type="s" direction="out" name="json"/></method>
<method name="MoveCursorFor"><arg type="s" direction="in" name="owner"/><arg type="s" direction="in" name="target"/><arg type="i" direction="in" name="x"/><arg type="i" direction="in" name="y"/></method>
<method name="ClickPulseFor"><arg type="s" direction="in" name="owner"/><arg type="s" direction="in" name="target"/><arg type="i" direction="in" name="x"/><arg type="i" direction="in" name="y"/></method>
<method name="HideCursorFor"><arg type="s" direction="in" name="owner"/></method>
<method name="RemoveCursor"><arg type="s" direction="in" name="owner"/></method>
<!-- v1 compatibility methods deliberately fail closed. An old driver must not
     render Shell-global chrome or activate/capture an unqualified target. -->
<method name="Capture"><arg type="s" direction="out" name="png_base64"/></method>
<method name="Activate"><arg type="u" direction="in" name="id"/><arg type="b" direction="out" name="activated"/></method>
<method name="MoveCursor"><arg type="i" direction="in" name="x"/><arg type="i" direction="in" name="y"/></method>
<method name="ClickPulse"><arg type="i" direction="in" name="x"/><arg type="i" direction="in" name="y"/></method>
<method name="HideCursor"></method>
</interface></node>`;

// The same agent cursor cua-driver renders on every other platform: the
// procedural gradient arrow from cursor-overlay (verts tip-at-+x, `default_blue`
// palette). Rotated to point up-left and translated so the tip sits at the
// actor's (TIPX, TIPY); MoveCursor/ClickPulse place that tip on the target.
const VERTS = [[14, 0], [-8, -9], [-3, 0], [-8, 9]];
const ANGLE = Math.PI * 1.25;           // tip points up-left
const TIPX = 2, TIPY = 2;
function arrowPoints() {
    const ca = Math.cos(ANGLE), sa = Math.sin(ANGLE);
    const rot = VERTS.map(([x, y]) => [ca * x - sa * y, sa * x + ca * y]);
    const tx = TIPX - rot[0][0], ty = TIPY - rot[0][1];
    return rot.map(([x, y]) => [x + tx, y + ty]);
}

export default class WinRectsExtension extends Extension {
    enable() {
        this._epoch = GLib.uuid_string_random();
        this._cursors = new Map();
        this._signals = [];
        this._foreground = null;
        this._impl = Gio.DBusExportedObject.wrapJSObject(IFACE, this);
        this._impl.export(Gio.DBus.session, '/org/cua/WinRects');
        this._nameId = Gio.bus_own_name(Gio.BusType.SESSION, 'org.cua.WinRects',
            Gio.BusNameOwnerFlags.REPLACE, null, null, null);

        const update = () => this._scheduleCursorVisibilityUpdate();
        this._connect(global.workspace_manager, 'active-workspace-changed', update);
        this._connect(global.display, 'restacked', update);
        this._connect(global.display, 'window-created', update);
        this._connect(global.display, 'window-entered-monitor', update);
        this._connect(global.display, 'window-left-monitor', update);
        this._connect(Main.layoutManager, 'monitors-changed', update);
    }

    disable() {
        if (this._foreground)
            this._finishForeground(this._foreground.transaction, 'extension-disabled');
        for (const [object, id] of this._signals) {
            try { object.disconnect(id); } catch (_error) {}
        }
        this._signals = [];
        for (const owner of [...this._cursors.keys()])
            this._removeCursor(owner);
        this._cursors.clear();
        if (this._impl) { this._impl.unexport(); this._impl = null; }
        if (this._nameId) { Gio.bus_unown_name(this._nameId); this._nameId = 0; }
    }

    _connect(object, signal, callback) {
        try {
            this._signals.push([object, object.connect(signal, callback)]);
        } catch (_error) {
            // Signals vary across supported Shell releases. Cursor visibility
            // is also revalidated on every command, so a missing optional
            // signal cannot make an action target a different window.
        }
    }

    _createCursorActor() {
        const cursor = new St.DrawingArea({
            width: 30,
            height: 30,
            visible: false,
            reactive: false,
            can_focus: false,
        });
        cursor.connect('repaint', area => {
            const cr = area.get_context();
            const P = arrowPoints();
            cr.moveTo(P[0][0], P[0][1]);
            for (let i = 1; i < P.length; i++) cr.lineTo(P[i][0], P[i][1]);
            cr.closePath();
            const tail = [(P[1][0] + P[3][0]) / 2, (P[1][1] + P[3][1]) / 2];
            try {
                const g = new Cairo.LinearGradient(P[0][0], P[0][1], tail[0], tail[1]);
                g.addColorStopRGBA(0.00, 219 / 255, 238 / 255, 255 / 255, 0.97);
                g.addColorStopRGBA(0.53, 94 / 255, 192 / 255, 232 / 255, 0.97);
                g.addColorStopRGBA(1.00, 84 / 255, 205 / 255, 160 / 255, 0.97);
                cr.setSource(g);
            } catch (e) {
                cr.setSourceRGBA(94 / 255, 192 / 255, 232 / 255, 0.97);
            }
            cr.fillPreserve();
            cr.setLineWidth(1.4); cr.setSourceRGBA(1, 1, 1, 0.95); cr.stroke();
            cr.$dispose();
        });
        cursor.set_pivot_point(TIPX / 30, TIPY / 30);
        Main.layoutManager.addTopChrome(cursor);
        return cursor;
    }

    _targetId(window) {
        return `${this._epoch}:${window.get_stable_sequence()}`;
    }

    _resolveTarget(targetId) {
        if (!targetTokenMatches(this._epoch, targetId))
            return null;
        const sequence = Number.parseInt(targetId.slice(this._epoch.length + 1), 10);
        if (!Number.isSafeInteger(sequence) || sequence <= 0)
            return null;
        return global.get_window_actors()
            .map(actor => actor.meta_window)
            .find(window => window?.get_stable_sequence() === sequence) ?? null;
    }

    _actorFor(window) {
        return global.get_window_actors()
            .find(actor => actor.meta_window === window) ?? null;
    }

    _isTargetVisible(window) {
        if (!window)
            return false;
        if (!captureContextIsSafe({
            overviewVisible: Main.overview?.visible,
            sessionLocked: Main.sessionMode?.isLocked,
        }))
            return false;
        const actor = this._actorFor(window);
        let shellShowing = false;
        try {
            shellShowing = window.showing_on_its_workspace();
        } catch (_error) {
            const workspace = window.get_workspace();
            shellShowing = window.is_on_all_workspaces()
                || workspace === global.workspace_manager.get_active_workspace();
        }
        return targetIsPainted({
            actorVisible: actor?.visible,
            minimized: window.minimized,
            shellShowing,
        });
    }

    _windowAppId(window) {
        for (const getter of ['get_gtk_application_id', 'get_sandboxed_app_id', 'get_wm_class']) {
            try {
                const value = window[getter]?.call(window);
                if (value)
                    return value;
            } catch (_error) {}
        }
        return '';
    }

    GetCapabilities() {
        return JSON.stringify({
            protocol_version: PROTOCOL_VERSION,
            epoch: this._epoch,
            capabilities: [
                'exact-target-v2',
                'workspace-metadata',
                'target-stage-capture',
                'keyed-target-cursors',
                'foreground-transaction',
            ],
        });
    }

    GetRects() {
        const actors = global.get_window_actors();
        const actorByWindow = new Map();
        for (const actor of actors) {
            if (actor.meta_window)
                actorByWindow.set(actor.meta_window, actor);
        }
        const windows = global.display.sort_windows_by_stacking([...actorByWindow.keys()]);
        const focusedWindow = global.display.focus_window;
        const out = [];
        for (let stacking = 0; stacking < windows.length; stacking++) {
            const w = windows[stacking];
            const actor = actorByWindow.get(w);
            const r = w.get_frame_rect();
            let buffer = r;
            try {
                buffer = w.get_buffer_rect();
            } catch (_error) {
                // Older Shell releases may not expose the buffer rectangle.
            }
            const minimized = Boolean(w.minimized);
            const workspace = w.get_workspace();
            const activeWorkspace = global.workspace_manager.get_active_workspace();
            let sticky = false;
            try { sticky = Boolean(w.is_on_all_workspaces()); } catch (_error) {}
            let workspaceIndex = -1;
            try { workspaceIndex = workspace?.index() ?? -1; } catch (_error) {}
            const captureCurrent = this._isTargetVisible(w);
            out.push({
                id: w.get_stable_sequence(),
                target_id: this._targetId(w),
                helper_epoch: this._epoch,
                protocol_version: PROTOCOL_VERSION,
                pid: w.get_pid(),
                app_id: this._windowAppId(w),
                title: w.get_title() ?? '',
                x: r.x,
                y: r.y,
                w: r.width,
                h: r.height,
                buffer_x: buffer.x,
                buffer_y: buffer.y,
                focused: focusedWindow === w,
                minimized,
                visible: captureCurrent,
                capture_current: captureCurrent,
                workspace_index: workspaceIndex,
                workspace_active: sticky || workspace === activeWorkspace,
                workspace_count: global.workspace_manager.n_workspaces,
                sticky,
                monitor: w.get_monitor(),
                monitor_primary: w.get_monitor() === Main.layoutManager.primaryIndex,
                stacking,
            });
        }
        return JSON.stringify(out);
    }

    async CaptureTargetAsync([targetId], invocation) {
        try {
            const target = this._resolveTarget(targetId);
            if (!target)
                throw new Error('stale_target: target belongs to another helper incarnation or no longer exists');
            if (!this._isTargetVisible(target))
                throw new Error('capture_foreground_required: target is not currently painted on the GNOME stage');
            const [displayWidth, displayHeight] = global.display.get_size();
            const [stageWidth, stageHeight] = global.stage.get_size();
            if (!captureAreaIsSafe({displayWidth, displayHeight, stageWidth, stageHeight}))
                throw new Error(
                    'capture_not_ready: refusing GNOME screenshot with invalid '
                    + `display ${displayWidth}x${displayHeight} or stage ${stageWidth}x${stageHeight}`
                );
            const width = Math.floor(displayWidth);
            const height = Math.floor(displayHeight);
            const shooter = new Shell.Screenshot();
            const stream = Gio.MemoryOutputStream.new_resizable();
            // Never call Shell.Screenshot.screenshot() here. GNOME 50 can pass
            // an implicit 0x0 stage view into Cogl immediately after a window
            // activation, which crashes Shell even when display.get_size() is
            // positive. screenshot_area() allocates the explicit positive full-
            // display rectangle and omits the real cursor; Rust keeps applying
            // the exact target crop to this full-display PNG.
            await shooter.screenshot_area(0, 0, width, height, stream);
            stream.close(null);
            const encoded = GLib.base64_encode(stream.steal_as_bytes().get_data());
            invocation.return_value(new GLib.Variant('(s)', [encoded]));
        } catch (error) {
            invocation.return_dbus_error('org.cua.WinRects.CaptureFailed', String(error));
        }
    }

    BeginForegroundAsync([targetId], invocation) {
        if (this._foreground) {
            invocation.return_dbus_error(
                'org.cua.WinRects.InputBusy',
                'input_busy: another GNOME foreground transaction is active'
            );
            return;
        }
        const target = this._resolveTarget(targetId);
        if (!target) {
            invocation.return_dbus_error(
                'org.cua.WinRects.StaleTarget',
                'stale_target: target belongs to another helper incarnation or no longer exists'
            );
            return;
        }
        const priorWindow = global.display.focus_window;
        const priorWorkspace = global.workspace_manager.get_active_workspace();
        const transaction = GLib.uuid_string_random();
        this._foreground = {
            transaction,
            target,
            targetId,
            priorWindow,
            priorWorkspace,
            timeoutId: 0,
        };
        target.activate(global.get_current_time());
        GLib.timeout_add(GLib.PRIORITY_DEFAULT, 100, () => {
            const active = this._foreground?.transaction === transaction &&
                global.display.focus_window === target;
            if (!active) {
                this._foreground = null;
                invocation.return_value(new GLib.Variant('(s)', [JSON.stringify({
                    activated: false,
                    reason: 'activation_not_confirmed',
                })]));
                return GLib.SOURCE_REMOVE;
            }
            this._foreground.timeoutId = GLib.timeout_add(
                GLib.PRIORITY_DEFAULT,
                FOREGROUND_TIMEOUT_MS,
                () => {
                    this._finishForeground(transaction, 'deadline');
                    return GLib.SOURCE_REMOVE;
                }
            );
            let priorWorkspaceIndex = -1;
            try { priorWorkspaceIndex = priorWorkspace?.index() ?? -1; } catch (_error) {}
            invocation.return_value(new GLib.Variant('(s)', [JSON.stringify({
                activated: true,
                transaction,
                target: targetId,
                prior_workspace_index: priorWorkspaceIndex,
                prior_window: priorWindow ? this._targetId(priorWindow) : null,
            })]));
            return GLib.SOURCE_REMOVE;
        });
    }

    EndForeground(transaction) {
        return JSON.stringify(this._finishForeground(transaction, 'complete'));
    }

    CommitForeground(transaction) {
        const foreground = this._foreground;
        if (!foreground || foreground.transaction !== transaction)
            return JSON.stringify({committed: false, reason: 'stale_transaction'});
        this._foreground = null;
        if (foreground.timeoutId) {
            try { GLib.source_remove(foreground.timeoutId); } catch (_error) {}
        }
        return JSON.stringify({committed: true});
    }

    _finishForeground(transaction, reason) {
        const foreground = this._foreground;
        if (!foreground || foreground.transaction !== transaction)
            return {restored: false, reason: 'stale_transaction'};
        this._foreground = null;
        if (foreground.timeoutId) {
            try { GLib.source_remove(foreground.timeoutId); } catch (_error) {}
        }
        if (global.display.focus_window !== foreground.target)
            return {restored: false, reason: 'user_focus_changed'};

        const windows = global.get_window_actors().map(actor => actor.meta_window);
        if (foreground.priorWindow && foreground.priorWindow !== foreground.target &&
            windows.includes(foreground.priorWindow)) {
            foreground.priorWindow.activate(global.get_current_time());
            return {restored: true, reason, restored_to: 'window'};
        }
        if (foreground.priorWindow === foreground.target)
            return {restored: false, reason: 'target_was_already_focused'};
        try {
            foreground.priorWorkspace?.activate(global.get_current_time());
            return {restored: true, reason, restored_to: 'workspace'};
        } catch (_error) {
            return {restored: false, reason: 'prior_context_unavailable'};
        }
    }

    _cursorFor(owner) {
        if (!owner)
            return null;
        let record = this._cursors.get(owner);
        if (!record) {
            record = {
                actor: this._createCursorActor(),
                targetId: null,
                requestedVisible: false,
                targetSignals: [],
                targetWindow: null,
            };
            this._cursors.set(owner, record);
        }
        return record;
    }

    _bindCursorTarget(record, target) {
        if (record.targetWindow === target)
            return;
        for (const [object, id] of record.targetSignals) {
            try { object.disconnect(id); } catch (_error) {}
        }
        record.targetSignals = [];
        record.targetWindow = target;
        if (!target)
            return;
        const update = () => this._scheduleCursorVisibilityUpdate();
        for (const signal of ['workspace-changed', 'notify::minimized', 'unmanaged']) {
            try { record.targetSignals.push([target, target.connect(signal, update)]); } catch (_error) {}
        }
        const actor = this._actorFor(target);
        try { record.targetSignals.push([actor, actor.connect('notify::visible', update)]); } catch (_error) {}
    }

    _syncCursorVisibility(record) {
        const target = record.targetId ? this._resolveTarget(record.targetId) : null;
        this._bindCursorTarget(record, target);
        const visible = record.requestedVisible && target && this._isTargetVisible(target);
        if (visible)
            record.actor.show();
        else
            record.actor.hide();
    }

    _scheduleCursorVisibilityUpdate() {
        if (this._cursorUpdatePending)
            return;
        this._cursorUpdatePending = true;
        GLib.idle_add(GLib.PRIORITY_DEFAULT_IDLE, () => {
            this._cursorUpdatePending = false;
            for (const record of this._cursors.values())
                this._syncCursorVisibility(record);
            return GLib.SOURCE_REMOVE;
        });
    }

    MoveCursorFor(owner, targetId, x, y) {
        const record = this._cursorFor(owner);
        if (!record)
            return;
        record.targetId = targetId;
        record.requestedVisible = true;
        this._syncCursorVisibility(record);
        record.actor.ease({
            x: x - TIPX,
            y: y - TIPY,
            duration: 480,
            mode: Clutter.AnimationMode.EASE_OUT_CUBIC,
        });
    }

    ClickPulseFor(owner, targetId, x, y) {
        const record = this._cursorFor(owner);
        if (!record)
            return;
        record.targetId = targetId;
        record.requestedVisible = true;
        record.actor.set_position(x - TIPX, y - TIPY);
        this._syncCursorVisibility(record);
        record.actor.ease({
            scale_x: 1.5,
            scale_y: 1.5,
            duration: 130,
            mode: Clutter.AnimationMode.EASE_OUT_QUAD,
            onComplete: () => {
                if (record.actor)
                    record.actor.ease({scale_x: 1, scale_y: 1, duration: 130});
            },
        });
    }

    HideCursorFor(owner) {
        const record = this._cursors.get(owner);
        if (!record)
            return;
        record.requestedVisible = false;
        record.actor.hide();
    }

    RemoveCursor(owner) {
        this._removeCursor(owner);
    }

    _removeCursor(owner) {
        const record = this._cursors.get(owner);
        if (!record)
            return;
        for (const [object, id] of record.targetSignals) {
            try { object.disconnect(id); } catch (_error) {}
        }
        record.actor.destroy();
        this._cursors.delete(owner);
    }

    // WinRects v1 compatibility methods fail closed. Keeping the D-Bus
    // signatures makes version skew deterministic rather than accidentally
    // falling through to global Shell chrome or unqualified activation.
    CaptureAsync(_params, invocation) {
        invocation.return_dbus_error(
            'org.cua.WinRects.UpgradeRequired',
            'background_unavailable: WinRects protocol v2 requires exact target capture'
        );
    }
    ActivateAsync(_params, invocation) {
        invocation.return_value(new GLib.Variant('(b)', [false]));
    }
    MoveCursor(_x, _y) {}
    ClickPulse(_x, _y) {}
    HideCursor() {}
}
