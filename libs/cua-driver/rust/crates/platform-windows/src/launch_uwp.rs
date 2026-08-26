//! Launch packaged (Microsoft Store / UWP / MSIX) apps on Windows via
//! `IApplicationActivationManager::ActivateApplication`.
//!
//! ## Why this exists
//!
//! On Win11 many built-in apps (Notepad, Calculator, Paint, …) ship as
//! packaged apps. The legacy `notepad.exe` / `calc.exe` / `mspaint.exe`
//! in `C:\Windows\System32\` are now ~7 KB stubs that activate the
//! packaged equivalent and exit almost immediately. Calling
//! `ShellExecuteExW("notepad")` and reading `GetProcessId(hProcess)`
//! therefore returns the **stub's** pid — gone within milliseconds —
//! rather than the pid of the actual UWP process the user can see.
//! `list_windows(pid)` for that stub pid is always empty.
//!
//! `IApplicationActivationManager::ActivateApplication` is the
//! Microsoft-canonical API for launching packaged apps from outside a
//! packaged context. It returns the **real** UWP process pid via its
//! `pid` out-parameter — the pid whose `MainWindowHandle` is the
//! window the user interacts with.
//!
//! ## Two entry points
//!
//! - [`launch_uwp`] — given an AUMID (App User Model ID, the
//!   `{PackageFamilyName}!{ApplicationId}` form a packaged app exposes),
//!   activate it and return the real pid.
//! - [`resolve_apps_folder_target_by_name`] — given a display name (e.g.
//!   `"Notepad"`), walk `shell:AppsFolder` (the same virtual folder the
//!   Start Menu reads) and return the matching launch target. Packaged apps
//!   use `ActivateApplication`; desktop registrations use their
//!   `shell:AppsFolder` parsing path with `ShellExecuteExW`.
//!
//! Both functions are no-ops / compile errors on non-Windows targets;
//! the module is `#[cfg(target_os = "windows")]`-gated at the crate
//! root.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use windows::core::{Interface, GUID, HSTRING, PCWSTR, PWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IBindCtx, CLSCTX_LOCAL_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::Win32::UI::Shell::{
    ApplicationActivationManager, BHID_EnumItems, IApplicationActivationManager, IEnumShellItems,
    IShellItem, IShellItem2, SHCreateItemFromParsingName, AO_NONE, SIGDN_NORMALDISPLAY,
};

/// `PKEY_AppUserModel_ID` (System.AppUserModel.ID) — the property key whose
/// string value on each `shell:AppsFolder` entry is the AUMID we need to
/// hand to `ActivateApplication`.
///
/// Defined inline (rather than pulled from `Win32_Storage_EnhancedStorage`)
/// to keep the windows-rs feature surface narrow — adding an entire
/// `Storage` feature subtree just for one constant is not worth the
/// build-time + binary-size cost.
///
/// Reference: `propkey.h` —
///   `DEFINE_PROPERTYKEY(PKEY_AppUserModel_ID,
///     0x9F4C2855, 0x9F79, 0x4B39, 0xA8,0xD0, 0xE1,0xD4,0x2D,0xE1,0xD5,0xF3, 5);`
const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9F4C2855_9F79_4B39_A8D0_E1D42DE1D5F3),
    pid: 5,
};

/// Activate a packaged app by AUMID. Returns the **real** process id of
/// the spawned UWP / MSIX process (not a stub pid).
///
/// `args` is forwarded verbatim as the activation `arguments` string;
/// pass `""` if the app takes no launch arguments. `AO_NONE` keeps
/// behavior identical to a user-driven Start Menu click — splash screen
/// shown, error UI shown on failure, default activation kind.
///
/// **Background-launch invariant** — the AppX runtime's default activation
/// brings the newly-activated app to the foreground (same as a Start Menu
/// click). cua-driver's contract is the opposite: launched apps must not
/// steal focus from whatever the human is doing. We snapshot the pre-call
/// foreground window with `GetForegroundWindow` and restore it after
/// activation returns. This matches macOS's `NSWorkspace.openApplication`
/// + `activates=false` + `oapp` AppleEvent invariant.
///
/// Errors propagate the `HRESULT` from `ActivateApplication`. The most
/// common failures:
/// - `E_INVALIDARG` — AUMID not installed for the current user
/// - `HRESULT 0x80073D54` (`ERROR_INSTALL_RESOLVE_DEPENDENCY_FAILED`) —
///   packaged dependency unresolved (rare)
/// - Any COM-init failure on a thread that has previously been
///   initialized with a conflicting apartment model
fn foreground_belongs_to_activated_app_with(
    hwnd: u64,
    foreground_owner_pid: u32,
    activated_pid: u32,
    resolve_hosted_pid: impl FnOnce(u32, u64) -> Option<u32>,
) -> bool {
    foreground_owner_pid == activated_pid
        || resolve_hosted_pid(foreground_owner_pid, hwnd) == Some(activated_pid)
}

