import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import test from 'node:test';

const policySource = await readFile(
    new URL('../winrects@cua/policy.js', import.meta.url),
    'utf8',
);
const {targetIsPainted, targetTokenMatches} = await import(
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
