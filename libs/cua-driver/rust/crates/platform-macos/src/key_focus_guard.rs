//! Keep the user's key window while a background target opens a menu.
//!
//! When an app that is not active opens a menu (a popup button, a `<select>`,
//! a context menu), AppKit's menu tracking calls
//! `SLPSStealKeyFocusReturningID`. WindowServer then tells the app that holds
//! key focus, normally the user's frontmost app, that it lost key focus, and
//! routes the keyboard to the menu until it closes. The user's window resigns
//! key and their typing stops landing in it, even though no app was activated
//! and no window was raised.
//!
//! While the driver acts on a background target, this module holds a
//! per-process event tap on the user's frontmost app. When that app is told it
//! lost key focus to a guarded target (a type-21 event, subtype `0x4000`, whose
//! field 73 is the thief's pid), the tap releases the theft by id with
//! `SLPSReleaseKeyFocusWithID` and drops the event. The user's app never sees
//! the loss; the menu stays open and is still operable through accessibility.
//!
//! Opening a menu also makes WindowServer order the menu's window group to the
//! front, which lifts the target's parent window above the user's window. No
//! client can stop that ordering or reorder another app's windows, so the
//! guard restores the order right after: once the menu window is on screen
//! (and again when the action ends), if a target window sits above the user's
//! front window, the already-active front app is re-activated with all its
//! windows. Key status does not move and the menu stays open; the raise is
//! visible for at most a frame or two.
//!
//! Theft ids come from one session-wide counter that only the thief learns.
//! The guard learns the target's id only after the tap has seen the theft:
//! it steals and releases key focus on the driver's own connection, which
//! takes the next id, and then releases the newest live theft below it. By
//! then the target's menu already holds the keyboard, so the round trip costs
//! the user nothing. Arming the guard never moves key focus: a steal on every
//! action used to route a user keystroke that arrived in that instant to the
//! driver, which dropped it.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::input::skylight;

/// How long a target stays guarded after the action that armed it ends.
/// AppKit opens a pressed popup's menu from a delayed perform that runs just
/// after the accessibility call returns.
const TAIL: Duration = Duration::from_secs(3);
/// Older ids tried after the newest one when another app stole in between.
const ID_SLACK: i32 = 3;
/// Longest wait for the thief's menu window before releasing the theft.
const MENU_WAIT: Duration = Duration::from_millis(150);
/// CGWindowLevel of popup menus (`kCGPopUpMenuWindowLevel`).
const POPUP_MENU_LAYER: i32 = 101;

/// Whether a normal-level window of `target` sits above the frontmost
/// normal-level window of `front` in the on-screen order.
fn target_above_front(target: i32, front: i32) -> bool {
    // Every layer, without the Space queries of `visible_windows`: this runs
    // while the raise is on screen, so it has to be quick.
    let windows = crate::windows::all_windows_any_layer();
    target_above_front_in(&windows, target, front)
}

fn target_above_front_in(windows: &[crate::windows::WindowInfo], target: i32, front: i32) -> bool {
    let top = |pid: i32| {
        windows
            .iter()
            .filter(|w| w.pid == pid && w.layer == 0 && w.is_on_screen)
            .filter(|w| w.bounds.width > 1.0 && w.bounds.height > 1.0)
            .map(|w| w.z_index)
            .max()
    };
    match (top(target), top(front)) {
        (Some(t), Some(f)) => t > f,
        _ => false,
    }
}

/// Put the user's front app back above a background target that raised its
/// window (AppKit orders a popup menu's window group to the front, which
/// lifts its parent window). Re-activating the already-active front app with
/// all its windows restores the order without moving key status; the
/// target's menu stays open.
fn restore_front_if_raised(target: i32, front: i32) -> bool {
    if crate::apps::frontmost_pid() != Some(front) || !target_above_front(target, front) {
        return false;
    }
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    let restored = unsafe {
        NSRunningApplication::runningApplicationWithProcessIdentifier(front).is_some_and(|app| {
            app.activateWithOptions(NSApplicationActivationOptions::NSApplicationActivateAllWindows)
        })
    };
    RESTORED.fetch_add(1, Ordering::Relaxed);
    tracing::debug!(
        target,
        front,
        restored,
        "restored the user's window above a background target"
    );
    true
}

