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
    foregroundTargetCanActivate,
    foregroundTargetIsSafe,
    rectanglesOverlap,
    shellInputIsGrabbed,
    targetIsPainted,
    targetTokenMatches,
    trustedCursorOverlayIsSafe,
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

test('only the exact sticky cua-driver cursor overlay is trusted', () => {
    const valid = {
        title: 'Cua.AgentCursorOverlay.default',
        appId: '',
        windowType: 15,
        overrideOtherType: 15,
        sticky: true,
        pid: 4104720,
        executable: '/home/user/.cua-driver/packages/releases/hardened/cua-driver',
    };
    assert.equal(trustedCursorOverlayIsSafe(valid), true);
    for (const invalid of [
        {...valid, title: 'Cua.AgentCursorOverlay'},
        {...valid, appId: 'spoofed.app'},
        {...valid, windowType: 0},
        {...valid, sticky: false},
        {...valid, pid: 0},
        {...valid, executable: '/tmp/not-cua-driver'},
        {...valid, executable: 'cua-driver'},
    ]) {
        assert.equal(trustedCursorOverlayIsSafe(invalid), false);
    }
});

test('overlap proof rejects only positive-area intersections', () => {
    const target = {x: 10, y: 10, width: 100, height: 100};
    assert.equal(rectanglesOverlap(target, {x: 50, y: 50, width: 20, height: 20}), true);
    assert.equal(rectanglesOverlap(target, {x: 110, y: 10, width: 20, height: 20}), false);
    assert.equal(rectanglesOverlap(target, {x: 10, y: 110, width: 20, height: 20}), false);
    assert.equal(rectanglesOverlap(target, {x: 10, y: 10, width: 0, height: 20}), false);
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

test('extension uses an explicit positive area and never implicit stage capture', async () => {
    const extensionSource = await readFile(
        new URL('../winrects@cua/extension.js', import.meta.url),
        'utf8',
    );
    assert.match(extensionSource, /screenshot_area\(0, 0, width, height, stream\)/);
    assert.match(extensionSource, /target_changed_during_capture/);
    assert.match(extensionSource, /target_occluded_during_capture/);
    assert.doesNotMatch(extensionSource, /\.screenshot\(false, stream\)/);
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
    assert.match(extensionSource, /unoccluded-target-v1/);
    assert.match(extensionSource, /trusted-cursor-overlay-v1/);
    assert.match(extensionSource, /exact-target-activation-v1/);
    assert.match(extensionSource, /shell-grab-classification-v1/);
    assert.match(extensionSource, /_canActivateTarget/);
    assert.match(extensionSource, /_keyFocusInShellUi/);
    assert.match(policySource, /Cua\.AgentCursorOverlay/);
    assert.match(extensionSource, /GLib\.file_read_link/);
    assert.match(extensionSource, /ValidateForeground\(transaction\)/);
    assert.match(extensionSource, /child_modal_present/);
    assert.match(extensionSource, /target_occluded/);
});
