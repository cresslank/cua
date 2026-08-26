//! Exact Windows UIA setup for Chromium existing-profile attachment.

use std::time::{Duration, Instant};
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::Write,
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use cua_driver_core::browser::{
    BrowserRefusal, BrowserRefusalCode, BrowserSetupDescriptor,
    EXISTING_PROFILE_SETUP_READY_TIMEOUT,
};
use windows::core::{Interface, BSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
use windows::Win32::UI::Accessibility::{
    IUIAutomationElement, IUIAutomationInvokePattern, IUIAutomationTogglePattern,
    IUIAutomationValuePattern, ToggleState_Off, ToggleState_On, UIA_InvokePatternId,
    UIA_TogglePatternId, UIA_ValuePatternId,
};

use crate::uia::UiaNode;

// Native Chromium chrome is localized, so its accessible names are diagnostic
// text rather than a stable automation contract. Bootstrap against the exact
// approved HWND, native-vs-renderer boundary, control type, supported action,
// uniqueness, and exact post-action state instead of maintaining language
// allowlists or accepting fuzzy labels. The internal setup page follows the
// same rule: its native URL and web-control topology are contracts; its
// localized document, heading, and checkbox names are not.

fn refusal(code: BrowserRefusalCode, message: impl Into<String>) -> BrowserRefusal {
    BrowserRefusal::new(code, message)
}

fn release_nodes(nodes: &[UiaNode]) {
    for node in nodes.iter().filter(|node| node.element_ptr != 0) {
        unsafe { drop(IUIAutomationElement::from_raw(node.element_ptr as *mut _)) };
    }
}

fn unique_web_actionable(
    nodes: &[UiaNode],
    control_type: &str,
    action: &str,
) -> Result<Option<usize>, BrowserRefusal> {
    let matches = nodes
        .iter()
        .filter(|node| {
            node.in_web_content
                && node.control_type == control_type
                && node.actions.iter().any(|value| value == action)
                && node.enabled != Some(false)
                && node.element_ptr != 0
        })
        .map(|node| node.element_ptr)
        .collect::<HashSet<_>>();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        _ => Err(refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            format!(
                "multiple web {control_type} controls expose the exact {action} action on the setup page"
            ),
        )),
    }
}

fn unique_native_actionable(
    nodes: &[UiaNode],
    control_type: &str,
    action: &str,
) -> Result<Option<usize>, BrowserRefusal> {
    unique_native_actionable_with_focus(nodes, control_type, action, element_has_keyboard_focus)
}

fn element_has_keyboard_focus(element_ptr: usize) -> bool {
    if element_ptr == 0 {
        return false;
    }
    let element = unsafe { IUIAutomationElement::from_raw(element_ptr as *mut _) };
    let focused = unsafe { element.CurrentHasKeyboardFocus() }
        .ok()
        .is_some_and(|value| value.as_bool());
    std::mem::forget(element);
    focused
}

fn unique_native_actionable_with_focus(
    nodes: &[UiaNode],
    control_type: &str,
    action: &str,
    mut has_keyboard_focus: impl FnMut(usize) -> bool,
) -> Result<Option<usize>, BrowserRefusal> {
    let matches = nodes
        .iter()
        .filter(|node| {
            !node.in_web_content
                && node.control_type == control_type
                && node.actions.iter().any(|value| value == action)
                && node.enabled != Some(false)
                && node.element_ptr != 0
        })
        .map(|node| node.element_ptr)
        .collect::<HashSet<_>>();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        _ => {
            let focused = matches
                .into_iter()
                .filter(|element| has_keyboard_focus(*element))
                .collect::<Vec<_>>();
            match focused.as_slice() {
                [element] => Ok(Some(*element)),
                _ => Err(refusal(
                    BrowserRefusalCode::BrowserWrongTargetRefused,
                    format!(
                        "multiple native {control_type} controls expose the exact {action} action, \
                         and keyboard focus did not identify exactly one"
                    ),
                )),
            }
        }
    }
}

fn native_tab_count(nodes: &[UiaNode]) -> usize {
    nodes
        .iter()
        .filter(|node| !node.in_web_content && node.control_type == "TabItem")
        .filter_map(|node| node.rect)
        .filter(|(left, top, right, bottom)| left < right && top < bottom)
        .collect::<std::collections::HashSet<_>>()
        .len()
}

fn exact_native_new_tab_button(nodes: &[UiaNode]) -> Result<Option<usize>, BrowserRefusal> {
    let Some(last_tab_index) = nodes.iter().rposition(|node| {
        !node.in_web_content
            && node.control_type == "TabItem"
            && node
                .rect
                .is_some_and(|(left, top, right, bottom)| left < right && top < bottom)
    }) else {
        return Ok(None);
    };
    let last_tab = &nodes[last_tab_index];
    let successor_index =
        (last_tab_index + 1..nodes.len()).find(|index| nodes[*index].depth <= last_tab.depth);
    let Some(successor) = successor_index.map(|index| &nodes[index]) else {
        return Ok(None);
    };
    let vertically_overlaps_tab_row = match (last_tab.rect, successor.rect) {
        (Some((_, tab_top, _, tab_bottom)), Some((_, button_top, _, button_bottom))) => {
            tab_top < button_bottom && button_top < tab_bottom
        }
        _ => false,
    };
    if successor.in_web_content
        || successor.control_type != "Button"
        || successor.enabled == Some(false)
        || successor.element_ptr == 0
        || !successor.actions.iter().any(|action| action == "invoke")
        || !vertically_overlaps_tab_row
    {
        return Err(refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            "the native tab strip did not expose one exact structural new-tab action",
        ));
    }
    Ok(Some(successor.element_ptr))
}

