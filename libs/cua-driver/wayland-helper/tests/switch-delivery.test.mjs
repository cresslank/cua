import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import vm from 'node:vm';
import test from 'node:test';

// Execute the shipped methods, not a second policy implementation. GI/compositor
// objects are the external edge; no desktop input or host state in plain tests.
const main = await readFile(new URL('../winrects@cua/impl/main.js', import.meta.url), 'utf8');
const policySource = await readFile(new URL('../winrects@cua/impl/policy.js', import.meta.url), 'utf8');
const policy = await import(`data:text/javascript;base64,${Buffer.from(policySource).toString('base64')}`);
function fixture() {
    const events = [];
    const focusRequests = [];
    const target = {get_stable_sequence: () => 7, get_pid: () => 42, unminimize: () => events.push('unminimize')};
    const shell = {get_parent: () => null};
    const entry = {mapped: true, get_parent: () => shell, has_key_focus: () => global.stage.focus === entry,
        grab_key_focus: () => { focusRequests.push(entry); global.stage.focus = entry; },
        set_text: text => { entry.text = text; events.push(['text', text]); }, get_text: () => entry.text};
    const global = {stage: {focus: entry, get_key_focus() { return this.focus; }},
        display: {focus_window: target}, get_current_time: () => 7};
    const Main = {uiGroup: shell, modalCount: 1, sessionMode: {isLocked: false, currentMode: 'user'},
        layoutManager: {monitors: [{width: 1920, height: 1080}], primaryIndex: 0},
        activateWindow: window => { assert.equal(window, target); events.push('activate'); },
        overview: {visible: true, visibleTarget: true, animationInProgress: false,
            searchEntry: {clutter_text: entry},
            hide() { this.visibleTarget = false; this.animationInProgress = true; events.push('hide'); }}};
    const app = {get_windows: () => [target], get_id: () => 'probe.desktop',
        activate_window: (window, time) => { assert.equal(window, target); assert.equal(time, 7); events.push('activate'); }};
    const Shell = {WindowTracker: {get_default: () => ({get_window_app: () => app})},
        AppSystem: {search: query => { events.push(['search', query]); return [['probe.desktop']]; }}};
    const forbiddenInput = () => { events.push('global-keyboard-input'); };
    const Clutter = {InputDeviceType: {KEYBOARD_DEVICE: 1}, KeyState: {PRESSED: 1}, KEY_Return: 65293,
        get_default_backend: () => ({get_default_seat: () => ({create_virtual_device: () => ({notify_keyval: forbiddenInput})})})};
    const context = vm.createContext({...policy, Main, Shell, Clutter, global,
        Gio: {Settings: class { get_string() { return 'Super_L'; } }}});
    const code = main.replace(/^import[\s\S]*?;\n/gm, '').replace('export default class WinRectsRuntime', 'class WinRectsRuntime');
    vm.runInContext(`${code}\nglobalThis.Runtime = WinRectsRuntime;`, context);
    const runtime = Object.create(context.Runtime.prototype);
    runtime._epoch = 'epoch'; runtime._resolveTarget = id => id === 'epoch:7' ? target : null;
    return {runtime, events, focusRequests, Main, global, entry, Shell, app, target};
}

const lifecycleUnavailable = [
    ['showing', f => { f.Main.overview.animationInProgress = true; }],
    ['hiding', f => { f.Main.overview.visibleTarget = false; f.Main.overview.animationInProgress = true; }],
    ['hide target without animation', f => { f.Main.overview.visibleTarget = false; }],
    ...[undefined, null, 0, 'false'].map(value => [`unknown animation ${String(value)}`,
        f => { f.Main.overview.animationInProgress = value; }]),
    ...[undefined, null, 1, 'true'].map(value => [`unknown visible target ${String(value)}`,
        f => { f.Main.overview.visibleTarget = value; }]),
];

