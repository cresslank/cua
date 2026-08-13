//! Exact Windows UIA setup for Chromium existing-profile attachment.

use std::time::{Duration, Instant};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::Write,
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use cua_driver_core::browser::{
    BrowserProduct, BrowserRefusal, BrowserRefusalCode, BrowserSetupDescriptor,
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

fn refusal(code: BrowserRefusalCode, message: impl Into<String>) -> BrowserRefusal {
    BrowserRefusal::new(code, message)
}

fn field_equals(node: &UiaNode, expected: &str) -> bool {
    [
        node.name.as_deref(),
        node.value.as_deref(),
        node.automation_id.as_deref(),
        node.help_text.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.trim().eq_ignore_ascii_case(expected))
}

fn release_nodes(nodes: &[UiaNode]) {
    for node in nodes.iter().filter(|node| node.element_ptr != 0) {
        unsafe { drop(IUIAutomationElement::from_raw(node.element_ptr as *mut _)) };
    }
}

fn unique_actionable(
    nodes: &[UiaNode],
    control_type: &str,
    label: &str,
    action: &str,
) -> Result<Option<usize>, BrowserRefusal> {
    let matches = nodes
        .iter()
        .filter(|node| {
            node.control_type == control_type
                && field_equals(node, label)
                && node.actions.iter().any(|value| value == action)
        })
        .map(|node| node.element_ptr)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [element] => Ok(Some(*element)),
        _ => Err(refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            format!("multiple exact {control_type} controls matched {label:?}"),
        )),
    }
}

fn setup_page_proven(nodes: &[UiaNode], descriptor: &BrowserSetupDescriptor) -> bool {
    let exact_url = nodes.iter().any(|node| {
        node.control_type == "Edit"
            && field_equals(node, "Address and search bar")
            && node
                .value
                .as_deref()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case(descriptor.setup_url))
    });
    let exact_heading = nodes.iter().any(|node| {
        matches!(node.control_type.as_str(), "Header" | "Text")
            && field_equals(node, descriptor.page_heading)
    });
    let exact_page = nodes.iter().any(|node| {
        node.control_type == "Document"
            && descriptor
                .page_titles
                .iter()
                .any(|title| field_equals(node, title))
    });
    exact_url && exact_page && exact_heading
}

fn exact_setup_checkbox(
    nodes: &[UiaNode],
    descriptor: &BrowserSetupDescriptor,
) -> Result<Option<usize>, BrowserRefusal> {
    if !setup_page_proven(nodes, descriptor) {
        return Ok(None);
    }
    unique_actionable(nodes, "CheckBox", descriptor.checkbox_label, "toggle")
}

unsafe fn invoke(
    element_ptr: usize,
    root_hwnd: u64,
    expected_pid: u32,
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
            format!("the exact UIA Invoke action failed: {error}"),
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

fn default_profile_path(descriptor: &BrowserSetupDescriptor) -> Result<PathBuf, BrowserRefusal> {
    let root = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
        refusal(
            BrowserRefusalCode::BrowserRouteUnavailable,
            "LOCALAPPDATA is unavailable for browser profile identity",
        )
    })?;
    let relative = match descriptor.product {
        BrowserProduct::GoogleChrome => "Google\\Chrome\\User Data",
        BrowserProduct::MicrosoftEdge => "Microsoft\\Edge\\User Data",
        BrowserProduct::Chromium => "Chromium\\User Data",
        _ => {
            return Err(refusal(
                BrowserRefusalCode::BrowserRouteUnavailable,
                "browser setup has no stable default profile identity",
            ))
        }
    };
    Ok(PathBuf::from(root).join(relative))
}

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

