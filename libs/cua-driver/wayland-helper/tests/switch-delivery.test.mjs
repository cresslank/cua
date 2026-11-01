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
    const target = {get_stable_sequence: () => 7, get_pid: () => 42, unminimize: () => events.push('unminimize')};
    const shell = {get_parent: () => null};
    const entry = {mapped: true, get_parent: () => shell, has_key_focus: () => global.stage.focus === entry,
        grab_key_focus: () => { global.stage.focus = entry; },
        set_text: text => { entry.text = text; events.push(['text', text]); }, get_text: () => entry.text};
    const global = {stage: {focus: entry, get_key_focus() { return this.focus; }},
        display: {focus_window: target}, get_current_time: () => 7};
    const Main = {uiGroup: shell, modalCount: 1, sessionMode: {isLocked: false, currentMode: 'user'},
        activateWindow: window => { assert.equal(window, target); events.push('activate'); },
        overview: {visible: true, searchEntry: {clutter_text: entry}, hide() { this.visible = false; events.push('hide'); }}};
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
    return {runtime, events, Main, global, entry, Shell, app, target};
}

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

test('cancel hides only owned Shell overview and never synthesizes Escape', () => {
    const f = fixture();
    assert.equal(JSON.parse(f.runtime.CancelSwitchSearch('epoch:7')).cancelled, true);
    assert.deepEqual(f.events, ['hide']);
});