pub(crate) fn launch_uwp(
    aumid: &str,
    args: &str,
    prior_foreground: crate::win32::ForegroundTarget,
) -> windows::core::Result<u32> {
    if matches!(crate::diagnostics::current_session_id(), Some(0)) {
        return Err(windows::core::Error::new(
            windows::core::HRESULT(0x80004005u32 as i32),
            format!("UWP activation of {aumid:?} requires an interactive session — current process is in Session 0 (services). Re-run from an interactive logon (RDP, console, or scheduled task in the user's session)."),
        ));
    }
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let manager: IApplicationActivationManager =
        unsafe { CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER)? };
    let aumid_h = HSTRING::from(aumid);
    let args_h = HSTRING::from(args);
    let pid = unsafe {
        manager.ActivateApplication(PCWSTR(aumid_h.as_ptr()), PCWSTR(args_h.as_ptr()), AO_NONE)?
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let Some(current) = crate::win32::capture_current_foreground_target() else {
            if Instant::now() >= deadline {
                return Ok(pid);
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        if current.hwnd() == prior_foreground.hwnd() && current.pid() == prior_foreground.pid() {
            if Instant::now() >= deadline {
                return Ok(pid);
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        if !foreground_belongs_to_activated_app_with(
            current.hwnd(),
            current.pid(),
            pid,
            crate::win32::resolve_uwp_app_pid,
        ) {
            return Ok(pid);
        }
        return match crate::win32::restore_foreground_target_if_still_displaced(
            prior_foreground,
            current,
            Duration::from_millis(500),
        ) {
            crate::win32::ForegroundRestoreOutcome::Restored
            | crate::win32::ForegroundRestoreOutcome::Superseded => Ok(pid),
            crate::win32::ForegroundRestoreOutcome::Failed => Err(windows::core::Error::new(
                windows::core::HRESULT(0x80004005u32 as i32),
                "UWP activation completed, but exact prior foreground restoration was not confirmed",
            )),
        };
    }
}

/// Heuristic for "this string looks like an AUMID".
///
/// An AUMID is `{PackageFamilyName}!{ApplicationId}`, e.g.
/// `Microsoft.WindowsNotepad_8wekyb3d8bbwe!App`. The unambiguous
/// marker is the literal `!` separator — no Win32 path or executable
/// name legitimately contains `!`, so its presence is a safe signal
/// the caller intends packaged-app activation.
pub fn is_aumid(s: &str) -> bool {
    // Must contain exactly one `!`, with non-empty halves on each side.
    let mut parts = s.split('!');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(pfn), Some(app_id), None) => !pfn.is_empty() && !app_id.is_empty(),
        _ => false,
    }
}

/// How an entry discovered through `shell:AppsFolder` must be launched.
///
/// `PKEY_AppUserModel_ID` is exposed by both packaged and desktop apps. Only
/// packaged IDs have the `{PackageFamilyName}!{ApplicationId}` shape accepted
/// by `IApplicationActivationManager`; desktop IDs must be handed back to the
/// shell namespace that supplied them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppsFolderLaunchTarget {
    PackagedAumid(String),
    ShellLaunchPath(String),
}

fn classify_apps_folder_app_id(app_id: &str) -> AppsFolderLaunchTarget {
    if is_aumid(app_id) {
        AppsFolderLaunchTarget::PackagedAumid(app_id.to_owned())
    } else {
        AppsFolderLaunchTarget::ShellLaunchPath(format!(r"shell:AppsFolder\{app_id}"))
    }
}

