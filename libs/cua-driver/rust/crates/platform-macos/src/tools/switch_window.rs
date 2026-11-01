//! Persistent window activation, with a fail-closed Spotlight keyboard rung.
//!
//! Native receipts are never focus evidence. Only the verified configured
//! Spotlight shortcut is global; search text/commands are addressed to the proven
//! Spotlight process, never whichever client wins a check-to-post focus race.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use core_foundation::{
    array::CFArray,
    base::{CFEqual, CFRelease, CFType, CFTypeRef, TCFType},
    boolean::CFBoolean,
    dictionary::CFDictionary,
    number::{CFNumber, CFNumberIsFloatType},
    string::{CFString, CFStringRef},
};
use core_graphics::{
    event::{CGEvent, CGEventFlags, CGEventTapLocation},
    event_source::{CGEventSource, CGEventSourceStateID},
};
use cua_driver_core::{
    blocking::{check_cancelled, spawn},
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    window_switch::{self, Attempt, SwitchArgs, SwitchMethod},
};
use objc2_app_kit::NSRunningApplication;
use serde_json::Value;

use crate::ax::bindings::{self as ax, AXUIElementRef};

pub struct SwitchWindowTool;

const WORKER_TIMEOUT: Duration = Duration::from_secs(8);
const SHELL_TIMEOUT: Duration = Duration::from_millis(1200);
const VERIFY_TIMEOUT: Duration = Duration::from_millis(1200);
const POLL: Duration = Duration::from_millis(25);

// Kept private to this adapter: the shared AX binding surface need not grow for
// a single proof. Both APIs are read-only and never request TCC authorization.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut i32) -> ax::AXError;
}

/// A retained AX element, used and released only inside the admitted worker.
struct Element(AXUIElementRef);

impl Element {
    unsafe fn owned(raw: AXUIElementRef) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            ax::AXUIElementSetMessagingTimeout(raw, 0.2);
            Some(Self(raw))
        }
    }

    fn attr(&self, name: &str) -> Option<Self> {
        unsafe { Self::owned(ax::copy_element_attr(self.0, name)?) }
    }

    fn string(&self, name: &str) -> Option<String> {
        unsafe { ax::copy_string_attr(self.0, name) }
    }

    fn window_id(&self) -> Option<u32> {
        unsafe { ax::ax_get_window_id(self.0) }
    }

    fn pid(&self) -> Option<i32> {
        let mut pid = 0;
        (unsafe { AXUIElementGetPid(self.0, &mut pid) } == ax::kAXErrorSuccess && pid > 0)
            .then_some(pid)
    }
}

impl Drop for Element {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as _) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Refusal(&'static str);

type SwitchResult<T> = Result<T, Refusal>;

/// Lock has precedence over unavailable permissions, missing windows and shell
/// readiness. Repeat after native reads and immediately before every mutation.
fn ready() -> SwitchResult<()> {
    let locked = crate::session::screen_locked();
    let cancelled = check_cancelled().is_err();
    readiness(locked, cancelled)
}

fn readiness(locked: bool, cancelled: bool) -> SwitchResult<()> {
    if locked {
        Err(Refusal("session_locked"))
    } else if cancelled {
        Err(Refusal("switch_unverified"))
    } else {
        Ok(())
    }
}

/// A pid-only activation uses an existing semantic key/main window, or the one
/// unambiguous ordinary window. Never choose a sibling by enumeration order.
fn select_window(
    requested: Option<u64>,
    focused: Option<u32>,
    main: Option<u32>,
    candidates: &[u32],
) -> SwitchResult<u32> {
    if let Some(requested) = requested {
        return u32::try_from(requested)
            .ok()
            .filter(|id| *id != 0 && candidates.contains(id))
            .ok_or(Refusal("window_not_found"));
    }
    focused
        .filter(|id| candidates.contains(id))
        .or_else(|| main.filter(|id| candidates.contains(id)))
        .or_else(|| (candidates.len() == 1).then(|| candidates[0]))
        .ok_or(Refusal(if candidates.len() > 1 {
            "target_ambiguous"
        } else {
            "window_not_found"
        }))
}

fn resolve_window(args: &SwitchArgs) -> SwitchResult<u32> {
    let candidates: Vec<_> = crate::windows::all_windows_without_spaces()
        .into_iter()
        .filter(|window| window.pid == args.pid && window.layer == 0)
        .filter(|window| !crate::cursor::overlay::is_overlay_window(window.window_id))
        .map(|window| window.window_id)
        .collect();
    ready()?;
    let app = unsafe { Element::owned(ax::AXUIElementCreateApplication(args.pid)) };
    let focused = app
        .as_ref()
        .and_then(|app| app.attr("AXFocusedWindow"))
        .and_then(|window| window.window_id());
    let main = app
        .as_ref()
        .and_then(|app| app.attr("AXMainWindow"))
        .and_then(|window| window.window_id());
    ready()?;
    select_window(args.window_id, focused, main, &candidates)
}

fn exact_ax_window(pid: i32, window_id: u32) -> SwitchResult<Element> {
    let app = unsafe { Element::owned(ax::AXUIElementCreateApplication(pid)) }
        .ok_or(Refusal("window_not_found"))?;
    let windows = unsafe { ax::copy_ax_windows_including(app.0, pid, window_id) };
    // Own every returned reference even when the match occurs early.
    let windows: Vec<_> = windows
        .into_iter()
        .filter_map(|window| unsafe { Element::owned(window) })
        .collect();
    let target = windows
        .into_iter()
        .find(|window| window.window_id() == Some(window_id));
    ready()?;
    target.ok_or(Refusal("switch_unverified"))
}

fn target_focused(pid: i32, window_id: u32) -> SwitchResult<bool> {
    ready()?;
    let focused = super::bring_to_front::exact_window_is_focused(pid, window_id);
    // A cached focused-window answer must not win over a lock or key-focus
    // stolen by a non-activating panel such as Spotlight.
    let key_owner = crate::input::skylight::key_focus_pid();
    ready()?;
    Ok(focused && key_owner == Some(pid))
}

fn native(pid: i32, window_id: u32) -> SwitchResult<bool> {
    let target = exact_ax_window(pid, window_id)?;
    let minimized = unsafe { ax::copy_bool_attr(target.0, "AXMinimized") };
    ready()?;
    if minimized == Some(true) {
        let attr = CFString::new("AXMinimized");
        let value = CFBoolean::false_value();
        ready()?;
        let status = unsafe {
            ax::AXUIElementSetAttributeValue(
                target.0,
                attr.as_concrete_TypeRef(),
                value.as_CFTypeRef(),
            )
        };
        ready()?;
        if status != ax::kAXErrorSuccess {
            return Err(Refusal("switch_unverified"));
        }
    }
    ready()?;
    crate::window_change_detector::end_tail();
    crate::window_change_detector::note_intentional_activation();
    // Same persistent SkyLight/Cocoa/exact AX machinery as bring_to_front.
    super::bring_to_front::activate_exact_window_with_guard(pid, window_id, || ready().is_ok());
    target_focused(pid, window_id)
}

fn safe_query(query: &str) -> bool {
    !query.trim().is_empty()
        && !query
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
}

/// Spotlight is a non-activating panel on some OS versions. WindowServer's
/// key-focus pid, not NSWorkspace's cached frontmost application, is the input
/// receiver oracle. Require system-wide AX focus to agree with that pid too.
fn search_field_role(role: Option<&str>, subrole: Option<&str>) -> bool {
    role == Some("AXSearchField")
        || (role == Some("AXTextField") && subrole == Some("AXSearchField"))
}

struct Spotlight {
    pid: i32,
    started: (u64, u64),
    field: Element,
}

/// PID recycling must not transfer a retained field's delivery authority.
fn process_started(pid: i32) -> Option<(u64, u64)> {
    if pid <= 0 {
        return None;
    }
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of_val(&info) as i32;
        let filled = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        );
        (filled == size && info.pbi_start_tvsec != 0)
            .then_some((info.pbi_start_tvsec, info.pbi_start_tvusec))
    }
}

