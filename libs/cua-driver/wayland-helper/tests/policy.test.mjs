import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import test from 'node:test';

const policySource = await readFile(
    new URL('../winrects@cua/policy.js', import.meta.url),
    'utf8',
);
const {
    captureAreaIsSafe,
    captureContextIsSafe,
    captureRectangleIsSafe,
    foregroundTargetCanActivate,
    foregroundTargetIsSafe,
    rectanglesOverlap,
    rectanglesEqual,
    shellInputIsGrabbed,
    targetIsPainted,
    targetTokenMatches,
} = await import(
    `data:text/javascript;base64,${Buffer.from(policySource).toString('base64')}`
);


test('visibility follows Mutter painting, not workspace index or count', () => {
    for (const workspaceIndex of [0, 1, 6, 11, 37]) {
        assert.equal(targetIsPainted({
            actorVisible: true,
            minimized: false,
            shellShowing: true,
            workspaceIndex,
        }), true);
        assert.equal(targetIsPainted({
            actorVisible: true,
            minimized: false,
            shellShowing: false,
            workspaceIndex,
        }), false);
    }
});

test('hidden actors and minimized windows never expose a cursor or capture', () => {
    assert.equal(targetIsPainted({
        actorVisible: false,
        minimized: false,
        shellShowing: true,
    }), false);
    assert.equal(targetIsPainted({
        actorVisible: true,
        minimized: true,
        shellShowing: true,
    }), false);
});

test('target tokens are bound to the helper epoch', () => {
    assert.equal(targetTokenMatches('epoch-a', 'epoch-a:42'), true);
    assert.equal(targetTokenMatches('epoch-a', 'epoch-b:42'), false);
    assert.equal(targetTokenMatches('', ':42'), false);
    assert.equal(targetTokenMatches('epoch-a', 42), false);
    assert.equal(targetTokenMatches('epoch-a', 'epoch-a:42:garbage'), false);
    assert.equal(targetTokenMatches('epoch-a', 'epoch-a:0'), false);
    assert.equal(targetTokenMatches('epoch-a', 'epoch-a:+42'), false);
});

test('overlap proof rejects only positive-area intersections', () => {
    const target = {x: 10, y: 10, width: 100, height: 100};
    assert.equal(rectanglesOverlap(target, {x: 50, y: 50, width: 20, height: 20}), true);
    assert.equal(rectanglesOverlap(target, {x: 110, y: 10, width: 20, height: 20}), false);
    assert.equal(rectanglesOverlap(target, {x: 10, y: 110, width: 20, height: 20}), false);
    assert.equal(rectanglesOverlap(target, {x: 10, y: 10, width: 0, height: 20}), false);
});

test('a one-pixel positive overlap blocks exact capture without erasing painted discovery', () => {
    const zen = {x: 0, y: 40, width: 2049, height: 1688};
    const ghostty = {x: 2048, y: 40, width: 2048, height: 1688};
    const visible = targetIsPainted({
        actorVisible: true,
        minimized: false,
        shellShowing: true,
    });
    const captureCurrent = visible && !rectanglesOverlap(zen, ghostty);
    assert.equal(visible, true);
    assert.equal(captureCurrent, false);
});

test('capture rejects zero-sized or non-finite display and stage geometry', () => {
    assert.equal(captureAreaIsSafe({
        displayWidth: 3136,
        displayHeight: 1293,
        stageWidth: 3136,
        stageHeight: 1293,
    }), true);
    for (const [field, value] of [
        ['displayWidth', 0],
        ['displayHeight', -1],
        ['stageWidth', 0],
        ['stageHeight', Number.NaN],
    ]) {
        const geometry = {
            displayWidth: 3136,
            displayHeight: 1293,
            stageWidth: 3136,
            stageHeight: 1293,
            [field]: value,
        };
        assert.equal(captureAreaIsSafe(geometry), false, field);
    }
});