/// Single AppsFolder entry: display name + application model ID.
///
/// `lowercase_display_name` is a one-time precomputation of
/// `display_name.to_lowercase()`, populated at enumeration time. The
/// prefix-match pass in [`resolve_apps_folder_target_by_name`] runs over every cached
/// entry on every lookup; without this field the loop would allocate a
/// fresh lowercased `String` per entry per call (~150–300 allocations
/// per resolve on a stock Win11 install). Caching it once turns the hot
/// path into a borrow.
#[derive(Clone)]
struct AppsFolderEntry {
    display_name: String,
    lowercase_display_name: String,
    app_id: String,
}

/// Cached snapshot of all `shell:AppsFolder` entries.
///
/// Enumeration is slow (~200 ms cold on a stock Win11 install — the
/// folder has ~150–300 entries and each `BindToHandler` round-trips
/// through the shell namespace). We cache the result for the lifetime
/// of the driver process. The pid returned by `ActivateApplication`
/// is the real packaged-app pid regardless of cache staleness, so the
/// only correctness cost of cache staleness is: an app installed after
/// the driver started will be invisible to name-based lookup until the
/// driver restarts. That is acceptable — explicit AUMID via
/// `bundle_id` always works regardless of cache state.
static APPS_FOLDER_CACHE: OnceLock<RwLock<Option<Vec<AppsFolderEntry>>>> = OnceLock::new();

const APPS_FOLDER_LOOKUP_TIMEOUT: Duration = Duration::from_secs(4);
const APPS_FOLDER_LOOKUP_RECOVERY_COOLDOWN: Duration = Duration::from_secs(30);

/// Why a display-name lookup could not produce a definitive answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppsFolderLookupError {
    Timeout,
    Busy,
    Unavailable,
}

/// Process-wide bound for the uncancellable `shell:AppsFolder` COM walk.
///
/// A timed-out blocking task keeps this gate until the COM call actually
/// returns. Retries therefore fail fast instead of accumulating abandoned
/// Tokio blocking workers. Once a late worker returns, a short cooldown keeps
/// a hot retry loop from immediately entering the same unhealthy shell broker.
struct AppsFolderLookupSingleFlight {
    in_flight: AtomicBool,
    cooldown_until_ms: AtomicU64,
    cooldown_ms: u64,
}

impl AppsFolderLookupSingleFlight {
    const fn new(cooldown_ms: u64) -> Self {
        Self {
            in_flight: AtomicBool::new(false),
            cooldown_until_ms: AtomicU64::new(0),
            cooldown_ms,
        }
    }

    async fn run<T, F>(
        self: &Arc<Self>,
        timeout: Duration,
        work: F,
    ) -> Result<T, AppsFolderLookupError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let now = apps_folder_lookup_now_ms();
        if now < self.cooldown_until_ms.load(Ordering::Acquire)
            || self
                .in_flight
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Err(AppsFolderLookupError::Busy);
        }

        let timed_out = Arc::new(AtomicBool::new(false));
        let worker_timed_out = Arc::clone(&timed_out);
        let worker_gate = Arc::clone(self);
        let worker = tokio::task::spawn_blocking(move || {
            let _guard = AppsFolderLookupInFlightGuard {
                gate: worker_gate,
                timed_out: worker_timed_out,
            };
            work()
        });

        match tokio::time::timeout(timeout, worker).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => {
                tracing::warn!(
                    target: "launch_uwp",
                    "shell:AppsFolder lookup worker failed: {error}"
                );
                Err(AppsFolderLookupError::Unavailable)
            }
            Err(_) => {
                timed_out.store(true, Ordering::Release);
                // Also arm the cooldown on the caller side. The worker can
                // return between the deadline firing and observing
                // `timed_out`; recording it here closes that race.
                self.cooldown_until_ms.store(
                    apps_folder_lookup_now_ms().saturating_add(self.cooldown_ms),
                    Ordering::Release,
                );
                Err(AppsFolderLookupError::Timeout)
            }
        }
    }
}

struct AppsFolderLookupInFlightGuard {
    gate: Arc<AppsFolderLookupSingleFlight>,
    timed_out: Arc<AtomicBool>,
}