/// Whether WindowServer's front process is the background target: it
/// activated itself (`activateIgnoringOtherApps:`) in answer to the action,
/// such as Chrome focusing its omnibox on cmd+L or GnuCash opening a register
/// on a double-click. No client can stop an app from activating itself.
fn target_self_activated(front_process: Option<i32>, target: i32, front: i32) -> bool {
    target != front && front_process == Some(target)
}

/// Undo a background target's self-activation: re-activate the user's app
/// and hand key focus back to its window. Reads WindowServer's front process
/// directly; `NSWorkspace` only learns of the activation once the daemon's run
/// loop processes the notice, which took about a second in Bench v2 CDB-G04.
fn revert_self_activation(target: i32, front: i32) -> bool {
    if !target_self_activated(skylight::front_process_pid(), target, front) {
        return false;
    }
    // The user may have switched to the target themselves (a click on its
    // window, or Cmd-Tab). Leave that alone.
    if user_switched_recently() {
        return false;
    }
    let started = Instant::now();
    // Make the user's key window frontmost and key again, the way the
    // foreground tools front an exact window (a user-generated front request
    // plus the make-key records). `NSRunningApplication.activate` from the
    // daemon is only a request under cooperative activation and can be
    // ignored, so it is the fallback.
    let restored = skylight::key_window_of_pid(front)
        .is_some_and(|wid| skylight::make_exact_window_key(front, wid))
        && skylight::front_process_pid() == Some(front);
    if !restored {
        use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
        unsafe {
            if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(front)
            {
                app.activateWithOptions(NSApplicationActivationOptions(0));
            }
        }
    }
    REVERTED.fetch_add(1, Ordering::Relaxed);
    tracing::debug!(
        target,
        front,
        restored,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "reverted a background target's self-activation"
    );
    true
}

/// A hardware mouse-down or modifier change within this long before an
/// activation means the user may have caused it.
const USER_SWITCH_WINDOW_S: f64 = 1.0;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
}

/// Whether the user pressed a mouse button or a modifier key on the real
/// hardware just now. Events the driver posts to a pid do not count.
pub(crate) fn user_switched_recently() -> bool {
    const HID_SYSTEM_STATE: i32 = 1;
    const LEFT_MOUSE_DOWN: u32 = 1;
    const RIGHT_MOUSE_DOWN: u32 = 3;
    const FLAGS_CHANGED: u32 = 12;
    [LEFT_MOUSE_DOWN, RIGHT_MOUSE_DOWN, FLAGS_CHANGED]
        .into_iter()
        .any(|kind| {
            let since = unsafe { CGEventSourceSecondsSinceLastEventType(HID_SYSTEM_STATE, kind) };
            since.is_finite() && since < USER_SWITCH_WINDOW_S
        })
}

/// How long the order stays watched after a guarded action ends. A target
/// can raise a window a moment after the call that caused it returns (a
/// banner, a sheet, or a menu that a renderer process opens later).
const ORDER_WATCH: Duration = Duration::from_millis(2500);
const ORDER_POLL: Duration = Duration::from_millis(20);
const MAX_RESTORES_PER_WATCH: u32 = 3;

