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
    assert.doesNotMatch(extensionSource, /\.screenshot\(false, stream\)/);
});

test('overview and lock screen make the capture context fail closed', () => {
    assert.equal(captureContextIsSafe({
        overviewVisible: false,
        sessionLocked: false,
    }), true);
    assert.equal(captureContextIsSafe({
        overviewVisible: true,
        sessionLocked: false,
    }), false);
    assert.equal(captureContextIsSafe({
        overviewVisible: false,
        sessionLocked: true,
    }), false);
});