const LEGACY_SPOTLIGHT: (&str, &str) = (
    "com.apple.Spotlight",
    "/System/Library/CoreServices/Spotlight.app/Contents/MacOS/Spotlight",
);
const CAMPO_SPOTLIGHT: (&str, &str) = (
    "com.apple.campo",
    "/System/Applications/Siri AI.app/Contents/MacOS/Siri AI",
);

fn spotlight_host(bundle: Option<&str>, executable: Option<&str>) -> bool {
    matches!((bundle, executable), (Some(bundle), Some(path))
        if (bundle, path) == LEGACY_SPOTLIGHT || (bundle, path) == CAMPO_SPOTLIGHT)
}

/// Pure authorization boundary: neither an app name, an AX text role, nor a
/// native fronting receipt alone authorizes any shell text or command key.
#[derive(Clone, Copy)]
struct SpotlightObservation<'a> {
    pid: i32,
    bundle: Option<&'a str>,
    executable: Option<&'a str>,
    started: Option<(u64, u64)>,
    final_started: Option<(u64, u64)>,
    element_pid: Option<i32>,
    role: Option<&'a str>,
    subrole: Option<&'a str>,
    identifier: Option<&'a str>,
    focused: Option<bool>,
    final_key_owner: Option<i32>,
}

impl SpotlightObservation<'_> {
    fn proven(&self) -> SwitchResult<()> {
        // macOS 27.2's Spotlight panel is hosted by Siri AI, NOT Dock or the
        // CampoRemoteService remote-view process. Trusted system-wide AX and
        // WindowServer observations agree on this field's owning/key-focus pid.
        // Siri has other text fields: its host identity alone is insufficient.
        let search_field = if self.bundle == Some(CAMPO_SPOTLIGHT.0) {
            self.role == Some("AXTextField")
                && self.subrole == Some("AXSearchField")
                && self.identifier == Some("SpotlightSearchField")
        } else {
            search_field_role(self.role, self.subrole)
        };
        (self.pid > 0
            && spotlight_host(self.bundle, self.executable)
            && self.started.is_some_and(|started| started.0 != 0)
            && self.final_started == self.started
            && self.element_pid == Some(self.pid)
            && search_field
            && self.focused == Some(true)
            && self.final_key_owner == Some(self.pid))
        .then_some(())
        .ok_or(Refusal("shell_not_ready"))
    }
}

fn spotlight() -> Option<Spotlight> {
    let pid = crate::input::skylight::key_focus_pid()?;
    let started = process_started(pid)?;
    if !spotlight_host(
        crate::apps::bundle_id_for_pid(pid).as_deref(),
        crate::apps::path_of_pid(pid).as_deref(),
    ) {
        return None;
    }
    let system = unsafe { Element::owned(AXUIElementCreateSystemWide()) }?;
    let field = system.attr("AXFocusedUIElement")?;
    let bundle = crate::apps::bundle_id_for_pid(pid);
    let executable = crate::apps::path_of_pid(pid);
    let element_pid = field.pid();
    let role = field.string("AXRole");
    let subrole = field.string("AXSubrole");
    let identifier = field.string("AXIdentifier");
    let focused = unsafe { ax::copy_bool_attr(field.0, "AXFocused") };
    // Reject a focus or process-incarnation change during the AX round trips.
    SpotlightObservation {
        pid,
        bundle: bundle.as_deref(),
        executable: executable.as_deref(),
        started: Some(started),
        final_started: process_started(pid),
        element_pid,
        role: role.as_deref(),
        subrole: subrole.as_deref(),
        identifier: identifier.as_deref(),
        focused,
        final_key_owner: crate::input::skylight::key_focus_pid(),
    }
    .proven()
    .ok()?;
    Some(Spotlight {
        pid,
        started,
        field,
    })
}

/// Native receiver boundary. Tests substitute only OS observations and event
/// posting; event construction and the guarded delivery path remain real.
trait SpotlightReceiver {
    fn pid(&self) -> i32;
    fn revalidate(&self) -> SwitchResult<()>;
}

impl SpotlightReceiver for Spotlight {
    fn pid(&self) -> i32 {
        self.pid
    }

    fn revalidate(&self) -> SwitchResult<()> {
        ready()?;
        let same = spotlight().is_some_and(|current| {
            current.pid == self.pid
                && current.started == self.started
                && unsafe { CFEqual(current.field.0 as _, self.field.0 as _) != 0 }
        });
        ready()?;
        if same {
            Ok(())
        } else {
            Err(Refusal("shell_not_ready"))
        }
    }
}

// Read the effective preference domain (including managed values), not just the
// user-override plist. These APIs never request permissions.
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFPreferencesCopyAppValue(key: CFStringRef, app: CFStringRef) -> CFTypeRef;
    fn CFPreferencesAppSynchronize(app: CFStringRef) -> u8;
}