/// One process-wide scheduler coalesces notices and tails. The event-tap
/// infrastructure never occupies a native mutation slot; each finite cleanup
/// turn must obtain admission and retains the originating dispatch authority.
fn ensure_cleanup_worker() {
    static WORKER: OnceLock<()> = OnceLock::new();
    WORKER.get_or_init(|| {
        std::thread::Builder::new()
            .name("cua-key-focus-cleanup".into())
            .spawn(|| loop {
                std::thread::sleep(ORDER_POLL);
                let mut st = lock();
                let now = Instant::now();
                st.guarded.retain(|pid, guard| {
                    let current = guard.epoch == crate::window_change_detector::intentional_epoch();
                    let due = guard.pending_release || guard.leases == 0;
                    if due {
                        let authority = guard.authority.clone();
                        // On saturation retain pending cleanup and its authority.
                        if authority
                            .try_run_cleanup(|| {
                                if guard.pending_release {
                                    if current && !user_switched_recently() {
                                        wait_for_menu_window(*pid, MENU_WAIT);
                                        if guard.epoch
                                            != crate::window_change_detector::intentional_epoch()
                                            || user_switched_recently()
                                        {
                                            return;
                                        }
                                        if release_target_theft() {
                                            RELEASED.fetch_add(1, Ordering::Relaxed);
                                        } else {
                                            MISSED.fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                    guard.pending_release = false;
                                }
                                if guard.can_restore(
                                    crate::window_change_detector::intentional_epoch(),
                                    Instant::now(),
                                    user_switched_recently(),
                                ) {
                                    if guard.reverts < MAX_RESTORES_PER_WATCH
                                        && revert_self_activation(*pid, guard.front)
                                    {
                                        guard.reverts += 1;
                                    }
                                    if guard.order
                                        && guard.restores < MAX_RESTORES_PER_WATCH
                                        && restore_front_if_raised(*pid, guard.front)
                                    {
                                        guard.restores += 1;
                                    }
                                }
                            })
                            .is_err()
                        {
                            return true;
                        }
                    }
                    guard.leases > 0 || now < guard.until || guard.pending_release
                });
                if st.guarded.is_empty() {
                    if let Some(tap) = st.tap.take() {
                        stop_tap(tap);
                    }
                }
            })
            .expect("key focus cleanup worker");
    });
}

/// Wait until `pid` shows a menu-level window, or `budget` runs out.
fn wait_for_menu_window(pid: i32, budget: Duration) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        // `visible_windows` lists layer 0 only; a menu is at layer 101.
        if crate::windows::all_windows_any_layer()
            .iter()
            .any(|w| w.pid == pid && w.layer >= POPUP_MENU_LAYER && w.is_on_screen)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// CGEvent type of the WindowServer focus notices an app receives.
const FOCUS_NOTICE_TYPE: u32 = 21;
/// Field holding the notice subtype.
const FIELD_SUBTYPE: u32 = 64;
/// Field holding the pid of the process that took key focus.
const FIELD_THIEF_PID: u32 = 73;
/// Subtype: "you lost key focus".
const SUBTYPE_LOST_KEY_FOCUS: i64 = 0x4000;

const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGEventTapCreateForPid(
        pid: i32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void,
        user_info: *mut c_void,
    ) -> *mut c_void;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: *mut c_void,
        order: isize,
    ) -> *mut c_void;
    fn CFMachPortInvalidate(port: *mut c_void);
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(rl: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRun();
    fn CFRunLoopStop(rl: *mut c_void);
    fn CFRelease(cf: *const c_void);
    static kCFRunLoopCommonModes: *const c_void;
}

/// What the guard did, for tool results and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Thefts by a guarded target that were released.
    pub released: u64,
    /// Thefts by a guarded target that could not be released.
    pub missed: u64,
}

struct Tap {
    front_pid: i32,
    port: usize,
    run_loop: usize,
}

struct Guard {
    id: u64,
    leases: usize,
    until: Instant,
    front: i32,
    epoch: u64,
    order: bool,
    pending_release: bool,
    restores: u32,
    reverts: u32,
    authority: cua_driver_core::blocking::ActionLeaseScope,
}
impl Guard {
    fn queue_release(&mut self, epoch: u64) -> bool {
        if self.epoch != epoch {
            return false;
        }
        self.pending_release = true;
        true
    }
    fn release_lease(&mut self, id: u64, now: Instant) {
        if self.id == id {
            self.leases = self.leases.saturating_sub(1);
            self.until = now + TAIL;
        }
    }
    fn can_restore(&self, epoch: u64, now: Instant, user_switched: bool) -> bool {
        self.epoch == epoch && now < self.until && !user_switched
    }
}