fn stable_native_tab_count(hwnd: u64, initial_count: usize) -> Result<usize, BrowserRefusal> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut previous = initial_count;
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let tree = crate::uia::walk_tree(hwnd, None);
        let current = native_tab_count(&tree.nodes);
        release_nodes(&tree.nodes);
        if current > 0 && current == previous {
            return Ok(current);
        }
        previous = current;
        if Instant::now() >= deadline {
            return Err(refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                "the approved Chromium window did not expose a stable native tab topology",
            ));
        }
    }
}

fn setup_page_proven(nodes: &[UiaNode], descriptor: &BrowserSetupDescriptor) -> bool {
    let exact_url_count = nodes
        .iter()
        .filter(|node| {
            !node.in_web_content
                && node.control_type == "Edit"
                && node.actions.iter().any(|value| value == "set_value")
                && node
                    .value
                    .as_deref()
                    .is_some_and(|value| value.trim().eq_ignore_ascii_case(descriptor.setup_url))
        })
        .count();
    let document_count = nodes
        .iter()
        .filter(|node| node.control_type == "Document" && !node.in_web_content)
        .count();
    exact_url_count == 1 && document_count == 1
}

fn exact_setup_checkbox(
    nodes: &[UiaNode],
    descriptor: &BrowserSetupDescriptor,
) -> Result<Option<usize>, BrowserRefusal> {
    if !setup_page_proven(nodes, descriptor) {
        return Ok(None);
    }
    unique_web_actionable(nodes, "CheckBox", "toggle")
}

unsafe fn invoke(
    element_ptr: usize,
    root_hwnd: u64,
    expected_pid: u32,
    description: &str,
) -> Result<(), BrowserRefusal> {
    let element = IUIAutomationElement::from_raw(element_ptr as *mut _);
    let result = (|| -> anyhow::Result<()> {
        let pattern = element.GetCurrentPattern(UIA_InvokePatternId)?;
        let pattern = pattern.cast::<IUIAutomationInvokePattern>()?;
        crate::uia::prove_element_mutation_target(&element, root_hwnd, expected_pid)?;
        pattern.Invoke()?;
        Ok(())
    })();
    std::mem::forget(element);
    result.map_err(|error| {
        refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            format!("the exact {description} UIA Invoke action failed: {error}"),
        )
    })
}

fn prove_window_owner(hwnd: u64, expected_pid: u32, operation: &str) -> Result<(), BrowserRefusal> {
    if crate::win32::window_owner_pid(hwnd) != Some(expected_pid) {
        return Err(refusal(
            BrowserRefusalCode::BrowserBindingStale,
            format!(
                "the approved browser window changed ownership immediately before {operation}; no UIA mutation was attempted"
            ),
        ));
    }
    Ok(())
}

unsafe fn set_value(
    element_ptr: usize,
    root_hwnd: u64,
    expected_pid: u32,
    value: &str,
) -> Result<(), BrowserRefusal> {
    let element = IUIAutomationElement::from_raw(element_ptr as *mut _);
    let result = (|| -> anyhow::Result<()> {
        let pattern = element.GetCurrentPattern(UIA_ValuePatternId)?;
        let pattern = pattern.cast::<IUIAutomationValuePattern>()?;
        crate::uia::prove_element_mutation_target(&element, root_hwnd, expected_pid)?;
        pattern.SetValue(&BSTR::from(value))?;
        Ok(())
    })();
    std::mem::forget(element);
    result.map_err(|error| {
        refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            format!("the exact UIA Value action failed: {error}"),
        )
    })
}

fn force_setup_foreground(
    target: windows::Win32::Foundation::HWND,
    expected_pid: u32,
) -> (bool, bool) {
    unsafe { crate::input::force_foreground_assisted_for_pid(target, expected_pid) }
}

