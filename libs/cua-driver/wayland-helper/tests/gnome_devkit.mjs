import GdkPixbuf from 'gi://GdkPixbuf';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as Scripting from 'resource:///org/gnome/shell/ui/scripting.js';

const BUS = GLib.getenv('CUA_GNOME_TEST_BUS') || 'org.cua.WinRects';
const PATH = '/org/cua/WinRects';
const IFACE = 'org.cua.WinRects';

export var METRICS = {};

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

async function waitUntil(predicate, failure, timeoutMs = 5000) {
    const deadline = GLib.get_monotonic_time() + timeoutMs * 1000;
    do {
        if (predicate())
            return;
        await Scripting.sleep(50);
    } while (GLib.get_monotonic_time() < deadline);
    throw new Error(typeof failure === 'function' ? failure() : failure);
}

async function closeOverview(failure) {
    Main.overview.hide();
    await waitUntil(() => !Main.overview.visible, failure);
}

function windowActor(window) {
    return global.get_window_actors()
        .find(actor => actor.meta_window === window) ?? null;
}

function windowRect(window) {
    const rect = window.get_frame_rect();
    return {x: rect.x, y: rect.y, width: rect.width, height: rect.height};
}

export async function run() {
    await closeOverview('GNOME dev-kit overview did not close');

    const existingIds = new Set(global.get_window_actors()
        .map(actor => actor.meta_window?.get_stable_sequence())
        .filter(id => id !== undefined));
    await Scripting.createTestWindow({width: 300, height: 220});
    let bottomWindow = null;
    await waitUntil(
        () => {
            const windows = global.get_window_actors()
                .map(actor => actor.meta_window)
                .filter(window =>
                    window && !existingIds.has(window.get_stable_sequence())
                );
            if (windows.length !== 1)
                return false;
            [bottomWindow] = windows;
            return true;
        },
        'GNOME dev-kit did not publish the first test window'
    );
    await Scripting.waitTestWindows();
    await closeOverview('GNOME dev-kit overview reopened while mapping the first test window');
    // GNOME 50's no-X11 dev-kit does not expose a perf-helper actor until
    // input arrives. WaitWindows proves that the client mapped and painted;
    // show() stages that painted actor without involving the live desktop.
    windowActor(bottomWindow).show();
    bottomWindow.unminimize();
    bottomWindow.raise();
    await waitUntil(
        () => windowActor(bottomWindow)?.visible
            && windowActor(bottomWindow)?.mapped,
        () => `GNOME dev-kit did not paint the first test window: ${JSON.stringify({
            focus: global.display.focus_window?.get_stable_sequence() ?? null,
            window: bottomWindow.get_stable_sequence(),
            visible: windowActor(bottomWindow)?.visible ?? null,
            mapped: windowActor(bottomWindow)?.mapped ?? null,
            showing: bottomWindow.showing_on_its_workspace(),
            rect: windowRect(bottomWindow),
            modalCount: Main.modalCount,
            overviewVisible: Main.overview.visible,
            stageKeyFocus: global.stage.get_key_focus()?.toString() ?? null,
        })}`
    );
    bottomWindow.move_frame(false, 100, 100);
    await waitUntil(
        () => windowRect(bottomWindow).x === 100 && windowRect(bottomWindow).y === 100,
        () => `GNOME dev-kit could not place the lower test window: ${JSON.stringify(windowRect(bottomWindow))}`
    );

    await Scripting.createTestWindow({width: 300, height: 220});
    let topWindow = null;
    await waitUntil(
        () => {
            const windows = global.get_window_actors()
                .map(actor => actor.meta_window)
                .filter(window =>
                    window
                    && window !== bottomWindow
                    && !existingIds.has(window.get_stable_sequence())
                );
            if (windows.length !== 1)
                return false;
            [topWindow] = windows;
            return true;
        },
        'GNOME dev-kit did not publish the second test window'
    );
    await Scripting.waitTestWindows();
    await closeOverview('GNOME dev-kit overview reopened while mapping the second test window');
    // Stage the second mapped client actor for the same contained fixture.
    windowActor(topWindow).show();
    topWindow.unminimize();
    topWindow.raise();
    await waitUntil(
        () => windowActor(topWindow)?.visible
            && windowActor(topWindow)?.mapped,
        'GNOME dev-kit did not paint the second test window'
    );
    topWindow.move_frame(false, 200, 150);
    await waitUntil(
        () => windowRect(topWindow).x === 200 && windowRect(topWindow).y === 150,
        () => `GNOME dev-kit could not place the top test window: ${JSON.stringify(windowRect(topWindow))}`
    );
    const fixtureIds = new Set([
        bottomWindow.get_stable_sequence(),
        topWindow.get_stable_sequence(),
    ]);
    bottomWindow.lower();
    topWindow.raise();
    topWindow.activate(global.get_current_time());
    await waitUntil(
        () =>
            windowActor(bottomWindow)?.visible
            && windowActor(topWindow)?.visible
            && bottomWindow.showing_on_its_workspace()
            && topWindow.showing_on_its_workspace(),
        () => `GNOME dev-kit test windows did not become focused and paintable: ${JSON.stringify({
            focus: global.display.focus_window?.get_stable_sequence() ?? null,
            bottom: {
                id: bottomWindow.get_stable_sequence(),
                visible: windowActor(bottomWindow)?.visible ?? null,
                mapped: windowActor(bottomWindow)?.mapped ?? null,
                showing: bottomWindow.showing_on_its_workspace(),
                rect: windowRect(bottomWindow),
            },
            top: {
                id: topWindow.get_stable_sequence(),
                visible: windowActor(topWindow)?.visible ?? null,
                mapped: windowActor(topWindow)?.mapped ?? null,
                showing: topWindow.showing_on_its_workspace(),
                rect: windowRect(topWindow),
            },
        })}`
    );

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
    const fixtureWindows = new Map([
        [bottomWindow.get_stable_sequence(), bottomWindow],
        [topWindow.get_stable_sequence(), topWindow],
    ]);
    const fixtureRecords = [...fixtureWindows.keys()]
        .map(id => byNativeId.get(id))
        .filter(Boolean);
    assert(fixtureRecords.length === 2, 'helper did not publish exact test-window identities');
    const currentRecords = fixtureRecords.filter(record => record.capture_current);
    assert(
        currentRecords.length === 1,
        `helper did not publish exactly one capture-current fixture: ${JSON.stringify(fixtureRecords)}`
    );
    const top = currentRecords[0];
    const bottom = fixtureRecords.find(record => record !== top);
    topWindow = fixtureWindows.get(top.id);
    bottomWindow = fixtureWindows.get(bottom.id);
    assert(top.target_id.startsWith(`${top.helper_epoch}:`), 'top target is not epoch-bound');
    assert(bottom.visible === true, 'painted overlapped target disappeared from discovery');
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
    print('Milestone A: exact target capture and occlusion proof PASS');

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
    await waitUntil(
        () => global.display.focus_window === bottomWindow,
        'fixture focus did not drift to the lower target'
    );
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
    print('Milestone B: foreground transaction lifecycle PASS');

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
    // The fixture actors were explicitly exposed above, so stage Mutter's
    // inactive-workspace paint state before checking the fail-closed path.
    windowActor(topWindow).hide();
    await waitUntil(
        () => topWindow.get_workspace() === workspace
            && !windowActor(topWindow)?.visible,
        () => `fixture target remained painted after moving to an inactive workspace: ${JSON.stringify({
            activeWorkspace: global.workspace_manager.get_active_workspace_index(),
            targetWorkspace: workspace.index(),
            windowWorkspace: topWindow.get_workspace().index(),
            showing: topWindow.showing_on_its_workspace(),
            visible: windowActor(topWindow)?.visible ?? null,
        })}`
    );
    await expectRefusal(
        'CaptureTarget',
        new GLib.Variant('(s)', [top.target_id]),
        'capture_foreground_required'
    );
    print('Milestone C: target-bound cursor and inactive-workspace refusal PASS');

    await Scripting.destroyTestWindows();
    await waitUntil(
        () => global.get_window_actors().every(actor =>
            !fixtureIds.has(actor.meta_window?.get_stable_sequence())
        ),
        'GNOME dev-kit test windows did not close'
    );
}

export function finish() {}