const monitorUnavailable = [
    ['all disconnected', f => { f.Main.layoutManager = {monitors: [], primaryIndex: -1}; }],
    ['empty layout', f => { f.Main.layoutManager.monitors = []; }],
    ['missing layout metadata', f => { delete f.Main.layoutManager; }],
    ...[undefined, null, {length: 1, 0: {width: 1920, height: 1080}}].map(value => [`invalid monitors ${String(value)}`,
        f => { f.Main.layoutManager.monitors = value; }]),
    ...[undefined, null, -1, 1, 0.5, '0', NaN].map(value => [`invalid primary index ${String(value)}`,
        f => { f.Main.layoutManager.primaryIndex = value; }]),
    ...['width', 'height'].flatMap(axis => [undefined, null, 0, -1, NaN, Infinity].map(value =>
        [`invalid primary ${axis} ${String(value)}`, f => { f.Main.layoutManager.monitors[0][axis] = value; }])),
];

function assertFocusRefused(change, label) {
    const f = fixture();
    f.global.stage.focus = null;
    change(f);
    const reply = JSON.parse(f.runtime.FocusSwitchSearch('epoch:7'));
    assert.equal(f.focusRequests.length, 0, `focus must not be grabbed: ${label}`);
    assert.equal(reply.code, 'shell_not_ready', label);
    assert.deepEqual(f.events, [], label);
}

function assertDeliveryRefused(change, label) {
    const f = fixture();
    assert.equal(JSON.parse(f.runtime.FocusSwitchSearch('epoch:7')).shell_search_has_focus, true);
    change(f); // receiver must recheck AFTER the driver's earlier focus proof
    const reply = JSON.parse(f.runtime.DeliverSwitchSearch('epoch:7', 'NEVER REACH CLIENT'));
    assert.equal(reply.delivered, false, label);
    assert.equal(reply.code, 'shell_not_ready', label);
    assert.deepEqual(f.events, [], label);
}

test('helper API 18 advertises additive Shell search lifecycle capability', () => {
    const f = fixture();
    assert.equal(f.runtime.GetVersion(), 18);
    const caps = JSON.parse(f.runtime.GetCapabilities());
    assert.equal(caps.protocol_version, 4);
    assert.equal(caps.epoch, 'epoch');
    for (const capability of ['persistent-switch-v1', 'shell-search-focus-v1', 'shell-search-delivery-v1', 'shell-search-lifecycle-v1'])
        assert.ok(caps.capabilities.includes(capability), capability);
});

test('GetSwitchState distinguishes visible, fully shown and unknown animation metadata', () => {
    const f = fixture();
    const flags = () => {
        const state = JSON.parse(f.runtime.GetSwitchState());
        return [state.overview_visible, state.overview_shown, state.overview_animating, state.monitor_ready];
    };
    assert.deepEqual(flags(), [true, true, false, true]);
    f.Main.overview.animationInProgress = true;
    assert.deepEqual(flags(), [true, false, true, true]);
    f.Main.overview.visibleTarget = false;
    assert.deepEqual(flags(), [true, false, true, true]);
    f.Main.overview.visible = false;
    f.Main.overview.animationInProgress = false;
    assert.deepEqual(flags(), [false, false, false, true]);
    for (const [label, change] of lifecycleUnavailable) {
        const other = fixture(); change(other);
        const state = JSON.parse(other.runtime.GetSwitchState());
        assert.equal(state.overview_shown, false, label);
        assert.equal(state.overview_animating, typeof other.Main.overview.animationInProgress === 'boolean'
            ? other.Main.overview.animationInProgress : true, label);
        assert.deepEqual(other.events, [], 'state readback must be read-only');
        assert.equal(other.focusRequests.length, 0);
    }
    assert.deepEqual(f.events, []);
});