impl Drop for AppsFolderLookupInFlightGuard {
    fn drop(&mut self) {
        if self.timed_out.load(Ordering::Acquire) {
            self.gate.cooldown_until_ms.store(
                apps_folder_lookup_now_ms().saturating_add(self.gate.cooldown_ms),
                Ordering::Release,
            );
        }
        self.gate.in_flight.store(false, Ordering::Release);
    }
}

fn apps_folder_lookup_gate() -> &'static Arc<AppsFolderLookupSingleFlight> {
    static GATE: OnceLock<Arc<AppsFolderLookupSingleFlight>> = OnceLock::new();
    GATE.get_or_init(|| {
        Arc::new(AppsFolderLookupSingleFlight::new(
            APPS_FOLDER_LOOKUP_RECOVERY_COOLDOWN.as_millis() as u64,
        ))
    })
}

fn apps_folder_lookup_now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn cache() -> &'static RwLock<Option<Vec<AppsFolderEntry>>> {
    APPS_FOLDER_CACHE.get_or_init(|| RwLock::new(None))
}

/// Resolve a display name without allowing an unhealthy shell broker to wedge
/// the async tool stream.
pub async fn resolve_apps_folder_target_by_name_bounded(
    display_name: String,
) -> Result<Option<AppsFolderLaunchTarget>, AppsFolderLookupError> {
    let result = apps_folder_lookup_gate()
        .run(APPS_FOLDER_LOOKUP_TIMEOUT, move || {
            resolve_apps_folder_target_by_name(&display_name)
        })
        .await;
    match result {
        Err(AppsFolderLookupError::Timeout) => tracing::warn!(
            target: "launch_uwp",
            "shell:AppsFolder name lookup exceeded {}ms; keeping its worker single-flight until COM returns",
            APPS_FOLDER_LOOKUP_TIMEOUT.as_millis()
        ),
        Err(AppsFolderLookupError::Busy) => tracing::debug!(
            target: "launch_uwp",
            "shell:AppsFolder name lookup skipped while a prior lookup is running or cooling down"
        ),
        Err(AppsFolderLookupError::Unavailable) | Ok(_) => {}
    }
    result
}

/// Resolve a packaged-app display name to its AUMID.
///
/// Lookup order, all case-insensitive against the display name shown
/// in the Start Menu:
/// 1. Exact match (e.g. `"Notepad"` → `Microsoft.WindowsNotepad_8wekyb3d8bbwe!App`).
/// 2. Case-insensitive prefix match — picks the shortest display name
///    that starts with the query, so `"calc"` resolves to `"Calculator"`
///    rather than `"Calculator Plus"`.
///
/// `"notepad.exe"` and `"notepad"` both resolve identically — the
/// `.exe` suffix is stripped before matching so callers carrying a
/// Win32 idiom still hit the packaged display name.
///
/// Returns `None` if no AppsFolder entry matches. The caller should fall back
/// to `ShellExecuteExW` PATH/association lookup in that case.
pub fn resolve_apps_folder_target_by_name(display_name: &str) -> Option<AppsFolderLaunchTarget> {
    // Session 0 short-circuit: `shell:AppsFolder` enumeration goes through
    // the interactive shell broker, which doesn't exist in services /
    // SSH-launched contexts and causes the underlying COM call to hang
    // indefinitely (~no CPU, no progress). Returning None here makes the
    // caller fall through to ShellExecuteExW's PATH-based lookup, which
    // works fine in Session 0 for any app reachable via PATH (notepad,
    // calc, regedit, etc.). Packaged-only apps (Windows 11 Notepad) won't
    // resolve here in Session 0 — explicit `aumid` is the workaround.
    if matches!(crate::diagnostics::current_session_id(), Some(0)) {
        tracing::debug!(
            target: "launch_uwp",
            "skipping shell:AppsFolder lookup in Session 0 (no interactive shell broker); falling back to ShellExecuteEx PATH lookup"
        );
        return None;
    }
    let entries = load_or_get_cache()?;
    let query = display_name.trim().to_lowercase();
    if query.is_empty() {
        return None;
    }

    // Strip an optional `.exe` suffix so callers passing `"notepad.exe"`
    // (a Win32 idiom) still resolve to the packaged display name `"Notepad"`.
    let query_stripped = query.strip_suffix(".exe").unwrap_or(&query);

    // Pass 1: exact case-insensitive match.
    if let Some(hit) = entries
        .iter()
        .find(|e| e.display_name.eq_ignore_ascii_case(query_stripped))
    {
        return Some(classify_apps_folder_app_id(&hit.app_id));
    }

    // Pass 2: shortest case-insensitive prefix match. Uses the
    // precomputed `lowercase_display_name` to avoid a fresh allocation
    // per entry per call (see `AppsFolderEntry` docs for rationale).
    let mut best: Option<&AppsFolderEntry> = None;
    for entry in entries.iter() {
        if entry.lowercase_display_name.starts_with(query_stripped) {
            match best {
                None => best = Some(entry),
                Some(current) if entry.display_name.len() < current.display_name.len() => {
                    best = Some(entry)
                }
                _ => {}
            }
        }
    }
    best.map(|e| classify_apps_folder_app_id(&e.app_id))
}