fn copy_app_preference(app: &str, key: &str) -> SwitchResult<Option<CFType>> {
    let app = CFString::new(app);
    let key = CFString::new(key);
    unsafe {
        // Refresh any cached preference value before authorizing global input.
        if CFPreferencesAppSynchronize(app.as_concrete_TypeRef()) == 0 {
            // A failed read is not evidence that the override is absent.
            return Err(Refusal("keyboard_unavailable"));
        }
        let value = CFPreferencesCopyAppValue(key.as_concrete_TypeRef(), app.as_concrete_TypeRef());
        Ok((!value.is_null()).then(|| CFType::wrap_under_create_rule(value)))
    }
}

fn dictionary_value(value: &CFType, key: &str) -> Option<CFType> {
    let dictionary = value.downcast::<CFDictionary>()?;
    let key = CFString::new(key);
    let value = *dictionary.find(key.as_CFTypeRef())?;
    Some(unsafe { CFType::wrap_under_get_rule(value) })
}

/// Entry 64 is Spotlight search. The domain stores overrides: an absent
/// dictionary or entry uses macOS's enabled Command-Space default. A present
/// override must explicitly describe that exact shortcut; malformed values are
/// not absence and must never inherit the default.
fn command_space_configured(hotkeys: Option<&CFType>) -> Option<()> {
    let Some(hotkeys) = hotkeys else {
        return Some(());
    };
    hotkeys.downcast::<CFDictionary>()?;
    let Some(entry) = dictionary_value(hotkeys, "64") else {
        return Some(());
    };
    if dictionary_value(&entry, "enabled")?.downcast::<CFBoolean>()? != CFBoolean::true_value() {
        return None;
    }
    let value = dictionary_value(&entry, "value")?;
    if dictionary_value(&value, "type")?
        .downcast::<CFString>()?
        .to_string()
        != "standard"
    {
        return None;
    }
    let parameters = dictionary_value(&value, "parameters")?.downcast::<CFArray>()?;
    if parameters.len() != 3 {
        return None;
    }
    for (index, expected) in [32, 49, 1_048_576].into_iter().enumerate() {
        let raw = *parameters.get(index as isize)?;
        let number = unsafe { CFType::wrap_under_get_rule(raw) }.downcast::<CFNumber>()?;
        if unsafe { CFNumberIsFloatType(number.as_concrete_TypeRef()) } != 0
            || number.to_i64() != Some(expected)
        {
            return None;
        }
    }
    Some(())
}

trait KeyboardEvents {
    fn global(&mut self, event: &CGEvent);
    fn to_pid(&mut self, pid: i32, event: &CGEvent);
}

struct NativeEvents;

impl KeyboardEvents for NativeEvents {
    fn global(&mut self, event: &CGEvent) {
        event.post(CGEventTapLocation::HID);
    }

    fn to_pid(&mut self, pid: i32, event: &CGEvent) {
        // The public API addresses a receiver even if focus changes after proof.
        // Do not use a global keyboard helper or the SkyLight input shim here.
        event.post_to_pid(pid);
    }
}

fn key_events(code: u16, flags: CGEventFlags, text: Option<&str>) -> SwitchResult<[CGEvent; 2]> {
    let source = CGEventSource::new(CGEventSourceStateID::Private)
        .map_err(|_| Refusal("keyboard_unavailable"))?;
    let make = |down| {
        let event = CGEvent::new_keyboard_event(source.clone(), code, down)
            .map_err(|_| Refusal("keyboard_unavailable"))?;
        event.set_flags(flags);
        if let Some(text) = text {
            event.set_string(text);
        }
        Ok(event)
    };
    // Prepare both events before any input; creation failure is zero delivery.
    Ok([make(true)?, make(false)?])
}

fn open_spotlight(
    hotkeys: Option<&CFType>,
    events: &mut impl KeyboardEvents,
    guard: impl FnOnce() -> SwitchResult<()>,
) -> SwitchResult<()> {
    command_space_configured(hotkeys).ok_or(Refusal("keyboard_unavailable"))?;
    let pair = key_events(49, CGEventFlags::CGEventFlagCommand, None)?;
    guard()?;
    // One back-to-back flagged Space chord. No Command-down/up events, modifier
    // sleeps, or fallible work between the two posts. This is not an OS lock.
    events.global(&pair[0]);
    events.global(&pair[1]);
    Ok(())
}

fn spotlight_key(
    shell: &impl SpotlightReceiver,
    events: &mut impl KeyboardEvents,
    code: u16,
    flags: CGEventFlags,
    text: Option<&str>,
) -> SwitchResult<()> {
    for event in key_events(code, flags, text)? {
        // Revalidate the retained pid/incarnation/AX field before EVERY delivery,
        // including key-up. A check-to-post focus change cannot redirect input.
        shell.revalidate()?;
        events.to_pid(shell.pid(), &event);
    }
    Ok(())
}

fn spotlight_query(
    shell: &impl SpotlightReceiver,
    events: &mut impl KeyboardEvents,
    query: &str,
) -> SwitchResult<()> {
    if !safe_query(query) {
        return Err(Refusal("keyboard_unavailable"));
    }
    spotlight_key(shell, events, 0, CGEventFlags::CGEventFlagCommand, None)?;
    for ch in query.chars() {
        spotlight_key(
            shell,
            events,
            0,
            CGEventFlags::CGEventFlagNull,
            Some(&ch.to_string()),
        )?;
    }
    Ok(())
}

/// Cleanup retains the ORIGINAL receiver; never reacquire a new Spotlight or
/// send Escape to whichever client stole focus. A failed proof means no input.
fn dismiss_spotlight(shell: &impl SpotlightReceiver, events: &mut impl KeyboardEvents) {
    let _ = spotlight_key(shell, events, 53, CGEventFlags::CGEventFlagNull, None);
}

