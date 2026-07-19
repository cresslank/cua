import GdkPixbuf from 'gi://GdkPixbuf';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

import * as Scripting from 'resource:///org/gnome/shell/ui/scripting.js';

const BUS = GLib.getenv('CUA_GNOME_TEST_BUS') || 'org.cua.WinRects';
const PATH = '/org/cua/WinRects';
const IFACE = 'org.cua.WinRects';

export var METRICS = {};
const fixtureProcesses = [];

function assert(condition, message) {
    if (!condition)
        throw new Error(message);
}

function call(method, parameters) {
    return new Promise((resolve, reject) => {
        Gio.DBus.session.call(
            BUS,
            PATH,
            IFACE,
            method,
            parameters,
            null,
            Gio.DBusCallFlags.NONE,
            5000,
            null,
            (connection, result) => {
                try {
                    resolve(connection.call_finish(result).deepUnpack());
                } catch (error) {
                    reject(error);
                }
            }
        );
    });
}

async function expectRefusal(method, parameters, expectedText) {
    try {
        await call(method, parameters);
    } catch (error) {
        assert(
            String(error).includes(expectedText),
            `${method} failed with the wrong refusal: ${error}`
        );
        return;
    }
    throw new Error(`${method} unexpectedly succeeded`);
}

function decodePng(base64) {
    const bytes = GLib.base64_decode(base64);
    const loader = new GdkPixbuf.PixbufLoader();
    loader.write(bytes);
    loader.close();
    return loader.get_pixbuf();
}

function spawnFixtureWindow(title) {
    const launcher = new Gio.SubprocessLauncher({
        flags: Gio.SubprocessFlags.STDOUT_SILENCE | Gio.SubprocessFlags.STDERR_SILENCE,
    });
    launcher.setenv('GDK_BACKEND', 'x11', true);
    launcher.unsetenv('WAYLAND_DISPLAY');
    launcher.setenv('DISPLAY', GLib.getenv('CUA_GNOME_TEST_DISPLAY') || ':2', true);
    const process = launcher.spawnv([
        '/usr/bin/zenity',
        '--info',
        `--title=${title}`,
        `--text=${title}`,
        '--no-wrap',
    ]);
    fixtureProcesses.push(process);
}