static NEXT_GUARD: AtomicU64 = AtomicU64::new(1);
const MAX_GUARDS: usize = 256;

#[derive(Default)]
struct State {
    tap: Option<Tap>,
    /// Guarded pid -> (live leases, end of the tail after the last lease).
    guarded: HashMap<i32, Guard>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn lock() -> std::sync::MutexGuard<'static, State> {
    state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static RELEASED: AtomicU64 = AtomicU64::new(0);
static MISSED: AtomicU64 = AtomicU64::new(0);
static RESTORED: AtomicU64 = AtomicU64::new(0);
static REVERTED: AtomicU64 = AtomicU64::new(0);

/// Counters since the daemon started.
pub fn stats() -> Stats {
    Stats {
        released: RELEASED.load(Ordering::Relaxed),
        missed: MISSED.load(Ordering::Relaxed),
    }
}

/// While alive, key-focus thefts by the guarded pid are handed back to the
/// user's frontmost app. Dropping it starts a short tail (see [`TAIL`]).
pub struct Lease {
    pid: i32,
    id: u64,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut st = lock();
        if let Some(guard) = st.guarded.get_mut(&self.pid).filter(|g| g.id == self.id) {
            guard.release_lease(self.id, Instant::now());
        }
    }
}

/// Guard the user's key focus while the driver acts on `target_pid` in the
/// background. Returns `None` (no guard) when the target is the frontmost app,
/// when there is no other frontmost app, or when the SPIs or the event tap are
/// unavailable; the action then runs exactly as before.
pub fn protect(target_pid: i32) -> Option<Lease> {
    if std::env::var_os("CUA_DRIVER_DISABLE_KEY_FOCUS_GUARD").is_some() {
        return None;
    }
    let own = std::process::id() as i32;
    if !skylight::key_focus_theft_available() {
        return None;
    }
    // Tap the process that holds key focus now. That is the one WindowServer
    // tells when a menu steals it, and it may be a non-activating panel
    // rather than the frontmost app. Only guard when it is the frontmost app:
    // otherwise a launcher-style panel would see the calibration theft.
    let front = crate::apps::frontmost_pid()?;
    let holder = skylight::key_focus_pid().unwrap_or(front);
    if holder != front || front == target_pid || front == own {
        return None;
    }
    cua_driver_core::blocking::check_cancelled().ok()?;
    ensure_cleanup_worker();
    let id = NEXT_GUARD.fetch_add(1, Ordering::Relaxed);
    let order = !target_above_front(target_pid, front);
    {
        let mut st = lock();
        if st.guarded.len() >= MAX_GUARDS && !st.guarded.contains_key(&target_pid) {
            return None;
        }
        // A newer guard supersedes the older tail atomically with callback
        // admission. Its authority is captured here, never on the tap thread.
        st.guarded.insert(
            target_pid,
            Guard {
                id,
                leases: 1,
                until: Instant::now() + TAIL,
                front,
                epoch: crate::window_change_detector::intentional_epoch(),
                order,
                pending_release: false,
                restores: 0,
                reverts: 0,
                authority: cua_driver_core::blocking::ActionLeaseScope::capture(),
            },
        );
    }
    let lease = Lease {
        pid: target_pid,
        id,
    };
    // Do not touch key focus here. This runs before every background action
    // while the user may be typing, and any key event that reaches
    // WindowServer while focus is away from their app is lost.
    if !ensure_tap(front) {
        return None;
    }
    Some(lease)
}