test('atomic capture geometry accepts only a bounded positive target rectangle', () => {
    const display = {displayWidth: 1920, displayHeight: 1080};
    assert.equal(captureRectangleIsSafe({x: 100, y: 80, width: 640, height: 480}, display), true);
    assert.equal(captureRectangleIsSafe({x: -1, y: 80, width: 640, height: 480}, display), false);
    assert.equal(captureRectangleIsSafe({x: 100, y: 80, width: 0, height: 480}, display), false);
    assert.equal(captureRectangleIsSafe({x: 1800, y: 80, width: 640, height: 480}, display), false);
    assert.equal(captureRectangleIsSafe({x: 100, y: 1000, width: 640, height: 480}, display), false);
});

test('atomic capture rejects geometry drift after pixels are produced', () => {
    const before = {x: 100, y: 80, width: 640, height: 480};
    assert.equal(rectanglesEqual(before, {...before}), true);
    assert.equal(rectanglesEqual(before, {...before, x: 101}), false);
    assert.equal(rectanglesEqual(before, {...before, width: 641}), false);
});

test('extension uses an explicit positive area and never implicit stage capture', async () => {
    const extensionSource = await readFile(
        new URL('../winrects@cua/extension.js', import.meta.url),
        'utf8',
    );
    assert.match(
        extensionSource,
        /screenshot_area\(\s*captureRect\.x,\s*captureRect\.y,\s*captureRect\.width,\s*captureRect\.height,\s*stream\s*\)/s,
    );
    assert.doesNotMatch(extensionSource, /screenshot_area\(0, 0, width, height, stream\)/);
    assert.match(extensionSource, /target_changed_during_capture/);
    assert.match(extensionSource, /target_geometry_changed_during_capture/);
    assert.match(extensionSource, /target_occluded_during_capture/);
    assert.doesNotMatch(extensionSource, /\.screenshot\(false, stream\)/);
    assert.match(extensionSource, /visible: paintedVisible/);
    assert.match(extensionSource, /capture_current: captureCurrent/);
});

test('overview and lock screen make the capture context fail closed', () => {
    assert.equal(captureContextIsSafe({
        overviewVisible: false,
        sessionLocked: false,
        shellInputGrabbed: false,
    }), true);
    assert.equal(captureContextIsSafe({
        overviewVisible: true,
        sessionLocked: false,
        shellInputGrabbed: false,
    }), false);
    assert.equal(captureContextIsSafe({
        overviewVisible: false,
        sessionLocked: true,
        shellInputGrabbed: false,
    }), false);
    assert.equal(captureContextIsSafe({
        overviewVisible: false,
        sessionLocked: false,
        shellInputGrabbed: true,
    }), false);
});

test('normal app key focus is not a Shell grab', () => {
    assert.equal(shellInputIsGrabbed({modalCount: 0, keyFocusInShellUi: false}), false);
    assert.equal(shellInputIsGrabbed({modalCount: 1, keyFocusInShellUi: false}), true);
    assert.equal(shellInputIsGrabbed({modalCount: 0, keyFocusInShellUi: true}), true);
});

test('foreground input rejects focus drift, stale targets, invisibility, and child modals', () => {
    const valid = {
        targetResolved: true,
        focusMatches: true,
        targetVisible: true,
        targetUnoccluded: true,
        modalChildPresent: false,
    };
    assert.equal(foregroundTargetIsSafe(valid), true);
    for (const invalid of [
        {...valid, targetResolved: false},
        {...valid, focusMatches: false},
        {...valid, targetVisible: false},
        {...valid, targetUnoccluded: false},
        {...valid, modalChildPresent: true},
    ])
        assert.equal(foregroundTargetIsSafe(invalid), false);
});