fn load_or_get_cache() -> Option<Vec<AppsFolderEntry>> {
    {
        let guard = cache().read().ok()?;
        if let Some(cached) = guard.as_ref() {
            return Some(cached.clone());
        }
    }

    // Double-checked init — if two threads race here both enumerate; the
    // second discards its result. AppsFolder enumeration is read-only, so
    // the duplicated work is wasted CPU only, not a correctness hazard.
    let fresh = enumerate_apps_folder().unwrap_or_default();
    if let Ok(mut guard) = cache().write() {
        if guard.is_none() {
            *guard = Some(fresh.clone());
        }
    }
    Some(fresh)
}

/// Walk `shell:AppsFolder` and collect `(display_name, app_id)` for every
/// entry that exposes `PKEY_AppUserModel_ID`. Despite the property name,
/// desktop apps can expose an explicit application model ID here too; those
/// entries remain launchable through their `shell:AppsFolder` parsing path.
fn enumerate_apps_folder() -> windows::core::Result<Vec<AppsFolderEntry>> {
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

    // `shell:AppsFolder` is the well-known parsing path for the virtual
    // folder that backs the Start Menu's "all apps" list. It enumerates
    // every activatable app on the system, packaged or not.
    let folder_path = HSTRING::from("shell:AppsFolder");
    let apps_folder: IShellItem =
        unsafe { SHCreateItemFromParsingName(PCWSTR(folder_path.as_ptr()), None::<&IBindCtx>)? };

    // Bind to the `BHID_EnumItems` handler to iterate children.
    let enumerator: IEnumShellItems =
        unsafe { apps_folder.BindToHandler(None::<&IBindCtx>, &BHID_EnumItems)? };

    let mut entries = Vec::with_capacity(256);
    loop {
        // Request one item at a time. windows-rs surfaces `S_FALSE`
        // (the natural end-of-enumeration HRESULT) as `Err`, so we
        // intentionally ignore the Result and gate on `fetched == 0`
        // — the documented sentinel for "iterator exhausted".
        let mut slot: [Option<IShellItem>; 1] = [None];
        let mut fetched: u32 = 0;
        let _ = unsafe { enumerator.Next(&mut slot, Some(&mut fetched as *mut u32)) };
        if fetched == 0 {
            break;
        }
        let Some(item) = slot[0].take() else {
            break;
        };

        // Cast to IShellItem2 to access PKEY_AppUserModel_ID; if the
        // entry doesn't expose AAM (rare; pure-Win32 shortcut), skip.
        let item2: IShellItem2 = match item.cast() {
            Ok(i) => i,
            Err(_) => continue,
        };

        let app_id = match unsafe { item2.GetString(&PKEY_APP_USER_MODEL_ID) } {
            Ok(pwstr) => pwstr_to_string_and_free(pwstr),
            Err(_) => continue, // No app ID → not a packaged/registered app.
        };
        if app_id.is_empty() {
            continue;
        }

        let display_name = match unsafe { item2.GetDisplayName(SIGDN_NORMALDISPLAY) } {
            Ok(pwstr) => pwstr_to_string_and_free(pwstr),
            Err(_) => String::new(),
        };
        if display_name.is_empty() {
            continue;
        }

        let lowercase_display_name = display_name.to_lowercase();
        entries.push(AppsFolderEntry {
            display_name,
            lowercase_display_name,
            app_id,
        });
    }

    Ok(entries)
}