fn confirm_setup_navigation(
    hwnd: u64,
    expected_pid: u32,
    element_ptr: usize,
    foregrounded_window: &mut bool,
    injected_global_input: &mut bool,
    focused_setup_address_field: &mut bool,
) -> Result<(), BrowserRefusal> {
    use windows::Win32::Foundation::HWND;
    let target = HWND(hwnd as *mut _);
    let displaced_target = crate::win32::capture_foreground_target(hwnd, Some(expected_pid))
        .ok_or_else(|| {
            refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                "the approved browser HWND changed ownership before setup navigation; no window or input mutation was attempted",
            )
        })?;
    let prior_target = match crate::win32::capture_current_foreground_target() {
        Some(prior) if prior.hwnd() != hwnd => Some(prior),
        Some(_) => None,
        None => {
            return Err(refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                "a stable prior foreground identity could not be captured before setup navigation",
            ));
        }
    };
    let navigation = (|| {
        let (fronted, injected) = force_setup_foreground(target, expected_pid);
        *injected_global_input |= injected;
        if !fronted {
            return Err(refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                "Windows refused the bounded foreground assist for the exact browser window",
            ));
        }
        *foregrounded_window = true;

        let element = unsafe { IUIAutomationElement::from_raw(element_ptr as *mut _) };
        let focused = unsafe {
            match crate::uia::prove_element_mutation_target(&element, hwnd, expected_pid) {
                Ok(()) => element
                    .SetFocus()
                    .and_then(|_| element.CurrentHasKeyboardFocus()),
                Err(error) => {
                    std::mem::forget(element);
                    return Err(refusal(
                        BrowserRefusalCode::BrowserWrongTargetRefused,
                        format!(
                            "could not revalidate the exact address field before focus: {error}"
                        ),
                    ));
                }
            }
        };
        std::mem::forget(element);
        match focused {
            Ok(value) if value.as_bool() => *focused_setup_address_field = true,
            Ok(_) => {
                return Err(refusal(
                    BrowserRefusalCode::BrowserWrongTargetRefused,
                    "the exact address-and-search field did not acquire keyboard focus",
                ));
            }
            Err(error) => {
                return Err(refusal(
                    BrowserRefusalCode::BrowserWrongTargetRefused,
                    format!("could not focus the exact address-and-search field: {error}"),
                ));
            }
        }

        *injected_global_input = true;
        crate::input::keyboard::send_key_synthesized_after_focus_for_pid(
            hwnd,
            Some(expected_pid),
            "enter",
            &[],
            || Ok(()),
        )
        .map_err(|error| {
            refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                format!("could not confirm the bounded setup navigation: {error}"),
            )
        })
    })();

    let restoration_failed = prior_target.is_some_and(|prior_target| {
        crate::win32::restore_foreground_target_if_still_displaced(
            prior_target,
            displaced_target,
            Duration::from_millis(500),
        ) == crate::win32::ForegroundRestoreOutcome::Failed
    });
    if restoration_failed {
        return Err(refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            "the setup navigation completed, but Windows refused to restore the prior foreground window",
        ));
    }
    navigation
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckboxState {
    Off,
    On,
}

unsafe fn checkbox_state(element_ptr: usize) -> Result<CheckboxState, BrowserRefusal> {
    let element = IUIAutomationElement::from_raw(element_ptr as *mut _);
    let result = element
        .GetCurrentPattern(UIA_TogglePatternId)
        .and_then(|pattern| pattern.cast::<IUIAutomationTogglePattern>())
        .and_then(|pattern| pattern.CurrentToggleState());
    std::mem::forget(element);
    match result {
        Ok(state) if state == ToggleState_Off => Ok(CheckboxState::Off),
        Ok(state) if state == ToggleState_On => Ok(CheckboxState::On),
        Ok(_) => Err(refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            "the exact remote-debugging checkbox had an indeterminate state",
        )),
        Err(error) => Err(refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            format!("could not read the exact remote-debugging checkbox: {error}"),
        )),
    }
}

unsafe fn toggle(
    element_ptr: usize,
    root_hwnd: u64,
    expected_pid: u32,
) -> Result<(), BrowserRefusal> {
    let element = IUIAutomationElement::from_raw(element_ptr as *mut _);
    let result = (|| -> anyhow::Result<()> {
        let pattern = element.GetCurrentPattern(UIA_TogglePatternId)?;
        let pattern = pattern.cast::<IUIAutomationTogglePattern>()?;
        crate::uia::prove_element_mutation_target(&element, root_hwnd, expected_pid)?;
        pattern.Toggle()?;
        Ok(())
    })();
    std::mem::forget(element);
    result.map_err(|error| {
        refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            format!("the exact UIA Toggle action failed: {error}"),
        )
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProfileResourceKey {
    volume: u32,
    index: u64,
}

struct SetupReservation {
    profile_path: PathBuf,
    marker_path: PathBuf,
    key: ProfileResourceKey,
    _profile_dir: File,
    release_on_drop: bool,
}

const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

fn profile_key(file: &File) -> Result<ProfileResourceKey, BrowserRefusal> {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }.map_err(
        |error| {
            refusal(
                BrowserRefusalCode::BrowserBindingStale,
                format!("could not query stable browser profile identity: {error}"),
            )
        },
    )?;
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(refusal(
            BrowserRefusalCode::BrowserBindingStale,
            "browser profile is not a direct non-reparse directory",
        ));
    }
    Ok(ProfileResourceKey {
        volume: information.dwVolumeSerialNumber,
        index: (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    })
}

impl SetupReservation {
    fn acquire(profile_path: &Path) -> Result<Self, BrowserRefusal> {
        let profile_path = profile_path.to_path_buf();
        let profile_dir = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&profile_path)
            .map_err(|error| {
                refusal(
                    BrowserRefusalCode::BrowserBindingStale,
                    format!("could not securely open browser profile: {error}"),
                )
            })?;
        let key = profile_key(&profile_dir)?;
        let marker_path = profile_path.join(format!(
            ".cua-driver-setup-{:08x}-{:016x}.active",
            key.volume, key.index
        ));
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker_path)
            .map_err(|error| {
                refusal(
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        BrowserRefusalCode::BrowserBindingAmbiguous
                    } else {
                        BrowserRefusalCode::BrowserRouteUnavailable
                    },
                    format!("browser profile has a pending or poisoned setup transaction: {error}"),
                )
            })?;
        marker
            .write_all(b"active\n")
            .and_then(|_| marker.sync_all())
            .map_err(|error| {
                refusal(
                    BrowserRefusalCode::BrowserRouteUnavailable,
                    format!("could not persist browser setup poison: {error}"),
                )
            })?;
        // Reprove the path still denotes the retained directory after creating
        // the durable marker; otherwise leave the marker poisoned.
        let reproved = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&profile_path)
            .map_err(|error| {
                refusal(
                    BrowserRefusalCode::BrowserBindingStale,
                    format!("browser profile moved while reserving it: {error}"),
                )
            })?;
        if profile_key(&reproved)? != key {
            return Err(refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "browser profile identity changed while reserving setup",
            ));
        }
        Ok(Self {
            profile_path,
            marker_path,
            key,
            _profile_dir: profile_dir,
            release_on_drop: true,
        })
    }

    fn retain_fail_closed(&mut self) {
        self.release_on_drop = false;
    }
    fn release_when_dropped(&mut self) {
        self.release_on_drop = true;
    }
}