export async function run() {
    spawnFixtureWindow('CUA fixture bottom');
    spawnFixtureWindow('CUA fixture top');
    await Scripting.sleep(500);

    const windows = global.display.sort_windows_by_stacking(
        global.get_window_actors()
            .map(actor => actor.meta_window)
            .filter(window => window?.get_title()?.startsWith('CUA fixture'))
    );
    assert(windows.length >= 2, 'GNOME dev-kit did not create two test windows');
    const bottomWindow = windows.at(-2);
    const topWindow = windows.at(-1);
    bottomWindow.move_frame(true, 100, 100);
    topWindow.move_frame(true, 100, 100);
    topWindow.activate(global.get_current_time());
    await Scripting.sleep(250);

    const [capabilitiesJson] = await call('GetCapabilities', null);
    const capabilities = JSON.parse(capabilitiesJson);
    assert(capabilities.protocol_version === 4, 'helper protocol is not v4');
    for (const capability of [
        'exact-target-v2',
        'unoccluded-target-v1',
        'atomic-target-capture-v1',
        'connection-owned-cursors-v1',
        'foreground-reconcile-v1',
    ]) {
        assert(capabilities.capabilities.includes(capability), `missing ${capability}`);
    }

    const [rectsJson] = await call('GetRects', null);
    const records = JSON.parse(rectsJson);
    const byNativeId = new Map(records.map(record => [record.id, record]));
    const bottom = byNativeId.get(bottomWindow.get_stable_sequence());
    const top = byNativeId.get(topWindow.get_stable_sequence());
    assert(bottom && top, 'helper did not publish exact test-window identities');
    assert(top.target_id.startsWith(`${top.helper_epoch}:`), 'top target is not epoch-bound');
    assert(top.capture_current === true, 'focused top target is not capture-current');
    assert(bottom.capture_current === false, 'overlapped lower target was marked capture-current');

    const [captureJson] = await call(
        'CaptureTarget',
        new GLib.Variant('(s)', [top.target_id])
    );
    const capture = JSON.parse(captureJson);
    assert(capture.protocol_version === 4, 'capture proof protocol mismatch');
    assert(capture.target === top.target_id, 'capture proof retargeted');
    assert(capture.rect.x === top.x && capture.rect.y === top.y, 'capture origin drifted');
    assert(
        capture.rect.width === top.w && capture.rect.height === top.h,
        'capture rectangle drifted'
    );
    const pixbuf = decodePng(capture.png_base64);
    assert(pixbuf.width >= top.w && pixbuf.height >= top.h, 'target PNG is undersized');
    assert(pixbuf.width <= top.w * 8 && pixbuf.height <= top.h * 8, 'target PNG is broad');

    await expectRefusal(
        'CaptureTarget',
        new GLib.Variant('(s)', [bottom.target_id]),
        'capture_occluded'
    );
    await expectRefusal(
        'CaptureTarget',
        new GLib.Variant('(s)', [`wrong-epoch:${top.id}`]),
        'stale_target'
    );

    const firstTransaction = 'cua-fg-devkit-first-0001';
    const [beginJson] = await call(
        'BeginForeground',
        new GLib.Variant('(ss)', [firstTransaction, top.target_id])
    );
    const begin = JSON.parse(beginJson);
    assert(begin.transaction === firstTransaction, 'helper replaced caller transaction identity');
    assert(begin.activated === true, 'exact top target did not activate');
    const [validatedJson] = await call(
        'ValidateForeground',
        new GLib.Variant('(s)', [begin.transaction])
    );
    assert(JSON.parse(validatedJson).valid === true, 'exact foreground validation failed');
    bottomWindow.activate(global.get_current_time());
    await Scripting.sleep(100);
    const [driftedJson] = await call(
        'ValidateForeground',
        new GLib.Variant('(s)', [begin.transaction])
    );
    const drifted = JSON.parse(driftedJson);
    assert(
        drifted.valid === false && drifted.reason === 'focus_changed',
        'focus drift did not invalidate the exact transaction'
    );
    const [queryJson] = await call(
        'QueryForeground',
        new GLib.Variant('(s)', [firstTransaction])
    );
    assert(JSON.parse(queryJson).terminal === false, 'focus drift deleted unresolved state');
    const [abortedJson] = await call(
        'AbortForeground',
        new GLib.Variant('(s)', [firstTransaction])
    );
    const aborted = JSON.parse(abortedJson);
    assert(aborted.terminal === true, 'foreground abort did not reach a terminal state');
    assert(aborted.outcome === 'preserved_user_context', 'abort overwrote intervening user focus');

    const secondTransaction = 'cua-fg-devkit-second-0002';
    const [secondBeginJson] = await call(
        'BeginForeground',
        new GLib.Variant('(ss)', [secondTransaction, top.target_id])
    );
    const secondBegin = JSON.parse(secondBeginJson);
    assert(
        secondBegin.transaction === secondTransaction,
        'second activation replaced caller transaction identity'
    );
    assert(secondBegin.activated === true, 'second exact activation failed');
    const [committedJson] = await call(
        'CommitForeground',
        new GLib.Variant('(s)', [secondBegin.transaction])
    );
    assert(JSON.parse(committedJson).committed === true, 'foreground commit failed');

    const owner = 'gnome-devkit-fixture';
    await call(
        'MoveCursorFor',
        new GLib.Variant('(ssii)', [owner, top.target_id, top.x + 20, top.y + 20])
    );
    await call(
        'ClickPulseFor',
        new GLib.Variant('(ssii)', [owner, top.target_id, top.x + 20, top.y + 20])
    );
    await call('HideCursorFor', new GLib.Variant('(s)', [owner]));
    await call('RemoveCursor', new GLib.Variant('(s)', [owner]));

    const workspace = global.workspace_manager.append_new_workspace(
        false,
        global.get_current_time()
    );
    topWindow.change_workspace(workspace);
    await Scripting.sleep(200);
    await expectRefusal(
        'CaptureTarget',
        new GLib.Variant('(s)', [top.target_id]),
        'capture_foreground_required'
    );

    for (const process of fixtureProcesses)
        process.force_exit();
    await Scripting.sleep(100);
}

export function finish() {}