fn is_guarded(pid: i32) -> bool {
    let st = lock();
    st.guarded
        .get(&pid)
        .is_some_and(|g| g.leases > 0 || Instant::now() < g.until)
}

/// Release the theft a guarded target just made. Only called once the tap
/// has seen that theft, so the target's menu holds the keyboard already: a
/// steal on the driver's own connection takes the next id, and the target's
/// theft is the newest live one below it.
fn release_target_theft() -> bool {
    let Some(own) = skylight::steal_key_focus() else {
        return false;
    };
    skylight::release_key_focus(own);
    release_newest_theft_below(own, skylight::release_key_focus).is_some()
}

/// Release the newest live theft older than `own`, trying [`ID_SLACK`] older
/// ids too in case another app stole in between. `release` returns 0 when the
/// id was live. Pure apart from `release`, for tests.
fn release_newest_theft_below(own: i32, mut release: impl FnMut(i32) -> i32) -> Option<i32> {
    (1..=ID_SLACK + 1)
        .map(|back| own.wrapping_sub(back))
        .find(|&id| id != 0 && release(id) == 0)
}

/// What the tap does with one focus notice. Pure, for tests.
#[derive(Debug, PartialEq, Eq)]
enum NoticeAction {
    Pass,
    /// The driver's own theft while it looks up a target's id: swallow it.
    DropOwn,
    /// A guarded target stole key focus: release it, then swallow the notice.
    ReleaseAndDrop,
}

fn classify_notice(
    event_type: u32,
    subtype: i64,
    thief_pid: i64,
    own_pid: i32,
    guarded: impl Fn(i32) -> bool,
) -> NoticeAction {
    if event_type != FOCUS_NOTICE_TYPE || subtype != SUBTYPE_LOST_KEY_FOCUS {
        return NoticeAction::Pass;
    }
    let Ok(thief) = i32::try_from(thief_pid) else {
        return NoticeAction::Pass;
    };
    if thief == own_pid {
        NoticeAction::DropOwn
    } else if guarded(thief) {
        NoticeAction::ReleaseAndDrop
    } else {
        NoticeAction::Pass
    }
}

extern "C" fn tap_callback(
    _proxy: *mut c_void,
    event_type: u32,
    event: *mut c_void,
    _user_info: *mut c_void,
) -> *mut c_void {
    if event_type == TAP_DISABLED_BY_TIMEOUT || event_type == TAP_DISABLED_BY_USER_INPUT {
        if let Some(port) = lock().tap.as_ref().map(|tap| tap.port) {
            unsafe { CGEventTapEnable(port as *mut c_void, true) };
        }
        return event;
    }
    if event.is_null() {
        return event;
    }
    let (subtype, thief) = unsafe {
        (
            CGEventGetIntegerValueField(event, FIELD_SUBTYPE),
            CGEventGetIntegerValueField(event, FIELD_THIEF_PID),
        )
    };
    match classify_notice(
        event_type,
        subtype,
        thief,
        std::process::id() as i32,
        is_guarded,
    ) {
        NoticeAction::Pass => event,
        NoticeAction::DropOwn => std::ptr::null_mut(),
        NoticeAction::ReleaseAndDrop => {
            // Swallow the notice now, so the user's window never resigns key,
            // but release the theft only once the menu window is on screen.
            // Released earlier, AppKit orders the menu's window group to the
            // front, which raises the target's window over the user's.
            let mut st = lock();
            if let Some(guard) = st.guarded.get_mut(&(thief as i32)) {
                if guard.queue_release(crate::window_change_detector::intentional_epoch()) {
                    return std::ptr::null_mut();
                }
            }
            event
        }
    }
}