test('GetSwitchState refuses absent, invalid or zero-area primary-monitor metadata', () => {
    for (const [label, change] of monitorUnavailable) {
        const f = fixture(); change(f);
        assert.equal(JSON.parse(f.runtime.GetSwitchState()).monitor_ready, false, label);
        assert.deepEqual(f.events, [], label);
        assert.equal(f.focusRequests.length, 0, label);
    }
    const f = fixture();
    f.Main.layoutManager.monitors = [{width: 0, height: 0}, {width: 1280, height: 720}];
    f.Main.layoutManager.primaryIndex = 1;
    assert.equal(JSON.parse(f.runtime.GetSwitchState()).monitor_ready, true);
});

test('FocusSwitchSearch refuses lifecycle-not-shown before grabbing entry focus', () => {
    for (const [label, change] of lifecycleUnavailable) assertFocusRefused(change, label);
});

test('FocusSwitchSearch refuses monitor-not-ready before grabbing entry focus', () => {
    for (const [label, change] of monitorUnavailable) assertFocusRefused(change, label);
});

test('DeliverSwitchSearch rechecks lifecycle-not-shown at the receiver before text', () => {
    for (const [label, change] of lifecycleUnavailable) assertDeliveryRefused(change, label);
});

test('DeliverSwitchSearch rechecks monitor-not-ready at the receiver before text', () => {
    for (const [label, change] of monitorUnavailable) assertDeliveryRefused(change, label);
});

test('receiver-side delivery owns query and exact matching running application activation', () => {
    const f = fixture();
    assert.equal(JSON.parse(f.runtime.FocusSwitchSearch('epoch:7')).shell_search_has_focus, true);
    const reply = JSON.parse(f.runtime.DeliverSwitchSearch('epoch:7', 'Probe'));
    assert.deepEqual(reply, {protocol_version: 4, epoch: 'epoch', target: 'epoch:7', delivery: 'shell_search', delivered: true});
    assert.deepEqual(f.events, [['search', 'Probe'], ['text', 'Probe'], 'hide', 'unminimize', 'activate']);
});

test('post-proof physical focus loss cannot deliver query, commit or cancellation to client', () => {
    for (const lose of [f => { f.Main.overview.visible = false; }, f => { f.global.stage.focus = null; }]) {
        const f = fixture();
        assert.equal(JSON.parse(f.runtime.FocusSwitchSearch('epoch:7')).shell_search_has_focus, true);
        lose(f); // deterministic interleaving AFTER proof, BEFORE delivery
        const reply = JSON.parse(f.runtime.DeliverSwitchSearch('epoch:7', 'NEVER REACH CLIENT'));
        assert.equal(reply.code, 'shell_not_ready');
        assert.equal(reply.delivered, false);
        f.runtime.CancelSwitchSearch('epoch:7');
        assert.deepEqual(f.events, []);
    }
});

test('unmatched query, stale target and wrong running app refuse without text or activation', () => {
    for (const [change, code] of [
        [f => { f.Shell.AppSystem.search = () => [['other.desktop']]; }, 'switch_unverified'],
        [f => { f.app.get_windows = () => []; }, 'switch_unverified'],
        [f => { f.runtime._resolveTarget = () => null; }, 'window_not_found'],
        [f => { f.Main.sessionMode.isLocked = true; }, 'session_locked'],
    ]) {
        const f = fixture(); change(f);
        const reply = JSON.parse(f.runtime.DeliverSwitchSearch('epoch:7', 'Probe'));
        assert.equal(reply.delivered, false); assert.equal(reply.code, code);
        assert.deepEqual(f.events.filter(event => !Array.isArray(event) || event[0] !== 'search'), []);
    }
});

test('FocusSwitchSearch rechecks the FULL Shell predicate after grabbing focus', () => {
    for (const change of [f => { f.Main.overview.visible = false; }, f => { f.Main.modalCount = 2; },
        f => { f.runtime._foreground = {}; }, f => { f.Main.sessionMode.isLocked = true; }]) {
        const f = fixture();
        f.entry.grab_key_focus = () => { f.global.stage.focus = f.entry; change(f); };
        assert.equal(JSON.parse(f.runtime.FocusSwitchSearch('epoch:7')).code, 'shell_not_ready');
        assert.deepEqual(f.events, []);
    }
});