/// Drain a COM-allocated wide string (`PWSTR` returned by `GetString` /
/// `GetDisplayName`) into a Rust `String`, then free the underlying
/// COM-task-allocated buffer with `CoTaskMemFree` as the shell requires.
fn pwstr_to_string_and_free(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let s = unsafe { p.to_string().unwrap_or_default() };
    unsafe { CoTaskMemFree(Some(p.0 as *const core::ffi::c_void)) };
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_uwp_foreground_is_attributed_to_the_activated_app() {
        assert!(foreground_belongs_to_activated_app_with(
            0x1234,
            700,
            900,
            |host_pid, hwnd| (host_pid == 700 && hwnd == 0x1234).then_some(900),
        ));
        assert!(foreground_belongs_to_activated_app_with(
            0x1234,
            900,
            900,
            |_, _| None,
        ));
        assert!(!foreground_belongs_to_activated_app_with(
            0x1234,
            701,
            900,
            |_, _| None,
        ));
    }

    #[tokio::test]
    async fn apps_folder_lookup_timeout_is_single_flight_and_recovers_after_cooldown() {
        let gate = Arc::new(AppsFolderLookupSingleFlight::new(20));
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let work_started = Arc::new(AtomicBool::new(false));
        let worker_started = Arc::clone(&work_started);

        let started = Instant::now();
        let first = gate
            .run(Duration::from_millis(100), move || {
                worker_started.store(true, Ordering::Release);
                release_rx.recv().expect("release timed-out lookup worker");
                1_u8
            })
            .await;
        assert_eq!(first, Err(AppsFolderLookupError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(work_started.load(Ordering::Acquire));

        let retry_count = Arc::new(AtomicU64::new(0));
        for _ in 0..100 {
            let retry_count = Arc::clone(&retry_count);
            let retry = gate
                .run(Duration::from_millis(10), move || {
                    retry_count.fetch_add(1, Ordering::AcqRel);
                })
                .await;
            assert_eq!(retry, Err(AppsFolderLookupError::Busy));
        }
        assert_eq!(retry_count.load(Ordering::Acquire), 0);

        release_tx.send(()).expect("release first lookup");
        let worker_deadline = Instant::now() + Duration::from_secs(1);
        while gate.in_flight.load(Ordering::Acquire) && Instant::now() < worker_deadline {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(!gate.in_flight.load(Ordering::Acquire));

        assert_eq!(
            gate.run(Duration::from_millis(10), || 2_u8).await,
            Err(AppsFolderLookupError::Busy),
            "late worker completion must leave a recovery cooldown"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            gate.run(Duration::from_millis(100), || 3_u8).await,
            Ok(3),
            "lookup should recover after the cooldown"
        );
    }

    #[test]
    fn is_aumid_recognises_canonical_form() {
        assert!(is_aumid("Microsoft.WindowsNotepad_8wekyb3d8bbwe!App"));
        assert!(is_aumid("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"));
    }

    #[test]
    fn is_aumid_rejects_plain_names_and_paths() {
        assert!(!is_aumid("notepad"));
        assert!(!is_aumid("notepad.exe"));
        assert!(!is_aumid(r"C:\Windows\System32\notepad.exe"));
        assert!(!is_aumid(""));
        assert!(!is_aumid("!App")); // empty PFN half
        assert!(!is_aumid("Pkg!")); // empty AppId half
        assert!(!is_aumid("a!b!c")); // two bangs
    }

    #[test]
    fn apps_folder_routes_packaged_ids_to_activation_manager() {
        assert_eq!(
            classify_apps_folder_app_id("Microsoft.WindowsNotepad_8wekyb3d8bbwe!App"),
            AppsFolderLaunchTarget::PackagedAumid(
                "Microsoft.WindowsNotepad_8wekyb3d8bbwe!App".to_owned()
            )
        );
    }

    #[test]
    fn apps_folder_routes_desktop_ids_back_through_shell_namespace() {
        assert_eq!(
            classify_apps_folder_app_id("MSEdge"),
            AppsFolderLaunchTarget::ShellLaunchPath(r"shell:AppsFolder\MSEdge".to_owned())
        );
    }
}