/// Make sure the tap sits on `front_pid`, the app that holds key focus.
fn ensure_tap(front_pid: i32) -> bool {
    static CREATION: Mutex<()> = Mutex::new(());
    let _creation = CREATION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    {
        let mut st = lock();
        if st
            .tap
            .as_ref()
            .is_some_and(|tap| tap.front_pid == front_pid)
        {
            return true;
        }
        if let Some(old) = st.tap.take() {
            stop_tap(old);
        }
    }
    let (tx, rx) = std::sync::mpsc::sync_channel::<Option<(usize, usize)>>(0);
    let spawned = std::thread::Builder::new()
        .name("cua-key-focus-guard".into())
        .spawn(move || unsafe {
            let mask = 1u64 << FOCUS_NOTICE_TYPE;
            // kCGHeadInsertEventTap = 0, kCGEventTapOptionDefault = 0.
            let port =
                CGEventTapCreateForPid(front_pid, 0, 0, mask, tap_callback, std::ptr::null_mut());
            if port.is_null() {
                let _ = tx.send(None);
                return;
            }
            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
            if source.is_null() {
                CFMachPortInvalidate(port);
                CFRelease(port);
                let _ = tx.send(None);
                return;
            }
            let run_loop = CFRunLoopGetCurrent();
            CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
            CGEventTapEnable(port, true);
            if tx.send(Some((port as usize, run_loop as usize))).is_ok() {
                CFRunLoopRun();
            }
            CFMachPortInvalidate(port);
            CFRelease(source);
            CFRelease(port);
        });
    if spawned.is_err() {
        return false;
    }
    match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Some((port, run_loop))) => {
            let mut st = lock();
            if let Some(old) = st.tap.replace(Tap {
                front_pid,
                port,
                run_loop,
            }) {
                stop_tap(old);
            }
            true
        }
        _ => {
            tracing::warn!(front_pid, "key-focus guard: event tap unavailable");
            false
        }
    }
}