impl Drop for SetupReservation {
    fn drop(&mut self) {
        if !self.release_on_drop {
            return;
        }
        let same = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&self.profile_path)
            .ok()
            .and_then(|file| profile_key(&file).ok())
            == Some(self.key);
        if !same || std::fs::remove_file(&self.marker_path).is_err() {
            eprintln!("cua-driver: browser setup reservation remains poisoned");
        }
    }
}

pub fn ensure_profile_discoverable(profile_path: &Path) -> Result<(), BrowserRefusal> {
    drop(SetupReservation::acquire(profile_path)?);
    Ok(())
}

pub struct SetupUiHandle {
    hwnd: u64,
    expected_pid: u32,
    descriptor: &'static BrowserSetupDescriptor,
    pub opened_setup_page: bool,
    pub enabled_remote_debugging: bool,
    pub focused_setup_address_field: bool,
    pub foregrounded_window: bool,
    pub injected_global_input: bool,
    enable_attempted: bool,
    remote_debugging_mutation_possible: bool,
    terminal_cleanup_attempted: bool,
    reservation: SetupReservation,
}

impl SetupUiHandle {
    fn rollback_remote_debugging(&mut self) -> bool {
        if !self.enabled_remote_debugging && !self.remote_debugging_mutation_possible {
            return true;
        }
        let tree = crate::uia::walk_tree(self.hwnd, None);
        let checkbox = exact_setup_checkbox(&tree.nodes, self.descriptor);
        let action_succeeded = match checkbox {
            Ok(Some(element)) => unsafe {
                match checkbox_state(element) {
                    Ok(CheckboxState::Off) => true,
                    Ok(CheckboxState::On) => prove_window_owner(
                        self.hwnd,
                        self.expected_pid,
                        "remote-debugging rollback",
                    )
                    .and_then(|()| toggle(element, self.hwnd, self.expected_pid))
                    .is_ok(),
                    Err(_) => false,
                }
            },
            _ => false,
        };
        release_nodes(&tree.nodes);
        // Toggle success is not proof. Rewalk and positively observe the exact
        // checkbox Off before releasing the profile transaction.
        let restored = action_succeeded && {
            let proof = crate::uia::walk_tree(self.hwnd, None);
            let off = matches!(
                exact_setup_checkbox(&proof.nodes, self.descriptor),
                Ok(Some(element)) if unsafe {
                    matches!(checkbox_state(element), Ok(CheckboxState::Off))
                }
            );
            release_nodes(&proof.nodes);
            off
        };
        if restored {
            self.enabled_remote_debugging = false;
            self.remote_debugging_mutation_possible = false;
        }
        restored
    }

    pub fn abort(mut self, error: BrowserRefusal) -> BrowserRefusal {
        let enabled_remote_debugging = self.enabled_remote_debugging;
        let restored_remote_debugging = self.rollback_remote_debugging();
        let opened_setup_page = self.opened_setup_page;
        let focused_setup_address_field = self.focused_setup_address_field;
        let foregrounded_window = self.foregrounded_window;
        let injected_global_input = self.injected_global_input;
        let closed_setup_page = self.close().unwrap_or(!opened_setup_page);
        if restored_remote_debugging && closed_setup_page {
            self.reservation.release_when_dropped();
        } else {
            self.reservation.retain_fail_closed();
        }
        self.terminal_cleanup_attempted = true;
        let mut error = error;
        let cause = error.detail.take();
        error.with_detail(serde_json::json!({
            "setup_side_effects": {
                "opened_setup_page": opened_setup_page,
                "closed_setup_page": closed_setup_page,
                "focused_setup_address_field": focused_setup_address_field,
                "enabled_remote_debugging": enabled_remote_debugging,
                "foregrounded_window": foregrounded_window,
                "injected_global_input": injected_global_input,
                "restored_remote_debugging": restored_remote_debugging,
            },
            "cause": cause,
        }))
    }

    pub fn close_for_success(mut self) -> Result<Option<bool>, BrowserRefusal> {
        if !self.opened_setup_page {
            self.reservation.release_when_dropped();
            self.terminal_cleanup_attempted = true;
            return Ok(None);
        }
        let tree = crate::uia::walk_tree(self.hwnd, None);
        let proven = setup_page_proven(&tree.nodes, self.descriptor);
        release_nodes(&tree.nodes);
        if !proven {
            let error = refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                "the temporary setup page was no longer exact before cleanup",
            );
            return Err(self.abort(error));
        }
        if let Err(error) = crate::input::keyboard::send_key_synthesized_after_focus_for_pid(
            self.hwnd,
            Some(self.expected_pid),
            "w",
            &["ctrl"],
            || Ok(()),
        ) {
            let error = refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                format!("could not close the exact temporary setup tab: {error}"),
            );
            return Err(self.abort(error));
        }
        self.opened_setup_page = false;
        self.reservation.release_when_dropped();
        self.terminal_cleanup_attempted = true;
        Ok(Some(true))
    }

    pub fn close(&mut self) -> Option<bool> {
        if !self.opened_setup_page {
            return None;
        }
        let tree = crate::uia::walk_tree(self.hwnd, None);
        let proven = setup_page_proven(&tree.nodes, self.descriptor);
        release_nodes(&tree.nodes);
        Some(
            proven
                && crate::input::keyboard::send_key_synthesized_after_focus_for_pid(
                    self.hwnd,
                    Some(self.expected_pid),
                    "w",
                    &["ctrl"],
                    || Ok(()),
                )
                .is_ok(),
        )
    }
}