test('exact targets may activate from behind another window but not from unsafe state', () => {
    const valid = {
        targetResolved: true,
        shellContextSafe: true,
        minimized: false,
        shellShowing: true,
        modalChildPresent: false,
    };
    assert.equal(foregroundTargetCanActivate(valid), true);
    for (const invalid of [
        {...valid, targetResolved: false},
        {...valid, shellContextSafe: false},
        {...valid, minimized: true},
        {...valid, shellShowing: false},
        {...valid, modalChildPresent: true},
    ])
        assert.equal(foregroundTargetCanActivate(invalid), false);
});

test('extension advertises and implements transaction revalidation', async () => {
    const extensionSource = await readFile(
        new URL('../winrects@cua/extension.js', import.meta.url),
        'utf8',
    );
    assert.match(extensionSource, /foreground-revalidate-v1/);
    assert.match(extensionSource, /foreground-reconcile-v1/);
    assert.match(extensionSource, /unoccluded-target-v1/);
    assert.match(extensionSource, /trusted-cursor-overlay-v1/);
    assert.match(extensionSource, /exact-target-activation-v1/);
    assert.match(extensionSource, /shell-grab-classification-v1/);
    assert.match(extensionSource, /_canActivateTarget/);
    assert.match(extensionSource, /_keyFocusInShellUi/);
    assert.doesNotMatch(policySource, /Cua\.AgentCursorOverlay/);
    assert.doesNotMatch(extensionSource, /_isTrustedCursorOverlay/);
    assert.doesNotMatch(extensionSource, /GLib\.file_read_link/);
    assert.match(extensionSource, /ValidateForeground\(transaction\)/);
    assert.match(extensionSource, /BeginForegroundAsync\(\[transaction, targetId\], invocation\)/);
    assert.match(extensionSource, /QueryForeground\(transaction\)/);
    assert.match(extensionSource, /AbortForegroundAsync\(\[transaction\], invocation\)/);
    assert.match(extensionSource, /caller must allocate a bounded transaction ID/);
    assert.match(extensionSource, /prior_window_not_confirmed/);
    assert.match(extensionSource, /prior_workspace_not_confirmed/);
    assert.match(extensionSource, /preserved_user_context/);
    assert.match(extensionSource, /child_modal_present/);
    assert.match(extensionSource, /target_occluded/);
});

test('cursor ownership is connection-derived with disconnect and bounded-idle cleanup', async () => {
    const extensionSource = await readFile(
        new URL('../winrects@cua/extension.js', import.meta.url),
        'utf8',
    );
    assert.match(extensionSource, /connection-owned-cursors-v1/);
    assert.match(extensionSource, /invocation\.get_sender\(\)/);
    assert.match(extensionSource, /NameOwnerChanged/);
    assert.match(extensionSource, /_removeCursorsForConnection\(name\)/);
    assert.match(extensionSource, /CURSOR_IDLE_TIMEOUT_US/);
    assert.match(extensionSource, /record\.lastUsedAt < cutoff/);
    assert.match(extensionSource, /MoveCursorForAsync/);
    assert.doesNotMatch(extensionSource, /\n\s*MoveCursorFor\(owner,/);
    assert.doesNotMatch(extensionSource, /this\._updateCursorBadge\(/);
});

test('capture protocol exposes one exact target and one atomic result', async () => {
    const extensionSource = await readFile(
        new URL('../winrects@cua/extension.js', import.meta.url),
        'utf8',
    );
    assert.match(
        extensionSource,
        /<method name="CaptureTarget"><arg type="s" direction="in" name="target"\/><arg type="s" direction="out" name="capture_json"\/><\/method>/,
    );
    assert.match(extensionSource, /async CaptureTargetAsync\(\[targetId\], invocation\)/);
    assert.match(extensionSource, /screenshot_area\(\s*captureRect\.x,/);
    assert.match(extensionSource, /target: targetId,\s*rect: captureRect,/);
    assert.doesNotMatch(extensionSource, /screenshot_area\(\s*0,\s*0,/);
});