/// Return-down commits the selection. Spotlight may disappear before key-up,
/// which revokes all further input but is not evidence that activation failed.
fn commit_spotlight(
    shell: &impl SpotlightReceiver,
    events: &mut impl KeyboardEvents,
    verify_focus: impl FnOnce() -> SwitchResult<bool>,
) -> SwitchResult<bool> {
    let [down, up] = key_events(36, CGEventFlags::CGEventFlagNull, None)?;
    shell.revalidate()?;
    events.to_pid(shell.pid(), &down);
    let receiver_proven = shell.revalidate().is_ok();
    if receiver_proven {
        events.to_pid(shell.pid(), &up);
    }
    // Readiness/lock checks still belong to exact-target readback. Never retry
    // input (including cleanup) after a failed proof, even if focus returns.
    let outcome = verify_focus();
    if receiver_proven && !matches!(outcome, Ok(true)) {
        dismiss_spotlight(shell, events);
    }
    outcome
}

fn validate_keyboard_target(pid: i32, window_id: u32) -> SwitchResult<()> {
    ready()?;
    let target = crate::windows::window_info_by_id(window_id);
    ready()?;
    if target.is_some_and(|window| window.pid == pid && window.layer == 0) {
        Ok(())
    } else {
        Err(Refusal("window_not_found"))
    }
}