impl Drop for SetupUiHandle {
    fn drop(&mut self) {
        if self.terminal_cleanup_attempted {
            return;
        }
        let restored = self.rollback_remote_debugging();
        let had_setup_page = self.opened_setup_page;
        let closed = self.close().unwrap_or(!had_setup_page);
        if restored && closed {
            self.reservation.release_when_dropped();
        } else {
            self.reservation.retain_fail_closed();
        }
        self.terminal_cleanup_attempted = true;
    }
}

fn pending_setups() -> &'static Mutex<HashMap<u64, SetupUiHandle>> {
    static PENDING: OnceLock<Mutex<HashMap<u64, SetupUiHandle>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn retain_pending(hwnd: u64, handle: SetupUiHandle) -> Result<(), BrowserRefusal> {
    let mut pending = pending_setups().lock().unwrap();
    if pending.contains_key(&hwnd) {
        drop(pending);
        return Err(handle.abort(refusal(
            BrowserRefusalCode::BrowserBindingAmbiguous,
            "another approved browser setup is already pending for this exact window",
        )));
    }
    pending.insert(hwnd, handle);
    Ok(())
}

pub fn commit_pending(hwnd: u64) -> Result<bool, BrowserRefusal> {
    let handle = pending_setups()
        .lock()
        .unwrap()
        .remove(&hwnd)
        .ok_or_else(|| {
            refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "the exact pending browser setup cleanup handle is missing",
            )
        })?;
    Ok(handle.close_for_success()?.unwrap_or(false))
}

pub fn abort_pending(hwnd: u64, error: BrowserRefusal) -> BrowserRefusal {
    match pending_setups().lock().unwrap().remove(&hwnd) {
        Some(handle) => handle.abort(error),
        None => error.with_detail(serde_json::json!({
            "setup_cleanup": "the exact pending browser setup cleanup handle was missing"
        })),
    }
}