fn stop_tap(tap: Tap) {
    unsafe {
        CGEventTapEnable(tap.port as *mut c_void, false);
        CFRunLoopStop(tap.run_loop as *mut c_void);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_notices_coalesce_and_newer_guard_or_activation_wins() {
        let now = Instant::now();
        let mut guard = Guard {
            id: 9,
            leases: 1,
            until: now + TAIL,
            front: 10,
            epoch: 4,
            order: true,
            pending_release: false,
            restores: 0,
            reverts: 0,
            authority: Default::default(),
        };
        assert!(guard.queue_release(4));
        assert!(guard.queue_release(4));
        assert!(guard.pending_release);
        assert!(!guard.queue_release(5));
        guard.release_lease(8, now);
        assert_eq!(guard.leases, 1);
        guard.release_lease(9, now);
        assert_eq!(guard.leases, 0);
        assert!(guard.can_restore(4, now, false));
        assert!(!guard.can_restore(5, now, false));
        assert!(!guard.can_restore(4, now, true));
        assert!(!guard.can_restore(4, now + TAIL, false));
    }

    const OWN: i32 = 100;

    #[test]
    fn a_guarded_target_theft_is_released_and_swallowed() {
        let action = classify_notice(21, 0x4000, 42, OWN, |pid| pid == 42);
        assert_eq!(action, NoticeAction::ReleaseAndDrop);
    }

    fn win(pid: i32, layer: i32, z_index: usize) -> crate::windows::WindowInfo {
        crate::windows::WindowInfo {
            window_id: z_index as u32,
            pid,
            app_name: String::new(),
            title: String::new(),
            bounds: crate::windows::WindowBounds {
                x: 0.0,
                y: 0.0,
                width: 400.0,
                height: 300.0,
            },
            layer,
            z_index,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    #[test]
    fn only_the_targets_own_activation_is_reverted() {
        // The target is WindowServer's front process: it activated itself.
        assert!(super::target_self_activated(Some(7), 7, OWN));
        // The user's app is still in front, or a third app took it (not ours
        // to undo), or nothing is known.
        assert!(!super::target_self_activated(Some(OWN), 7, OWN));
        assert!(!super::target_self_activated(Some(9), 7, OWN));
        assert!(!super::target_self_activated(None, 7, OWN));
        // A target that is the user's app is never reverted.
        assert!(!super::target_self_activated(Some(7), 7, 7));
    }

    #[test]
    fn a_target_window_above_the_users_window_is_detected() {
        // z_index: higher is closer to the front.
        let raised = [win(7, 0, 3), win(42, 0, 5), win(7, 0, 4)];
        assert!(target_above_front_in(&raised, 42, 7));
        let below = [win(7, 0, 5), win(42, 0, 3)];
        assert!(!target_above_front_in(&below, 42, 7));
        // A menu-level window does not count; only normal windows are raised.
        let menu_only = [win(7, 0, 4), win(42, 101, 9), win(42, 0, 2)];
        assert!(!target_above_front_in(&menu_only, 42, 7));
        // Without a window of either app there is nothing to restore.
        assert!(!target_above_front_in(&[win(42, 0, 9)], 42, 7));
    }

    #[test]
    fn the_drivers_own_theft_is_swallowed() {
        assert_eq!(
            classify_notice(21, 0x4000, OWN as i64, OWN, |_| false),
            NoticeAction::DropOwn
        );
    }

    #[test]
    fn other_apps_and_other_notices_pass_through() {
        // An app the driver is not acting on keeps normal macOS behaviour.
        assert_eq!(
            classify_notice(21, 0x4000, 7, OWN, |pid| pid == 42),
            NoticeAction::Pass
        );
        // "Key focus returned" and other subtypes are never touched.
        assert_eq!(
            classify_notice(21, 0x8000, 42, OWN, |_| true),
            NoticeAction::Pass
        );
        // Other event types are never touched.
        assert_eq!(
            classify_notice(13, 0x4000, 42, OWN, |_| true),
            NoticeAction::Pass
        );
        // A malformed pid passes.
        assert_eq!(
            classify_notice(21, 0x4000, i64::MAX, OWN, |_| true),
            NoticeAction::Pass
        );
    }

    #[test]
    fn the_targets_theft_is_the_newest_live_one_below_the_drivers() {
        // The target stole id 41 and the driver's lookup took 42.
        let mut tried = Vec::new();
        let released = release_newest_theft_below(42, |id| {
            tried.push(id);
            if id == 41 {
                0
            } else {
                1010
            }
        });
        assert_eq!(released, Some(41));
        assert_eq!(tried, vec![41]);

        // Another app stole in between and already let go: skip its id.
        assert_eq!(
            release_newest_theft_below(42, |id| if id == 40 { 0 } else { 1010 }),
            Some(40)
        );

        // Nothing live within reach: nothing is released, and the search
        // never goes further back than the slack allows.
        let mut tried = Vec::new();
        let released = release_newest_theft_below(42, |id| {
            tried.push(id);
            1010
        });
        assert_eq!(released, None);
        assert_eq!(tried, vec![41, 40, 39, 38]);

        // Id 0 means "no theft" and is never released.
        let mut tried = Vec::new();
        release_newest_theft_below(2, |id| {
            tried.push(id);
            1010
        });
        assert!(!tried.contains(&0));
    }

    /// Arming the guard runs before every background action, while the user
    /// may be typing. It must not steal key focus: a key event that reached
    /// WindowServer in that instant went to the driver and was lost (0.35.2,
    /// one keystroke in about 600 to 2500 during an agent run).
    #[test]
    fn arming_the_guard_never_steals_key_focus() {
        let source = include_str!("key_focus_guard.rs");
        let body = |name: &str| {
            let start = source
                .find(&format!("pub fn {name}("))
                .unwrap_or_else(|| panic!("{name} not found"));
            let end = source[start..].find("\n}\n").expect("end of fn") + start;
            &source[start..end]
        };
        let protect = body("protect");
        for call in ["steal_key_focus", "calibrate", "release_target_theft"] {
            assert!(!protect.contains(call), "protect() must not call {call}");
        }
    }
}