fn keyboard(args: &SwitchArgs, window_id: u32) -> SwitchResult<bool> {
    validate_keyboard_target(args.pid, window_id)?;
    let query = args.query.clone().or_else(|| unsafe {
        NSRunningApplication::runningApplicationWithProcessIdentifier(args.pid)
            .and_then(|app| app.localizedName())
            .map(|name| name.to_string())
    });
    ready()?;
    let query = query
        .filter(|query| safe_query(query))
        .ok_or(Refusal("keyboard_unavailable"))?;
    // AXIsProcessTrusted does not prompt. Never open the launcher when we
    // already know we cannot prove its receiver.
    if !unsafe { ax::AXIsProcessTrusted() } {
        return Err(Refusal("keyboard_unavailable"));
    }
    ready()?;
    crate::window_change_detector::end_tail();
    crate::window_change_detector::note_intentional_activation();
    crate::post_action::cancel();
    ready()?;
    let mut events = NativeEvents;
    let hotkeys = copy_app_preference("com.apple.symbolichotkeys", "AppleSymbolicHotKeys");
    ready()?;
    open_spotlight(hotkeys?.as_ref(), &mut events, ready)?;
    let deadline = Instant::now() + SHELL_TIMEOUT;
    let shell = loop {
        ready()?;
        if let Some(shell) = spotlight() {
            break shell;
        }
        ready()?;
        if Instant::now() >= deadline {
            // No proven Spotlight => no query, Return OR Escape.
            return Err(Refusal("shell_not_ready"));
        }
        std::thread::sleep(POLL);
    };
    let preparation = (|| {
        // Receiver-addressed Unicode input still requires exact AX readback;
        // enqueueing events alone never authorizes Return.
        spotlight_query(&shell, &mut events, &query)?;
        // Wait for AX to publish the complete text, without typing a second
        // query or pressing Return on an unknown/stale selection.
        let deadline = Instant::now() + SHELL_TIMEOUT;
        loop {
            shell.revalidate()?;
            let value = shell.field.string("AXValue");
            shell.revalidate()?;
            if value.as_deref() == Some(query.as_str()) {
                break;
            }
            if Instant::now() >= deadline {
                return Err(Refusal("shell_not_ready"));
            }
            std::thread::sleep(POLL);
        }
        // Never launch a replacement app after the selected running window exits.
        validate_keyboard_target(args.pid, window_id)
    })();
    if let Err(refusal) = preparation {
        dismiss_spotlight(&shell, &mut events);
        return Err(refusal);
    }
    commit_spotlight(&shell, &mut events, || {
        let deadline = Instant::now() + VERIFY_TIMEOUT;
        let mut stable_since = None;
        loop {
            if target_focused(args.pid, window_id)? {
                let since = stable_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_millis(100) {
                    return Ok(true);
                }
            } else {
                stable_since = None;
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(POLL);
        }
    })
}

fn finish(
    args: &SwitchArgs,
    window_id: Option<u32>,
    route: &str,
    focused: bool,
    attempts: Vec<Attempt>,
    code: Option<&str>,
) -> ToolResult {
    window_switch::result(
        args,
        window_id.map(u64::from),
        "macos",
        None,
        route,
        focused,
        attempts,
        code,
    )
}

fn switch_blocking(args: &SwitchArgs) -> ToolResult {
    let mut attempts = Vec::new();
    let route = if args.method == SwitchMethod::Keyboard {
        "keyboard"
    } else {
        "native"
    };
    let preparation = (|| {
        ready()?;
        if !crate::session::has_graphic_access() {
            ready()?;
            return Err(Refusal("unsupported"));
        }
        let window_id = resolve_window(args)?;
        let focused = target_focused(args.pid, window_id)?;
        Ok((window_id, focused))
    })();
    let (window_id, already_focused) = match preparation {
        Ok(target) => target,
        Err(Refusal(code)) => {
            attempts.push(Attempt::refused(route, code));
            return finish(args, None, route, false, attempts, Some(code));
        }
    };
    if already_focused {
        attempts.push(Attempt::focused("already_focused"));
        return finish(
            args,
            Some(window_id),
            "already_focused",
            true,
            attempts,
            None,
        );
    }
    if args.method != SwitchMethod::Keyboard {
        let outcome = native(args.pid, window_id);
        let code = match outcome {
            Ok(true) => {
                attempts.push(Attempt::focused("native"));
                return finish(args, Some(window_id), "native", true, attempts, None);
            }
            Ok(false) => "switch_unverified",
            Err(Refusal(code)) => code,
        };
        let attempt = if code == "switch_unverified" {
            Attempt::unverified("native", code)
        } else {
            Attempt::refused("native", code)
        };
        let fallback = window_switch::should_fallback(args.method, &attempt)
            && !matches!(
                code,
                "session_locked" | "window_not_found" | "target_ambiguous"
            )
            && check_cancelled().is_ok();
        attempts.push(attempt);
        if !fallback {
            return finish(args, Some(window_id), "native", false, attempts, Some(code));
        }
    }
    finish_keyboard(args, window_id, attempts, keyboard(args, window_id))
}

fn finish_keyboard(
    args: &SwitchArgs,
    window_id: u32,
    mut attempts: Vec<Attempt>,
    outcome: SwitchResult<bool>,
) -> ToolResult {
    match outcome {
        Ok(true) => {
            attempts.push(Attempt::focused("keyboard"));
            finish(args, Some(window_id), "keyboard", true, attempts, None)
        }
        outcome => {
            let code = match outcome {
                Err(Refusal(code)) => code,
                _ => "switch_unverified",
            };
            attempts.push(if code == "switch_unverified" {
                Attempt::unverified("keyboard", code)
            } else {
                Attempt::refused("keyboard", code)
            });
            finish(
                args,
                Some(window_id),
                "keyboard",
                false,
                attempts,
                Some(code),
            )
        }
    }
}

#[async_trait]
impl Tool for SwitchWindowTool {
    fn def(&self) -> &ToolDef {
        window_switch::definition()
    }

    async fn invoke(&self, value: Value) -> ToolResult {
        let args = match SwitchArgs::parse(&value) {
            Ok(args) => args,
            Err(error) => return error,
        };
        // spawn propagates and retains ActionLeaseScope, even after timeout or
        // caller cancellation; its native admission slot stays with the worker.
        let work = spawn(move || switch_blocking(&args));
        match tokio::time::timeout(WORKER_TIMEOUT, work).await {
            Ok(Ok(result)) => result,
            error => {
                // Parse again instead of requiring Clone on the shared args.
                let args = SwitchArgs::parse(&value).expect("already validated arguments");
                let route = if args.method == SwitchMethod::Keyboard {
                    "keyboard"
                } else {
                    "native"
                };
                let code = if crate::session::screen_locked() {
                    "session_locked"
                } else {
                    "switch_unverified"
                };
                let reason = if error.is_err() {
                    "bounded native worker timed out; no further input is authorized"
                } else {
                    "bounded native worker failed"
                };
                finish(
                    &args,
                    None,
                    route,
                    false,
                    vec![Attempt::unverified(route, reason)],
                    Some(code),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Every test below constructs real CF values / CGEvents but substitutes the
    // OS observation and event-posting edges. No test can emit host input.
    fn preference(value: Value) -> CFType {
        match value {
            Value::Bool(true) => CFBoolean::true_value().as_CFType(),
            Value::Bool(false) => CFBoolean::false_value().as_CFType(),
            Value::Number(number) => match number.as_i64() {
                Some(integer) => CFNumber::from(integer).as_CFType(),
                None => CFNumber::from(number.as_f64().unwrap()).as_CFType(),
            },
            Value::String(text) => CFString::new(&text).as_CFType(),
            Value::Array(values) => {
                CFArray::from_CFTypes(&values.into_iter().map(preference).collect::<Vec<_>>())
                    .as_CFType()
            }
            Value::Object(values) => CFDictionary::from_CFType_pairs(
                &values
                    .into_iter()
                    .map(|(key, value)| (CFString::new(&key), preference(value)))
                    .collect::<Vec<_>>(),
            )
            .as_CFType(),
            Value::Null => panic!("null is not a property-list value"),
        }
    }

    fn command_space() -> Value {
        json!({"64": {"enabled": true, "value": {
            "type": "standard", "parameters": [32, 49, 1048576]
        }}})
    }

    #[derive(Default)]
    struct RecordedEvents {
        deliveries: Vec<(Option<i32>, CGEvent)>,
    }

    impl KeyboardEvents for RecordedEvents {
        fn global(&mut self, event: &CGEvent) {
            self.deliveries.push((None, event.clone()));
        }
        fn to_pid(&mut self, pid: i32, event: &CGEvent) {
            self.deliveries.push((Some(pid), event.clone()));
        }
    }

    #[test]
    fn shortcut_refuses_present_disabled_malformed_or_remapped_preferences_without_input() {
        let mut invalid = vec![json!(true), json!([]), json!("AppleSymbolicHotKeys")];
        for (path, replacements) in [
            ("/64", vec![json!({}), json!("64")]),
            ("/64/enabled", vec![json!(false), json!(1), json!("true")]),
            ("/64/value", vec![json!({}), json!(true)]),
            ("/64/value/type", vec![json!("unknown"), json!(1)]),
            (
                "/64/value/parameters",
                vec![
                    json!([]),
                    json!([32, 49]),
                    json!([32, 49, 1048576, 0]),
                    json!([33, 49, 1048576]),
                    json!([32, 50, 1048576]),
                    json!([32, 49, 0]),
                    json!([32, 49, 1179648]),
                    json!([32, 49, "1048576"]),
                    json!([32, 49, 1048576.0]),
                    json!([true, 49, 1048576]),
                    json!("32,49,1048576"),
                ],
            ),
        ] {
            // Only the entire absent override inherits the OS default. Missing
            // fields inside a present entry remain malformed and refuse.
            if path != "/64" {
                let mut missing = command_space();
                let (parent, key) = path.rsplit_once('/').unwrap();
                missing
                    .pointer_mut(parent)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove(key);
                invalid.push(missing);
            }
            for replacement in replacements {
                let mut value = command_space();
                *value.pointer_mut(path).unwrap() = replacement;
                invalid.push(value);
            }
        }
        for value in invalid.into_iter().map(preference) {
            let mut events = RecordedEvents::default();
            assert_eq!(
                open_spotlight(Some(&value), &mut events, || Ok(())),
                Err(Refusal("keyboard_unavailable"))
            );
            assert!(events.deliveries.is_empty(), "refusal emitted input");
        }
    }

    #[test]
    fn missing_preference_file_uses_default_without_creating_a_user_override() {
        // Exercise the real read-only preference seam against a fresh namespace,
        // never the user's Spotlight configuration or any preference setters.
        let app = format!("com.trycua.switch-window-test.{}", uuid::Uuid::new_v4());
        let path = std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
            .join("Library/Preferences")
            .join(format!("{app}.plist"));
        assert!(!path.exists());
        let value = copy_app_preference(&app, "AppleSymbolicHotKeys").unwrap();
        assert!(value.is_none());
        assert_command_space(value.as_ref());
        assert!(!path.exists(), "read must not persist default overrides");
    }

    #[test]
    fn absent_dictionary_uses_default_command_space() {
        // CFPreferences returns NULL both for an absent AppleSymbolicHotKeys key
        // and a nonexistent domain/file, but not for a malformed present value.
        assert_command_space(None);
    }

    #[test]
    fn absent_entry_and_empty_dictionary_use_default_command_space() {
        for value in [json!({}), json!({"65": command_space()["64"]})] {
            assert_command_space(Some(&preference(value)));
        }
    }

    #[test]
    fn enabled_standard_override_uses_command_space() {
        assert_command_space(Some(&preference(command_space())));
    }

    fn assert_command_space(value: Option<&CFType>) {
        use core_graphics::event::{CGEventType, EventField};
        let mut events = RecordedEvents::default();
        assert_eq!(
            open_spotlight(value, &mut events, || Err(Refusal("session_locked"))),
            Err(Refusal("session_locked"))
        );
        assert!(events.deliveries.is_empty());
        assert_eq!(open_spotlight(value, &mut events, || Ok(())), Ok(()));
        assert_eq!(
            events.deliveries.len(),
            2,
            "no separate Command key transitions"
        );
        for ((pid, event), kind) in events
            .deliveries
            .iter()
            .zip([CGEventType::KeyDown, CGEventType::KeyUp])
        {
            assert_eq!(*pid, None);
            assert_eq!(event.get_type() as u32, kind as u32);
            assert_eq!(
                event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE),
                49
            );
            assert_eq!(event.get_flags(), CGEventFlags::CGEventFlagCommand);
        }
    }

    struct Receiver {
        valid: std::cell::Cell<bool>,
        checks: std::cell::Cell<usize>,
        lose_after_proof: Option<usize>,
        refusal: Refusal,
    }

    impl Receiver {
        fn focused() -> Self {
            Self {
                valid: std::cell::Cell::new(true),
                checks: std::cell::Cell::new(0),
                lose_after_proof: None,
                refusal: Refusal("shell_not_ready"),
            }
        }
    }

    impl SpotlightReceiver for Receiver {
        fn pid(&self) -> i32 {
            42
        }
        fn revalidate(&self) -> SwitchResult<()> {
            self.checks.set(self.checks.get() + 1);
            if !self.valid.get() {
                return Err(self.refusal);
            }
            if self.lose_after_proof == Some(self.checks.get()) {
                // Deterministic interleaving: proof succeeded, then a client
                // stole focus BEFORE the corresponding post was delivered.
                self.valid.set(false);
            }
            Ok(())
        }
    }

    #[test]
    fn spotlight_query_return_and_escape_are_pid_routed_even_after_the_final_focus_proof() {
        use core_graphics::event::{CGEventType, EventField};
        for (code, text, flags) in [
            (0, Some("é"), CGEventFlags::CGEventFlagNull),
            (0, None, CGEventFlags::CGEventFlagCommand),
            (36, None, CGEventFlags::CGEventFlagNull),
            (53, None, CGEventFlags::CGEventFlagNull),
        ] {
            for steal_at in [1, 2] {
                let shell = Receiver {
                    lose_after_proof: Some(steal_at),
                    ..Receiver::focused()
                };
                let mut events = RecordedEvents::default();
                let result = spotlight_key(&shell, &mut events, code, flags, text);
                assert!(
                    !shell.valid.get(),
                    "focus loss must occur after a successful proof"
                );
                assert_eq!(
                    result,
                    if steal_at == 1 {
                        Err(Refusal("shell_not_ready"))
                    } else {
                        Ok(())
                    }
                );
                assert_eq!(events.deliveries.len(), steal_at);
                // With global posting this last event would reach pid 99, not
                // Spotlight. The receiver is fixed even across this exact race.
                assert!(events.deliveries.iter().all(|(pid, _)| *pid == Some(42)));
                let event = &events.deliveries.last().unwrap().1;
                assert_eq!(
                    event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE),
                    code as i64
                );
                assert_eq!(event.get_flags(), flags);
                assert_eq!(
                    event.get_type() as u32,
                    if steal_at == 1 {
                        CGEventType::KeyDown
                    } else {
                        CGEventType::KeyUp
                    } as u32
                );
                let delivered = events.deliveries.len();
                dismiss_spotlight(&shell, &mut events);
                assert_eq!(
                    events.deliveries.len(),
                    delivered,
                    "no cleanup key after lost focus"
                );
            }
        }
    }

    #[test]
    fn committed_return_reconciles_focus_after_spotlight_vanishes_without_further_input() {
        use core_graphics::event::{CGEventType, EventField};
        let args = SwitchArgs::parse(&json!({
            "pid": 99, "window_id": 7, "method": "keyboard"
        }))
        .unwrap();
        for focused in [true, false] {
            let shell = Receiver {
                lose_after_proof: Some(1),
                ..Receiver::focused()
            };
            let mut events = RecordedEvents::default();
            let outcome = commit_spotlight(&shell, &mut events, || {
                assert_eq!(shell.checks.get(), 2, "key-up proof must have failed");
                assert!(!shell.valid.get(), "Spotlight vanished after Return-down");
                // Focus returning during readback must not reauthorize Escape.
                shell.valid.set(true);
                Ok(focused)
            });
            assert_eq!(outcome, Ok(focused));
            assert_eq!(shell.checks.get(), 2, "no cleanup/retry after failed proof");
            assert_eq!(events.deliveries.len(), 1, "no key-up or Escape");
            let (pid, event) = &events.deliveries[0];
            assert_eq!(*pid, Some(42));
            assert_eq!(event.get_type() as u32, CGEventType::KeyDown as u32);
            assert_eq!(
                event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE),
                36
            );
            let result = finish_keyboard(&args, 7, Vec::new(), outcome);
            assert_eq!(result.is_error, if focused { None } else { Some(true) });
            let result = result.structured_content.unwrap();
            assert_eq!(result["route"], "keyboard");
            assert_eq!(result["pid"], 99);
            assert_eq!(result["window_id"], 7);
            assert_eq!(result["focused"], focused);
            if focused {
                assert_eq!(result["effect"], "verified");
                assert_eq!(result["attempts"][0]["outcome"], "focused");
                assert!(result.get("code").is_none());
            } else {
                assert_eq!(result["code"], "switch_unverified");
                assert_eq!(result["attempts"][0]["outcome"], "unverified");
            }
        }
        let shell = Receiver::focused();
        shell.valid.set(false);
        let mut events = RecordedEvents::default();
        assert_eq!(
            commit_spotlight(&shell, &mut events, || panic!("no commit delivered")),
            Err(Refusal("shell_not_ready"))
        );
        assert!(events.deliveries.is_empty());
    }

    #[test]
    fn every_query_event_revalidates_and_no_later_input_or_cleanup_survives_refusal() {
        let query = "É日🦀";
        let count = 2 + query.chars().count() * 2;
        let shell = Receiver::focused();
        let mut events = RecordedEvents::default();
        spotlight_query(&shell, &mut events, query).unwrap();
        assert_eq!(events.deliveries.len(), count);
        assert_eq!(shell.checks.get(), count);
        assert!(events.deliveries.iter().all(|(pid, _)| *pid == Some(42)));
        // Inspect the real Unicode payload, including a surrogate pair, without
        // posting any event. A route-only fake must not hide broken text input.
        use foreign_types::ForeignType;
        extern "C" {
            fn CGEventKeyboardGetUnicodeString(
                event: *const std::ffi::c_void,
                max_length: usize,
                actual_length: *mut usize,
                text: *mut u16,
            );
        }
        for (pair, ch) in events.deliveries[2..].chunks_exact(2).zip(query.chars()) {
            for (_, event) in pair {
                let mut text = [0_u16; 2];
                let mut length = 0;
                unsafe {
                    CGEventKeyboardGetUnicodeString(
                        event.as_ptr().cast(),
                        text.len(),
                        &mut length,
                        text.as_mut_ptr(),
                    );
                }
                assert_eq!(String::from_utf16(&text[..length]).unwrap(), ch.to_string());
                assert_eq!(event.get_flags(), CGEventFlags::CGEventFlagNull);
            }
        }
        for steal_at in 1..count {
            let shell = Receiver {
                lose_after_proof: Some(steal_at),
                ..Receiver::focused()
            };
            let mut events = RecordedEvents::default();
            assert_eq!(
                spotlight_query(&shell, &mut events, query),
                Err(Refusal("shell_not_ready"))
            );
            assert_eq!(events.deliveries.len(), steal_at);
            assert_eq!(shell.checks.get(), steal_at + 1);
            dismiss_spotlight(&shell, &mut events);
            assert_eq!(events.deliveries.len(), steal_at);
            assert!(events.deliveries.iter().all(|(pid, _)| *pid == Some(42)));
        }
        // Missing/stale/replaced receiver, lock and cancellation refuse before
        // select-all, query, Return AND Escape. Never reacquire a different pid.
        for refusal in [
            Refusal("shell_not_ready"),
            Refusal("session_locked"),
            Refusal("switch_unverified"),
        ] {
            let shell = Receiver {
                refusal,
                ..Receiver::focused()
            };
            shell.valid.set(false);
            let mut events = RecordedEvents::default();
            assert_eq!(spotlight_query(&shell, &mut events, query), Err(refusal));
            assert_eq!(
                spotlight_key(&shell, &mut events, 36, CGEventFlags::CGEventFlagNull, None),
                Err(refusal)
            );
            dismiss_spotlight(&shell, &mut events);
            assert!(events.deliveries.is_empty());
        }
    }

    #[test]
    fn spotlight_incarnation_uses_stable_native_birth_time_and_refuses_dead_or_invalid_pids() {
        let pid = std::process::id() as i32;
        let started = process_started(pid).expect("own process must have a kernel birth stamp");
        assert!(started.0 > 0);
        assert_eq!(process_started(pid), Some(started));
        assert_eq!(process_started(0), None);
        assert_eq!(process_started(-1), None);
        let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let exited_pid = child.id() as i32;
        assert!(child.wait().unwrap().success());
        assert_eq!(process_started(exited_pid), None);
    }

    #[test]
    fn native_receiver_posting_keeper_cannot_be_replaced_with_global_or_skylight_delivery() {
        // The real OS call cannot execute in hermetic tests. Pin only the tiny
        // native edge; behavioral tests above own the dispatch/race contract.
        let source = include_str!("switch_window.rs");
        let native = source
            .split("impl KeyboardEvents for NativeEvents {")
            .nth(1)
            .unwrap()
            .split("\nfn key_events(")
            .next()
            .unwrap();
        let receiver = native.split("fn to_pid(").nth(1).unwrap();
        assert!(receiver.contains("event.post_to_pid(pid);"));
        assert!(!receiver.contains("event.post("));
        assert!(!receiver.contains("skylight"));
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("press_key_global("));
        assert!(!production.contains("type_text_global("));
    }

    #[test]
    fn selection_never_substitutes_a_sibling_or_truncates_window_ids() {
        assert_eq!(select_window(Some(7), Some(8), Some(8), &[7, 8]), Ok(7));
        assert_eq!(
            select_window(Some((1_u64 << 32) | 7), None, None, &[7]),
            Err(Refusal("window_not_found"))
        );
        assert_eq!(
            select_window(Some(9), Some(7), Some(7), &[7]),
            Err(Refusal("window_not_found"))
        );
        assert_eq!(select_window(None, Some(7), Some(8), &[7, 8]), Ok(7));
        assert_eq!(select_window(None, None, Some(8), &[7, 8]), Ok(8));
        assert_eq!(select_window(None, Some(9), None, &[7]), Ok(7));
        assert_eq!(
            select_window(None, None, None, &[7, 8]),
            Err(Refusal("target_ambiguous"))
        );
        assert_eq!(
            select_window(None, None, None, &[]),
            Err(Refusal("window_not_found"))
        );
    }

    #[test]
    fn spotlight_requires_search_role_not_an_arbitrary_text_field() {
        assert!(search_field_role(Some("AXSearchField"), None));
        assert!(search_field_role(
            Some("AXTextField"),
            Some("AXSearchField")
        ));
        for (role, subrole) in [
            (None, None),
            (Some("AXTextField"), None),
            (Some("AXTextField"), Some("AXSecureTextField")),
            (Some("AXTextArea"), Some("AXSearchField")),
            (Some("AXButton"), None),
        ] {
            assert!(!search_field_role(role, subrole));
        }
    }

    fn observed_spotlight(campo: bool) -> SpotlightObservation<'static> {
        let (bundle, executable) = if campo {
            CAMPO_SPOTLIGHT
        } else {
            LEGACY_SPOTLIGHT
        };
        SpotlightObservation {
            pid: 42,
            bundle: Some(bundle),
            executable: Some(executable),
            started: Some((123, 456)),
            final_started: Some((123, 456)),
            element_pid: Some(42),
            role: Some("AXTextField"),
            subrole: Some("AXSearchField"),
            identifier: campo.then_some("SpotlightSearchField"),
            focused: Some(true),
            final_key_owner: Some(42),
        }
    }

    fn assert_receiver_refused(observation: SpotlightObservation<'_>) {
        let mut events = RecordedEvents::default();
        let outcome = (|| {
            observation.proven()?;
            // No receiver proof => no select-all or query events.
            spotlight_query(&Receiver::focused(), &mut events, "Slack")?;
            Ok(true)
        })();
        assert_eq!(outcome, Err(Refusal("shell_not_ready")));
        assert!(events.deliveries.is_empty());
        let args =
            SwitchArgs::parse(&json!({"pid": 99, "window_id": 7, "method": "keyboard"})).unwrap();
        let result = finish_keyboard(&args, 7, Vec::new(), outcome);
        assert_eq!(result.is_error, Some(true));
        let result = result.structured_content.unwrap();
        assert_eq!(result["code"], "shell_not_ready");
        assert_eq!(result["effect"], "refused");
        assert_eq!(result["attempts"][0]["outcome"], "refused");
        assert_eq!(result["focused"], false);
    }

    #[test]
    fn spotlight_accepts_legacy_and_observed_campo_receiver_without_a_cg_window() {
        // Campo's layer-23 system dialog is absent from list_windows. Neither
        // receiver proof nor query delivery requires a shell CGWindow record.
        for observation in [
            observed_spotlight(false),
            SpotlightObservation {
                role: Some("AXSearchField"),
                subrole: None,
                ..observed_spotlight(false)
            },
            observed_spotlight(true),
        ] {
            assert_eq!(observation.proven(), Ok(()));
        }
    }

    #[test]
    fn spotlight_proof_fails_closed_on_every_missing_or_contested_receiver_fact() {
        for campo in [false, true] {
            let valid = observed_spotlight(campo);
            for bundle in [
                None,
                Some("com.apple.Terminal"),
                Some("com.apple.spotlight"),
                Some("com.apple.dock"),
                Some("com.apple.CampoRemoteService"),
            ] {
                assert_receiver_refused(SpotlightObservation { bundle, ..valid });
            }
            for executable in [
                None,
                Some("/tmp/Spotlight"),
                Some("/tmp/Siri AI"),
                Some("/System/Library/CoreServices/Dock.app/Contents/MacOS/Dock"),
            ] {
                assert_receiver_refused(SpotlightObservation {
                    executable,
                    ..valid
                });
            }
            // A trusted bundle identifier paired with the other host's path is
            // not an allowed identity; host contracts are pairs, not two lists.
            assert_receiver_refused(SpotlightObservation {
                executable: Some(if campo {
                    LEGACY_SPOTLIGHT.1
                } else {
                    CAMPO_SPOTLIGHT.1
                }),
                ..valid
            });
            for started in [None, Some((0, 0))] {
                assert_receiver_refused(SpotlightObservation {
                    started,
                    final_started: started,
                    ..valid
                });
            }
            for final_started in [None, Some((123, 457)), Some((124, 456))] {
                assert_receiver_refused(SpotlightObservation {
                    final_started,
                    ..valid
                });
            }
            for element_pid in [None, Some(99)] {
                assert_receiver_refused(SpotlightObservation {
                    element_pid,
                    ..valid
                });
            }
            for focused in [None, Some(false)] {
                assert_receiver_refused(SpotlightObservation { focused, ..valid });
            }
            for final_key_owner in [None, Some(99)] {
                assert_receiver_refused(SpotlightObservation {
                    final_key_owner,
                    ..valid
                });
            }
            for role in [None, Some("AXTextArea"), Some("AXButton")] {
                assert_receiver_refused(SpotlightObservation { role, ..valid });
            }
            for subrole in [None, Some("AXSecureTextField"), Some("AXUnknown")] {
                assert_receiver_refused(SpotlightObservation { subrole, ..valid });
            }
            assert_receiver_refused(SpotlightObservation { pid: 0, ..valid });
        }
    }

    #[test]
    fn campo_requires_exact_spotlight_identifier_and_role_not_arbitrary_siri_or_dock_fields() {
        let valid = observed_spotlight(true);
        for identifier in [
            None,
            Some(""),
            Some("SiriSearchField"),
            Some("spotlightsearchfield"),
            Some("SpotlightSearchField-extra"),
        ] {
            assert_receiver_refused(SpotlightObservation {
                identifier,
                ..valid
            });
        }
        assert_receiver_refused(SpotlightObservation {
            role: Some("AXSearchField"),
            ..valid
        });
        for identifier in [None, Some("SpotlightSearchField")] {
            assert_receiver_refused(SpotlightObservation {
                bundle: Some("com.apple.dock"),
                executable: Some("/System/Library/CoreServices/Dock.app/Contents/MacOS/Dock"),
                identifier,
                ..valid
            });
        }
    }

    #[test]
    fn keyboard_query_cannot_embed_return_escape_or_other_control_input() {
        assert!(safe_query("Visual Studio Code"));
        assert!(safe_query("Éditeur 日本語"));
        for query in [
            "",
            "  ",
            "Safari\n",
            "Safari\r",
            "\u{1b}",
            "a\t",
            "a\0",
            "a\u{2028}",
        ] {
            assert!(!safe_query(query), "unsafe query {query:?}");
        }
    }

    #[test]
    fn lock_refusal_precedes_cancellation() {
        assert_eq!(readiness(true, true), Err(Refusal("session_locked")));
        assert_eq!(readiness(true, false), Err(Refusal("session_locked")));
        assert_eq!(readiness(false, true), Err(Refusal("switch_unverified")));
        assert_eq!(readiness(false, false), Ok(()));
    }

    #[test]
    fn macos_uses_the_shared_schema_and_result_contract_without_gui_workers() {
        let definition = SwitchWindowTool.def();
        assert_eq!(definition.name, "switch_window");
        assert!(!definition.read_only);
        assert!(!definition.destructive);
        assert!(definition.idempotent);
        assert_eq!(definition.input_schema["additionalProperties"], false);
        let args = SwitchArgs::parse(&json!({"pid": 42, "window_id": 7})).unwrap();
        let failed = finish(
            &args,
            Some(7),
            "keyboard",
            false,
            vec![Attempt::refused("keyboard", "shell_not_ready")],
            Some("shell_not_ready"),
        );
        assert_eq!(failed.is_error, Some(true));
        let failed = failed.structured_content.unwrap();
        assert_eq!(failed["platform"], "macos");
        assert_eq!(failed["focused"], false);
        assert_eq!(failed["code"], "shell_not_ready");
        assert_eq!(failed["attempts"][0]["outcome"], "refused");
        assert!(failed.get("session").is_none());
    }
}