pub fn ensure_profile_discoverable(
    descriptor: &BrowserSetupDescriptor,
) -> Result<(), BrowserRefusal> {
    let profile = default_profile_path(descriptor)?;
    drop(SetupReservation::acquire(&profile)?);
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

pub fn enable(
    hwnd: u64,
    expected_pid: u32,
    descriptor: &'static BrowserSetupDescriptor,
) -> Result<SetupUiHandle, BrowserRefusal> {
    // Reserve the stable profile resource before the first tree read/mutation.
    let profile = default_profile_path(descriptor)?;
    let mut reservation = Some(SetupReservation::acquire(&profile)?);
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
            let tab_count_before = initial
                .nodes
                .iter()
                .filter(|node| node.control_type == "TabItem")
                .count();
            let new_tab = match unique_actionable(&initial.nodes, "Button", "New Tab", "invoke") {
                Ok(Some(element)) => element,
                Ok(None) => {
                    release_nodes(&initial.nodes);
                    return Err(refusal(
                        BrowserRefusalCode::BrowserWrongTargetRefused,
                        format!(
                            "the approved {} window has no exact New Tab button",
                            descriptor.product_name
                        ),
                    ));
                }
                Err(error) => {
                    release_nodes(&initial.nodes);
                    return Err(error);
                }
            };
            // Invoke may mutate even when UIA reports failure. Persist poison
            // before the call so response loss cannot authorize discovery.
            reservation
                .as_mut()
                .expect("setup reservation available")
                .retain_fail_closed();
            let invoked = prove_window_owner(hwnd, expected_pid, "New Tab invocation")
                .and_then(|()| unsafe { invoke(new_tab, hwnd, expected_pid) });
            release_nodes(&initial.nodes);
            invoked?;

            let mut handle = SetupUiHandle {
                hwnd,
                expected_pid,
                descriptor,
                opened_setup_page: true,
                enabled_remote_debugging: false,
                focused_setup_address_field: false,
                foregrounded_window: false,
                injected_global_input: false,
                enable_attempted: false,
                remote_debugging_mutation_possible: false,
                terminal_cleanup_attempted: false,
                reservation: reservation.take().expect("setup reservation available"),
            };

            let deadline = Instant::now() + Duration::from_secs(3);
            let mut created = loop {
                let tree = crate::uia::walk_tree(hwnd, None);
                let tab_count_after = tree
                    .nodes
                    .iter()
                    .filter(|node| node.control_type == "TabItem")
                    .count();
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
            let omnibox = unique_actionable(
                &created.nodes,
                "Edit",
                "Address and search bar",
                "set_value",
            )?
            .ok_or_else(|| {
                refusal(
                    BrowserRefusalCode::BrowserWrongTargetRefused,
                    format!(
                        "the approved {} window has no exact address-and-search field",
                        descriptor.product_name
                    ),
                )
            });
            let omnibox = match omnibox {
                Ok(element) => element,
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
            let refreshed_omnibox = created
                .nodes
                .iter()
                .find(|node| {
                    node.control_type == "Edit"
                        && field_equals(node, "Address and search bar")
                        && node.value.as_deref().is_some_and(|value| {
                            value.trim().eq_ignore_ascii_case(descriptor.setup_url)
                        })
                })
                .map(|node| node.element_ptr);
            let Some(refreshed_omnibox) = refreshed_omnibox else {
                release_nodes(&created.nodes);
                return Err(handle.abort(refusal(
                    BrowserRefusalCode::BrowserWrongTargetRefused,
                    "the exact address field did not retain the setup URL",
                )));
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
                    Ok(CheckboxState::On) => {
                        if handle.enable_attempted {
                            handle.enabled_remote_debugging = true;
                        }
                        Ok(true)
                    }
                    Ok(CheckboxState::Off) if !handle.enable_attempted => {
                        // Arm rollback and durable poison before Toggle: an
                        // HRESULT/transport failure may follow a real mutation.
                        handle.enable_attempted = true;
                        handle.remote_debugging_mutation_possible = true;
                        handle.reservation.retain_fail_closed();
                        prove_window_owner(hwnd, expected_pid, "remote-debugging enable").and_then(
                            |()| unsafe { toggle(element, hwnd, expected_pid).map(|_| false) },
                        )
                    }
                    Ok(CheckboxState::Off) => Ok(false),
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
    fn checkbox_requires_exact_url_heading_and_unique_toggle() {
        let nodes = vec![
            node(
                "Edit",
                "Address and search bar",
                Some(descriptor().setup_url),
                &["set_value"],
            ),
            node("Document", descriptor().page_titles[0], None, &[]),
            node("Header", descriptor().page_heading, None, &[]),
            node("CheckBox", descriptor().checkbox_label, None, &["toggle"]),
        ];
        assert_eq!(exact_setup_checkbox(&nodes, descriptor()).unwrap(), Some(7));

        let mut wrong_url = nodes.clone();
        wrong_url[0].value = Some("https://example.test/".to_owned());
        assert_eq!(
            exact_setup_checkbox(&wrong_url, descriptor()).unwrap(),
            None
        );
    }
}