fn set_remote_debugging(
    hwnd: u64,
    expected_pid: u32,
    descriptor: &'static BrowserSetupDescriptor,
    profile_path: &Path,
    desired_enabled: bool,
) -> Result<SetupUiHandle, BrowserRefusal> {
    // Reserve the stable profile resource before the first tree read/mutation.
    let mut reservation = Some(SetupReservation::acquire(profile_path)?);
    let initial = crate::uia::walk_tree(hwnd, None);
    let initial_checkbox = exact_setup_checkbox(&initial.nodes, descriptor);
    let mut handle = match initial_checkbox {
        Ok(Some(_)) => SetupUiHandle {
            hwnd,
            expected_pid,
            descriptor,
            opened_setup_page: false,
            enabled_remote_debugging: false,
            focused_setup_address_field: false,
            foregrounded_window: false,
            injected_global_input: false,
            enable_attempted: false,
            remote_debugging_mutation_possible: false,
            terminal_cleanup_attempted: false,
            reservation: reservation.take().expect("setup reservation available"),
        },
        Ok(None) => {
            let initial_tab_count = native_tab_count(&initial.nodes);
            release_nodes(&initial.nodes);
            let tab_count_before = stable_native_tab_count(hwnd, initial_tab_count)?;

            let mut handle = SetupUiHandle {
                hwnd,
                expected_pid,
                descriptor,
                opened_setup_page: false,
                enabled_remote_debugging: false,
                focused_setup_address_field: false,
                foregrounded_window: false,
                injected_global_input: false,
                enable_attempted: false,
                remote_debugging_mutation_possible: false,
                terminal_cleanup_attempted: false,
                reservation: reservation.take().expect("setup reservation available"),
            };
            let tab_tree = crate::uia::walk_tree(hwnd, None);
            let new_tab_button = match exact_native_new_tab_button(&tab_tree.nodes) {
                Ok(Some(element)) => element,
                Ok(None) => {
                    release_nodes(&tab_tree.nodes);
                    return Err(handle.abort(refusal(
                        BrowserRefusalCode::BrowserWrongTargetRefused,
                        format!(
                            "the exact {} window has no structural native new-tab action",
                            descriptor.product_name
                        ),
                    )));
                }
                Err(error) => {
                    release_nodes(&tab_tree.nodes);
                    return Err(handle.abort(error));
                }
            };
            // Invocation can mutate before UIA reports an error. Persist the
            // profile poison and revalidate the exact HWND/element at the action boundary.
            handle.reservation.retain_fail_closed();
            if let Err(error) = prove_window_owner(hwnd, expected_pid, "native new-tab invocation")
                .and_then(|()| unsafe {
                    invoke(new_tab_button, hwnd, expected_pid, "native new-tab button")
                })
            {
                release_nodes(&tab_tree.nodes);
                return Err(handle.abort(error));
            }
            release_nodes(&tab_tree.nodes);
            handle.opened_setup_page = true;

            let deadline = Instant::now() + Duration::from_secs(3);
            let mut created = loop {
                let tree = crate::uia::walk_tree(hwnd, None);
                let tab_count_after = native_tab_count(&tree.nodes);
                if tab_count_after == tab_count_before + 1 {
                    break tree;
                }
                release_nodes(&tree.nodes);
                if Instant::now() >= deadline {
                    return Err(handle.abort(refusal(
                        BrowserRefusalCode::BrowserWrongTargetRefused,
                        format!(
                            "{} did not expose exactly one newly created tab",
                            descriptor.product_name
                        ),
                    )));
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            let omnibox = match unique_native_actionable(&created.nodes, "Edit", "set_value") {
                Ok(Some(element)) => element,
                Ok(None) => {
                    release_nodes(&created.nodes);
                    return Err(handle.abort(refusal(
                        BrowserRefusalCode::BrowserWrongTargetRefused,
                        format!(
                            "the approved {} window has no unique native editable address field",
                            descriptor.product_name
                        ),
                    )));
                }
                Err(error) => {
                    release_nodes(&created.nodes);
                    return Err(handle.abort(error));
                }
            };
            let updated = prove_window_owner(hwnd, expected_pid, "address-field update").and_then(
                |()| unsafe { set_value(omnibox, hwnd, expected_pid, descriptor.setup_url) },
            );
            release_nodes(&created.nodes);
            if let Err(error) = updated {
                return Err(handle.abort(error));
            }
            created = crate::uia::walk_tree(hwnd, None);
            let refreshed_omnibox =
                match unique_native_actionable(&created.nodes, "Edit", "set_value") {
                    Ok(Some(element))
                        if created.nodes.iter().any(|node| {
                            node.element_ptr == element
                                && node.value.as_deref().is_some_and(|value| {
                                    value.trim().eq_ignore_ascii_case(descriptor.setup_url)
                                })
                        }) =>
                    {
                        element
                    }
                    Ok(_) => {
                        release_nodes(&created.nodes);
                        return Err(handle.abort(refusal(
                            BrowserRefusalCode::BrowserWrongTargetRefused,
                            "the unique native address field did not retain the exact setup URL",
                        )));
                    }
                    Err(error) => {
                        release_nodes(&created.nodes);
                        return Err(handle.abort(error));
                    }
                };
            if let Err(error) = confirm_setup_navigation(
                hwnd,
                expected_pid,
                refreshed_omnibox,
                &mut handle.foregrounded_window,
                &mut handle.injected_global_input,
                &mut handle.focused_setup_address_field,
            ) {
                release_nodes(&created.nodes);
                return Err(handle.abort(error));
            }
            release_nodes(&created.nodes);
            handle
        }
        Err(error) => {
            release_nodes(&initial.nodes);
            return Err(error);
        }
    };
    if !handle.opened_setup_page {
        release_nodes(&initial.nodes);
    }

    let deadline = Instant::now() + EXISTING_PROFILE_SETUP_READY_TIMEOUT;
    loop {
        let tree = crate::uia::walk_tree(hwnd, None);
        let checkbox = exact_setup_checkbox(&tree.nodes, descriptor);
        match checkbox {
            Ok(Some(element)) => {
                let state = unsafe { checkbox_state(element) };
                let outcome = match state {
                    Ok(state) if (state == CheckboxState::On) == desired_enabled => {
                        if desired_enabled && handle.enable_attempted {
                            handle.enabled_remote_debugging = true;
                        } else if !desired_enabled {
                            handle.enabled_remote_debugging = false;
                        }
                        Ok(true)
                    }
                    Ok(_) if !handle.enable_attempted => {
                        // Arm rollback and durable poison before Toggle: an
                        // HRESULT/transport failure may follow a real mutation.
                        handle.enable_attempted = true;
                        handle.remote_debugging_mutation_possible = true;
                        handle.reservation.retain_fail_closed();
                        prove_window_owner(hwnd, expected_pid, "remote-debugging toggle").and_then(
                            |()| unsafe { toggle(element, hwnd, expected_pid).map(|_| false) },
                        )
                    }
                    Ok(_) => Ok(false),
                    Err(error) => Err(error),
                };
                release_nodes(&tree.nodes);
                match outcome {
                    Ok(true) => return Ok(handle),
                    Ok(false) => handle.enable_attempted = true,
                    Err(error) => return Err(handle.abort(error)),
                }
            }
            Ok(None) => release_nodes(&tree.nodes),
            Err(error) => {
                release_nodes(&tree.nodes);
                return Err(handle.abort(error));
            }
        }
        if Instant::now() >= deadline {
            return Err(handle.abort(refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                format!(
                    "the exact {} remote-debugging setup page did not become ready",
                    descriptor.product_name
                ),
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn enable(
    hwnd: u64,
    expected_pid: u32,
    descriptor: &'static BrowserSetupDescriptor,
    profile_path: &Path,
) -> Result<SetupUiHandle, BrowserRefusal> {
    set_remote_debugging(hwnd, expected_pid, descriptor, profile_path, true)
}

pub fn disable(
    hwnd: u64,
    expected_pid: u32,
    descriptor: &'static BrowserSetupDescriptor,
    profile_path: &Path,
) -> Result<bool, BrowserRefusal> {
    let handle = set_remote_debugging(hwnd, expected_pid, descriptor, profile_path, false)?;
    Ok(handle.close_for_success()?.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_driver_core::browser::{existing_profile_setup_descriptor, BrowserProduct};

    fn descriptor() -> &'static BrowserSetupDescriptor {
        existing_profile_setup_descriptor(BrowserProduct::GoogleChrome).unwrap()
    }

    fn node(control_type: &str, name: &str, value: Option<&str>, actions: &[&str]) -> UiaNode {
        UiaNode {
            element_index: (!actions.is_empty()).then_some(0),
            control_type: control_type.to_owned(),
            name: Some(name.to_owned()),
            value: value.map(str::to_owned),
            automation_id: None,
            help_text: None,
            actions: actions.iter().map(|value| (*value).to_owned()).collect(),
            enabled: None,
            selected: None,
            element_ptr: 7,
            center_x: 0,
            center_y: 0,
            rect: None,
            msaa_role: None,
            depth: 0,
            parent_element_index: None,
            in_web_content: false,
        }
    }

    #[test]
    fn checkbox_requires_exact_internal_url_and_unique_web_toggle() {
        let mut checkbox = node("CheckBox", descriptor().checkbox_label, None, &["toggle"]);
        checkbox.in_web_content = true;
        let nodes = vec![
            node(
                "Edit",
                "Address and search bar",
                Some(descriptor().setup_url),
                &["set_value"],
            ),
            node("Document", descriptor().page_titles[0], None, &[]),
            node("Header", descriptor().page_heading, None, &[]),
            checkbox,
        ];
        assert_eq!(exact_setup_checkbox(&nodes, descriptor()).unwrap(), Some(7));

        let mut localized = nodes.clone();
        localized[0].name = Some("アドレス検索バー".to_owned());
        localized[1].name = Some("远程调试页面".to_owned());
        localized[2].name = Some("Удалённая отладка".to_owned());
        localized[3].name = Some("السماح بتصحيح الأخطاء لهذا المتصفح".to_owned());
        localized[3].in_web_content = true;
        assert_eq!(
            exact_setup_checkbox(&localized, descriptor()).unwrap(),
            Some(7)
        );

        let mut wrong_url = nodes.clone();
        wrong_url[0].value = Some("https://example.test/".to_owned());
        assert_eq!(
            exact_setup_checkbox(&wrong_url, descriptor()).unwrap(),
            None
        );
    }

    #[test]
    fn setup_page_names_are_opaque_across_unicode_scripts_and_normalization() {
        let samples = [
            ("e\u{301}", "é", "✅"),
            ("हिन्दी", "ไทย", "עברית"),
            ("日本語", "한국어", "简体中文"),
            ("\u{2067}العربية\u{2069}", "فارسی", "اردو"),
            ("Հայերեն", "ქართული", "አማርኛ"),
            ("👩🏽‍💻", "A\u{200d}B", "𐐷"),
            ("", "", ""),
        ];
        for (document_name, heading_name, checkbox_name) in samples {
            let mut checkbox = node("CheckBox", checkbox_name, None, &["toggle"]);
            checkbox.in_web_content = true;
            let nodes = vec![
                node(
                    "Edit",
                    "opaque native address field",
                    Some(descriptor().setup_url),
                    &["set_value"],
                ),
                node("Document", document_name, None, &[]),
                node("Header", heading_name, None, &[]),
                checkbox,
            ];
            assert_eq!(exact_setup_checkbox(&nodes, descriptor()).unwrap(), Some(7));
        }
    }

    #[test]
    fn setup_page_does_not_require_accessible_names() {
        let mut checkbox = node("CheckBox", "placeholder", None, &["toggle"]);
        checkbox.name = None;
        checkbox.in_web_content = true;
        let mut address = node(
            "Edit",
            "placeholder",
            Some(descriptor().setup_url),
            &["set_value"],
        );
        address.name = None;
        let mut document = node("Document", "placeholder", None, &[]);
        document.name = None;
        let nodes = vec![address, document, checkbox];

        assert_eq!(exact_setup_checkbox(&nodes, descriptor()).unwrap(), Some(7));
    }

    #[test]
    fn setup_page_refuses_ambiguous_web_toggles_without_reading_their_names() {
        let mut first = node("CheckBox", "A", None, &["toggle"]);
        first.element_ptr = 41;
        first.in_web_content = true;
        let mut second = node("CheckBox", "B", None, &["toggle"]);
        second.element_ptr = 42;
        second.in_web_content = true;
        let nodes = vec![
            node(
                "Edit",
                "address",
                Some(descriptor().setup_url),
                &["set_value"],
            ),
            node("Document", "opaque", None, &[]),
            first,
            second,
        ];

        assert_eq!(
            exact_setup_checkbox(&nodes, descriptor()).unwrap_err().code,
            BrowserRefusalCode::BrowserWrongTargetRefused
        );
    }

    #[test]
    fn native_address_control_is_language_independent_without_trusting_web_content() {
        let mut native_address = node(
            "Edit",
            "アドレス検索バー",
            Some("chrome://inspect/#remote-debugging"),
            &["set_value"],
        );
        native_address.element_ptr = 11;
        let mut renderer_spoof = node(
            "Edit",
            "アドレス検索バー",
            Some("chrome://inspect/#remote-debugging"),
            &["set_value"],
        );
        renderer_spoof.element_ptr = 12;
        renderer_spoof.in_web_content = true;

        assert_eq!(
            unique_native_actionable(&[native_address, renderer_spoof], "Edit", "set_value")
                .unwrap(),
            Some(11)
        );
    }

    #[test]
    fn native_browser_controls_still_refuse_ambiguous_structural_matches() {
        let mut first = node("Edit", "Adress- und Suchleiste", None, &["set_value"]);
        first.element_ptr = 31;
        let mut second = node(
            "Edit",
            "Barre d'adresse et de recherche",
            None,
            &["set_value"],
        );
        second.element_ptr = 32;

        let error =
            unique_native_actionable_with_focus(&[first, second], "Edit", "set_value", |_| false)
                .unwrap_err();
        assert_eq!(error.code, BrowserRefusalCode::BrowserWrongTargetRefused);
    }

    #[test]
    fn native_address_control_uses_unique_keyboard_focus_when_edge_exposes_multiple_edits() {
        let mut first = node("Edit", "opaque first", None, &["set_value"]);
        first.element_ptr = 31;
        let mut second = node("Edit", "opaque second", None, &["set_value"]);
        second.element_ptr = 32;

        assert_eq!(
            unique_native_actionable_with_focus(&[first, second], "Edit", "set_value", |element| {
                element == 32
            },)
            .unwrap(),
            Some(32)
        );
    }

    #[test]
    fn native_address_control_refuses_multiple_focused_edits() {
        let mut first = node("Edit", "opaque first", None, &["set_value"]);
        first.element_ptr = 31;
        let mut second = node("Edit", "opaque second", None, &["set_value"]);
        second.element_ptr = 32;

        assert_eq!(
            unique_native_actionable_with_focus(&[first, second], "Edit", "set_value", |_| true,)
                .unwrap_err()
                .code,
            BrowserRefusalCode::BrowserWrongTargetRefused
        );
    }

    #[test]
    fn setup_page_proof_rejects_duplicate_native_exact_urls() {
        let mut nodes = vec![
            node(
                "Edit",
                "Barra de direcciones y de búsqueda",
                Some(descriptor().setup_url),
                &["set_value"],
            ),
            node("Document", descriptor().page_titles[0], None, &[]),
            node("Header", descriptor().page_heading, None, &[]),
        ];
        let mut duplicate = nodes[0].clone();
        duplicate.element_ptr = 99;
        nodes.push(duplicate);

        assert!(!setup_page_proven(&nodes, descriptor()));
    }

    #[test]
    fn native_tab_count_is_structural_and_deduplicates_repeated_uia_rows() {
        let mut first = node("TabItem", "新标签页", None, &[]);
        first.rect = Some((10, 10, 110, 40));
        let duplicate = first.clone();
        let mut second = node("TabItem", "Neue Registerkarte", None, &[]);
        second.rect = Some((120, 10, 220, 40));
        let mut renderer_spoof = node("TabItem", "Tab", None, &[]);
        renderer_spoof.rect = Some((230, 10, 330, 40));
        renderer_spoof.in_web_content = true;
        let mut invalid = node("TabItem", "Onglet", None, &[]);
        invalid.rect = Some((0, 0, 0, 0));

        assert_eq!(
            native_tab_count(&[first, duplicate, second, renderer_spoof, invalid]),
            2
        );
    }

    #[test]
    fn native_new_tab_button_is_the_strict_successor_of_the_tab_strip() {
        let mut first = node("TabItem", "opaque-1", None, &["select"]);
        first.element_index = Some(10);
        first.element_ptr = 10;
        first.depth = 8;
        first.rect = Some((10, 10, 110, 40));
        let mut first_close = node("Button", "opaque-close-1", None, &["invoke"]);
        first_close.element_index = Some(11);
        first_close.element_ptr = 11;
        first_close.depth = 9;
        first_close.parent_element_index = Some(10);
        let mut second = node("TabItem", "opaque-2", None, &["select"]);
        second.element_index = Some(20);
        second.element_ptr = 20;
        second.depth = 8;
        second.rect = Some((120, 10, 220, 40));
        let mut second_close = node("Button", "opaque-close-2", None, &["invoke"]);
        second_close.element_index = Some(21);
        second_close.element_ptr = 21;
        second_close.depth = 9;
        second_close.parent_element_index = Some(20);
        let mut new_tab = node("Button", "opaque-new-tab", None, &["invoke"]);
        new_tab.element_ptr = 30;
        new_tab.depth = 6;
        new_tab.rect = Some((220, 10, 260, 40));

        assert_eq!(
            exact_native_new_tab_button(&[first, first_close, second, second_close, new_tab,])
                .unwrap(),
            Some(30)
        );
    }

    #[test]
    fn setup_reservation_is_exact_profile_resource_scoped() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("custom-profile");
        let other = root.path().join("other-profile");
        std::fs::create_dir(&profile).unwrap();
        std::fs::create_dir(&other).unwrap();

        let reservation = SetupReservation::acquire(&profile).unwrap();
        assert_eq!(
            ensure_profile_discoverable(&profile).unwrap_err().code,
            BrowserRefusalCode::BrowserBindingAmbiguous
        );
        ensure_profile_discoverable(&other).unwrap();
        drop(reservation);
        ensure_profile_discoverable(&profile).unwrap();
    }

    #[test]
    fn failed_cleanup_poison_blocks_the_exact_custom_profile() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("custom-profile");
        std::fs::create_dir(&profile).unwrap();

        let mut reservation = SetupReservation::acquire(&profile).unwrap();
        let marker = reservation.marker_path.clone();
        reservation.retain_fail_closed();
        drop(reservation);
        assert_eq!(
            ensure_profile_discoverable(&profile).unwrap_err().code,
            BrowserRefusalCode::BrowserBindingAmbiguous
        );
        std::fs::remove_file(marker).unwrap();
    }
}
