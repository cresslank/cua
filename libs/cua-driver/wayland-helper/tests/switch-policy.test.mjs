import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import test from 'node:test';

const source = await readFile(new URL('../winrects@cua/impl/policy.js', import.meta.url), 'utf8');
const {persistentSwitchRefusal, persistentSwitchFocused, shellSearchHasFocus, shellSearchCanRequestFocus, overlayKeyUsesLeftSuper} = await import(
    `data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);

test('only the default or explicit left Super overlay binding permits initial input', () => {
    assert.equal(overlayKeyUsesLeftSuper('Super'), true);
    assert.equal(overlayKeyUsesLeftSuper('Super_L'), true);
    for (const binding of ['', 'Super_R', 'Alt_L', '<Super>space', null, undefined])
        assert.equal(overlayKeyUsesLeftSuper(binding), false);
});

const admissible = {sessionLocked: false, userSession: true, targetResolved: true, busy: false, modalChildPresent: false};
test('persistent switching refuses locked, non-user, stale, busy and modal states', () => {
    assert.equal(persistentSwitchRefusal(admissible), null);
    for (const [key, value, reason] of [
        ['sessionLocked', true, 'session_locked'], ['userSession', false, 'session_locked'],
        ['targetResolved', false, 'window_not_found'], ['busy', true, 'shell_not_ready'],
        ['modalChildPresent', true, 'shell_not_ready'],
    ]) {
        assert.equal(persistentSwitchRefusal({...admissible, [key]: value}), reason, key);
        const missing = {...admissible}; delete missing[key];
        assert.equal(persistentSwitchRefusal(missing), reason, `missing ${key}`);
    }
    // Minimized, inactive-workspace and overview states are *not* activation
    // preconditions for this dedicated operation (unlike BeginForeground).
    assert.equal(persistentSwitchRefusal({...admissible, minimized: true, workspaceActive: false, overviewVisible: true}), null);
});

const focused = {sessionLocked: false, targetResolved: true, focusMatches: true, workspaceActive: true,
    minimized: false, overviewVisible: false, keyFocusInShellUi: false, modalCount: 0};
test('persistent focus success requires exact current unlocked compositor focus', () => {
    assert.equal(persistentSwitchFocused(focused), true);
    for (const [key, value] of Object.entries(focused)) {
        assert.equal(persistentSwitchFocused({...focused, [key]: key === 'modalCount' ? 1 : !value}), false, key);
        const missing = {...focused}; delete missing[key];
        assert.equal(persistentSwitchFocused(missing), false, `missing ${key}`);
    }
});

test('visible overview or cached application focus cannot authorize text or Return', () => {
    const safe = {overviewVisible: true, sessionLocked: false, searchEntryHasKeyFocus: true, keyFocusInShellUi: true, modalCount: 1};
    assert.equal(shellSearchHasFocus(safe), true);
    for (const [key, value] of Object.entries(safe)) {
        assert.equal(shellSearchHasFocus({...safe, [key]: key === 'modalCount' ? 2 : !value}), false, key);
        const missing = {...safe}; delete missing[key];
        assert.equal(shellSearchHasFocus(missing), false, `missing ${key}`);
    }
    assert.equal(shellSearchHasFocus({...safe, searchEntryHasKeyFocus: false, focus_window: 'target'}), false);
});

test('requesting search focus is possible only inside the sole visible overview', () => {
    const safe = {overviewVisible: true, sessionLocked: false, busy: false, entryAvailable: true, modalCount: 1};
    assert.equal(shellSearchCanRequestFocus(safe), true);
    for (const [key, value] of Object.entries(safe)) {
        assert.equal(shellSearchCanRequestFocus({...safe, [key]: key === 'modalCount' ? 2 : !value}), false, key);
        const missing = {...safe}; delete missing[key];
        assert.equal(shellSearchCanRequestFocus(missing), false, `missing ${key}`);
    }
});

test('persistent helper is additive, bounded and does not restore focus' , async () => {
    const main = await readFile(new URL('../winrects@cua/impl/main.js', import.meta.url), 'utf8');
    const state = main.slice(main.indexOf('    GetSwitchState() {'), main.indexOf('    FocusSwitchSearch('));
    assert.match(state, /entry === focus && entry\.has_key_focus\(\)/);
    assert.doesNotMatch(state, /activateWindow|\.hide\(|\.close\(|press_key/);
    const native = main.slice(main.indexOf('    SwitchToTargetAsync('), main.indexOf('    BeginForegroundAsync('));
    assert.match(native, /persistentSwitchRefusal\(/);
    assert.match(native, /Main\.activateWindow\(target\)/);
    assert.match(native, /target\.unminimize\(\)/);
    assert.match(native, /Main\.overview\.hide\(\)/);
    assert.match(native, /Main\.panel\.closeCalendar\(\)/);
    assert.match(native, /previous\.owner === owner && previous\.targetId === targetId/);
    assert.match(native, /1_200_000/);
    assert.doesNotMatch(native, /priorWindow|priorWorkspace|_finishForeground|BeginForeground\(/);
    for (const cap of ['persistent-switch-v1', 'shell-search-focus-v1']) assert.ok(main.includes(`'${cap}'`));
});
