// Execute the complete shipped-method regression suite against in-memory source
// mutations. Never swap shared-worktree files or contact a real desktop.
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {spawnSync} from 'node:child_process';

const mainUrl = new URL('../winrects@cua/impl/main.js', import.meta.url);
const policyUrl = new URL('../winrects@cua/impl/policy.js', import.meta.url);
const testUrl = new URL('switch-delivery.test.mjs', import.meta.url);
const [main, policy, tests] = await Promise.all([mainUrl, policyUrl, testUrl].map(url => readFile(url, 'utf8')));
const expectedTests = [...tests.matchAll(/^test\('/gm)].length;
assert.ok(expectedTests > 0, 'delivery regression inventory must be nonempty');
function replaceOnce(source, oldText, newText) {
    assert.equal(source.split(oldText).length, 2, `mutation anchor must be unique: ${oldText}`);
    return source.replace(oldText, newText);
}
function run(source) {
    let module = replaceOnce(tests, "await readFile(new URL('../winrects@cua/impl/main.js', import.meta.url), 'utf8')", JSON.stringify(source));
    module = replaceOnce(module, "await readFile(new URL('../winrects@cua/impl/policy.js', import.meta.url), 'utf8')", JSON.stringify(policy));
    const result = spawnSync(process.execPath, ['--input-type=module', '--test-reporter=tap'], {
        input: module, encoding: 'utf8', timeout: 30_000, maxBuffer: 4 * 1024 * 1024,
    });
    assert.ifError(result.error);
    assert.equal(result.signal, null, result.stderr);
    assert.match(result.stdout, new RegExp(`^# tests ${expectedTests}$`, 'm'), result.stdout + result.stderr);
    assert.match(result.stdout, /^# skipped 0$/m);
    assert.match(result.stdout, /^# cancelled 0$/m);
    return result;
}
function green(label) {
    const result = run(main);
    assert.equal(result.status, 0, result.stdout + result.stderr);
    assert.match(result.stdout, /^# fail 0$/m);
    assert.match(result.stdout, new RegExp(`^# pass ${expectedTests}$`, 'm'));
    console.log(`${label}: ${result.stdout.match(/^# pass ([0-9]+)$/m)[1]} tests passed`);
}

const beforeFocus = 'if (!code && (!state.overview_shown || !state.monitor_ready || !shellSearchCanRequestFocus({';
const afterFocus = "if (!state.overview_shown || !state.monitor_ready || !state.shell_search_has_focus\n                || state.foreground_transaction_open || state.persistent_switch_open) code = 'shell_not_ready';";
const delivery = "if (!state.overview_shown || !state.monitor_ready || !state.shell_search_has_focus\n                || state.foreground_transaction_open || state.persistent_switch_open)\n                return reply(false, 'shell_not_ready');";
const focusTest = 'FocusSwitchSearch rechecks lifecycle and monitor state after grabbing focus';
const mutations = [
    ['visible-only overview metadata',
        'overview_shown: overviewVisible && Main.overview?.visibleTarget === true && !overviewAnimating,',
        'overview_shown: overviewVisible,',
        'GetSwitchState distinguishes visible, fully shown and unknown animation metadata'],
    ['unknown animation admitted as idle',
        "typeof overviewAnimation === 'boolean' ? overviewAnimation : true;",
        "typeof overviewAnimation === 'boolean' ? overviewAnimation : false;",
        'GetSwitchState distinguishes visible, fully shown and unknown animation metadata'],
    ['monitor readiness always true',
        'monitor_ready: Boolean(primary && Number.isFinite(primary.width) && primary.width > 0\n                && Number.isFinite(primary.height) && primary.height > 0),',
        'monitor_ready: true,',
        'GetSwitchState refuses absent, invalid or zero-area primary-monitor metadata'],
    ['pre-focus shown guard removed', beforeFocus, beforeFocus.replace('!state.overview_shown || ', ''),
        'FocusSwitchSearch refuses lifecycle-not-shown before grabbing entry focus'],
    ['pre-focus monitor guard removed', beforeFocus, beforeFocus.replace('!state.monitor_ready || ', ''),
        'FocusSwitchSearch refuses monitor-not-ready before grabbing entry focus'],
    ['post-focus shown guard removed', afterFocus, afterFocus.replace('!state.overview_shown || ', ''), focusTest],
    ['post-focus monitor guard removed', afterFocus, afterFocus.replace('!state.monitor_ready || ', ''), focusTest],
    ['delivery shown guard removed', delivery, delivery.replace('!state.overview_shown || ', ''),
        'DeliverSwitchSearch rechecks lifecycle-not-shown at the receiver before text'],
    ['delivery monitor guard removed', delivery, delivery.replace('!state.monitor_ready || ', ''),
        'DeliverSwitchSearch rechecks monitor-not-ready at the receiver before text'],
    ['delivery existing full focus predicate removed', delivery, delivery.replace('!state.shell_search_has_focus\n                || ', ''),
        'DeliverSwitchSearch retains the FULL existing receiver-side Shell predicate'],
    ['cancellation returns stale pre-hide state', 'return JSON.stringify({...JSON.parse(this.GetSwitchState()),',
        'return JSON.stringify({...state,', 'cancel reports a guarded hide request and fresh state, not hide completion'],
];

green('control before mutations');
for (const [label, oldText, newText, expectedTest] of mutations) {
    const result = run(replaceOnce(main, oldText, newText));
    assert.equal(result.status, 1, `${label} survived\n${result.stdout}\n${result.stderr}`);
    const failures = [...result.stdout.matchAll(/^not ok [0-9]+ - ([^\n]+)\n([\s\S]*?)(?=\n(?:not ok|ok) [0-9]+ - |\n1\.\.|(?![\s\S]))/gm)];
    const failure = failures.find(match => match[1] === expectedTest);
    assert.ok(failure, `${label} did not fail its intended regression: ${expectedTest}\n${result.stdout}`);
    assert.match(failure[2], /code: 'ERR_ASSERTION'/, `${label}: not a behavioral assertion\n${failure[2]}`);
    console.log(`killed: ${label} [${expectedTest}]`);
}
green('control after mutations');
assert.equal(await readFile(mainUrl, 'utf8'), main, 'shared runtime source changed during qualification');
assert.equal(await readFile(testUrl, 'utf8'), tests, 'regression source changed during qualification');
assert.equal(await readFile(policyUrl, 'utf8'), policy, 'shared policy source changed during qualification');
console.log(`${mutations.length}/${mutations.length} lifecycle mutations killed; source files unchanged`);