test('FocusSwitchSearch rechecks lifecycle and monitor state after grabbing focus', () => {
    for (const [label, change] of [...lifecycleUnavailable, ...monitorUnavailable]) {
        const f = fixture();
        f.entry.grab_key_focus = () => { f.focusRequests.push(f.entry); f.global.stage.focus = f.entry; change(f); };
        const reply = JSON.parse(f.runtime.FocusSwitchSearch('epoch:7'));
        assert.equal(reply.code, 'shell_not_ready', label);
        assert.equal(f.focusRequests.length, 1, label);
        assert.deepEqual(f.events, [], label);
    }
});

test('DeliverSwitchSearch retains the FULL existing receiver-side Shell predicate', () => {
    for (const [label, change] of [
        ['entry focus flag lost', f => { f.entry.has_key_focus = () => false; }],
        ['entry no longer inside Shell UI', f => { f.entry.get_parent = () => null; }],
        ['extra Shell modal', f => { f.Main.modalCount = 2; }],
        ['foreground transaction', f => { f.runtime._foreground = {}; }],
        ['persistent switch', f => { f.runtime._switching = {}; }],
        ['entry unmapped', f => { f.entry.mapped = false; }],
    ]) assertDeliveryRefused(change, label);
});

test('cancel reports a guarded hide request and fresh state, not hide completion', () => {
    const f = fixture();
    const reply = JSON.parse(f.runtime.CancelSwitchSearch('epoch:7'));
    assert.equal(reply.cancelled, true);
    assert.equal(reply.hide_requested, true);
    assert.equal(reply.overview_visible, true);
    assert.equal(reply.overview_shown, false);
    assert.equal(reply.overview_animating, true);
    assert.equal(reply.monitor_ready, true);
    assert.equal(reply.target, 'epoch:7');
    assert.equal(reply.delivery, 'shell_search');
    assert.deepEqual(f.events, ['hide']);
    f.Main.overview.visible = false;
    f.Main.overview.animationInProgress = false;
    const settled = JSON.parse(f.runtime.GetSwitchState());
    assert.equal(settled.overview_visible, false);
    assert.equal(settled.overview_shown, false);
    assert.equal(settled.overview_animating, false);
    assert.deepEqual(f.events, ['hide'], 'cleanup completion is observed, never forced');
});

test('cancel preserves public hide during showing or monitor disconnection', () => {
    const f = fixture();
    f.Main.overview.animationInProgress = true;
    f.global.stage.focus = null; // showing has not mapped/focused the entry yet
    f.Main.layoutManager = {monitors: [], primaryIndex: -1};
    const reply = JSON.parse(f.runtime.CancelSwitchSearch('epoch:7'));
    assert.equal(reply.hide_requested, true);
    assert.equal(reply.overview_visible, true);
    assert.equal(reply.overview_shown, false);
    assert.equal(reply.overview_animating, true);
    assert.equal(reply.monitor_ready, false);
    assert.deepEqual(f.events, ['hide']);
});

test('cancel never hides unowned or unsafe Shell state and never synthesizes Escape', () => {
    for (const change of [f => { f.global.stage.focus = null; }, f => { f.Main.overview.visible = false; },
        f => { f.Main.sessionMode.isLocked = true; }, f => { f.runtime._foreground = {}; },
        f => { f.runtime._switching = {}; }, f => { f.Main.modalCount = 2; }]) {
        const f = fixture(); change(f);
        const reply = JSON.parse(f.runtime.CancelSwitchSearch('epoch:7'));
        assert.equal(reply.cancelled, false);
        assert.equal(reply.hide_requested, false);
        assert.deepEqual(f.events, []);
    }
});
