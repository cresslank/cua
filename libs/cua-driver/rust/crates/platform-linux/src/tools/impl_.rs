//! Real Linux tool implementations (compiled only on Linux).

use async_trait::async_trait;
use cua_driver_contract::{
    ClickButton, DragInput, GetCursorPositionInput, GetDesktopStateInput, GetScreenSizeInput,
    HotkeyInput, InvokeMenuInput, MoveCursorInput, PressKeyInput, ScrollInput, TypeTextInput,
};
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef, ToolRegistry},
    tool_args::{parse_typed_input, parse_typed_projection, ArgsExt},
    window_target::{PidOnlyWindowTargetGuard, WindowTargetCandidate, WindowTargetCandidates},
};
use serde_json::{json, Value};
use std::fs;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, RwLock};

use crate::atspi::Snapshots;
use cursor_overlay::CursorRegistry;

fn coordinate_frame_schema() -> Value {
    json!({"type":"string","enum":["window","desktop"],"default":"window","description":"Window screenshot pixels. Explicit desktop-to-window translation is unavailable in the exact-target adapter."})
}

fn window_target_candidates_for_pid(
    windows: impl IntoIterator<Item = crate::x11::WindowInfo>,
    pid: u32,
) -> Vec<WindowTargetCandidate> {
    windows
        .into_iter()
        .filter(|window| window.pid == Some(pid))
        .map(|window| WindowTargetCandidate {
            window_id: window.xid,
            transient_for: (!crate::wayland::is_wayland())
                .then(|| crate::x11::transient_for(window.xid))
                .flatten(),
            title: window.title,
            app_name: Some(window.app_name),
            is_on_screen: window.is_on_screen,
        })
        .collect()
}

fn desktop_point_window_resolver() -> cua_driver_core::window_target::DesktopPointWindowResolver {
    Arc::new(move |pid, x, y| {
        let pid = u32::try_from(pid).ok()?;
        // Window geometry is in layout coordinates; on Hyprland the desktop
        // frame starts at the top-left powered output, not the layout origin.
        let (x, y) = if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
            let frame = crate::wayland::hyprland::desktop_frame().ok()?;
            (x + f64::from(frame.x), y + f64::from(frame.y))
        } else {
            (x, y)
        };
        topmost_window_at(&crate::wayland::list_windows_dispatch(Some(pid)), pid, x, y)
    })
}

fn pid_window_target_candidates(pid: i64) -> Vec<WindowTargetCandidate> {
    let Ok(pid) = u32::try_from(pid) else {
        return Vec::new();
    };
    window_target_candidates_for_pid(crate::wayland::list_windows_dispatch(Some(pid)), pid)
}

struct ExactPidWindowTargetGuard {
    inner: Box<dyn Tool>,
}

fn guarded_pid(args: &Value) -> Result<u32, ToolResult> {
    let raw = args.get("pid").and_then(Value::as_u64);
    match raw
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0)
    {
        Some(pid) => Ok(pid),
        None => Err(ToolResult::error(
            "pid must be a positive integer in the Linux process-id range.",
        )
        .with_structured(json!({
            "code": "window_target_mismatch",
            "effect": "refused",
            "pid": args.get("pid").cloned().unwrap_or(Value::Null),
        }))),
    }
}

fn explicit_window_belongs_to_pid(pid: u32, window_id: u64) -> bool {
    if crate::wayland::is_wayland() {
        crate::wayland::list_windows_dispatch(Some(pid))
            .iter()
            .any(|window| window.xid == window_id && window.pid == Some(pid))
    } else {
        crate::x11::window_belongs_to_pid(window_id, pid)
    }
}

#[async_trait]
impl Tool for ExactPidWindowTargetGuard {
    fn def(&self) -> &ToolDef {
        self.inner.def()
    }

    async fn protected_resource_ownership(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> cua_driver_core::tool::ProtectedResourceOwnership {
        self.inner
            .protected_resource_ownership(adapter_id, args)
            .await
    }

    async fn protected_resource_scope(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> Result<Option<Value>, String> {
        self.inner.protected_resource_scope(adapter_id, args).await
    }

    async fn validate_protected_resource_scope(
        &self,
        adapter_id: &str,
        args: &Value,
        approved_scope: &Value,
    ) -> Result<(), String> {
        self.inner
            .validate_protected_resource_scope(adapter_id, args, approved_scope)
            .await
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if args.get("coordinate_frame").and_then(Value::as_str) == Some("desktop") {
            return ToolResult::error("Exact-target desktop coordinate translation is not qualified on Linux; use the target window's current screenshot.")
                .with_structured(json!({"code":"coordinate_frame_unavailable","effect":"refused"}));
        }
        let windowless_desktop = args.get("scope").and_then(Value::as_str) == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none();
        if !windowless_desktop {
            let pid = match guarded_pid(&args) {
                Ok(pid) => pid,
                Err(result) => return result,
            };
            if let Some(window_id) = args.get("window_id").and_then(Value::as_u64) {
                let owned = cua_driver_core::blocking::spawn(move || {
                    explicit_window_belongs_to_pid(pid, window_id)
                })
                .await
                .unwrap_or(false);
                if !owned {
                    return ToolResult::error(format!(
                        "window_id {window_id} is stale or does not belong to pid {pid}."
                    ))
                    .with_structured(json!({
                        "code": "window_target_mismatch",
                        "effect": "refused",
                        "pid": pid,
                        "window_id": window_id,
                    }));
                }
            }
        }
        self.inner.invoke(args).await
    }
}

#[cfg(test)]
mod exact_pid_window_guard_tests {
    use super::guarded_pid;
    use serde_json::json;

    #[test]
    fn guarded_pid_rejects_negative_zero_and_out_of_range_values() {
        for value in [json!(-1), json!(0), json!(u64::from(u32::MAX) + 1)] {
            let result = guarded_pid(&json!({"pid": value})).unwrap_err();
            assert_eq!(result.is_error, Some(true));
        }
        assert_eq!(guarded_pid(&json!({"pid": 42})).unwrap(), 42);
    }
}

/// The pid's topmost on-screen window containing screen point `(sx, sy)`:
/// the highest `z_index` (bottom-to-top stacking position) wins, so an
/// app's own dialog beats the main window it covers. Windows without a
/// stacking position rank lowest.
fn topmost_window_at(
    windows: &[crate::x11::WindowInfo],
    pid: u32,
    sx: f64,
    sy: f64,
) -> Option<u64> {
    windows
        .iter()
        .filter(|w| w.pid == Some(pid) && w.is_on_screen && w.width > 0 && w.height > 0)
        .filter(|w| {
            sx >= f64::from(w.x)
                && sy >= f64::from(w.y)
                && sx < f64::from(w.x) + f64::from(w.width)
                && sy < f64::from(w.y) + f64::from(w.height)
        })
        .max_by_key(|w| w.z_index.map(|z| z as i64).unwrap_or(-1))
        .map(|w| w.xid)
}

/// The window a pid-only keyboard action means in a multi-window app: the
/// pid's window that holds the core focus, else its topmost transient dialog,
/// else the WM's active window when it is the pid's, else the largest mapped
/// toplevel (see [`crate::x11::pick_pid_window`]). A dialog the app just
/// opened therefore receives the following `type_text` / `press_key` even
/// while the main window is still the WM's active window.
fn pid_fallback_window_resolver() -> cua_driver_core::window_target::PidFallbackWindowResolver {
    Arc::new(|pid| {
        let pid = u32::try_from(pid).ok()?;
        let windows: Vec<crate::x11::WindowInfo> = crate::wayland::list_windows_dispatch(Some(pid))
            .into_iter()
            .filter(|w| w.pid == Some(pid))
            .collect();
        if crate::wayland::is_wayland() {
            return windows
                .into_iter()
                .filter(|w| w.is_on_screen)
                .max_by_key(|w| w.z_index.unwrap_or(0))
                .map(|w| w.xid);
        }
        let ids: Vec<u64> = windows.iter().map(|w| w.xid).collect();
        crate::x11::pick_pid_window(
            &windows,
            crate::x11::focused_window_among(&ids),
            crate::x11::transient_for,
            crate::x11::active_window(),
        )
    })
}

#[cfg(test)]
mod pid_window_resolver_tests;

type PidWindowGuardParts = (
    WindowTargetCandidates,
    cua_driver_core::window_target::DesktopPointWindowResolver,
    cua_driver_core::window_target::PidFallbackWindowResolver,
    cua_driver_core::window_target::SnapshotWindowResolver,
);

fn pid_window_guarded<T: Tool + 'static>(
    tool: T,
    (candidates, _point_resolver, _fallback_resolver, snapshot_resolver): &PidWindowGuardParts,
) -> Box<dyn Tool> {
    Box::new(ExactPidWindowTargetGuard {
        inner: Box::new(
            PidOnlyWindowTargetGuard::new(Box::new(tool), candidates.clone())
                .with_snapshot_resolver(snapshot_resolver.clone()),
        ),
    })
}

/// The window a `snapshot_id` was published for (the element cache lane of
/// that pid), so pid-only element actions follow the snapshot to its popup
/// or dialog.
fn snapshot_window_resolver(
    state: Arc<ToolState>,
) -> cua_driver_core::window_target::SnapshotWindowResolver {
    Arc::new(move |pid, handle| {
        let pid = i32::try_from(pid).ok()?;
        let id = cua_driver_core::element_token::parse_snapshot_handle(handle)?;
        state.snapshots.window_for_snapshot(pid, id)
    })
}

// ── DriverConfig + ZoomRegistry ─────────────────────────────────────────────

#[derive(Clone)]
pub struct DriverConfig {
    pub capture_mode: String,
    pub max_image_dimension: u32,
    pub agent_cursor_glide_duration_ms: f64,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            capture_mode: "ax".into(),
            max_image_dimension: 1568,
            agent_cursor_glide_duration_ms: 0.0,
        }
    }
}

/// Load `DriverConfig` from `~/.cua-driver/config.json`, falling back to
/// defaults for any missing/malformed keys. Called once at `ToolState`
/// construction (i.e. on every fresh `cua-driver call` process) so that a
/// prior `set_config capture_mode=vision` survives across stateless one-shot
/// invocations — matching the macOS daemon's startup load. See #2008.
pub fn load_driver_config() -> DriverConfig {
    let mut cfg = DriverConfig::default();
    if let Some(v) =
        pip_preview::read_config_value("capture_mode").and_then(|v| v.as_str().map(str::to_owned))
    {
        cfg.capture_mode = v;
    }
    if let Some(v) = pip_preview::read_config_value("max_image_dimension").and_then(|v| v.as_u64())
    {
        if let Ok(v32) = u32::try_from(v) {
            cfg.max_image_dimension = v32;
        }
    }
    cfg.agent_cursor_glide_duration_ms =
        pip_preview::read_config_value(cua_driver_core::agent_cursor::GLIDE_DURATION_CONFIG_KEY)
            .as_ref()
            .and_then(|v| cua_driver_core::agent_cursor::parse_glide_duration(v).ok())
            .unwrap_or(0.0);

    cfg
}

use cua_driver_core::snapshot_store::ZoomContext;

pub struct ToolState {
    pub snapshots: Arc<Snapshots>,
    pub cursor_registry: Arc<CursorRegistry>,
    pub capture_service: Arc<cua_driver_core::capture_runtime::CaptureService>,
    pub mouse_hold: std::sync::Mutex<std::collections::HashMap<String, MouseHoldState>>,
    pub config: Arc<RwLock<DriverConfig>>,
    #[cfg(test)]
    production_route_backend: Option<Arc<ProductionRouteBackend>>,
    #[cfg(test)]
    observed_scroll_backend: Option<Arc<retained_scroll_route_tests::Backend>>,
    #[cfg(test)]
    observed_click_backend: Option<Arc<retained_click::tests::Backend>>,
}

#[cfg(test)]
struct ProductionRouteBackend {
    establish:
        Arc<dyn Fn(u32, u64) -> anyhow::Result<crate::wayland::ExactTargetProof> + Send + Sync>,
    point_action: Arc<
        dyn Fn(&crate::wayland::ExactTargetProof, i32, i32) -> anyhow::Result<Option<String>>
            + Send
            + Sync,
    >,
    click: Arc<
        dyn Fn(
                crate::wayland::ExactTargetProof,
                i32,
                i32,
                u32,
                u8,
            )
                -> anyhow::Result<Option<crate::wayland::shell_helper::ForegroundTerminalOutcome>>
            + Send
            + Sync,
    >,
    set_value: Arc<
        dyn Fn(&crate::wayland::ExactTargetProof, u64, &str) -> anyhow::Result<()> + Send + Sync,
    >,
}

impl ToolState {
    fn zoom_context(
        &self,
        args: &Value,
        pid: u32,
        window_id: Option<u64>,
    ) -> Result<ZoomContext, ToolResult> {
        self.snapshots.zoom(
            pid as i32,
            window_id,
            args.get("_session_id").and_then(Value::as_str),
        )
    }
}

#[derive(Debug)]
enum CoordinateContext {
    Zoom(ZoomContext),
    Screenshot(f64),
}

fn coordinate_click_context(
    native_refusal: Option<ToolResult>,
    resolve_context: impl FnOnce() -> Result<CoordinateContext, ToolResult>,
) -> Result<CoordinateContext, ToolResult> {
    if let Some(refusal) = native_refusal {
        return Err(refusal);
    }
    resolve_context()
}

/// What a native Wayland background pixel click could use once its screenshot
/// frame exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackgroundClickForm {
    /// Unmodified single left click: the exact-point AT-SPI action.
    ExactPoint,
    /// Other unmodified clicks: no focus-free route; foreground can deliver.
    ForegroundOnly,
    /// Modified clicks: the native Wayland pointer route cannot carry
    /// keyboard modifier state in either delivery mode.
    Modified,
}

fn background_click_form(button: u8, count: usize, modified: bool) -> BackgroundClickForm {
    if modified {
        BackgroundClickForm::Modified
    } else if button == 1 && count == 1 {
        BackgroundClickForm::ExactPoint
    } else {
        BackgroundClickForm::ForegroundOnly
    }
}

/// Refusal for a native Wayland background pixel click whose screenshot frame
/// is missing. Only an unmodified single left click can still be delivered in
/// the background once a screenshot exists, so only it is told to capture and
/// retry in the background; other unmodified clicks get the native refusal
/// (foreground) plus the screenshot prerequisite; modified clicks are told that
/// no mode can deliver them. Every other frame error (zoom context,
/// arguments), and every session without the native refusal, is unchanged.
fn missing_frame_click_refusal(
    native_refusal: Option<ToolResult>,
    frame_refusal: ToolResult,
    form: BackgroundClickForm,
) -> ToolResult {
    let frame = frame_refusal.structured_content.as_ref();
    let missing_screenshot = frame
        .and_then(|structured| structured.get("code"))
        .and_then(Value::as_str)
        == Some("screenshot_context_missing");
    let Some(mut native) = native_refusal.filter(|_| missing_screenshot) else {
        return frame_refusal;
    };
    let code = native
        .structured_content
        .as_ref()
        .and_then(|structured| structured.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("background_unavailable")
        .to_owned();
    let field = |name: &str| frame.and_then(|structured| structured.get(name)).cloned();
    match form {
        BackgroundClickForm::ExactPoint => ToolResult::error(
            "Background delivery is not available without a screenshot: on native Wayland a \
             background pixel click can only use the exact-point accessibility action, which \
             needs a screenshot of this window owned by this session. Call get_window_state \
             with a screenshot on the same connection, then retry this click with \
             delivery_mode:\"background\".",
        )
        .with_structured(json!({
            "code": code,
            "cause": "screenshot_context_missing",
            "pid": field("pid"),
            "window_id": field("window_id"),
            "suggestion": "Call get_window_state with a screenshot on the same connection, then \
                           retry this click in the background.",
        })),
        BackgroundClickForm::ForegroundOnly => {
            native
                .content
                .push(cua_driver_core::protocol::Content::text(
                    "Pixel coordinates also need a screenshot of this window owned by this \
                     session: call get_window_state with a screenshot on the same connection \
                     before retrying.",
                ));
            if let Some(structured) = native.structured_content.as_mut() {
                structured["cause"] = json!("screenshot_context_missing");
                structured["pid"] = field("pid").unwrap_or(Value::Null);
                structured["window_id"] = field("window_id").unwrap_or(Value::Null);
            }
            native
        }
        BackgroundClickForm::Modified => ToolResult::error(
            "Background delivery is not available: modified pixel clicks are unavailable on \
             native Wayland in either delivery mode, because the pointer route cannot carry \
             keyboard modifier state. Neither a screenshot nor foreground delivery can deliver \
             this click; retry it without modifiers.",
        )
        .with_structured(json!({
            "code": code,
            "reason": "modified_pixel_click_unsupported",
            "pid": field("pid"),
            "window_id": field("window_id"),
        })),
    }
}

fn coordinate_drag_context(
    native_refusal: Option<ToolResult>,
    resolve_context: impl FnOnce() -> Result<CoordinateContext, ToolResult>,
) -> Result<CoordinateContext, ToolResult> {
    if let Some(refusal) = native_refusal {
        return Err(refusal);
    }
    resolve_context()
}

fn coordinate_scroll_scale(
    native_refusal: Option<ToolResult>,
    resolve_scale: impl FnOnce() -> Result<f64, ToolResult>,
) -> Result<f64, ToolResult> {
    if let Some(refusal) = native_refusal {
        return Err(refusal);
    }
    resolve_scale()
}

fn mouse_button_up_coordinates(
    state: &ToolState,
    args: &Value,
    hold: &MouseHoldState,
) -> Result<(f64, f64), ToolResult> {
    if args.get("x").is_none() && args.get("y").is_none() {
        return Ok((hold.x, hold.y));
    }

    let mut x = args.opt_f64("x").unwrap_or(hold.x);
    let mut y = args.opt_f64("y").unwrap_or(hold.y);
    if args.bool_or("from_zoom", false) {
        let context = state.zoom_context(args, hold.pid, Some(hold.xid))?;
        return Ok(context.zoom_to_window(x, y));
    }

    let ratio = screenshot_scale(state, args, hold.pid, Some(hold.xid))?;
    x *= ratio;
    y *= ratio;
    Ok((x, y))
}

fn screenshot_scale(
    state: &ToolState,
    args: &Value,
    pid: u32,
    window_id: Option<u64>,
) -> Result<f64, ToolResult> {
    state
        .snapshots
        .screenshot_scale(pid as i32, window_id, args)
}

#[derive(Clone, Debug)]
pub struct MouseHoldState {
    pub pid: u32,
    pub xid: u64,
    pub button: u8,
    pub x: f64,
    pub y: f64,
}

impl ToolState {
    pub fn new() -> Arc<Self> {
        Self::new_with_capture_service(Arc::new(
            cua_driver_core::capture_runtime::CaptureService::default(),
        ))
    }

    fn new_with_capture_service(
        capture_service: Arc<cua_driver_core::capture_runtime::CaptureService>,
    ) -> Arc<Self> {
        let snapshots = Arc::new(Snapshots::new());
        Arc::new(Self {
            snapshots,
            cursor_registry: Arc::new(CursorRegistry::new()),
            capture_service,
            mouse_hold: std::sync::Mutex::new(Default::default()),
            config: Arc::new(RwLock::new(load_driver_config())),
            #[cfg(test)]
            production_route_backend: None,
            #[cfg(test)]
            observed_scroll_backend: None,
            #[cfg(test)]
            observed_click_backend: None,
        })
    }

    #[cfg(test)]
    fn new_with_production_route_backend(backend: ProductionRouteBackend) -> Arc<Self> {
        let mut state = Arc::try_unwrap(Self::new()).ok().expect("fresh tool state");
        state.production_route_backend = Some(Arc::new(backend));
        Arc::new(state)
    }

    fn wayland_input_enabled(&self) -> bool {
        #[cfg(test)]
        if self.production_route_backend.is_some() {
            return true;
        }
        crate::wayland::wayland_input_enabled()
    }

    fn wayland_inject_mode(&self) -> bool {
        #[cfg(test)]
        if self.production_route_backend.is_some() {
            return true;
        }
        crate::wayland::is_inject_mode()
    }

    fn window_local_to_output(&self, xid: u64, x: i32, y: i32) -> (i32, i32) {
        #[cfg(test)]
        if self.production_route_backend.is_some() {
            return (x, y);
        }
        crate::wayland::window_local_to_output(xid, x, y)
    }

    fn establish_exact_target(
        &self,
        pid: u32,
        xid: u64,
    ) -> anyhow::Result<crate::wayland::ExactTargetProof> {
        #[cfg(test)]
        if let Some(backend) = &self.production_route_backend {
            return (backend.establish)(pid, xid);
        }
        crate::wayland::establish_exact_target(pid, xid)
    }

    fn point_action(
        &self,
        proof: &crate::wayland::ExactTargetProof,
        x: i32,
        y: i32,
    ) -> anyhow::Result<Option<String>> {
        #[cfg(test)]
        if let Some(backend) = &self.production_route_backend {
            return (backend.point_action)(proof, x, y);
        }
        crate::atspi::perform_action_at_screen_point(proof, x, y)
    }

    fn exact_click(
        &self,
        proof: crate::wayland::ExactTargetProof,
        x: i32,
        y: i32,
        count: u32,
        button: u8,
    ) -> anyhow::Result<Option<crate::wayland::shell_helper::ForegroundTerminalOutcome>> {
        #[cfg(test)]
        if let Some(backend) = &self.production_route_backend {
            return (backend.click)(proof, x, y, count, button);
        }
        crate::wayland::click_with_outcome(proof, x, y, count, button)
    }

    fn exact_set_value(
        &self,
        proof: &crate::wayland::ExactTargetProof,
        element_key: u64,
        identity: Option<&crate::atspi::AtspiIdentity>,
        index: usize,
        value: &str,
    ) -> anyhow::Result<()> {
        #[cfg(test)]
        if let Some(backend) = &self.production_route_backend {
            return (backend.set_value)(proof, element_key, value);
        }
        let identity = identity
            .ok_or_else(|| anyhow::anyhow!("stale_element_token: native identity unavailable"))?;
        crate::atspi::native::resolve_observed_target(
            proof.pid(),
            index,
            proof.window_id(),
            identity,
            Some(proof.clone()),
        )?
        .set_value(value)
    }
}

// ── list_apps ────────────────────────────────────────────────────────────────

pub struct ListAppsTool;
static LIST_APPS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn exact_installed_process_match(
    process: &crate::proc_fs::ProcessInfo,
    installed: &[crate::installed_apps::InstalledApp],
) -> Option<usize> {
    let argv0 = process
        .argv
        .first()
        .map(String::as_str)
        .or_else(|| (!process.cmdline.is_empty()).then_some(process.cmdline.as_str()))
        .unwrap_or(&process.name);
    let flatpak_id_matches = installed
        .iter()
        .enumerate()
        .filter(|(_, app)| {
            let launcher_tokens = app.launch_path.split_whitespace().collect::<Vec<_>>();
            let is_flatpak_launcher = exec_basename(&app.launch_path) == "flatpak"
                && launcher_tokens.get(1) == Some(&"run")
                && launcher_tokens.iter().any(|token| *token == app.bundle_id);
            let claims_id = process
                .argv
                .windows(2)
                .any(|pair| pair[0] == "--name" && pair[1] == app.bundle_id)
                || process
                    .argv
                    .iter()
                    .any(|arg| arg.strip_prefix("--name=") == Some(app.bundle_id.as_str()));
            let wm_class_matches = app
                .startup_wm_class
                .as_deref()
                .is_none_or(|wm_class| wm_class == process.name);
            is_flatpak_launcher && claims_id && wm_class_matches
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if flatpak_id_matches.len() == 1 {
        return flatpak_id_matches.first().copied();
    }
    if !flatpak_id_matches.is_empty() {
        return None;
    }

    let exact_executable_matches = installed
        .iter()
        .enumerate()
        .filter(|(_, app)| {
            exec_basename(&app.launch_path) != "flatpak"
                && exec_program_token(&app.launch_path) == argv0
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if exact_executable_matches.len() == 1 {
        return exact_executable_matches.first().copied();
    }
    None
}

fn merge_processes_with_installed_apps(
    processes: &[crate::proc_fs::ProcessInfo],
    installed: &[crate::installed_apps::InstalledApp],
) -> Vec<Value> {
    let mut consumed = std::collections::HashSet::new();
    let mut out = Vec::new();
    for process in processes {
        let merged = exact_installed_process_match(process, installed);
        if let Some(index) = merged {
            consumed.insert(index);
        }
        let executable = process
            .argv
            .first()
            .map(String::as_str)
            .or_else(|| (!process.cmdline.is_empty()).then_some(process.cmdline.as_str()))
            .unwrap_or(&process.name);
        let basename = exec_basename(executable);
        if basename.is_empty() {
            continue;
        }
        let (name, bundle_id, launch_path, kind, last_used) = match merged {
            Some(index) => {
                let app = &installed[index];
                (
                    app.name.clone(),
                    Some(app.bundle_id.clone()),
                    Some(app.launch_path.clone()),
                    Some("desktop".to_owned()),
                    app.last_used.clone(),
                )
            }
            None => (
                if process.name.is_empty() {
                    basename
                } else {
                    process.name.clone()
                },
                None,
                None,
                None,
                None,
            ),
        };
        out.push(json!({
            "pid": process.pid, "bundle_id": bundle_id, "name": name,
            "running": true, "active": false, "kind": kind,
            "launch_path": launch_path, "last_used": last_used,
            "windows": Vec::<Value>::new(),
        }));
    }
    for (index, app) in installed.iter().enumerate() {
        if consumed.contains(&index) {
            continue;
        }
        out.push(json!({
            "pid": 0, "bundle_id": app.bundle_id, "name": app.name,
            "running": false, "active": false, "kind": "desktop",
            "launch_path": app.launch_path, "last_used": app.last_used,
            "windows": Vec::<Value>::new(),
        }));
    }
    out
}

#[async_trait]
impl Tool for ListAppsTool {
    fn def(&self) -> &ToolDef {
        LIST_APPS_DEF.get_or_init(|| ToolDef {
            name: "list_apps".into(),
            description:
                "List Linux apps — both currently running and installed-but-not-running — \
                with per-app state flags:\n\n\
                - running: is a process for this app live? (pid is 0 when false)\n\
                - active: reserved (Linux X11/Wayland focus model differs from frontmost-app); \
                always false.\n\
                - kind: `\"desktop\"` for XDG `.desktop` launcher entries.\n\
                - launch_path: the launcher command from `Exec=` (field codes stripped). \
                Pass to `launch_app(launch_path=...)`.\n\
                - bundle_id: the XDG \"desktop file id\" — the `.desktop` file's path \
                relative to its XDG `applications/` root with the `.desktop` suffix \
                stripped and path separators replaced with `-` \
                (e.g. `kde4/konqbrowser.desktop` → `kde4-konqbrowser`).\n\
                - last_used: RFC3339 mtime of the `.desktop` file, when readable.\n\
                - windows: the app's current top-level windows (same records as list_windows); \
                empty for a process without one.\n\n\
                Running apps come from `/proc`. Installed apps come from XDG Desktop Entry \
                files in $XDG_DATA_HOME/applications and each $XDG_DATA_DIRS entry's \
                applications/ subdir. Entries with `NoDisplay=true` or `Hidden=true` are \
                filtered. A `.desktop` file is merged with a running process only from an exact, \
                unambiguous executable path or a constrained `--name` desktop-ID claim \
                corroborated by StartupWMClass when present; ambiguous aliases fail closed.\n\n\
                Use this for \"is X installed?\" as well as \"is X running?\". For per-window \
                state — visibility, geometry, titles — call list_windows instead."
                    .into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        let apps = cua_driver_core::blocking::spawn(|| -> Vec<serde_json::Value> {
            let procs = crate::proc_fs::list_processes();
            let installed = crate::installed_apps::list_installed_apps();
            let windows = crate::wayland::list_windows_dispatch(None);

            // An app whose launcher is not its process (`libreoffice --calc`
            // runs `soffice.bin`, `google-chrome` runs `chrome`, `gnome-terminal`
            // hands off to `gnome-terminal-server`) never matches by
            // basename. Its top-level windows still carry its WM_CLASS, so
            // owning one is what makes it running.
            let by_window: std::collections::HashMap<usize, u32> = installed
                .iter()
                .enumerate()
                .filter_map(|(i, app)| {
                    windows
                        .iter()
                        .find(|w| w.pid.is_some() && app.owns_window_class(&w.app_name))
                        .and_then(|w| w.pid)
                        .map(|pid| (i, pid))
                })
                .collect();

            merge_processes_with_installed_apps(&procs, &installed)
        })
        .await
        .unwrap_or_default();

        let running_count = apps
            .iter()
            .filter(|a| a["running"].as_bool().unwrap_or(false))
            .count();
        let total = apps.len();
        let installed_only = total - running_count;
        let mut lines = vec![format!(
            "✅ Found {total} app(s): {running_count} running, {installed_only} installed-not-running."
        )];
        for app in apps
            .iter()
            .filter(|a| a["running"].as_bool().unwrap_or(false))
        {
            let name = app["name"].as_str().unwrap_or("?");
            let pid = app["pid"].as_u64().unwrap_or(0);
            lines.push(format!("- {name} (pid {pid})"));
        }
        // Unified `apps` array + legacy `processes` alias for older callers.
        let structured = json!({
            "apps": apps,
            "processes": apps.iter().filter(|a| a["running"].as_bool().unwrap_or(false))
                .map(|a| json!({
                    "pid":  a["pid"], "name": a["name"]
                })).collect::<Vec<_>>(),
        });
        ToolResult::text(lines.join("\n")).with_structured(structured)
    }
}

/// Return the executable token after stripping leading `env` assignments and
/// outer quoting. `env FOO=1 /usr/bin/firefox %U` → `/usr/bin/firefox`.
fn exec_program_token(s: &str) -> String {
    // Take the first whitespace-separated token that looks like a binary,
    // skipping `env`-style prefixes and `K=V` assignments.
    for tok in s.split_whitespace() {
        if tok == "env" {
            continue;
        }
        if tok.contains('=') && !tok.starts_with('/') && !tok.starts_with('-') {
            // `FOO=bar` env-var assignment — skip.
            continue;
        }
        let cleaned = tok.trim_matches(|c| c == '"' || c == '\'');
        return cleaned.to_owned();
    }
    String::new()
}

/// Return the lowercase basename of the executable token.
fn exec_basename(s: &str) -> String {
    let program = exec_program_token(s);
    std::path::Path::new(&program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&program)
        .to_ascii_lowercase()
}

#[cfg(test)]
mod list_apps_tests {
    use super::*;

    fn app(
        name: &str,
        id: &str,
        exec: &str,
        wm_class: Option<&str>,
    ) -> crate::installed_apps::InstalledApp {
        crate::installed_apps::InstalledApp {
            name: name.to_owned(),
            bundle_id: id.to_owned(),
            launch_path: exec.to_owned(),
            startup_wm_class: wm_class.map(str::to_owned),
            last_used: None,
        }
    }
    fn process(pid: u32, name: &str, argv: &[&str]) -> crate::proc_fs::ProcessInfo {
        crate::proc_fs::ProcessInfo {
            pid,
            name: name.to_owned(),
            cmdline: argv.first().copied().unwrap_or_default().to_owned(),
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    #[test]
    fn exact_flatpak_identity_merges_running_zen_without_pid_zero_duplicate() {
        let installed = vec![app(
            "Zen Browser",
            "app.zen_browser.zen",
            "/usr/bin/flatpak run --branch=stable app.zen_browser.zen",
            Some("zen"),
        )];
        let processes = vec![
            process(
                4242,
                "zen",
                &["/app/zen/zen", "--name", "app.zen_browser.zen"],
            ),
            process(
                4243,
                "Socket Process",
                &["/app/zen/zen", "-contentproc", "-parentBuildID", "fixture"],
            ),
        ];
        let apps = merge_processes_with_installed_apps(&processes, &installed);
        let canonical = apps
            .iter()
            .filter(|app| app["bundle_id"] == "app.zen_browser.zen")
            .collect::<Vec<_>>();
        assert_eq!(canonical.len(), 1);
        assert_eq!(canonical[0]["pid"], 4242);
        assert_eq!(canonical[0]["name"], "Zen Browser");
        assert!(apps
            .iter()
            .any(|app| app["pid"] == 4243 && app["bundle_id"].is_null()));
        assert!(!apps.iter().any(|app| app["pid"] == 0));
    }

    #[test]
    fn ambiguous_exact_aliases_fail_closed() {
        let installed = vec![
            app(
                "First",
                "org.example.First",
                "/opt/first/shared",
                Some("shared"),
            ),
            app(
                "Second",
                "org.example.Second",
                "/opt/second/shared",
                Some("shared"),
            ),
        ];
        let apps = merge_processes_with_installed_apps(
            &[process(7, "shared", &["/app/bin/shared"])],
            &installed,
        );
        assert!(apps.iter().find(|app| app["pid"] == 7).unwrap()["bundle_id"].is_null());
        assert_eq!(apps.iter().filter(|app| app["pid"] == 0).count(), 2);
    }

    #[test]
    fn freeform_bundle_argument_and_wm_class_alone_do_not_claim_identity() {
        let installed = vec![app(
            "Zen Browser",
            "app.zen_browser.zen",
            "/usr/bin/flatpak run app.zen_browser.zen",
            Some("zen"),
        )];
        let apps = merge_processes_with_installed_apps(
            &[process(
                11,
                "zen",
                &["/tmp/unrelated", "app.zen_browser.zen"],
            )],
            &installed,
        );
        assert!(apps
            .iter()
            .any(|app| app["pid"] == 11 && app["bundle_id"].is_null()));
        assert!(apps
            .iter()
            .any(|app| { app["pid"] == 0 && app["bundle_id"] == "app.zen_browser.zen" }));
    }

    #[test]
    fn flatpak_launcher_process_and_nonflatpak_name_claim_do_not_cross_identity_lanes() {
        let installed = vec![
            app(
                "Zen Browser",
                "app.zen_browser.zen",
                "/usr/bin/flatpak run app.zen_browser.zen",
                Some("zen"),
            ),
            app("Native", "org.example.Native", "/usr/bin/native", None),
        ];
        let apps = merge_processes_with_installed_apps(
            &[
                process(
                    21,
                    "flatpak",
                    &["/usr/bin/flatpak", "run", "app.zen_browser.zen"],
                ),
                process(
                    22,
                    "unrelated",
                    &["/tmp/unrelated", "--name", "org.example.Native"],
                ),
            ],
            &installed,
        );
        for pid in [21, 22] {
            assert!(apps
                .iter()
                .any(|app| app["pid"] == pid && app["bundle_id"].is_null()));
        }
        assert_eq!(apps.iter().filter(|app| app["pid"] == 0).count(), 2);
    }

    #[test]
    fn unmatched_stays_installed_only_and_exact_executable_merges() {
        let installed = vec![
            app("Calculator", "org.example.Calc", "/usr/bin/calc", None),
            app("Editor", "org.example.Editor", "/usr/bin/editor", None),
        ];
        let apps = merge_processes_with_installed_apps(
            &[process(9, "calc", &["/usr/bin/calc"])],
            &installed,
        );
        assert!(apps
            .iter()
            .any(|app| app["pid"] == 9 && app["name"] == "Calculator"));
        assert!(apps
            .iter()
            .any(|app| app["pid"] == 0 && app["name"] == "Editor"));
        assert!(!apps
            .iter()
            .any(|app| app["pid"] == 0 && app["name"] == "Calculator"));
    }
}

// ── list_windows ─────────────────────────────────────────────────────────────

pub struct ListWindowsTool;
static LIST_WINDOWS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn filter_on_screen_windows(
    mut windows: Vec<crate::x11::WindowInfo>,
    on_screen_only: bool,
) -> Vec<crate::x11::WindowInfo> {
    if on_screen_only {
        windows.retain(|window| window.is_on_screen);
    }
    windows
}

#[async_trait]
impl Tool for ListWindowsTool {
    fn def(&self) -> &ToolDef {
        LIST_WINDOWS_DEF.get_or_init(|| ToolDef {
            name: "list_windows".into(),
            description: "List top-level windows. Each record includes z_index (integer or null; \
                higher values are closer to the front; null means stacking order is unavailable \
                and callers must not infer one). To select a frontmost candidate, take the maximum \
                integer z_index; if every value is null, use an explicit fallback instead of \
                relying on array order.".into(),
            input_schema: json!({"type":"object","properties":{
                "pid":{"type":"integer","description":"Only list windows owned by this process ID."},
                "on_screen_only":{"type":"boolean","description":"When true, filter to visible windows only. Default false."}
            },"additionalProperties":false}),
            read_only: true, destructive: false, idempotent: true, open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let filter_pid = args.opt_u64("pid").map(|v| v as u32);
        let on_screen_only = args.bool_or("on_screen_only", false);
        let mut windows = cua_driver_core::blocking::spawn(move || {
            crate::wayland::list_windows_dispatch(filter_pid)
        })
        .await
        .unwrap_or_default();
        // AT-SPI can retain defunct applications after exit; never expose stale targets.
        windows.retain(|window| window.pid.map_or(true, crate::proc_fs::is_process_live));
        windows = filter_on_screen_windows(windows, on_screen_only);
        if crate::wayland::is_wayland() {
            crate::wayland::remember_observed_window_origins(&windows);
        }
        let mut lines = vec![format!("Found {} windows:", windows.len())];
        for w in &windows {
            lines.push(format!(
                "  window_id={} pid={} \"{}\" {}x{}+{}+{}{}",
                w.xid,
                w.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
                w.title,
                w.width,
                w.height,
                w.x,
                w.y,
                if w.app_name.is_empty() {
                    String::new()
                } else {
                    format!(" app={}", w.app_name)
                }
            ));
        }
        let structured =
            json!({ "windows": windows.iter().map(window_record_json).collect::<Vec<_>>() });
        ToolResult::text(lines.join("\n")).with_structured(structured)
    }
}

/// Build the structured `list_windows` record for one Linux window.
///
/// Emits the canonical cross-platform shape — geometry nested under a
/// `bounds: {x,y,width,height}` object plus `app_name` / `is_on_screen`,
/// matching the macOS and Windows backends (#2017) — while KEEPING the
/// historical flat `x/y/width/height` fields inline as a legacy alias so
/// existing Linux callers don't break. Fully additive; no field removed,
/// no schema_version bump.
///
fn window_record_json(w: &crate::x11::WindowInfo) -> Value {
    json!({
        "window_id": w.xid,
        "pid": w.pid,
        "app_name": w.app_name,
        "title": w.title,
        // Canonical cross-platform geometry (macOS/Windows parity).
        "bounds": { "x": w.x, "y": w.y, "width": w.width, "height": w.height },
        "is_on_screen": w.is_on_screen,
        "z_index": w.z_index,
        "native_window_id": w.native_window_id,
        "target_id": w.target_id,
        "helper_epoch": w.helper_epoch,
        "transient_for_window_id": w.transient_for_window_id,
        "transient_for_target_id": w.transient_for_target_id,
        "is_attached_dialog": w.is_attached_dialog,
        "is_modal": w.is_modal,
        "window_type": w.window_type,
        // Workspace indexes are live diagnostic metadata, not durable IDs.
        "workspace_index": w.workspace_index,
        "workspace_active": w.workspace_active,
        "sticky": w.sticky,
        "monitor": w.monitor,
        "capture_current": w.capture_current,
        "identity_capabilities": w.identity_capabilities,
        // Legacy alias: flat fields kept inline for pre-existing callers.
        "x": w.x, "y": w.y,
        "width": w.width, "height": w.height,
    })
}

#[cfg(test)]
mod list_windows_tests {
    use super::*;

    #[test]
    fn record_has_bounds_and_flat_legacy_fields() {
        let w = crate::x11::WindowInfo {
            xid: 42,
            pid: Some(1234),
            app_name: "example-app".to_owned(),
            title: "Example".to_owned(),
            is_on_screen: true,
            z_index: Some(3),
            x: 10,
            y: 20,
            width: 300,
            height: 400,
            native_window_id: None,
            target_id: None,
            helper_epoch: None,
            transient_for_window_id: None,
            transient_for_target_id: None,
            is_attached_dialog: None,
            is_modal: None,
            window_type: None,
            workspace_index: None,
            workspace_active: None,
            sticky: None,
            monitor: None,
            capture_current: None,
            identity_capabilities: None,
        };
        let rec = window_record_json(&w);

        // Canonical cross-platform shape: nested `bounds` object.
        let bounds = rec
            .get("bounds")
            .expect("record must carry a `bounds` object");
        assert_eq!(bounds["x"], json!(10));
        assert_eq!(bounds["y"], json!(20));
        assert_eq!(bounds["width"], json!(300));
        assert_eq!(bounds["height"], json!(400));

        // Legacy alias: flat fields must still be present.
        assert_eq!(rec["x"], json!(10));
        assert_eq!(rec["y"], json!(20));
        assert_eq!(rec["width"], json!(300));
        assert_eq!(rec["height"], json!(400));
        assert_eq!(rec["identity_capabilities"], serde_json::Value::Null);

        // Cross-platform companions.
        assert_eq!(rec["app_name"], json!("example-app"));
        assert_eq!(rec["is_on_screen"], json!(true));
        assert_eq!(rec["z_index"], json!(3));
        assert_eq!(rec["window_id"], json!(42));
        assert!(rec["transient_for_window_id"].is_null());
        assert!(rec["transient_for_target_id"].is_null());
        assert!(rec["is_attached_dialog"].is_null());
        assert!(rec["is_modal"].is_null());
        assert!(rec["window_type"].is_null());
        assert_eq!(rec["title"], json!("Example"));
    }

    #[test]
    fn unavailable_wayland_order_serializes_as_null() {
        let w = crate::x11::WindowInfo {
            xid: 43,
            pid: Some(1234),
            app_name: "native-wayland-app".to_owned(),
            title: "Example".to_owned(),
            is_on_screen: true,
            z_index: None,
            x: 0,
            y: 0,
            width: 300,
            height: 400,
            native_window_id: None,
            target_id: None,
            helper_epoch: None,
            transient_for_window_id: None,
            transient_for_target_id: None,
            is_attached_dialog: None,
            is_modal: None,
            window_type: None,
            workspace_index: None,
            workspace_active: None,
            sticky: None,
            monitor: None,
            capture_current: None,
            identity_capabilities: None,
        };

        assert_eq!(window_record_json(&w)["z_index"], Value::Null);
    }

    #[test]
    fn painted_visible_capture_stale_survives_on_screen_filter_and_serialization() {
        let w = crate::x11::WindowInfo {
            xid: 44,
            pid: Some(1234),
            app_name: "zen".to_owned(),
            title: "Zen".to_owned(),
            is_on_screen: true,
            z_index: Some(1),
            x: 0,
            y: 40,
            width: 2049,
            height: 1688,
            native_window_id: Some(44),
            target_id: Some("epoch:44".to_owned()),
            helper_epoch: Some("epoch".to_owned()),
            transient_for_window_id: None,
            transient_for_target_id: None,
            is_attached_dialog: None,
            is_modal: None,
            window_type: None,
            workspace_index: Some(0),
            workspace_active: Some(true),
            sticky: Some(false),
            monitor: Some(0),
            capture_current: Some(false),
            identity_capabilities: None,
        };
        let filtered = filter_on_screen_windows(vec![w], true);
        assert_eq!(filtered.len(), 1);
        let record = window_record_json(&filtered[0]);
        assert_eq!(record["is_on_screen"], true);
        assert_eq!(record["capture_current"], false);
    }

    #[test]
    fn chromium_launch_detection_uses_executable_basename() {
        assert!(chromium_family_program("/usr/bin/google-chrome-stable"));
        assert!(chromium_family_program("CuaTestHarness.Electron"));
        assert!(chromium_family_program("chromium-browser"));
        assert!(chromium_family_program("/opt/Obsidian.AppImage"));
        assert!(chromium_family_program("Discord-0.0.91.AppImage"));
        assert!(!chromium_family_program("/usr/bin/gnome-text-editor"));
    }

    #[test]
    fn chromium_launch_uses_per_process_accessibility_flag() {
        let mut args = vec!["--user-data-dir=/tmp/cua-test-profile".to_owned()];
        append_renderer_accessibility_argument("/usr/bin/google-chrome-stable", &mut args);
        append_renderer_accessibility_argument("/usr/bin/google-chrome-stable", &mut args);

        assert_eq!(
            args.iter()
                .filter(|arg| arg.as_str() == "--force-renderer-accessibility")
                .count(),
            1
        );
    }

    #[test]
    fn chromium_launch_detection_handles_common_wrappers_and_electron_products() {
        for (program, arguments) in [
            ("env", vec!["PROFILE=test".to_owned(), "code".to_owned()]),
            (
                "flatpak",
                vec!["run".to_owned(), "com.slack.Slack".to_owned()],
            ),
            ("snap", vec!["run".to_owned(), "chromium".to_owned()]),
        ] {
            let mut arguments = arguments;
            append_renderer_accessibility_argument(program, &mut arguments);
            assert_eq!(
                arguments.last().map(String::as_str),
                Some("--force-renderer-accessibility"),
                "program={program} arguments={arguments:?}"
            );
        }
    }

    #[test]
    fn non_chromium_launch_does_not_get_renderer_accessibility_flag() {
        let mut args = Vec::new();
        append_renderer_accessibility_argument("/usr/bin/gnome-text-editor", &mut args);
        assert!(args.is_empty());
    }
}

// ── get_window_state ─────────────────────────────────────────────────────────

/// The largest window screenshot delivered as-is. The Anthropic API downsizes
/// any image above ~1.15 megapixels before the model sees it, so a 1568x861
/// PNG reached the model as ~1447x795 while the driver still mapped its x/y
/// as 1568x861: every pixel click landed a uniform 0.94x short. Capping here
/// keeps "pixels of THIS screenshot" true for the image the model reads.
const WINDOW_SCREENSHOT_MAX_PIXELS: u64 = 1_150_000;

/// The long edge a `w`x`h` image must shrink to so that it holds at most
/// `max_pixels` pixels; `None` when it already fits.
fn megapixel_long_edge_cap(w: u32, h: u32, max_pixels: u64) -> Option<u32> {
    let pixels = u64::from(w) * u64::from(h);
    if w == 0 || h == 0 || pixels <= max_pixels {
        return None;
    }
    let long = w.max(h) as f64;
    let short = w.min(h) as f64;
    let mut edge = (long * (max_pixels as f64 / pixels as f64).sqrt()).floor() as u32;
    // The resizer rounds both sides; step down until the rounded image fits.
    while edge > 1 {
        let scale = edge as f64 / long;
        let fits = ((long * scale).round() as u64) * ((short * scale).round() as u64) <= max_pixels;
        if fits {
            break;
        }
        edge -= 1;
    }
    Some(edge)
}

#[cfg(test)]
mod megapixel_cap_tests;

/// Build a single structured element entry for `get_window_state`.
/// Returns `None` when the node has no `element_index` (non-actionable rows).
fn build_element_entry(
    n: &crate::atspi::AtspiNode,
    snapshot_id: Option<u32>,
    bounds: Option<(i32, i32, u32, u32)>,
) -> Option<serde_json::Value> {
    let idx = n.element_index?;
    // `label` mirrors what a human reading the markdown row would call this
    // element: its name, else its description (Qt keeps a button's tooltip
    // there: "Play"/"Pause"). Never its value: four spin buttons all labelled
    // "0.0" cannot be told apart — such a control is `unlabelled` instead,
    // with its place among its siblings in `description`.
    let unlabelled = n.name.is_none()
        && n.description
            .as_deref()
            .is_some_and(|d| d.starts_with(crate::atspi::native::UNLABELLED_NOTE_PREFIX));
    let label = n
        .name
        .clone()
        .or_else(|| n.description.clone().filter(|_| !unlabelled));
    let mut entry = json!({
        "element_index": idx,
        "role": n.role,
        "depth": n.depth,
    });
    if let Some(snapshot_id) = snapshot_id {
        entry["element_token"] = json!(cua_driver_core::element_token::token_for(snapshot_id, idx));
    }
    if n.in_web_content {
        entry["in_web_content"] = json!(true);
    }
    if let Some(label) = label {
        entry["label"] = json!(label);
    }
    if unlabelled {
        entry["unlabelled"] = json!(true);
    }
    // Surface the element's value separately from `label` (which collapses
    // name→value→description): a field with both a name AND typed text would
    // otherwise hide the text from a caller reading the structured side,
    // leaving it only in tree_markdown. See the macOS get_window_state builder
    // for the rationale.
    if let Some(value) = n.value.clone().filter(|v| !v.is_empty()) {
        entry["value"] = json!(value);
    }
    if let Some(enabled) = n.enabled {
        entry["enabled"] = json!(enabled);
    }
    if let Some(selected) = n.selected {
        entry["selected"] = json!(selected);
    }
    let actions: Vec<String> = n
        .actions
        .iter()
        .filter(|a| !a.trim().is_empty())
        .cloned()
        .collect();
    if !actions.is_empty() {
        entry["actions"] = json!(actions);
    }
    if let Some(parent) = n.parent_element_index {
        entry["parent_index"] = json!(parent);
    }
    if let Some((x, y, w, h)) = bounds {
        entry["frame"] = json!({ "x": x, "y": y, "w": w, "h": h });
    }
    if let Some(description) = n.description.clone().filter(|d| !d.is_empty()) {
        entry["description"] = json!(description);
    }
    Some(entry)
}

/// Roles a popup's rows carry: menu entries, and the list / tree / table
/// rows of a combo list, a completer or a chooser popover.
fn popup_item_role(role: &str) -> bool {
    let role = role.trim().to_ascii_lowercase();
    role.contains("menu item")
        || role == "menu"
        || role.contains("list item")
        || role.contains("tree item")
        || role.contains("table cell")
        || role == "cell"
        || role == "item"
        || role == "option"
}

/// The entries drawn inside a popup of `width`x`height` (frames are
/// popup-local here). When an indexed container (Qt exposes the combo list
/// under the combo box inside the dialog) fills the popup, its subtree is
/// the popup's content; otherwise the item-role elements inside the box.
/// Falls back to the input when nothing matches, so a popup that is not a
/// menu (a GTK popover without its own frame) still returns what the walk
/// found.
fn popup_menu_elements(
    elements: Vec<serde_json::Value>,
    (origin_x, origin_y): (i32, i32),
    width: u32,
    height: u32,
) -> Vec<serde_json::Value> {
    // `frame` is in screen coordinates; compare it with the popup's screen
    // rectangle.
    let frame_inside = |entry: &serde_json::Value| {
        let frame = &entry["frame"];
        let x = frame["x"].as_i64().unwrap_or(i64::MIN) - i64::from(origin_x);
        let y = frame["y"].as_i64().unwrap_or(i64::MIN) - i64::from(origin_y);
        frame.is_object()
            && x >= -2
            && y >= -2
            && x + frame["w"].as_i64().unwrap_or(0) <= width as i64 + 2
            && y + frame["h"].as_i64().unwrap_or(0) <= height as i64 + 2
    };
    let role_of =
        |entry: &serde_json::Value| entry["role"].as_str().unwrap_or("").to_ascii_lowercase();
    // A container whose frame covers most of the popup: the list behind a
    // combo box / completer, the menu behind a context menu.
    let popup_area = u64::from(width) * u64::from(height);
    let container = elements
        .iter()
        .filter(|entry| frame_inside(entry))
        .filter(|entry| {
            let role = role_of(entry);
            matches!(
                role.as_str(),
                "list"
                    | "list box"
                    | "menu"
                    | "tree"
                    | "tree table"
                    | "table"
                    | "panel"
                    | "scroll pane"
                    | "filler"
            )
        })
        .filter(|entry| {
            let frame = &entry["frame"];
            let area = frame["w"].as_u64().unwrap_or(0) * frame["h"].as_u64().unwrap_or(0);
            popup_area > 0 && area * 10 >= popup_area * 6
        })
        .max_by_key(|entry| {
            let frame = &entry["frame"];
            frame["w"].as_u64().unwrap_or(0) * frame["h"].as_u64().unwrap_or(0)
        })
        .and_then(|entry| entry["element_index"].as_u64());
    if let Some(container) = container {
        let parent_of: std::collections::HashMap<u64, u64> = elements
            .iter()
            .filter_map(|entry| {
                Some((
                    entry["element_index"].as_u64()?,
                    entry["parent_index"].as_u64()?,
                ))
            })
            .collect();
        let descends = |mut idx: u64| {
            for _ in 0..64 {
                match parent_of.get(&idx) {
                    Some(parent) if *parent == container => return true,
                    Some(parent) => idx = *parent,
                    None => return false,
                }
            }
            false
        };
        let subtree: Vec<serde_json::Value> = elements
            .iter()
            .filter(|entry| {
                entry["element_index"]
                    .as_u64()
                    .is_some_and(|idx| idx != container && descends(idx))
            })
            .cloned()
            .collect();
        if !subtree.is_empty() {
            return subtree;
        }
    }
    let items: Vec<serde_json::Value> = elements
        .iter()
        .filter(|entry| popup_item_role(&role_of(entry)) && frame_inside(entry))
        .cloned()
        .collect();
    // A popup is either a menu or a list: when list / tree / table rows are
    // drawn inside it, menubar entries whose extents happen to fit the box
    // (Qt reports the main window's menubar items inside a chooser popup's
    // rectangle) are not its content.
    let rows: Vec<serde_json::Value> = items
        .iter()
        .filter(|entry| !role_of(entry).contains("menu"))
        .cloned()
        .collect();
    if !rows.is_empty() {
        rows
    } else if items.is_empty() {
        elements
    } else {
        items
    }
}

/// Elements that have a frame (visible, hit-testable on the screenshot)
/// come first, in walk order; frameless ones follow. A client that caps the
/// result then still shows the document, toolbar and sidebar controls.
fn framed_elements_first(elements: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let (framed, frameless): (Vec<_>, Vec<_>) = elements
        .into_iter()
        .partition(|entry| entry.get("frame").is_some());
    framed.into_iter().chain(frameless).collect()
}

/// Same-pid windows mapped over a target window: transient dialogs (with
/// their modality) and override-redirect popups, plus whether any of them
/// overlaps the target's rectangle (then its own drawable is not what the
/// user sees).
#[derive(Default)]
struct WindowOverlays {
    dialogs: Vec<serde_json::Value>,
    popups: Vec<serde_json::Value>,
    covers_window: bool,
    window_rect: Option<(i32, i32, u32, u32)>,
}

impl WindowOverlays {
    /// One sentence per overlay naming the call that acts on it.
    fn follow_up(&self, pid: u32) -> Option<String> {
        let mut parts = Vec::new();
        for dialog in &self.dialogs {
            let id = dialog["window_id"].as_u64().unwrap_or(0);
            let owning_pid = dialog.get("owning_pid").and_then(|v| v.as_u64());
            parts.push(format!(
                "dialog \"{}\" (window_id {id}, transient of window {}{}{}) is open over this \
                 window: call get_window_state(pid={pid}, window_id={id}) to index it and act \
                 there (pid-only keys go to it).",
                dialog["title"].as_str().unwrap_or(""),
                dialog["transient_for"].as_u64().unwrap_or(0),
                if dialog["modal"].as_bool() == Some(true) {
                    ", modal"
                } else {
                    ""
                },
                owning_pid
                    .map(|owner| format!(
                        ", owned by a different process pid {owner}, not pid {pid}"
                    ))
                    .unwrap_or_default(),
            ));
        }
        for popup in &self.popups {
            let id = popup["window_id"].as_u64().unwrap_or(0);
            parts.push(format!(
                "popup (window_id {id}, bounds x={} y={} {}x{}) is open: call \
                 get_window_state(pid={pid}, window_id={id}) to index its items and click them \
                 by element_token.",
                popup["bounds"]["x"],
                popup["bounds"]["y"],
                popup["bounds"]["width"],
                popup["bounds"]["height"]
            ));
        }
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

fn rects_intersect(a: (i32, i32, u32, u32), b: (i32, i32, u32, u32)) -> bool {
    a.0 < b.0 + b.2 as i32
        && a.0 + a.2 as i32 > b.0
        && a.1 < b.1 + b.3 as i32
        && a.1 + a.3 as i32 > b.1
}

fn window_overlays(pid: u32, xid: u64) -> WindowOverlays {
    let mut out = WindowOverlays {
        window_rect: crate::x11::window_info(xid).map(|w| (w.x, w.y, w.width, w.height)),
        ..Default::default()
    };
    let over = |rect: (i32, i32, u32, u32)| {
        out.window_rect
            .is_some_and(|target| rects_intersect(rect, target))
    };
    let mut covers = false;
    for window in crate::x11::list_windows(Some(pid)) {
        if window.xid == xid || !window.is_on_screen || window.width == 0 || window.height == 0 {
            continue;
        }
        let Some(owner) = crate::x11::transient_for(window.xid) else {
            continue;
        };
        covers |= over((window.x, window.y, window.width, window.height));
        out.dialogs.push(json!({
            "window_id": window.xid,
            "title": window.title,
            "transient_for": owner,
            "modal": crate::x11::window_is_modal(window.xid),
            "bounds": { "x": window.x, "y": window.y, "width": window.width, "height": window.height },
        }));
    }
    // A dialog/plugin window can legitimately run as a DIFFERENT process than
    // the application it belongs to (GIMP's separate-process export dialogs,
    // LibreOffice's Document Recovery dialog under a distinct soffice.bin).
    // Exact-pid matching above would make such a window invisible even though
    // it is unambiguously this application's own popup. WM_TRANSIENT_FOR
    // resolving to one of pid's own windows is the correlation signal (see
    // `list_cross_pid_transient_windows`); such entries are tagged with
    // `owning_pid` so callers can tell it apart from a same-process dialog.
    for window in crate::x11::list_cross_pid_transient_windows(pid) {
        if window.xid == xid || !window.is_on_screen || window.width == 0 || window.height == 0 {
            continue;
        }
        let Some(owner) = crate::x11::transient_for(window.xid) else {
            continue;
        };
        covers |= over((window.x, window.y, window.width, window.height));
        out.dialogs.push(json!({
            "window_id": window.xid,
            "title": window.title,
            "transient_for": owner,
            "modal": crate::x11::window_is_modal(window.xid),
            "bounds": { "x": window.x, "y": window.y, "width": window.width, "height": window.height },
            "owning_pid": window.pid,
        }));
    }
    for popup in crate::input::mapped_popup_windows() {
        if popup.window == xid || popup.pid.is_some_and(|owner| owner != pid) {
            continue;
        }
        // Desktop-wide override-redirect windows that are nobody's menu:
        // mutter's guard window and the driver's own cursor overlay.
        if popup.pid.is_none()
            && (popup.title.contains("guard window") || popup.title.starts_with("Cua."))
        {
            continue;
        }
        covers |= over((popup.x, popup.y, popup.width, popup.height));
        out.popups.push(popup.to_json());
    }
    out.covers_window = covers;
    out
}

pub struct GetWindowStateTool {
    state: Arc<ToolState>,
}

const COLD_START_WALK_TIMEOUT_MS: u64 = 2_000;

fn linux_snapshot_timeout_ms(timeout: Option<&Value>, has_prior_snapshot: bool) -> u64 {
    cua_driver_core::tool_schema::resolve_timeout_ms_with_first_snapshot_grace(
        timeout,
        has_prior_snapshot,
        COLD_START_WALK_TIMEOUT_MS,
    )
}

static GWS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn snapshot_publication_error(error: cua_driver_core::element_token::RegistryError) -> ToolResult {
    use cua_driver_core::element_token::RegistryError;
    let code = match error {
        RegistryError::CapacityExhausted { .. }
        | RegistryError::GenerationExhausted
        | RegistryError::PublicHandleExhausted
        | RegistryError::EntropyUnavailable
        | RegistryError::Collision => "capacity_exhausted",
        RegistryError::Poisoned(_) => "snapshot_poisoned",
        RegistryError::Stale | RegistryError::NotCurrent | RegistryError::Superseded => {
            "stale_snapshot"
        }
        RegistryError::Closing | RegistryError::PublicationTimeout => "snapshot_busy",
        RegistryError::ProcessIncarnationUnavailable(_) => "process_identity_unavailable",
    };
    let message = error.to_string();
    ToolResult::error(message.clone()).with_structured(json!({
        "status": "refused",
        "refusal": { "code": code, "message": message }
    }))
}

#[async_trait]
impl Tool for GetWindowStateTool {
    fn def(&self) -> &ToolDef {
        GWS_DEF.get_or_init(|| ToolDef {
            name: "get_window_state".into(),
            description: "Walk a running app's AT-SPI tree and return BOTH a \
                structured `elements` array (preferred) AND a Markdown rendering of \
                the same tree (back-compat). Every actionable element is tagged \
                with [element_index N] in the markdown and as `element_index` in \
                the structured array; pass each element's `element_token` to \
                `click`, `type_text`, `set_value`, etc.\n\n\
                PREFERRED CONSUMERS read `structuredContent.elements` (one entry \
                per indexed row with `element_index`, `role`, `label`, `value`, \
                `enabled`, `selected`, `actions` (names of AT-SPI actions exposed \
                by the element, omitted when empty), \
                `frame: {x,y,w,h}` when AT-SPI reports usable bounds, \
                `parent_index`, `depth`). The markdown `tree_markdown` stays \
                available and unchanged in shape for existing text-parsing \
                callers — but new fields will only be added to the structured \
                side. Set `query` to project BOTH representations to matching \
                rows plus their ancestor chain while preserving original indices. \
                `total_element_count` reports the complete snapshot and \
                `returned_element_count` reports the projection.\n\n\
                Always returns BOTH the element tree AND a screenshot — ground on \
                both and cross-check (the tree lies on some surfaces). Choose the \
                modality at ACTION time: an element ax action \
                (element_token → accessibility rung) or an element px \
                action (x,y → pixel rung off this screenshot). capture_mode is \
                deprecated and ignored. On Wayland, where output capture cannot prove \
                the requested surface's identity, the truthful tree is returned without \
                a screenshot and `screenshot_error.code` is \
                `surface_identity_unproven`.\n\n\
                The mirror image: pass `include_accessibility_tree:false` to SKIP \
                the AT-SPI walk entirely and return just the screenshot plus \
                window metadata (window_bounds, app_name, window_title) — the \
                capture-only path for a live window preview / picture-in-picture. \
                Setting BOTH `include_accessibility_tree:false` and \
                `include_screenshot:false` is an error. Optional `max_dimension` \
                caps the returned screenshot's long edge in pixels for a cheap \
                thumbnail.\n\n\
                Optional `max_elements` / `max_depth` bound the AT-SPI walk to \
                mitigate context-window blow-up on Electron / large web apps \
                that produce 10k+ element trees. When applied, BOTH \
                the markdown and the structured elements are truncated \
                identically. Omit both for current default behaviour.\n\n\
                TIME BUDGET: `timeout_ms` (default 1000) bounds the whole AT-SPI \
                walk. When omitted, a window's first snapshot gets a 2000 ms \
                cold-start budget so Chromium/Electron can finish publishing its \
                initial tree. Explicit values are always honored. Large apps \
                (LibreOffice, GIMP, file managers) can exceed the budget; \
                the call then returns the PARTIAL tree with `truncated: true`, \
                `truncation_reason`, `nodes_visited`/`nodes_pending` and \
                `elements_complete: false`. Every element listed is real and \
                clickable; elements after the cut are simply missing. Retry with a \
                larger `timeout_ms` (e.g. 5000) or narrow with `query`/`max_depth` \
                when the element you need is absent.\n\n\
                SCREENSHOT SCALE: the screenshot is delivered at or below 1.15 megapixels \
                (long edge <= max_image_dimension, 1568 by default), because larger images \
                are downsized before a model reads them and its pixel coordinates would then \
                be uniformly short. Element `screenshot_frame`s and x/y for the pointer tools \
                are pixels of the delivered screenshot (`frame` stays in screen coordinates, \
                as on every platform); `frame_scale` < 1 reports the downsizing. An \
                explicit per-call `max_image_dimension` (0 = native) replaces this cap.\n\n\
                POPUP MENUS: a context menu / popover / combo list is an \
                override-redirect window that list_windows never shows. A click or \
                right_click that opened one names it in its result (`popup: \
                {window_id, bounds, title}`); pass that window_id here to walk the \
                popup's own AT-SPI toplevel so its menu items get element indices \
                (then click them by element_token). Omitting window_id while a popup \
                of this pid is open walks that popup.".into(),
            input_schema: json!({"type":"object","required":["pid"],"properties":{
                "session": cua_driver_core::tool_schema::session_schema(),
                "pid":{"type":"integer","description":"Process ID that owns the window."},
                "window_id":{"type":"integer","description":"Native window identifier from list_windows, or the `popup.window_id` a click / right_click result named (an open context menu / popover; its menu items then get element indices). Omitted: the pid's open popup menu when one is mapped, else its focused / active / largest window."},
                "capture_mode": cua_driver_core::capture_mode::capture_mode_schema(),
                "include_accessibility_tree":{"type":"boolean",
                    "description":"Default true — walk the AT-SPI tree and return `elements` + `tree_markdown` alongside the screenshot. Set false to SKIP the AT-SPI walk entirely and return just the screenshot plus window metadata (window_bounds, app_name, window_title) — the capture-only path for a live window preview / picture-in-picture. Mirrors include_screenshot. Setting BOTH include_accessibility_tree:false AND include_screenshot:false is an error (nothing to return)."},
                "include_screenshot":{"type":"boolean",
                    "description":"Default true — returns a grounding screenshot alongside the tree. Set false to skip the grab and return tree only (the cheap path for re-indexing before an element ax action)."},
                "screenshot_out_file":{"type":"string",
                    "description":"When set, write the PNG to this file path (~ expanded) instead of embedding base64 in the response. The structured output carries screenshot_file_path instead."},
                "query":{"type":"string","description":"Optional case-insensitive substring. Projects both tree_markdown and structured elements to matches plus ancestors while preserving original indices. Compare total_element_count with returned_element_count."},
                "max_elements":{"type":"integer","minimum":1,"description":"Cap on total AT-SPI nodes walked. Omit for the default (5 000). Lower for huge web/Electron trees."},
                "max_depth":{"type":"integer","minimum":1,"description":"Cap on the AT-SPI tree walk depth. Omit for the default (uncapped). Lower for deeply nested apps."},
                "timeout_ms": cua_driver_core::tool_schema::timeout_ms_schema(),
                "max_dimension":{"type":"integer","minimum":1,"description":"Legacy optional cap on the returned screenshot's long edge. Applied on top of the configured max_image_dimension ceiling when max_image_dimension is omitted."},
                "max_image_dimension":{"type":"integer","minimum":0,"description":"Per-call long-edge override. This value wins over configured and legacy limits; 0 returns native-resolution PNG bytes. Omit to preserve configured behavior."}
            },"additionalProperties":false}),
            // Each call mints a new snapshot and its element tokens, which
            // retires the previous ones: repeating it is not idempotent.
            read_only: true, destructive: false, idempotent: false, open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        // `window_id` omitted: the open popup menu of this pid when one is
        // mapped (a context menu the caller wants to read), else the pid's
        // focused / dialog / active / largest window.
        let xid = match args.opt_u64("window_id") {
            Some(v) => v,
            None => {
                let chosen = cua_driver_core::blocking::spawn(move || {
                    if let Some(popup) = crate::input::mapped_popup_windows()
                        .into_iter()
                        .rev()
                        .find(|p| p.pid == Some(pid))
                    {
                        return Some(popup.window);
                    }
                    let windows = crate::x11::list_windows(Some(pid));
                    let candidates: Vec<u64> = windows.iter().map(|w| w.xid).collect();
                    crate::x11::pick_pid_window(
                        &windows,
                        crate::x11::focused_window_among(&candidates),
                        crate::x11::transient_for,
                        crate::x11::active_window(),
                    )
                })
                .await
                .ok()
                .flatten();
                match chosen {
                    Some(v) => v,
                    None => {
                        return ToolResult::error(format!(
                            "No windows found for pid {pid}. Provide window_id."
                        ))
                    }
                }
            }
        };
        // Retain the legacy additive cap, but let the canonical per-call field
        // replace the configured ceiling entirely (including 0 = native size).
        let max_dimension = args
            .get("max_dimension")
            .and_then(|v| v.as_u64())
            .map(|v| v.max(1) as u32);
        let max_image_dimension = match args.get("max_image_dimension") {
            None => None,
            Some(value) => match value.as_u64().and_then(|value| u32::try_from(value).ok()) {
                Some(value) => Some(value),
                None => {
                    return ToolResult::error(
                        "get_window_state.max_image_dimension must be an integer from 0 through 4294967295.",
                    )
                    .with_structured(json!({ "code": "invalid_arguments" }))
                }
            },
        };
        let explicit_max_image_dimension = max_image_dimension.is_some();
        let max_dim = {
            let cfg = self.state.config.read().unwrap();
            cua_driver_core::image_utils::ImageDimensionLimits {
                configured: cfg.max_image_dimension,
                legacy_max_dimension: max_dimension,
                max_image_dimension,
            }
            .resolve()
        };
        // `capture_mode` is DEPRECATED and ignored — get_window_state always
        // returns BOTH the AT-SPI tree and a screenshot now, so the agent grounds
        // on both and cross-checks (the tree lies often enough that a grounding
        // screenshot should always be present). The modality is chosen at action
        // time: an element ax action (element_token) or element px action (x,y).
        // We don't even read the arg; it stays in the schema only so old callers
        // don't trip additionalProperties:false.
        let query = args.opt_str("query");
        let session_id = args.opt_str("_session_id");
        // include_screenshot (default true) — the perf opt-out. The tree+screenshot
        // pair is the default; `include_screenshot:false` skips the grab and returns
        // tree only (the cheap re-index path before an element ax action). A
        // screenshot_out_file still forces a capture (to disk), regardless.
        let include_screenshot = args.get("include_screenshot").and_then(|v| v.as_bool());
        // `include_accessibility_tree` (default true) mirrors include_screenshot:
        // set false to SKIP the AT-SPI walk and return just the screenshot +
        // window metadata (the capture-only / preview path).
        let want_tree = args
            .get("include_accessibility_tree")
            .and_then(|v| v.as_bool())
            != Some(false);
        // screenshot_out_file: when set, write the PNG to disk and surface the
        // path instead of embedding base64 in the response. `~` expands.
        let screenshot_out_file = args.opt_str("screenshot_out_file").map(|s| {
            if let Some(rest) = s.strip_prefix("~/") {
                let home = std::env::var("HOME").unwrap_or_default();
                format!("{home}/{rest}")
            } else {
                s
            }
        });
        // Optional caps — when omitted, the AT-SPI walker uses its built-in
        // defaults (#22865).
        let max_elements = args
            .get("max_elements")
            .and_then(|v| v.as_u64())
            .map(|v| v.max(1) as usize);
        let max_depth = args
            .get("max_depth")
            .and_then(|v| v.as_u64())
            .map(|v| v.max(1) as usize);
        let timeout_ms = linux_snapshot_timeout_ms(
            args.get("timeout_ms"),
            self.state
                .snapshots
                .contains_semantic_window(pid as i32, xid),
        );
        let walk_timeout = std::time::Duration::from_millis(timeout_ms);

        let process_is_live = crate::proc_fs::is_process_live(pid);
        // Enumerate the pid's windows ONCE and reuse the result for both the
        // window-ownership check (Wayland) and the additive window metadata
        // below, instead of paying for the compositor/X11 enumeration twice.
        // `window_meta` also names the surface + its on-screen rectangle on the
        // capture-only path, where no AT-SPI tree identifies it.
        let popup_meta = (!crate::wayland::is_wayland())
            .then(|| crate::input::popup_window_info(xid))
            .flatten();
        let window_meta = crate::wayland::list_windows_dispatch(Some(pid))
            .into_iter()
            .find(|w| w.xid == xid)
            .or_else(|| {
                popup_meta.as_ref().map(|popup| crate::x11::WindowInfo {
                    xid: popup.window,
                    pid: popup.pid,
                    app_name: String::new(),
                    title: popup.title.clone(),
                    is_on_screen: true,
                    z_index: None,
                    x: popup.x,
                    y: popup.y,
                    width: popup.width,
                    height: popup.height,
                    native_window_id: None,
                    target_id: None,
                    helper_epoch: None,
                    transient_for_window_id: None,
                    transient_for_target_id: None,
                    is_attached_dialog: None,
                    is_modal: None,
                    window_type: None,
                    workspace_index: None,
                    workspace_active: None,
                    sticky: None,
                    monitor: None,
                    capture_current: None,
                    identity_capabilities: None,
                })
            });
        let window_matches = explicit_window_belongs_to_pid(pid, xid);
        if !process_is_live || !window_matches {
            return ToolResult::error(format!(
                "Window target pid {pid}, window_id {xid} is stale or no longer running; refresh list_windows."
            ));
        }

        // Always walk the AT-SPI tree; capture the screenshot by default. The
        // tree+screenshot pair is the default so the agent grounds on both and
        // cross-checks the (sometimes-lying) tree against the frame. An explicit
        // `include_screenshot:false` skips the grab; an unproven Wayland surface
        // returns the tree with a typed screenshot error instead of unrelated pixels.
        let should_capture = include_screenshot != Some(false) || screenshot_out_file.is_some();
        if !want_tree && !should_capture {
            return ToolResult::error(
                "Nothing to return: both include_accessibility_tree:false and \
                 include_screenshot:false. Set at least one to true, or pass \
                 screenshot_out_file to force a capture.",
            );
        }
        let observation_only = args
            .get("_observation_only")
            .and_then(|value| value.as_bool())
            == Some(true);
        let state = self.state.clone();
        let capture_args = args.clone();
        let query_for_walk = query.clone();

        let result = cua_driver_core::blocking::spawn(move || -> anyhow::Result<_> {
            let tree_result = want_tree.then(|| crate::atspi::walk_tree_bounded_within(
                pid, xid, query_for_walk.as_deref(), max_elements, max_depth, walk_timeout,
            ));
            // Bounds and element indices come from the same captured AT-SPI
            // traversal. Joining two live walks by ordinal mis-associated
            // Chromium controls when its lazy subtree changed between walks.
            let bounds = tree_result
                .as_ref()
                .map(|tree| tree.bounds.clone())
                .unwrap_or_default();
            // Capture and DELIVER the screenshot alongside the tree by default — the
            // grounding frame the agent cross-checks the tree against. With
            // screenshot_out_file set, write to disk and surface the path instead
            // of embedding base64; otherwise embed base64. Skipped only when
            // include_screenshot:false and no disk path was requested.
            // Keep the exact delivered PNG for capture publication. The action
            // binding must identify the bytes returned to the caller, including
            // any max-dimension resize, rather than the backend's raw frame.
            let mut screenshot_error = None;
            // Same-pid popups (menus, combo lists) and transient dialogs mapped
            // over this window: listed for the caller, and when one overlaps
            // the window the screenshot is taken from the screen, since the
            // window's own drawable never shows them.
            let overlays = if crate::wayland::is_wayland() {
                WindowOverlays::default()
            } else {
                window_overlays(pid, xid)
            };
            let screenshot = if should_capture {
                let captured = if overlays.covers_window {
                    overlays
                        .window_rect
                        .ok_or_else(|| anyhow::anyhow!("window geometry unavailable"))
                        .and_then(|(x, y, w, h)| {
                            crate::capture::screenshot_root_region_png(x, y, w, h)
                        })
                        .or_else(|_| crate::wayland::screenshot_dispatch_with_pid(xid, pid))
                } else {
                    crate::wayland::screenshot_dispatch_with_pid(xid, pid)
                };
                match captured {
                    Ok(raw) => {
                        let (orig_w, orig_h) = crate::capture::png_dimensions_pub(&raw)?;
                        let png = crate::capture::resize_png_if_needed(&raw, max_dim)?;
                        let (w, h) = crate::capture::png_dimensions_pub(&png)?;
                        // An explicit per-call max_image_dimension (0 = native)
                        // is authoritative; the model-safe cap applies otherwise.
                        let cap = if explicit_max_image_dimension {
                            None
                        } else {
                            megapixel_long_edge_cap(w, h, WINDOW_SCREENSHOT_MAX_PIXELS)
                        };
                        let png = match cap {
                            Some(edge) => crate::capture::resize_png_if_needed(&png, edge)?,
                            None => png,
                        };
                        let (w, h) = crate::capture::png_dimensions_pub(&png)?;
                        let original_w = if w < orig_w { Some(orig_w) } else { None };
                        let (b64, file_path) = if let Some(ref path) = screenshot_out_file {
                            std::fs::write(path, &png)?;
                            (None, Some(path.clone()))
                        } else {
                            use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
                            (Some(B64.encode(&png)), None)
                        };
                        Some((b64, file_path, w, h, original_w, (png, orig_w, orig_h)))
                    }
                    Err(error) if crate::wayland::is_surface_identity_unproven(&error) => {
                        screenshot_error = Some(error.to_string());
                        None
                    }
                    // The window passed the ownership check above and went
                    // away during the AT-SPI walk (a document closed by the
                    // action being observed): a stale target, not a capture
                    // failure.
                    Err(_) if !crate::wayland::is_wayland() && !crate::x11::window_exists(xid) => {
                        return Err(anyhow::anyhow!(
                            "Window target pid {pid}, window_id {xid} is stale or no longer running; refresh list_windows."
                        ));
                    }
                    Err(error) => {
                        return Err(anyhow::anyhow!(
                            "window screenshot failed for window {xid}: {error}"
                        ));
                    }
                }
            } else {
                None
            };
            Ok((tree_result, screenshot, bounds, screenshot_error, overlays))
        })
        .await;

        match result {
            Ok(Ok((tree_opt, shot_opt, bounds, screenshot_error, overlays))) => {
                let mut content = Vec::new();
                let mut structured = json!({ "window_id": xid, "pid": pid });
                let screenshot_scale = shot_opt.as_ref().map(|(_, _, w, _, original_w, _)| {
                    original_w.map_or(1.0, |ow| ow as f64 / *w as f64)
                });
                let mut published_snapshot = None;
                let mut invalidated = Vec::new();

                if let Some(tr) = tree_opt {
                    let source_trusted = tr.trusted;
                    let source_degraded_reason = tr.degraded_reason.clone();
                    let count = tr
                        .nodes
                        .iter()
                        .filter(|n| n.element_index.is_some())
                        .count();
                    let mut header = format!(
                        "window_id={xid} pid={pid} elements={count} walk_ms={}\n",
                        tr.elapsed_ms
                    );
                    if tr.truncated {
                        header.push_str(&cua_driver_core::walk_budget::truncation_note(
                            tr.truncation_reason.as_deref(),
                            timeout_ms,
                            tr.nodes_visited,
                            tr.nodes_pending,
                        ));
                        header.push('\n');
                    } else if !tr.bounds_complete {
                        header.push_str(
                            "⚠️ bounds phase ran out of time: some elements have no frame \
                             (element_token clicks still work; pixel targeting may not). \
                             Retry with a larger timeout_ms if you need frames.\n",
                        );
                    }
                    header.push('\n');
                    content.push(cua_driver_core::protocol::Content::text(
                        header + &tr.tree_markdown,
                    ));
                    // Build the immutable AT-SPI payload outside registry locks,
                    // then publish token metadata and cache rows as one generation.
                    // This is fallible; Linux has no production `expect` wrapper.
                    let target_scoped = !(crate::wayland::is_wayland()
                        && crate::wayland::hyprland::is_session())
                        || tr.window_scoped;
                    let snapshot_id = if observation_only {
                        None
                    } else {
                        // Publish an empty generation on lost Hyprland scope,
                        // retiring old authority through the same drain barrier.
                        let nodes = if target_scoped {
                            tr.nodes.as_slice()
                        } else {
                            &[]
                        };
                        let payload =
                            match crate::atspi::snapshot::AtspiSnapshot::try_from_nodes(nodes) {
                                Ok(payload) => payload,
                                Err(error) => return snapshot_publication_error(error),
                            };
                        match state.snapshots.try_publish_for_session(
                            pid as i32,
                            xid,
                            payload,
                            session_id.as_deref(),
                            screenshot_scale,
                        ) {
                            Ok(Some((id, replaced))) => {
                                invalidated.extend(replaced);
                                published_snapshot = Some(id);
                                target_scoped.then_some(id)
                            }
                            Ok(None) => {
                                return ToolResult::error(
                                    "Session ended before snapshot publication",
                                )
                                .with_structured(json!({"refusal":{"code":"session_ended"}}))
                            }
                            Err(error) => return snapshot_publication_error(error),
                        }
                    };
                    structured["element_count"] = json!(count);
                    // Neither trust nor an unspent budget proves exhaustion:
                    // depth caps and individual AT-SPI/child-enumeration failures
                    // can omit subtrees without setting `truncated`. Keep absence
                    // unknown until the walker explicitly proves full traversal.
                    structured["elements_complete"] = json!(false);
                    structured["truncated"] = json!(tr.truncated);
                    if let Some(reason) = &tr.truncation_reason {
                        structured["truncation_reason"] = json!(reason);
                    }
                    structured["nodes_visited"] = json!(tr.nodes_visited);
                    structured["nodes_pending"] = json!(tr.nodes_pending);
                    structured["bounds_complete"] = json!(tr.bounds_complete);
                    structured["walk_elapsed_ms"] = json!(tr.elapsed_ms as u64);
                    structured["timeout_ms"] = json!(timeout_ms);
                    structured["tree_markdown"] = json!(tr.tree_markdown);

                    // Structured `elements` array: one entry per actionable node.
                    // Shape: `{element_index, element_token, role, label,
                    // depth, actions?, parent_index?, frame?: {x,y,w,h}}`. Frame is
                    // included whenever AT-SPI Component.GetExtents(Screen)
                    // reported usable bounds; omitted otherwise (some
                    // toolkits leave bounds unset on hidden / virtual
                    // elements).
                    use std::collections::HashMap;
                    // The walk produces screen extents (what the element
                    // cache and hit-tests compare against). The published
                    // `frame` is the element's screen rectangle, as on macOS and
                    // Windows; `screenshot_frame` (added below) is the same
                    // rectangle in pixels of the screenshot in this response,
                    // the space of the pointer tools' window-local x/y. The
                    // window-local origin is the X11 window's root-relative
                    // origin, the one `window_bounds` and
                    // `coordinate_frame:"desktop"` translation use. A popup
                    // (override-redirect) window is not in the WM's client
                    // list; when its origin cannot be read, the popup's own
                    // screen rectangle is the origin.
                    let local_origin = (!crate::wayland::is_wayland())
                        .then(|| crate::atspi::native::x11_window_origin(xid))
                        .flatten()
                        .or_else(|| popup_meta.as_ref().map(|popup| (popup.x, popup.y)));
                    let bounds_by_idx: HashMap<usize, (i32, i32, u32, u32)> = bounds
                        .into_iter()
                        .map(|(i, x, y, w, h)| (i, (x, y, w, h)))
                        .collect();
                    let elements: Vec<serde_json::Value> = tr
                        .nodes
                        .iter()
                        .filter_map(|n| {
                            build_element_entry(
                                n,
                                snapshot_id,
                                bounds_by_idx.get(&n.element_index?).copied(),
                            )
                        })
                        .collect();
                    let elements = cua_driver_core::element_query::project_elements_for_query(
                        elements,
                        query.as_deref(),
                        &tr.tree_markdown,
                    );
                    let elements = framed_elements_first(elements);
                    // Screenshot pixels: the capture is the window's own X11
                    // drawable (screen pixels), downsized by delivered/native.
                    let elements = match (local_origin, shot_opt.as_ref()) {
                        (Some((ox, oy)), Some((_, _, w, _, orig_w, _))) => {
                            let scale = orig_w.map_or(1.0, |ow| *w as f64 / ow as f64);
                            cua_driver_core::element_frame::with_screenshot_frames(
                                elements,
                                (f64::from(ox), f64::from(oy)),
                                scale,
                            )
                        }
                        _ => elements,
                    };
                    // A popup that has no AT-SPI frame of its own (LibreOffice
                    // VCL menus live under the menubar's `menu` node): return
                    // the open menu's items, the ones drawn inside the popup,
                    // instead of the whole application.
                    let elements = match (&popup_meta, tr.window_scoped) {
                        (Some(popup), false) => popup_menu_elements(
                            elements,
                            (popup.x, popup.y),
                            popup.width,
                            popup.height,
                        ),
                        _ => elements,
                    };
                    structured["total_element_count"] = json!(count);
                    structured["returned_element_count"] = json!(elements.len());
                    structured["elements"] = json!(elements);
                    // Surface 6: snapshot id mirror for debug correlation.
                    if let Some(snapshot_id) = snapshot_id {
                        structured["snapshot_id"] = json!(
                            cua_driver_core::element_token::format_snapshot_id(snapshot_id)
                        );
                    }
                    structured["_note"] = json!(
                        "Prefer `elements` — `tree_markdown` will continue to work \
                         but new fields will only be added to the structured side. \
                         Use `max_elements` / `max_depth` to bound the \
                         AT-SPI walk on apps with very large trees."
                    );
                    // Best-effort-background ladder parity with macOS/Windows: an
                    // AT-SPI walk that ran but found zero actionable elements is
                    // NOT a clean "this window has no controls" — far more often
                    // the bridge wasn't ready (toolkit-accessibility off, or the
                    // daemon isn't on the desktop session bus so the registry is
                    // empty), or it's a non-AX surface (canvas/WebGL). Mark it
                    // degraded so callers don't read `elements: []` as authoritative.
                    if !target_scoped {
                        structured["degraded"] = json!(true);
                        structured["degraded_reason"] = json!("accessibility_window_identity_unproven: tree is application-scoped; exact-window element tokens and bounds are unavailable");
                    } else if !source_trusted {
                        structured["degraded"] = json!(true);
                        structured["degraded_reason"] = json!(source_degraded_reason
                            .unwrap_or_else(|| {
                                "x11_property_fallback_partial: AT-SPI was unavailable and \
                                 Cua Driver only recovered window metadata. Treat it as \
                                 discovery evidence; it cannot prove checked state."
                                    .to_owned()
                            }));
                    } else if count == 0 && tr.truncated {
                        structured["degraded"] = json!(true);
                        structured["degraded_reason"] =
                            json!(cua_driver_core::walk_budget::truncation_note(
                                tr.truncation_reason.as_deref(),
                                timeout_ms,
                                tr.nodes_visited,
                                tr.nodes_pending,
                            ));
                    } else if count == 0 {
                        structured["degraded"] = json!(true);
                        structured["degraded_reason"] = json!(
                            "atspi_tree_empty: the AT-SPI walk returned no actionable \
                             elements. Common causes: the a11y bridge is off (enable \
                             `gsettings set org.gnome.desktop.interface \
                             toolkit-accessibility true`), the daemon is not on the \
                             desktop session bus (DBUS_SESSION_BUS_ADDRESS unreachable — \
                             run `cua-driver doctor`), or the window is a non-AX surface \
                             (canvas/WebGL/custom-drawn). Do not treat element data as \
                             authoritative — verify via the screenshot, and re-snapshot \
                             after enabling a11y or if the app just launched."
                        );
                        // Point the agent at the next rung explicitly: an empty
                        // tree means element_index has nothing to bind to. The
                        // recommendation is session-dependent (X11 → px,
                        // Wayland → foreground) — see non_ax_escalation.
                        structured["escalation"] = non_ax_escalation();
                    }
                }

                if !observation_only && published_snapshot.is_none() {
                    let payload = crate::atspi::snapshot::AtspiSnapshot::try_from_nodes(&[])
                        .expect("empty payload");
                    match state.snapshots.try_publish_capture_for_session(
                        pid as i32,
                        xid,
                        payload,
                        session_id.as_deref(),
                        screenshot_scale,
                    ) {
                        Ok(Some((id, replaced))) => {
                            invalidated.extend(replaced);
                            published_snapshot = Some(id);
                        }
                        Ok(None) => {
                            return ToolResult::error("Session ended before snapshot publication")
                                .with_structured(json!({"refusal":{"code":"session_ended"}}))
                        }
                        Err(error) => return snapshot_publication_error(error),
                    }
                }
                if !invalidated.is_empty() {
                    let ids: Vec<String> = invalidated
                        .into_iter()
                        .map(cua_driver_core::element_token::format_snapshot_id)
                        .collect();
                    content.push(cua_driver_core::protocol::Content::text(format!(
                        "Invalidated snapshots {}: their element_tokens are stale.",
                        ids.join(", ")
                    )));
                    structured["invalidated_snapshot_ids"] = json!(ids);
                }

                if let Some((b64_opt, file_path, w, h, orig_w, (png, native_w, native_h))) =
                    shot_opt
                {
                    let capture_id = if let Some(id) = published_snapshot {
                        let Some(identity) = state.snapshots.identity_for_snapshot(pid as i32, id)
                        else {
                            return snapshot_publication_error(
                                cua_driver_core::element_token::RegistryError::NotCurrent,
                            );
                        };
                        match crate::capture_action_frame::publish_window(
                            &state.capture_service,
                            &capture_args,
                            &png,
                            pid,
                            xid,
                            (w, h),
                            (native_w, native_h),
                            identity,
                        ) {
                            Ok(id) => Some(id),
                            Err(error) => {
                                return ToolResult::error(format!(
                                    "capture publication failed: {error}"
                                ))
                            }
                        }
                    } else {
                        None
                    };
                    // ax mode + screenshot_out_file writes the PNG to disk and
                    // returns b64=None — never embed the image bytes in that case.
                    // Keep a text content part when the image went to disk so the
                    // response is never empty on the capture-only path (which has
                    // no tree markdown either).
                    if let Some(b64) = b64_opt {
                        content.push(cua_driver_core::protocol::Content::image_png(b64));
                    } else if let Some(fp) = &file_path {
                        content.push(cua_driver_core::protocol::Content::text(format!(
                            "window_id={xid} pid={pid} size={w}x{h} screenshot written to {fp}"
                        )));
                    }
                    structured["screenshot_width"] = json!(w);
                    structured["screenshot_height"] = json!(h);
                    structured["screenshot_frame_valid"] = json!(true);
                    if let Some(ow) = orig_w {
                        if ow > 0 {
                            structured["frame_scale"] = json!(w as f64 / ow as f64);
                            structured["screenshot_original_width"] = json!(ow);
                        }
                    }
                    // Surface 7: mirror the MCP image part's `mimeType` onto
                    // the structured payload so consumers don't have to sniff
                    // magic bytes off the base64 to know the format.
                    structured["screenshot_mime_type"] = json!("image/png");
                    if let Some(capture_id) = capture_id {
                        structured["capture_id"] = json!(capture_id);
                    }
                    if let Some(fp) = file_path {
                        structured["screenshot_file_path"] = json!(fp);
                    }
                }
                if let Some(reason) = &screenshot_error {
                    structured["screenshot_frame_valid"] = json!(false);
                    structured["screenshot_error"] =
                        surface_identity_unproven_error(xid, reason.clone());
                }
                // Window identity metadata (additive): app + title + on-screen
                // rectangle for the requested window_id, useful on the
                // capture-only path where no AT-SPI tree names the surface.
                if popup_meta.is_some() {
                    structured["popup"] = json!(true);
                }
                if let Some(meta) = &window_meta {
                    if !meta.app_name.is_empty() {
                        structured["app_name"] = json!(meta.app_name);
                    }
                    if !meta.title.is_empty() {
                        structured["window_title"] = json!(meta.title);
                    }
                    structured["window_bounds"] = json!({
                        "x": meta.x, "y": meta.y, "width": meta.width, "height": meta.height
                    });
                }
                // Transient dialogs and popups of this pid that are open over
                // the window, each with the call that targets it.
                if !overlays.dialogs.is_empty() {
                    structured["dialogs"] = json!(overlays.dialogs);
                }
                if !overlays.popups.is_empty() {
                    structured["popups"] = json!(overlays.popups);
                }
                if overlays.covers_window {
                    structured["screenshot_composited"] = json!(true);
                }
                if let Some(note) = overlays.follow_up(pid) {
                    structured["follow_up"] = json!(note);
                    content.push(cua_driver_core::protocol::Content::text(note));
                }
                structured["coordinate_frame"] = json!("window");
                structured["frame_note"] = json!(
                    "x/y for click / double_click / right_click / drag / scroll on this \
                     window are pixels of THIS screenshot (window-local, 0..screenshot_width \
                     x 0..screenshot_height), as are the element frames; the screenshot is \
                     kept at or below 1.15 megapixels so the image you read is the image these \
                     pixels index (frame_scale < 1 says the window was downsized to it). \
                     window_bounds is where it sits on the screen. Pass scope:\"desktop\" only \
                     for get_desktop_state pixels."
                );

                // The capture-only path (include_accessibility_tree:false) leaves
                // `content` empty when the screenshot was also unavailable — most
                // often on Wayland, where per-window capture cannot prove surface
                // identity. Return a structured error rather than a "successful"
                // response with no content parts.
                if content.is_empty() {
                    let reason_note = match &screenshot_error {
                        Some(reason) => format!(" and no screenshot could be captured ({reason})"),
                        None => " and no screenshot was returned".to_string(),
                    };
                    return ToolResult::error(format!(
                        "No content produced for window_id {xid}: the accessibility tree was \
                         skipped (include_accessibility_tree:false){reason_note}."
                    ))
                    .with_structured(structured);
                }

                ToolResult {
                    content,
                    is_error: None,
                    structured_content: Some(structured),
                    action_record: None,
                }
            }
            Ok(Err(e)) => {
                if !observation_only {
                    state.snapshots.remove(pid as i32, xid);
                }
                ToolResult::error(format!("Capture error: {e}"))
            }
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// One-line, model-facing explanation of a partial tree and what to do about
/// it. Shared by the text header and the structured `degraded_reason`.
fn surface_identity_unproven_error(xid: u64, reason: String) -> Value {
    json!({
        "code": "surface_identity_unproven",
        "window_id": xid,
        "reason": reason,
        "suggestion": "capture the full output explicitly, or retry on a compositor backend that supports identified per-window capture"
    })
}

// ── launch_app ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod get_window_state_actions_tests;

pub struct LaunchAppTool;
static LAUNCH_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn contains_remote_debugging_flag(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("--remote-debugging-port") || lower.contains("--remote-debugging-pipe")
}

/// Enable a Chromium-family renderer's accessibility tree for this child only.
/// Keep this scoped to the launched process instead of using the session-wide
/// `ScreenReaderEnabled` signal, which can cause GNOME to launch Orca.
fn append_renderer_accessibility_argument(prog: &str, args: &mut Vec<String>) {
    if launch_command_targets_chromium_family(prog, args)
        && !args
            .iter()
            .any(|arg| arg == "--force-renderer-accessibility")
    {
        args.push("--force-renderer-accessibility".to_owned());
    }
}

fn launch_command_targets_chromium_family(prog: &str, args: &[String]) -> bool {
    if chromium_family_program(prog) {
        return true;
    }
    let launcher = std::path::Path::new(prog)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(prog)
        .to_ascii_lowercase();
    match launcher.as_str() {
        "env" => {
            let mut skip_next = false;
            args.iter()
                .find(|arg| {
                    if skip_next {
                        skip_next = false;
                        return false;
                    }
                    if arg == &"-u" || arg == &"--unset" || arg == &"-C" || arg == &"--chdir" {
                        skip_next = true;
                        return false;
                    }
                    arg == &"-" || (!arg.starts_with('-') && !arg.contains('='))
                })
                .is_some_and(|target| chromium_family_program(target))
        }
        "flatpak" | "snap" => args
            .iter()
            .skip_while(|arg| arg.as_str() != "run")
            .skip(1)
            .find(|arg| !arg.starts_with('-'))
            .is_some_and(|target| chromium_family_program(target)),
        _ => false,
    }
}

fn url_scheme_handler_mime(url: &str) -> Option<String> {
    let (scheme, _) = url.split_once(':')?;
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    {
        return None;
    }
    Some(format!("x-scheme-handler/{}", scheme.to_ascii_lowercase()))
}

fn default_url_handler_id(url: &str) -> Option<String> {
    let mime = url_scheme_handler_mime(url)?;
    let child = std::process::Command::new("xdg-mime")
        .args(["query", "default", &mime])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let output = bounded_child_output(child, std::time::Duration::from_millis(1500))?;
    let id = std::str::from_utf8(&output).ok()?;
    let id = id.trim().strip_suffix(".desktop").unwrap_or(id.trim());
    (!id.is_empty()).then(|| id.to_owned())
}

fn bounded_child_output(
    mut child: std::process::Child,
    timeout: std::time::Duration,
) -> Option<Vec<u8>> {
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait().ok()? {
            Some(status) => break status,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    };
    if !status.success() {
        return None;
    }
    let stdout = child.stdout.take()?;
    let descriptor = stdout.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return None;
    }
    let mut output = vec![0u8; 4096];
    let read = unsafe {
        libc::read(
            descriptor,
            output.as_mut_ptr().cast::<libc::c_void>(),
            output.len(),
        )
    };
    if read <= 0 {
        return None;
    }
    output.truncate(read as usize);
    Some(output)
}

fn match_url_handler<'a>(
    apps: &'a [crate::installed_apps::InstalledApp],
    desktop_id: &str,
) -> Option<&'a crate::installed_apps::InstalledApp> {
    apps.iter()
        .find(|app| app.bundle_id.eq_ignore_ascii_case(desktop_id))
}

/// Spawn a launcher command line (an executable plus arguments, e.g. an XDG
/// `Exec=` value with field codes stripped) in the background and return the
/// child pid.
fn spawn_launch_child(
    cmd: &str,
    additional_arguments: &[String],
) -> std::io::Result<std::process::Child> {
    let mut parts = cmd.split_whitespace();
    let prog = parts.next().unwrap_or(cmd);
    let mut rest: Vec<String> = parts.map(str::to_owned).collect();
    rest.extend(additional_arguments.iter().cloned());
    append_renderer_accessibility_argument(prog, &mut rest);
    let mut launch = std::process::Command::new(prog);
    launch
        .args(&rest)
        // The child must not inherit the driver's stdio: in private-worker mode
        // stdout is the JSON-RPC stream to the SDK, and a chatty app (Chromium's
        // zygote logs, GTK warnings) writing there corrupts a response and shuts
        // the worker down.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Enable accessibility for this child without toggling GNOME's global
        // ScreenReaderEnabled setting (which can launch Orca). Native
        // toolkits ignore these when they do not need them.
        .env("ACCESSIBILITY_ENABLED", "1")
        .env("NO_AT_BRIDGE", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    launch.spawn()
}

fn spawn_launch_command(cmd: &str, additional_arguments: &[String]) -> std::io::Result<u32> {
    let child = spawn_launch_child(cmd, additional_arguments)?;
    let pid = child.id();
    reap_in_background(child);
    Ok(pid)
}

/// Collect a launched child's exit status in the background.
///
/// `std::process::Child` does not reap on drop and the driver never waits on
/// an app it launches, so every launched app used to linger as a zombie for
/// the daemon's lifetime: it held a pid slot, and — because the pid stays
/// present — made "does this pid exist" liveness checks report a terminated
/// app as still running.
///
/// Reaping is scoped to one thread per child rather than setting
/// `SIGCHLD` to `SIG_IGN`, which is process-global and would break every
/// caller that needs an exit status, including the `xdg-open` check below
/// and any `Command::status()`/`output()` elsewhere in the daemon. The
/// thread blocks in `wait` until the app exits, so it costs nothing while
/// the app runs and never delays the launch itself.
fn reap_in_background(mut child: std::process::Child) {
    let pid = child.id();
    let reaper = std::thread::Builder::new()
        .name(format!("cua-reap-{pid}"))
        .stack_size(64 * 1024)
        .spawn(move || {
            if let Err(e) = child.wait() {
                tracing::debug!(pid, "launched app could not be reaped: {e}");
            }
        });
    if let Err(e) = reaper {
        tracing::debug!(pid, "could not start reaper thread for launched app: {e}");
    }
}

/// Match a launch_app `name` that failed direct exec against installed XDG
/// .desktop applications: exact display name, desktop-file id, or `Exec=`
/// basename first, then a display-name substring when it is unambiguous.
fn match_installed_app<'a>(
    apps: &'a [crate::installed_apps::InstalledApp],
    query: &str,
) -> Option<&'a crate::installed_apps::InstalledApp> {
    let q = query.to_ascii_lowercase();
    if q.is_empty() {
        return None;
    }
    apps.iter()
        .find(|a| {
            a.name.to_ascii_lowercase() == q
                || a.bundle_id.to_ascii_lowercase() == q
                || exec_basename(&a.launch_path) == q
        })
        .or_else(|| {
            let mut matches = apps
                .iter()
                .filter(|a| a.name.to_ascii_lowercase().contains(&q));
            match (matches.next(), matches.next()) {
                (Some(only), None) => Some(only),
                _ => None,
            }
        })
}

/// Watch a just-spawned launcher long enough to catch a fast failure.
/// xdg-open's generic fallback can `exec` the target app and stay alive for
/// its whole lifetime, so a child still running after the grace period counts
/// as success; a quick non-zero exit (2 = file not found, 3 = no handler
/// tool, 4 = action failed) is the only reliable failure signal.
fn quick_launch_failure(mut child: std::process::Child, label: &str) -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    loop {
        // A branch that observes an exit status has already reaped the child;
        // one that gives up on it still owes that, so hand it to the reaper
        // instead of dropping it and leaving a zombie behind.
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return None,
            Ok(Some(status)) => return Some(format!("{label} failed ({status})")),
            Ok(None) if std::time::Instant::now() >= deadline => {
                reap_in_background(child);
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => {
                reap_in_background(child);
                return Some(format!("could not observe {label}: {e}"));
            }
        }
    }
}

fn launch_urls_with_resolver(
    urls: &[String],
    installed: &[crate::installed_apps::InstalledApp],
    resolve_handler: impl Fn(&str) -> Option<String>,
    fallback_program: &str,
) -> anyhow::Result<(usize, usize)> {
    let mut direct = 0usize;
    let mut fallback = 0usize;
    for url in urls {
        if let Some(handler) = resolve_handler(url)
            .as_deref()
            .and_then(|id| match_url_handler(installed, id))
        {
            if let Ok(child) = spawn_launch_child(&handler.launch_path, std::slice::from_ref(url)) {
                if quick_launch_failure(child, "desktop URL handler").is_none() {
                    direct += 1;
                    continue;
                }
            }
        }

        let child = std::process::Command::new(fallback_program)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        if let Some(reason) = quick_launch_failure(child, "xdg-open") {
            anyhow::bail!("could not open '{url}': {reason}");
        }
        fallback += 1;
    }
    Ok((direct, fallback))
}

#[cfg(test)]
mod launch_app_tests {
    use super::*;
    use crate::installed_apps::InstalledApp;
    use std::os::unix::fs::PermissionsExt;

    fn app(name: &str, bundle_id: &str, launch_path: &str) -> InstalledApp {
        InstalledApp {
            name: name.to_owned(),
            bundle_id: bundle_id.to_owned(),
            launch_path: launch_path.to_owned(),
            startup_wm_class: None,
            last_used: None,
        }
    }

    fn write_executable(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).expect("write launcher fixture");
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    fn fixture() -> Vec<InstalledApp> {
        vec![
            app("Galculator", "galculator", "galculator"),
            app(
                "Google Chrome",
                "google-chrome",
                "/usr/bin/google-chrome-stable",
            ),
            app("File Manager", "thunar", "thunar"),
            app(
                "File Manager Settings",
                "thunar-settings",
                "thunar-settings",
            ),
        ]
    }

    /// Process state letter from `/proc/<pid>/stat`, or `None` once the entry
    /// is gone. `comm` may itself contain spaces and parentheses, so the state
    /// is read after the final `)` rather than by splitting from the left.
    fn proc_state(pid: u32) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = stat.rsplit_once(')')?.1;
        after_comm.split_whitespace().next()?.chars().next()
    }

    #[test]
    fn launched_children_are_reaped_instead_of_lingering_as_zombies() {
        // A launched app that exits must not stay in the process table. The
        // driver never waits on what it launches, so without an explicit
        // reaper every launch leaked a pid slot for the daemon's lifetime and
        // left "does this pid exist" liveness checks reporting a terminated
        // app as still running.
        // `/bin/sh` rather than a richer coreutils binary: it is the one
        // executable POSIX and the Nix build sandbox both guarantee, and the
        // sandbox has no `/bin/true`.
        let pid = spawn_launch_command("/bin/sh -c exit", &[]).expect("/bin/sh should spawn");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match proc_state(pid) {
                None => break,                    // reaped, entry gone
                Some(state) if state != 'Z' => {} // still running or exiting
                Some(_) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "pid {pid} was still a zombie after the reaper deadline"
                    );
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "pid {pid} was never reaped"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    #[test]
    fn bounded_helper_output_does_not_wait_for_inherited_stdout_or_slow_processes() {
        let mut inherited = std::process::Command::new("/bin/sh");
        inherited
            .args(["-c", "printf 'google-chrome.desktop\\n'; sleep 1 &"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let started = std::time::Instant::now();
        let output = bounded_child_output(
            inherited.spawn().expect("spawn inherited-stdout fixture"),
            std::time::Duration::from_millis(500),
        )
        .expect("bounded output should retain the helper line");
        assert_eq!(output, b"google-chrome.desktop\n");
        assert!(started.elapsed() < std::time::Duration::from_millis(800));

        let mut slow = std::process::Command::new("/bin/sh");
        slow.args(["-c", "sleep 1"]).stdout(Stdio::piped());
        let started = std::time::Instant::now();
        assert!(bounded_child_output(
            slow.spawn().expect("spawn slow helper"),
            std::time::Duration::from_millis(50),
        )
        .is_none());
        assert!(started.elapsed() < std::time::Duration::from_millis(800));
    }

    #[test]
    fn url_launch_path_resolves_chromium_flags_and_falls_back_after_fast_failure() {
        let directory = std::env::temp_dir().join(format!(
            "cua-url-launch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let browser = directory.join("google-chrome-stable");
        let browser_args = directory.join("browser-args");
        write_executable(
            &browser,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                browser_args.display()
            ),
        );
        let fallback = directory.join("xdg-open-fixture");
        let fallback_args = directory.join("fallback-args");
        write_executable(
            &fallback,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                fallback_args.display()
            ),
        );
        let url = "https://example.invalid/".to_owned();
        let apps = vec![app(
            "Google Chrome",
            "google-chrome",
            browser.to_str().unwrap(),
        )];

        let (direct, fallback_count) = launch_urls_with_resolver(
            std::slice::from_ref(&url),
            &apps,
            |_| Some("google-chrome".to_owned()),
            fallback.to_str().unwrap(),
        )
        .expect("resolved URL handler launch");
        assert_eq!((direct, fallback_count), (1, 0));
        let arguments = std::fs::read_to_string(&browser_args).unwrap();
        assert!(arguments.lines().any(|argument| argument == url));
        assert!(arguments
            .lines()
            .any(|argument| argument == "--force-renderer-accessibility"));

        let failed = vec![app("Broken Browser", "broken-browser", "/bin/false")];
        let (direct, fallback_count) = launch_urls_with_resolver(
            std::slice::from_ref(&url),
            &failed,
            |_| Some("broken-browser".to_owned()),
            fallback.to_str().unwrap(),
        )
        .expect("fast handler failure should use the compatibility fallback");
        assert_eq!((direct, fallback_count), (0, 1));
        assert_eq!(std::fs::read_to_string(fallback_args).unwrap().trim(), url);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn launch_app_tool_urls_use_resolved_handler_and_fast_failure_fallback() {
        const CHILD_ENV: &str = "CUA_URL_LAUNCH_TOOL_TEST_CHILD";
        const HANDLER_ENV: &str = "CUA_URL_LAUNCH_TOOL_TEST_HANDLER";
        let url = "https://example.invalid/";
        if std::env::var_os(CHILD_ENV).is_some() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = runtime.block_on(LaunchAppTool.invoke(json!({"urls": [url]})));
            assert_ne!(result.is_error, Some(true), "tool result: {result:?}");
            return;
        }

        let directory = std::env::temp_dir().join(format!(
            "cua-url-tool-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bin_dir = directory.join("bin");
        let applications = directory.join("data/applications");
        let empty_data = directory.join("empty-data");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&applications).unwrap();
        std::fs::create_dir_all(&empty_data).unwrap();

        let browser_args = directory.join("browser-args");
        let browser = bin_dir.join("google-chrome-stable");
        write_executable(
            &browser,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                browser_args.display()
            ),
        );
        let fallback_args = directory.join("fallback-args");
        write_executable(
            &bin_dir.join("xdg-open"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                fallback_args.display()
            ),
        );
        write_executable(
            &bin_dir.join("xdg-mime"),
            &format!("#!/bin/sh\nprintf '%s.desktop\\n' \"${{{HANDLER_ENV}}}\"\n"),
        );
        std::fs::write(
            applications.join("google-chrome.desktop"),
            format!(
                "[Desktop Entry]\nType=Application\nName=Chrome URL Handler\nExec={} %U\nNoDisplay=true\n",
                browser.display()
            ),
        )
        .unwrap();
        std::fs::write(
            applications.join("broken-browser.desktop"),
            "[Desktop Entry]\nType=Application\nName=Broken URL Handler\nExec=/bin/false %U\nNoDisplay=true\n",
        )
        .unwrap();

        let path = format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let run_child = |handler: &str| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tools::impl_::launch_app_tests::launch_app_tool_urls_use_resolved_handler_and_fast_failure_fallback",
                    "--nocapture",
                ])
                .env(CHILD_ENV, "1")
                .env(HANDLER_ENV, handler)
                .env("PATH", &path)
                .env("XDG_DATA_HOME", directory.join("data"))
                .env("XDG_DATA_DIRS", &empty_data)
                .status()
                .expect("spawn tool-level URL launch test")
        };

        assert!(run_child("google-chrome").success());
        let arguments = std::fs::read_to_string(&browser_args).unwrap();
        assert!(arguments.lines().any(|argument| argument == url));
        assert!(arguments
            .lines()
            .any(|argument| argument == "--force-renderer-accessibility"));
        assert!(!fallback_args.exists());

        assert!(run_child("broken-browser").success());
        assert_eq!(std::fs::read_to_string(&fallback_args).unwrap().trim(), url);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn matches_exact_display_name_case_insensitively() {
        let apps = fixture();
        let hit = match_installed_app(&apps, "google chrome").expect("should match");
        assert_eq!(hit.bundle_id, "google-chrome");
    }

    #[test]
    fn matches_desktop_file_id_and_exec_basename() {
        let apps = fixture();
        assert_eq!(
            match_installed_app(&apps, "galculator").unwrap().name,
            "Galculator"
        );
        assert_eq!(
            match_installed_app(&apps, "google-chrome-stable")
                .unwrap()
                .name,
            "Google Chrome"
        );
    }

    #[test]
    fn matches_unambiguous_display_name_substring() {
        let apps = fixture();
        assert_eq!(
            match_installed_app(&apps, "chrome").unwrap().name,
            "Google Chrome"
        );
    }

    #[test]
    fn refuses_ambiguous_substring_and_unknown_names() {
        let apps = fixture();
        // "file manager" is a substring of two entries — refuse to guess.
        // ("File Manager" itself still resolves via the exact-name rung.)
        assert!(match_installed_app(&apps, "file man").is_none());
        assert_eq!(
            match_installed_app(&apps, "file manager")
                .unwrap()
                .bundle_id,
            "thunar"
        );
        assert!(match_installed_app(&apps, "gnome-calculator").is_none());
        assert!(match_installed_app(&apps, "").is_none());
    }
}

#[async_trait]
impl Tool for LaunchAppTool {
    fn def(&self) -> &ToolDef {
        LAUNCH_DEF.get_or_init(|| ToolDef {
            name: "launch_app".into(),
            description: "Launch a Linux app in the background. Provide launch_path (preferred — \
                round-trip the value from list_apps), name (tried as a direct command, then \
                matched against installed .desktop applications, then handed to xdg-open if it \
                is a URL or existing file path), bundle_id (ignored on Linux), or urls (list of \
                URLs to open). Resolution precedence: launch_path > name > bundle_id. Errors \
                when the name resolves to nothing launchable.".into(),
            input_schema: json!({"type":"object","properties":{
                "launch_path":{"type":"string","description":"Round-trip the `launch_path` returned by `list_apps` — the Exec= command from the .desktop file with XDG field codes already stripped. Highest precedence on Linux; spawned directly via the system shell."},
                "name":{"type":"string","description":"App name or command to launch. Tried as a direct command first, then matched against installed .desktop applications (exact display name, desktop-file id, or Exec basename; else an unambiguous display-name substring)."},
                "bundle_id":{"type":"string","description":"Ignored on Linux (macOS/Windows concept)."},
                "urls":{"type":"array","items":{"type":"string"},"description":"URLs to open through the resolved XDG desktop handler, with xdg-open as a compatibility fallback."},
                "additional_arguments":{"type":"array","items":{"type":"string"},"description":"Extra command-line arguments passed to the launched process."}
            },"additionalProperties":false}),
            read_only: false, destructive: false, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let launch_path_opt = args.opt_str("launch_path");
        let name_opt = args.opt_str("name");
        let urls: Vec<String> = args.str_array("urls");
        let additional_arguments: Vec<String> = args.str_array("additional_arguments");
        if args.get("cdp_debugging_port").is_some() {
            return ToolResult::error(
                "cdp_debugging_port moved to browser_prepare so DevTools is never enabled on an unproven user profile",
            );
        }
        if launch_path_opt
            .as_deref()
            .into_iter()
            .chain(name_opt.as_deref())
            .chain(additional_arguments.iter().map(String::as_str))
            .any(cua_driver_core::launch_guard::contains_remote_debugging_flag)
        {
            return ToolResult::error(
                cua_driver_core::launch_guard::REMOTE_DEBUGGING_LAUNCH_REFUSAL,
            );
        }

        if launch_path_opt.is_none() && name_opt.is_none() && urls.is_empty() {
            return ToolResult::error("Provide at least one of: launch_path, name, or urls.");
        }

        let windows_before: std::collections::HashSet<u64> =
            match cua_driver_core::blocking::spawn(|| crate::wayland::list_windows_dispatch(None))
                .await
            {
                Ok(windows) => windows.into_iter().map(|window| window.xid).collect(),
                Err(error) => {
                    return ToolResult::error(format!("Launch discovery task failed: {error}"))
                }
            };
        let result = cua_driver_core::blocking::spawn(
            move || -> anyhow::Result<(String, Option<u32>, String)> {
                // Resolve URL handlers to their desktop command first so a
                // Chromium-family browser can receive the per-process renderer
                // accessibility flag. Keep xdg-open as the compatibility
                // fallback when no handler command can be resolved safely.
                if !urls.is_empty() {
                    let installed = crate::installed_apps::list_url_handler_apps();
                    let (direct, fallback) = launch_urls_with_resolver(
                        &urls,
                        &installed,
                        default_url_handler_id,
                        "xdg-open",
                    )?;
                    let launch_name = match (direct, fallback) {
                        (_, 0) => "default URL handler",
                        (0, _) => "xdg-open",
                        _ => "default URL handler and xdg-open",
                    };
                    return Ok((
                        format!(
                            "Opened {} URL(s): {direct} via resolved desktop handlers, {fallback} via xdg-open.",
                            urls.len(),
                        ),
                        None,
                        launch_name.to_owned(),
                    ));
                }
                // launch_path > name. Both go through the same direct-exec path
                // (so XDG `Exec=` commands round-trip), but launch_path is the
                // canonical form preferred by list_apps callers.
                let command = launch_path_opt.as_deref().or(name_opt.as_deref());
                if let Some(cmd) = command {
                    match spawn_launch_command(cmd, &additional_arguments) {
                        Ok(pid) => {
                            return Ok((
                                format!("✅ Launched {cmd} (pid {pid}) in background."),
                                Some(pid),
                                cmd.to_owned(),
                            ));
                        }
                        Err(_) => {
                            // Not an executable on PATH. Resolve the name against
                            // installed XDG .desktop applications — the same
                            // source list_apps reads — and run the match's Exec=.
                            let installed = crate::installed_apps::list_installed_apps();
                            if let Some(app) = match_installed_app(&installed, cmd) {
                                let pid =
                                    spawn_launch_command(&app.launch_path, &additional_arguments)
                                        .map_err(|e| {
                                        anyhow::anyhow!(
                                            "'{cmd}' matched installed app '{}' but its launcher \
                                         `{}` failed to start: {e}",
                                            app.name,
                                            app.launch_path
                                        )
                                    })?;
                                return Ok((
                                    format!(
                                        "✅ Launched {} (`{}`, pid {pid}) in background — \
                                         resolved '{cmd}' via its desktop entry.",
                                        app.name, app.launch_path
                                    ),
                                    Some(pid),
                                    format!("{}\u{0}{cmd}\u{0}{}", app.name, app.launch_path),
                                ));
                            }
                            // xdg-open handles URLs and file paths, not app
                            // names — only fall through for something it can
                            // plausibly open, and surface its fast non-zero
                            // exit instead of reporting a launch that never
                            // happened.
                            if !cmd.contains("://") && !std::path::Path::new(cmd).exists() {
                                anyhow::bail!(
                                    "'{cmd}' is not an executable on PATH and matches no \
                                     installed .desktop application. Call list_apps and \
                                     round-trip its launch_path."
                                );
                            }
                            let child = std::process::Command::new("xdg-open")
                                .arg(cmd)
                                .stdin(Stdio::null())
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .spawn()?;
                            if let Some(reason) = quick_launch_failure(child, "xdg-open") {
                                anyhow::bail!("could not open '{cmd}': {reason}");
                            }
                            // xdg-open may spawn a helper and exit, so do not
                            // claim its pid is the app pid.
                            return Ok((
                                format!("Opened '{cmd}' via xdg-open."),
                                None,
                                cmd.to_owned(),
                            ));
                        }
                    }
                }
                unreachable!()
            },
        )
        .await;

        match result {
            Ok(Ok((message, pid_opt, name))) => {
                if let Some(launcher_pid) = pid_opt {
                    // Keep all desktop-entry match keys for discovery; publish
                    // only the display name in the result.
                    let query = name.clone();
                    let name = name.split('\u{0}').next().unwrap_or("").to_owned();
                    let resolved = match cua_driver_core::blocking::spawn(move || {
                        resolve_launched_windows(launcher_pid, &query, &windows_before)
                    })
                    .await
                    {
                        Ok(resolved) => resolved,
                        Err(error) => {
                            return ToolResult::error(format!(
                                "Launch resolution task failed: {error}"
                            ))
                        }
                    };
                    let windows: Vec<Value> =
                        resolved.windows.iter().map(window_record_json).collect();
                    let mut text = message;
                    let mut structured = json!({
                        "pid": resolved.pid,
                        "bundle_id": Value::Null,
                        "name": name,
                        "running": resolved.pid.is_some(),
                        "active": false,
                        "windows": windows,
                        "launcher_pid": launcher_pid,
                    });
                    if resolved.handed_off {
                        match resolved.pid {
                            Some(pid) => {
                                structured["handoff"] = json!("dbus_activation");
                                text.push_str(&format!(
                                    " The launcher handed the request to another process \
                                     (a wrapper or D-Bus activation); the window belongs to pid {pid}."
                                ));
                            }
                            None => {
                                // The launcher exited (D-Bus activation or a failed
                                // start) and the hand-off budget in
                                // `resolve_launched_windows` passed with no new
                                // window ever appearing. This is not a success: we
                                // have no pid, no window, and no way to know the app
                                // actually started. Report a typed refusal instead
                                // of a `pid: null` shape that reads as success.
                                return launch_handoff_timeout_result(
                                    &name,
                                    launcher_pid,
                                    LAUNCH_HANDOFF_BUDGET.as_secs(),
                                );
                            }
                        }
                    }
                    ToolResult::text(text).with_structured(structured)
                } else {
                    ToolResult::text(message).with_structured(json!({
                        "pid": Value::Null,
                        "bundle_id": Value::Null,
                        "name": name,
                        "running": Value::Null,
                        "active": false,
                        "windows": [],
                    }))
                }
            }
            Ok(Err(e)) => ToolResult::error(format!("Failed to launch: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// The `launch_app` D-Bus-activation hand-off timed out: the launcher exited
/// (handing the request to a running service) but no new window of the app
/// ever appeared within the deadline. This has no pid and no confirmed
/// window, so it is a refusal (`code: "launch_handoff_timeout"`), never a
/// `pid: null` shape that a caller could mistake for success.
fn launch_handoff_timeout_result(name: &str, launcher_pid: u32, waited_secs: u64) -> ToolResult {
    ToolResult::error(format!(
        "'{name}' was handed off for D-Bus activation but no new window appeared within \
         {waited_secs}s; the launch could not be confirmed. Call list_windows to check \
         whether it started anyway, or retry."
    ))
    .with_structured(json!({
        "code": "launch_handoff_timeout",
        "effect": "refused",
        "handoff": "dbus_activation",
        "name": name,
        "launcher_pid": launcher_pid,
        "pid": Value::Null,
        "running": Value::Null,
        "waited_secs": waited_secs,
    }))
}

/// What `launch_app` could attribute to the launch after the spawn.
#[derive(Default)]
struct LaunchedWindows {
    /// The process that owns the app's window: the launcher itself, or the
    /// wrapper target / running service it handed off to.
    pid: Option<u32>,
    windows: Vec<crate::x11::WindowInfo>,
    /// The window belongs to another process than the launcher, or the
    /// launcher exited without one.
    handed_off: bool,
}

/// Whether the launched process is gone or a zombie (already reaped or being
/// reaped by the launch thread).
fn process_exited(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state == "Z" || state == "X"),
        Err(_) => true,
    }
}

/// Case-insensitive identity match between a launch query
/// (`nautilus`, `org.gnome.Nautilus`, `gnome-control-center`, a display
/// name) and a window's WM_CLASS / app id.
fn window_matches_launch(window: &crate::x11::WindowInfo, query: &str) -> bool {
    query
        .split('\u{0}')
        .filter(|key| !key.trim().is_empty())
        .any(|key| window_matches_launch_key(window, key))
}

fn window_matches_launch_key(window: &crate::x11::WindowInfo, query: &str) -> bool {
    let class = window.app_name.to_ascii_lowercase();
    if class.is_empty() {
        return false;
    }
    let query = query.to_ascii_lowercase();
    let stem = query
        .rsplit('/')
        .next()
        .unwrap_or(&query)
        .trim_end_matches(".desktop")
        .to_owned();
    let last = stem.rsplit('.').next().unwrap_or(&stem).to_owned();
    let class_last = class.rsplit('.').next().unwrap_or(&class).to_owned();
    class == stem
        || class_last == last
        || class.contains(&last)
        || last.contains(&class_last)
        || stem
            .split_whitespace()
            .next()
            .is_some_and(|word| class.contains(word))
}

/// How long a launch may take to show a window while the launcher process is
/// still alive (the app is starting; LibreOffice on a small VM needs well over
/// ten seconds), and once it has exited (a wrapper or D-Bus activation has
/// taken over, so the window is either imminent or never coming).
const LAUNCH_STARTING_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
const LAUNCH_HANDOFF_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

/// Resolve the window(s) of a launch. Prefers the launcher's own window;
/// otherwise adopts a newly mapped top-level whose WM_CLASS matches the
/// launch, whoever owns it: a wrapper script that exec'd the real binary
/// (`libreoffice` → `soffice.bin`), or the running D-Bus service GNOME apps
/// hand their request to.
fn resolve_launched_windows(
    launcher_pid: u32,
    query: &str,
    before: &std::collections::HashSet<u64>,
) -> LaunchedWindows {
    let started = std::time::Instant::now();
    loop {
        let all = crate::wayland::list_windows_dispatch(None);
        let own: Vec<_> = all
            .iter()
            .filter(|w| w.pid == Some(launcher_pid))
            .cloned()
            .collect();
        if !own.is_empty() {
            return LaunchedWindows {
                pid: Some(launcher_pid),
                windows: own,
                handed_off: false,
            };
        }
        let fresh: Vec<_> = all
            .iter()
            .filter(|w| !before.contains(&w.xid) && w.pid.is_some())
            .filter(|w| window_matches_launch(w, query))
            .cloned()
            .collect();
        if let Some(pid) = fresh.first().and_then(|w| w.pid) {
            let windows = fresh.into_iter().filter(|w| w.pid == Some(pid)).collect();
            return LaunchedWindows {
                pid: Some(pid),
                windows,
                handed_off: true,
            };
        }
        let launcher_exited = process_exited(launcher_pid);
        let budget = if launcher_exited {
            LAUNCH_HANDOFF_BUDGET
        } else {
            LAUNCH_STARTING_BUDGET
        };
        if started.elapsed() >= budget {
            return LaunchedWindows {
                pid: (!launcher_exited).then_some(launcher_pid),
                windows: Vec::new(),
                handed_off: launcher_exited,
            };
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(test)]
mod launch_resolution_tests;

fn chromium_family_program(program: &str) -> bool {
    let basename = std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    let product = basename.strip_suffix(".appimage").unwrap_or(&basename);
    ["chrome", "chromium", "electron", "brave", "edge"]
        .iter()
        .any(|needle| basename.contains(needle))
        || [
            "code",
            "code-insiders",
            "codium",
            "vscodium",
            "slack",
            "discord",
            "discord-canary",
            "obsidian",
            "com.slack.slack",
            "com.discordapp.discord",
            "md.obsidian.obsidian",
            "com.visualstudio.code",
            "com.vscodium.codium",
        ]
        .iter()
        .any(|alias| {
            product == *alias
                || product.strip_prefix(alias).is_some_and(|suffix| {
                    suffix
                        .strip_prefix('-')
                        .and_then(|version| version.bytes().next())
                        .is_some_and(|first| first.is_ascii_digit())
                })
        })
}

// ── shared helpers ────────────────────────────────────────────────────────────

fn with_foreground_diagnostics(
    mut structured: serde_json::Value,
    outcome: Option<crate::wayland::shell_helper::ForegroundTerminalOutcome>,
) -> serde_json::Value {
    let Some(outcome) = outcome else {
        return structured;
    };
    structured["transport_prepared"] = json!(true);
    structured["activation_required"] = json!(outcome.activation_required);
    structured["target_activation_verified"] = json!(outcome.target_activation_verified);
    structured["restoration_attempted"] = json!(outcome.restoration_attempted);
    structured["prior_context_restored"] = json!(outcome.restoration_succeeded);
    structured["focus_preserved_for_explicit_action"] = json!(false);
    structured["foreground_terminal"] = json!(outcome.terminal && outcome.state == "terminal");
    structured["restoration_outcome"] = json!(outcome.outcome);
    structured["restoration_reason"] = json!(outcome.reason);
    structured
}

/// Resolve an AT-SPI element's center in window-local coordinates.
///
/// Returns `(xid, window_local_x, window_local_y)`.
/// Looks up element bounds via native AT-SPI, finds the owning window
/// (via `xid_hint` or the first window for `pid`), then converts screen-absolute
/// → window-local coords via compositor metadata on Wayland or
/// XTranslateCoordinates on X11.
fn resolve_element_local_coords(
    pid: u32,
    idx: usize,
    xid_hint: Option<u64>,
) -> anyhow::Result<(u64, f64, f64)> {
    let (bx, by, bw, bh) = if let Some(xid) = xid_hint {
        crate::atspi::get_element_bounds_for_window(pid, xid, idx)?
    } else {
        crate::atspi::get_element_bounds(pid, idx)?
    };
    let screen_cx = bx as f64 + bw as f64 / 2.0;
    let screen_cy = by as f64 + bh as f64 / 2.0;

    let xid = if let Some(x) = xid_hint {
        x
    } else if crate::wayland::wayland_input_enabled() {
        crate::wayland::list_windows_dispatch(Some(pid))
            .into_iter()
            .next()
            .map(|window| window.xid)
            .ok_or_else(|| anyhow::anyhow!("No Wayland windows for pid {pid}"))?
    } else {
        crate::x11::list_windows(Some(pid))
            .into_iter()
            .next()
            .map(|w| w.xid)
            .ok_or_else(|| anyhow::anyhow!("No windows for pid {pid}"))?
    };

    if crate::wayland::wayland_input_enabled() {
        let (window_x, window_y, window_width, window_height) =
            crate::wayland::window_geometry(xid)
                .ok_or_else(|| anyhow::anyhow!("No Wayland geometry for window {xid}"))?;
        if window_width == 0 || window_height == 0 {
            anyhow::bail!("Wayland window {xid} has no usable geometry");
        }
        return Ok((
            xid,
            screen_cx - window_x as f64,
            screen_cy - window_y as f64,
        ));
    }

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::rust_connection::RustConnection;
    let (conn, screen_num) = RustConnection::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let reply = conn
        .translate_coordinates(xid as u32, root, 0, 0)?
        .reply()?;
    let local_x = screen_cx - reply.dst_x as f64;
    let local_y = screen_cy - reply.dst_y as f64;
    Ok((xid, local_x, local_y))
}

fn element_screen_center(pid: u32, idx: usize, xid: Option<u64>) -> anyhow::Result<(f64, f64)> {
    element_screen_center_and_rect(pid, idx, xid).map(|(center, _)| center)
}

/// The element's screen centre plus its AT-SPI extents as the overlay's
/// `[x, y, width, height]`, both from one bounds lookup (same screen space).
fn element_screen_center_and_rect(
    pid: u32,
    idx: usize,
    xid: Option<u64>,
) -> anyhow::Result<((f64, f64), [f64; 4])> {
    let (bx, by, bw, bh) = match xid {
        Some(xid) => crate::atspi::get_element_bounds_for_window(pid, xid, idx)?,
        None => crate::atspi::get_element_bounds(pid, idx)?,
    };
    let rect = [bx as f64, by as f64, bw as f64, bh as f64];
    Ok(((rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0), rect))
}

/// A snapshot frame `(x, y, w, h)` as the overlay target rect, only when it
/// is non-empty and actually contains the glide point (a redirected press or
/// a frame in another space gets no rect rather than a wrong one).
fn frame_target_rect(bounds: Option<(i32, i32, u32, u32)>, sx: f64, sy: f64) -> Option<[f64; 4]> {
    let (x, y, w, h) = bounds?;
    let rect = [f64::from(x), f64::from(y), f64::from(w), f64::from(h)];
    (w > 0
        && h > 0
        && sx >= rect[0]
        && sy >= rect[1]
        && sx <= rect[0] + rect[2]
        && sy <= rect[1] + rect[3])
        .then_some(rect)
}

fn window_local_to_screen(xid: u64, x: f64, y: f64) -> anyhow::Result<(f64, f64)> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::rust_connection::RustConnection;

    let (conn, screen_num) = RustConnection::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let reply = conn
        .translate_coordinates(xid as u32, root, 0, 0)?
        .reply()?;
    Ok((reply.dst_x as f64 + x, reply.dst_y as f64 + y))
}

fn parse_mouse_button(name: &str) -> u8 {
    match name {
        "right" => 3,
        "middle" => 2,
        _ => 1,
    }
}

/// Escalation hint for a non-AX / suspected-no-op surface — the cross-platform
/// `escalation` field mirrored from macOS. The recommended next rung depends on
/// the session type: X11 can pixel-target a specific window in the background,
/// so the deliberate move is an element px action — click by pixel (x,y) off the
/// screenshot already in this response (`px`). Native Wayland CANNOT
/// background-target an unfocused window (libei injects to the compositor's input
/// focus — see `input::delivery`), and the pixel path itself routes through the
/// same coordinate-free AT-SPI actuation that just no-op'd, so the move there is
/// to bring the window to the foreground first (`foreground`).
fn non_ax_escalation() -> Value {
    if crate::wayland::is_wayland() {
        json!({
            "recommended": "foreground",
            "reason": "non-AX surface on Wayland: a specific unfocused window can't be \
                       pixel-targeted in the background (libei injects to the compositor's \
                       focus). bring_to_front, then click by pixel off the screenshot \
                       with delivery_mode:\"foreground\"."
        })
    } else {
        json!({
            "recommended": "px",
            "reason": "non-AX surface — act by pixel (x,y) off the screenshot \
                       in this response (an element px action)."
        })
    }
}

/// Structured payload for a `type_text` response. Keeps the legacy
/// `path`/`characters`/`verified` fields for back-compat and adds the cross-tool
/// `effect` tri-state. Linux's AT-SPI `insertText` return value acknowledges
/// the method call but does not read the widget value back, so it and every
/// keystroke / XSendEvent / XTest / Wayland rung are `"unverifiable"` (the
/// caller confirms through a separate observation) and
/// carries a `foreground` escalation, because the field IS in the AT-SPI tree —
/// it's a delivery/focus problem, not a missing element. The foreground rung
/// itself (`key_events_fg`) is already the last resort, so it emits no
/// escalation. Mirrors the macOS `type_text` contract.
fn type_text_structured(path: &str, characters: usize, verified: bool) -> Value {
    let mut s = json!({
        "path": path,
        "characters": characters,
        "verified": verified,
        "effect": if verified { "confirmed" } else { "unverifiable" },
    });
    if !verified && path != "key_events_fg" {
        s["escalation"] = json!({
            "recommended": "foreground",
            "reason": "background insert could not be confirmed — re-call with \
                       delivery_mode:\"foreground\" if a screenshot shows the text \
                       didn't land."
        });
    }
    s
}

/// Structured payload for the Electron/Chromium AX-echo case: the AT-SPI
/// `insertText` rung returned success on a Chromium embedder, but on those
/// surfaces the a11y bridge can accept and echo the write while the renderer
/// never observes it — so a path=="ax" "confirm" is a shim echo, not ground
/// truth. Mirrors the macOS `type_text` `ax_echo_surface` branch: report
/// effect:"unverifiable" + escalation:{recommended:"px"} (a renderer-focus
/// problem — pixel-focus the field, not foreground-activate it).
fn type_text_structured_electron(text_len: usize) -> Value {
    json!({
        "path": "ax",
        "characters": text_len,
        "verified": false,
        "effect": "unverifiable",
        "escalation": {
            "recommended": "px",
            "reason": "Electron/web surface — the AX write was echoed but the \
                       renderer may not have observed it. Confirm via the screenshot; \
                       if it didn't land, re-type with the element px action (pass x,y \
                       to pixel-focus the field, then type)."
        }
    })
}

/// Build the success `ToolResult` for an AT-SPI insert. The EditableText
/// method's boolean is a delivery acknowledgement, not a fresh value readback,
/// so native widgets remain `unverifiable`. Chromium embedders additionally
/// recommend the pixel rung because their accessibility bridge can acknowledge
/// a write the renderer never observes.
fn type_text_ax_result(pid: u32, text_len: usize, route: &str) -> ToolResult {
    if is_chromium_embedder(pid) {
        return ToolResult::text(format!(
            "📨 Sent (unverified) {text_len} character(s) ({route}). — Electron/web \
             surface: the AX layer accepts and echoes the write but the renderer may \
             not have observed it, so the driver cannot confirm via AX. Verify via the \
             screenshot; if it didn't land, re-type with the px form (pass x,y to \
             pixel-focus the field)."
        ))
        .with_structured(type_text_structured_electron(text_len));
    }
    ToolResult::text(format!("Typed {text_len} character(s) ({route})."))
        .with_structured(type_text_structured("ax", text_len, false))
}

/// True when `pid` is a Chromium-based embedder — a Chrome/Chromium browser or
/// any Electron/CEF app. On these surfaces an AT-SPI `EditableText.insertText`
/// can succeed at the bridge while the Chromium *renderer* never observes it,
/// so the AT-SPI "ax" rung must not be trusted as a confirmed insert (see
/// [`type_text_ax_result`]).
///
/// This is the Linux analogue of macOS `ElectronJs::is_electron` (which checks
/// for a bundled Electron Framework). Linux has no bundle, so the signal is
/// Chromium's multiprocess fingerprint: the embedder forks sandboxed helpers
/// whose argv carries `--type=renderer` / `--type=zygote` / `--type=gpu-process`.
/// Native GTK/Qt apps never spawn such helpers, so this is conservative (very
/// low false-positive). The single-process fallback also matches the embedder's
/// own argv. Reads `/proc`; cheap and only invoked on the rare AT-SPI confirm.
fn is_chromium_embedder(pid: u32) -> bool {
    fn argv_is_chromium_helper(p: u32) -> bool {
        match fs::read(format!("/proc/{p}/cmdline")) {
            Ok(raw) => String::from_utf8_lossy(&raw).split('\0').any(|arg| {
                arg == "--type=renderer" || arg == "--type=zygote" || arg == "--type=gpu-process"
            }),
            Err(_) => false,
        }
    }
    // Single-process / the embedder itself carrying a Chromium switch.
    if argv_is_chromium_helper(pid) {
        return true;
    }
    // Build PPid → children across /proc, then BFS the descendants of `pid`
    // looking for a Chromium helper. Same /proc-walk shape as
    // `terminal_descendant_ttys`.
    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    let entries = match fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let child: u32 = match name.to_string_lossy().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let status = match fs::read_to_string(format!("/proc/{child}/status")) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Some(ppid) = status
            .lines()
            .find(|l| l.starts_with("PPid:"))
            .and_then(|l| l[5..].trim().parse::<u32>().ok())
        {
            children.entry(ppid).or_default().push(child);
        }
    }
    let mut queue = std::collections::VecDeque::from([pid]);
    let mut seen = std::collections::HashSet::new();
    while let Some(cur) = queue.pop_front() {
        if !seen.insert(cur) {
            continue;
        }
        if let Some(kids) = children.get(&cur) {
            for &kid in kids {
                if argv_is_chromium_helper(kid) {
                    return true;
                }
                queue.push_back(kid);
            }
        }
    }
    false
}

fn is_webkitgtk_embedder(pid: u32) -> bool {
    fn argv_is_webkit_helper(pid: u32) -> bool {
        fs::read(format!("/proc/{pid}/cmdline"))
            .map(|raw| {
                let cmdline = String::from_utf8_lossy(&raw);
                cmdline.contains("WebKitWebProcess") || cmdline.contains("WebKitNetworkProcess")
            })
            .unwrap_or(false)
    }

    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(child) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(status) = fs::read_to_string(format!("/proc/{child}/status")) else {
            continue;
        };
        if let Some(ppid) = status
            .lines()
            .find(|line| line.starts_with("PPid:"))
            .and_then(|line| line[5..].trim().parse::<u32>().ok())
        {
            children.entry(ppid).or_default().push(child);
        }
    }
    let mut queue = std::collections::VecDeque::from([pid]);
    let mut seen = std::collections::HashSet::new();
    while let Some(current) = queue.pop_front() {
        if !seen.insert(current) {
            continue;
        }
        if argv_is_webkit_helper(current) {
            return true;
        }
        if let Some(descendants) = children.get(&current) {
            queue.extend(descendants.iter().copied());
        }
    }
    false
}

fn maps_indicate_gtk(maps: &str) -> bool {
    maps.contains("libgtk-3.so") || maps.contains("libgtk-4.so")
}

fn is_gtk_process(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/maps"))
        .map(|maps| maps_indicate_gtk(&maps))
        .unwrap_or(false)
}

fn unavailable_webkit_background(
    pid: u32,
    delivery: crate::input::delivery::DeliveryMode,
) -> Option<ToolResult> {
    (!delivery.is_foreground()
        && is_webkitgtk_embedder(pid)
        && !crate::wayland::is_inject_mode()
        && !crate::input::real_pointer_input_available())
    .then(|| {
        crate::input::delivery::background_unavailable_error(
            crate::input::delivery::BackgroundUnavailable::WebKitSyntheticInput,
        )
    })
}

fn unavailable_webkit_keyboard_background(
    pid: u32,
    delivery: crate::input::delivery::DeliveryMode,
) -> Option<ToolResult> {
    (!delivery.is_foreground() && is_webkitgtk_embedder(pid) && !crate::wayland::is_inject_mode())
        .then(|| {
            crate::input::delivery::background_unavailable_error(
                crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
            )
        })
}

fn unavailable_gtk_keyboard_background(
    pid: u32,
    delivery: crate::input::delivery::DeliveryMode,
) -> Option<ToolResult> {
    (!delivery.is_foreground() && is_gtk_process(pid) && !crate::wayland::is_inject_mode()).then(
        || {
            crate::input::delivery::background_unavailable_error(
                crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
            )
        },
    )
}

/// Refusal for a background element click that falls back to an X11
/// XSendEvent. Unlike [`unavailable_gtk_pointer_background`] and
/// [`unavailable_webkit_background`], it does not step aside when a real
/// pointer is available: that fallback never uses it.
fn synthetic_pointer_fallback_refusal(
    pid: u32,
    delivery: crate::input::delivery::DeliveryMode,
) -> Option<ToolResult> {
    if delivery.is_foreground() {
        return None;
    }
    synthetic_pointer_fallback_refusal_for(false, is_webkitgtk_embedder(pid), is_gtk_process(pid))
        .map(crate::input::delivery::background_unavailable_error)
}

fn synthetic_pointer_fallback_refusal_for(
    foreground: bool,
    webkit: bool,
    gtk: bool,
) -> Option<crate::input::delivery::BackgroundUnavailable> {
    use crate::input::delivery::BackgroundUnavailable;
    match (foreground, webkit, gtk) {
        (true, _, _) => None,
        (false, true, _) => Some(BackgroundUnavailable::WebKitSyntheticInput),
        (false, false, true) => Some(BackgroundUnavailable::FocusedInputOnly),
        (false, false, false) => None,
    }
}

fn unavailable_gtk_pointer_background(
    pid: u32,
    delivery: crate::input::delivery::DeliveryMode,
) -> Option<ToolResult> {
    (!delivery.is_foreground()
        && is_gtk_process(pid)
        && !crate::wayland::is_inject_mode()
        && !crate::input::real_pointer_input_available())
    .then(|| {
        crate::input::delivery::background_unavailable_error(
            crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
        )
    })
}

fn unavailable_wayland_focused_input_background(
    delivery: crate::input::delivery::DeliveryMode,
    focus_free_inject_supported: bool,
) -> Option<ToolResult> {
    (crate::wayland::wayland_input_enabled()
        && !(focus_free_inject_supported && crate::wayland::is_inject_mode())
        && !delivery.is_foreground())
    .then(|| {
        crate::input::delivery::background_unavailable_error(
            crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
        )
    })
}

/// Chromium's X11 renderer drops synthetic input sent to an occluded,
/// unfocused toplevel. Returning success here would be a silent loss, so all
/// input tools expose the same typed refusal and leave foreground activation
/// as the explicit escalation.
fn unavailable_chromium_background(
    pid: u32,
    delivery: crate::input::delivery::DeliveryMode,
) -> Option<ToolResult> {
    if chromium_background_must_refuse(
        delivery.is_foreground(),
        crate::wayland::is_inject_mode(),
        is_chromium_embedder(pid),
    ) {
        Some(crate::input::delivery::background_unavailable_error(
            crate::input::delivery::BackgroundUnavailable::ChromiumInput,
        ))
    } else {
        None
    }
}

fn chromium_background_must_refuse(
    foreground: bool,
    focus_free_inject_mode: bool,
    chromium: bool,
) -> bool {
    chromium && !foreground && !focus_free_inject_mode
}

/// Screen-absolute center of a window (top-left from translate_coordinates plus
/// half its geometry). Used to position the no-focus-steal scroll over the
/// window's content. Blocking — call inside spawn_blocking.
fn window_screen_center(xid: u64) -> anyhow::Result<(i32, i32)> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::rust_connection::RustConnection;

    let (conn, screen_num) = RustConnection::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let geom = conn.get_geometry(xid as u32)?.reply()?;
    let trans = conn
        .translate_coordinates(xid as u32, root, 0, 0)?
        .reply()?;
    Ok((
        trans.dst_x as i32 + geom.width as i32 / 2,
        trans.dst_y as i32 + geom.height as i32 / 2,
    ))
}

/// X11 no-focus-steal pixel click with graceful fallback. On a real Xorg host
/// the MPX uinput pointer + XI2 shield grab lands a *true* button event on
/// XInput2 toolkits (GTK3/4) that silently drop synthetic `XSendEvent` pointers
/// — so right / middle / double clicks actually register. On Xvfb / Xtigervnc /
/// unsupported servers (`real_pointer_input_available()` returns false) or if
/// the MPX attempt fails, it falls back to the legacy `XSendEvent` path so
/// headless tests and core-only toolkits keep working. `lx`,`ly` are
/// window-local; screen-absolute coords for the warp are derived here. Blocking
/// — call inside spawn_blocking.
fn x11_pixel_click_no_focus_steal(
    cursor_id: &str,
    xid: u64,
    lx: i32,
    ly: i32,
    button: u8,
    count: usize,
) -> anyhow::Result<()> {
    if crate::input::real_pointer_input_available() {
        if let Ok((sx, sy)) = window_local_to_screen(xid, lx as f64, ly as f64) {
            match crate::input::send_virtual_pointer_click(
                cursor_id,
                &crate::input::VirtualPointerClick {
                    target_window: xid,
                    x: sx.round() as i32,
                    y: sy.round() as i32,
                    button,
                    count,
                },
            ) {
                Ok(_effect) => return Ok(()),
                Err(error) if crate::input::is_uinput_unavailable(&error) => return Err(error),
                Err(e) => tracing::warn!("MPX click fell back to XSendEvent: {e}"),
            }
        }
    }
    crate::input::send_click(xid, lx, ly, count, button)
}

fn linux_input_error(error: anyhow::Error) -> ToolResult {
    if crate::input::is_uinput_unavailable(&error) {
        ToolResult::error(error.to_string()).with_structured(json!({
            "code": crate::input::UINPUT_UNAVAILABLE_CODE,
        }))
    } else {
        ToolResult::error(error.to_string())
    }
}

fn capture_admission_error(error: anyhow::Error) -> ToolResult {
    let code = cua_driver_core::capture_runtime::admission_error_code(&error);
    ToolResult::error(format!("capture-bound click refused: {error}"))
        .with_structured(json!({ "code": code, "effect": "refused" }))
}

fn isolated_hyprland_background(delivery: crate::input::delivery::DeliveryMode) -> bool {
    !delivery.is_foreground() && crate::wayland::hyprland_input::enabled()
}

fn hyprland_foreground(delivery: crate::input::delivery::DeliveryMode) -> bool {
    delivery.is_foreground()
        && crate::wayland::wayland_input_enabled()
        && crate::wayland::hyprland::is_session()
}

fn element_click_prefers_ax(
    hyprland_foreground: bool,
    button: u8,
    count: usize,
    has_modifiers: bool,
) -> bool {
    !has_modifiers && (!hyprland_foreground || (button == 1 && count == 1))
}

fn foreground_ax_click_unknown(pid: u32, idx: usize) -> ToolResult {
    use cua_driver_core::action_record::{
        ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
    };
    let record = ActionExecutionRecord::builder(
        ActionEffect::Unverifiable,
        ActionTransport::LinuxAtSpiAction,
        RequestedDelivery::Foreground,
    )
    .actual_delivery(ActualDelivery::Unknown)
    .build()
    .expect("uncertain semantic activation has no delivered count or replay");
    let public = serde_json::to_value(record.public_result().unwrap()).unwrap();
    ToolResult::error(format!(
        "click: AT-SPI activation outcome is unknown for element [{idx}] (pid {pid}); refresh state before another action."
    ))
    .with_structured(public)
    .with_action_record(record)
}

#[cfg(test)]
#[test]
fn foreground_ax_click_error_does_not_claim_delivery_or_request_replay() {
    let result = foreground_ax_click_unknown(123, 4);
    assert_eq!(result.is_error, Some(true));
    let wire = serde_json::to_value(&result).unwrap();
    assert_eq!(wire["isError"], true);
    let public = &wire["structuredContent"];
    assert_eq!(
        *public,
        serde_json::to_value(result.action_record.unwrap().public_result().unwrap()).unwrap()
    );
    assert_eq!(public["effect"], "unverifiable");
    assert_eq!(public["route"], "accessibility");
    assert_eq!(public["delivery"]["mode"], "unknown");
    assert!(public.get("escalation").is_none());
    assert!(public["delivery"].get("delivered_count").is_none());
}

#[cfg(test)]
#[test]
fn hyprland_foreground_semantic_click_preserves_pointer_gestures() {
    assert!(element_click_prefers_ax(true, 1, 1, false));
    for (button, count) in [(2, 1), (3, 1), (1, 0), (1, 2), (1, 3), (2, 2), (3, 3)] {
        assert!(!element_click_prefers_ax(true, button, count, false));
    }
    // Preserve the existing AT-SPI eligibility for all other delivery routes.
    for button in [1, 2, 3] {
        for count in [0, 1, 2, 3] {
            assert!(element_click_prefers_ax(false, button, count, false));
            assert!(!element_click_prefers_ax(false, button, count, true));
            assert!(!element_click_prefers_ax(true, button, count, true));
        }
    }
}

fn isolated_hyprland_refusal(detail: impl Into<String>) -> ToolResult {
    let detail = detail.into();
    ToolResult::error(format!("background_unavailable: {detail}")).with_structured(json!({
        "ok": false, "code": "background_unavailable", "reason": "unsupported_operation",
        "detail": detail, "route": "synthetic_events", "verified": false, "effect": "refused"
    }))
}

fn isolated_hyprland_result(result: anyhow::Result<Value>) -> ToolResult {
    hyprland_input_result(result, false)
}

/// Background text dispatches one key action per character, so a refusal
/// after N acknowledged keys is a partial with count N. Single actions keep
/// the one-phase bound in [`hyprland_input_outcome`].
fn isolated_hyprland_text_result(result: anyhow::Result<Value>) -> ToolResult {
    hyprland_input_outcome(result, false, true)
}

fn hyprland_input_result(result: anyhow::Result<Value>, foreground: bool) -> ToolResult {
    hyprland_input_outcome(result, foreground, false)
}

/// `sequence` marks a multi-action dispatch (background text), whose partial
/// progress may exceed one acknowledged phase.
fn hyprland_input_outcome(
    result: anyhow::Result<Value>,
    foreground: bool,
    sequence: bool,
) -> ToolResult {
    use cua_driver_core::action_record::{
        ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
    };
    let record = |effect| {
        ActionExecutionRecord::builder(
            effect,
            if foreground {
                ActionTransport::LinuxHyprlandForegroundInput
            } else {
                ActionTransport::LinuxHyprlandIsolatedInput
            },
            if foreground {
                RequestedDelivery::Foreground
            } else {
                RequestedDelivery::Background
            },
        )
    };
    let actual = if foreground {
        ActualDelivery::Foreground
    } else {
        ActualDelivery::Background
    };
    let unavailable = if foreground {
        "foreground_unavailable"
    } else {
        "background_unavailable"
    };
    match result {
        Ok(value) if value["ok"] == true => ToolResult::text(
            "Dispatched exact-target Hyprland input; application effect is unverifiable.",
        )
        .with_action_record(
            record(ActionEffect::Unverifiable)
                .actual_delivery(actual)
                .build()
                .expect("isolated input record is valid"),
        ),
        Ok(mut value) => {
            if foreground && value["code"] == "foreground_partial_unknown" {
                return hyprland_input_outcome(
                    Err(crate::wayland::hyprland_input::unknown_dispatch(
                        anyhow::anyhow!(
                            "{}",
                            value["detail"]
                                .as_str()
                                .unwrap_or("foreground acquisition may have changed focus")
                        ),
                        value["delivery"]["delivered_count"]
                            .as_u64()
                            .and_then(|count| u32::try_from(count).ok())
                            .unwrap_or(0),
                    )),
                    true,
                    sequence,
                );
            }
            let code = value["code"]
                .as_str()
                .unwrap_or("protocol_error")
                .to_owned();
            let detail = value["detail"]
                .as_str()
                .unwrap_or("input refused")
                .to_owned();
            value["reason"] = json!(code);
            value["code"] = json!(unavailable);
            value["route"] = json!(if foreground {
                "global_input"
            } else {
                "synthetic_events"
            });
            value["verified"] = json!(false);
            let delivered = value["delivery"]["delivered_count"]
                .as_u64()
                .and_then(|count| u32::try_from(count).ok())
                .filter(|count| *count > 0);
            let outcome = if value["effect"] == "partial"
                && delivered.is_some_and(|count| foreground || sequence || count == 1)
            {
                record(ActionEffect::Partial)
                    .actual_delivery(actual)
                    .delivered_count(delivered.unwrap())
            } else {
                value["effect"] = json!("refused");
                value
                    .as_object_mut()
                    .expect("protocol reply is an object")
                    .remove("delivery");
                record(ActionEffect::Refused)
            }
            .detail(&detail)
            .build()
            .expect("isolated input outcome is valid");
            ToolResult::error(format!("{unavailable} ({code}): {detail}"))
                .with_structured(value)
                .with_action_record(outcome)
        }
        Err(error) if error.is::<crate::wayland::hyprland_input::LaneBusy>() => {
            hyprland_input_outcome(
                Ok(json!({
                    "ok": false, "code": "lane_busy", "detail": error.to_string()
                })),
                foreground,
                sequence,
            )
        }
        Err(error) if error.is::<crate::wayland::hyprland_input::ActionCancelled>() => {
            hyprland_input_outcome(
                Ok(json!({
                    "ok": false, "code": "cancelled", "detail": error.to_string()
                })),
                foreground,
                sequence,
            )
        }
        Err(error) => {
            let count = error
                .downcast_ref::<crate::wayland::hyprland_input::DispatchUnknown>()
                .map(|error| error.acknowledged_phases)
                .filter(|count| *count > 0);
            let effect = if count.is_some() {
                ActionEffect::Partial
            } else {
                ActionEffect::Unverifiable
            };
            let mut outcome = record(effect)
                .actual_delivery(ActualDelivery::Unknown)
                .detail(error.to_string());
            if let Some(count) = count {
                outcome = outcome.delivered_count(count);
            }
            let outcome = outcome
                .build()
                .expect("unknown delivery preserves acknowledged progress");
            let mut value = serde_json::to_value(outcome.public_result().unwrap()).unwrap();
            value["ok"] = json!(false);
            value["code"] = json!(unavailable);
            value["reason"] = json!("transport_or_protocol_error");
            value["detail"] = json!(error.to_string());
            ToolResult::error(format!("{unavailable}: {error}"))
                .with_structured(value)
                .with_action_record(outcome)
        }
    }
}

fn isolated_hyprland_task_error(error: tokio::task::JoinError, acknowledged: bool) -> ToolResult {
    isolated_hyprland_result(Err(crate::wayland::hyprland_input::unknown_dispatch(
        error.into(),
        u32::from(acknowledged),
    )))
}

fn spawn_isolated_hyprland(
    args: &Value,
    work: impl FnOnce(crate::wayland::hyprland_input::ActionCancellation) -> anyhow::Result<Value>
        + Send
        + 'static,
) -> Result<
    (
        crate::wayland::hyprland_input::CancelOnDrop,
        tokio::task::JoinHandle<anyhow::Result<Value>>,
    ),
    ToolResult,
> {
    use cua_driver_core::session;
    let owner = named_session_cursor_key(args)
        .ok_or_else(|| isolated_hyprland_refusal("authenticated lifecycle required"))?;
    let transport_owner = args
        .get("_transport_session_id")
        .and_then(Value::as_str)
        .unwrap_or(&owner);
    let snapshot = session::session_snapshot(&owner, transport_owner, std::time::Duration::ZERO)
        .ok_or_else(|| isolated_hyprland_refusal("admitted lifecycle required"))?;
    // Retain admission before spawning: aborting the async caller must not
    // complete session teardown while its native worker still owns input.
    let lifecycle = session::begin_session_dispatch(
        &owner,
        snapshot.public_label.as_deref(),
        transport_owner,
        snapshot.implicit,
        snapshot.transport,
        snapshot.client_kind,
    )
    .map_err(isolated_hyprland_refusal)?;
    let (guard, cancellation) = crate::wayland::hyprland_input::ActionCancellation::invocation();
    let dispatch = cua_driver_core::blocking::spawn(move || {
        let _lifecycle = lifecycle;
        work(cancellation)
    });
    Ok((guard, dispatch))
}

async fn isolated_hyprland_action(
    args: &Value,
    pid: u32,
    xid: u64,
    action: crate::wayland::hyprland_input::Action,
) -> ToolResult {
    let owner = named_session_cursor_key(args);
    let (_cancellation, dispatch) = match spawn_isolated_hyprland(args, move |cancellation| {
        crate::wayland::hyprland_input::execute(owner, pid, xid, action, cancellation)
    }) {
        Ok(dispatch) => dispatch,
        Err(refusal) => return refusal,
    };
    match dispatch.await {
        Ok(result) => isolated_hyprland_result(result),
        Err(error) => isolated_hyprland_task_error(error, false),
    }
}

fn foreground_hyprland_refusal(detail: impl Into<String>) -> ToolResult {
    hyprland_input_result(
        Ok(json!({
            "ok": false, "code": "unsupported_operation", "detail": detail.into()
        })),
        true,
    )
}

async fn foreground_hyprland_action(
    args: &Value,
    pid: u32,
    xid: u64,
    action: crate::wayland::hyprland_input::Action,
) -> ToolResult {
    if !crate::wayland::hyprland_input::enabled() {
        return foreground_hyprland_refusal("production Hyprland input plugin is unavailable");
    }
    let owner = named_session_cursor_key(args);
    let (_cancellation, dispatch) = match spawn_isolated_hyprland(args, move |cancellation| {
        crate::wayland::hyprland_input::execute_foreground(owner, pid, xid, action, cancellation)
    }) {
        Ok(dispatch) => dispatch,
        Err(_) => return foreground_hyprland_refusal("authenticated admitted lifecycle required"),
    };
    hyprland_input_result(
        match dispatch.await {
            Ok(result) => result,
            Err(error) => Err(crate::wayland::hyprland_input::unknown_dispatch(
                error.into(),
                0,
            )),
        },
        true,
    )
}

async fn foreground_hyprland_scroll(
    args: &Value,
    pid: u32,
    xid: u64,
    actions: Vec<crate::wayland::hyprland_input::Action>,
) -> ToolResult {
    if !crate::wayland::hyprland_input::enabled() {
        return foreground_hyprland_refusal("production Hyprland input plugin is unavailable");
    }
    let owner = named_session_cursor_key(args);
    let (_cancellation, dispatch) = match spawn_isolated_hyprland(args, move |cancellation| {
        crate::wayland::hyprland_input::execute_foreground_scroll(
            owner,
            pid,
            xid,
            actions,
            cancellation,
        )
    }) {
        Ok(dispatch) => dispatch,
        Err(_) => return foreground_hyprland_refusal("authenticated admitted lifecycle required"),
    };
    hyprland_input_result(
        match dispatch.await {
            Ok(result) => result,
            Err(error) => Err(crate::wayland::hyprland_input::unknown_dispatch(
                error.into(),
                0,
            )),
        },
        true,
    )
}

fn foreground_hyprland_key(
    key: String,
    mut modifiers: Vec<String>,
) -> Result<crate::wayland::hyprland_input::Action, ToolResult> {
    let parts: Vec<&str> = key.split('+').collect();
    let key = if parts.len() > 1 && key != "+" {
        if parts[..parts.len() - 1]
            .iter()
            .any(|part| !is_modifier(part))
            || parts.last() == Some(&"")
        {
            return Err(foreground_hyprland_refusal("invalid modified key chord"));
        }
        modifiers.extend(
            parts[..parts.len() - 1]
                .iter()
                .map(|part| part.to_lowercase()),
        );
        parts.last().unwrap().to_string()
    } else {
        key
    };
    Ok(crate::wayland::hyprland_input::Action::Key { key, modifiers })
}

async fn focus_hyprland_foreground(
    state: &Arc<ToolState>,
    args: &Value,
    pid: u32,
    xid: u64,
    pixel: Option<(f64, f64)>,
) -> Result<(), ToolResult> {
    if !crate::wayland::hyprland_input::enabled() {
        return Err(foreground_hyprland_refusal(
            "production Hyprland input plugin is unavailable",
        ));
    }
    if let Some((x, y)) = pixel {
        return focus_by_pixel(
            state,
            pid,
            Some(xid),
            (x, y),
            true,
            args,
            args.bool_or("from_zoom", false),
        )
        .await;
    }
    Ok(())
}

#[cfg(test)]
#[tokio::test]
async fn isolated_worker_retains_lifecycle_until_native_work_exits() {
    use cua_driver_core::session::{self, SessionClientKind, SessionTransport};
    let sid = "isolated-worker-lifecycle-test";
    let owner = "isolated-worker-transport-test";
    let caller = session::begin_session_dispatch(
        sid,
        None,
        owner,
        true,
        SessionTransport::McpStdio,
        SessionClientKind::Mcp,
    )
    .unwrap();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (cancellation, dispatch) = spawn_isolated_hyprland(
        &json!({"_session_id":sid,"_transport_session_id":owner}),
        move |_| {
            started.send(()).unwrap();
            released.recv().unwrap();
            Ok(json!({"ok":true}))
        },
    )
    .unwrap();
    ready.await.unwrap();
    drop(cancellation);
    drop(caller);
    assert!(session::end_session_for_owner(sid, owner));
    assert!(session::is_session_ending(sid));
    assert!(!session::is_session_ended(sid));
    release.send(()).unwrap();
    dispatch.await.unwrap().unwrap();
    assert!(session::is_session_ended(sid));
}

#[cfg(test)]
#[test]
fn isolated_background_routes_do_not_reprobe_availability_before_primary_fallback() {
    // Structural regression for the availability-loss race: each tool keeps
    // the decision that bypassed background refusal until its returning
    // native branch. A second probe here could select primary-seat input.
    let source = include_str!("impl_.rs");
    for (start, end) in [
        ("impl Tool for ClickTool {", "impl Tool for TypeTextTool {"),
        (
            "impl Tool for ScrollTool {",
            "impl Tool for DoubleClickTool {",
        ),
        (
            "impl Tool for DragTool {",
            "impl Tool for MouseButtonDownTool {",
        ),
    ] {
        let body = source
            .rsplit_once(start)
            .unwrap()
            .1
            .split_once(end)
            .unwrap()
            .0;
        let invoke = body.split_once("async fn invoke").unwrap().1;
        assert_eq!(
            invoke
                .matches("isolated_hyprland_background(delivery)")
                .count(),
            1,
            "{start}"
        );
        let native = invoke.split_once("if isolated_background {").unwrap().1;
        // Upstream returns its own dispatch here; the fork's click refuses the
        // element-path pointer fallback and returns through
        // `isolated_hyprland_action`, which awaits the same single decision.
        assert!(
            native.contains("return match dispatch.await")
                || native.contains("return isolated_hyprland_action("),
            "{start}"
        );
    }
}

#[cfg(test)]
#[test]
fn indexed_x11_pointer_fallback_refuses_synthetic_dropping_toolkits() {
    use crate::input::delivery::BackgroundUnavailable;
    // The decision has no real-pointer input: the fallback it guards always
    // sends XSendEvent, so host pointer capability must not admit GTK/WebKit.
    for foreground in [false, true] {
        for webkit in [false, true] {
            for gtk in [false, true] {
                let refusal = synthetic_pointer_fallback_refusal_for(foreground, webkit, gtk);
                let expected = match (foreground, webkit, gtk) {
                    (true, _, _) => None,
                    (false, true, _) => Some(BackgroundUnavailable::WebKitSyntheticInput),
                    (false, false, true) => Some(BackgroundUnavailable::FocusedInputOnly),
                    (false, false, false) => None,
                };
                assert_eq!(
                    refusal.map(|r| format!("{r:?}")),
                    expected.map(|r| format!("{r:?}")),
                    "foreground={foreground} webkit={webkit} gtk={gtk}"
                );
            }
        }
    }
    // Needles are split with concat! so this test's own text never matches.
    let source = include_str!("impl_.rs");
    let one = |needle: &str| {
        assert_eq!(source.matches(needle).count(), 1, "{needle}");
        source.split_once(needle).unwrap().1
    };
    let indexed = one(concat!("async fn click_", "indexed_x11("))
        .split_once("\n    }\n")
        .unwrap()
        .0;
    let refusal = indexed
        .find(concat!(
            "synthetic_pointer_fallback_",
            "refusal(pid, delivery)"
        ))
        .expect("indexed X11 fallback must check synthetic-pointer toolkits");
    let synthetic = indexed
        .find(concat!("send_click_with_", "modifiers("))
        .expect("indexed X11 background fallback");
    assert!(refusal < synthetic, "refuse before any synthetic dispatch");
    let helper = one(concat!("fn synthetic_pointer_", "fallback_refusal("))
        .split_once("\n}\n")
        .unwrap()
        .0;
    assert!(!helper.contains(concat!("real_pointer_", "input_available")));
}

#[cfg(test)]
#[test]
fn right_click_refuses_synthetic_pointer_targets_before_element_delegation() {
    // e161b001e put the element delegation to ClickTool above these refusals,
    // so a background right click on a GTK or WebKit element was sent as an
    // XSendEvent the toolkit drops, and reported as dispatched.
    let source = include_str!("impl_.rs");
    let invoke = source
        .rsplit_once("impl Tool for RightClickTool {")
        .unwrap()
        .1
        .split_once("impl Tool for DragTool {")
        .unwrap()
        .0
        .split_once("async fn invoke")
        .unwrap()
        .1;
    let delegation = invoke
        .find("args.get(\"element_token\").is_some()")
        .expect("right_click must route element targets through ClickTool");
    for refusal in [
        "unavailable_chromium_background(pid, delivery)",
        "unavailable_webkit_background(pid, delivery)",
        "unavailable_gtk_pointer_background(pid, delivery)",
        "unavailable_wayland_focused_input_background(delivery, true)",
    ] {
        let at = invoke.find(refusal).expect(refusal);
        assert!(
            at < delegation,
            "{refusal} must run before element delegation"
        );
    }
}

#[cfg(test)]
#[test]
fn keyboard_element_routes_never_rewalk_ordinals() {
    // Split item/section anchors so this test cannot terminate another source
    // guard's section before it reaches the production implementation.
    let source = include_str!("impl_.rs");
    let forbidden = [
        concat!("focus_", "element(pid"),
        concat!("type_into_editable", "_at("),
        concat!("get_element_", "bounds(pid"),
        concat!("get_element_bounds_", "for_window("),
        concat!("resolve_element_", "local_coords("),
    ];
    for (start, end) in [
        (
            concat!("impl ", "Tool for TypeTextTool {"),
            concat!("impl ", "Tool for PressKeyTool {"),
        ),
        (
            concat!("impl ", "Tool for PressKeyTool {"),
            concat!("impl ", "Tool for HotkeyTool {"),
        ),
        (
            concat!("impl ", "Tool for HotkeyTool {"),
            concat!("impl ", "Tool for SetValueTool {"),
        ),
    ] {
        let invoke = source
            .rsplit_once(start)
            .unwrap()
            .1
            .split_once(end)
            .unwrap()
            .0
            .split_once(concat!("async fn ", "invoke"))
            .unwrap()
            .1;
        for ordinal in forbidden {
            assert!(
                !invoke.contains(ordinal),
                "{start} contains ordinal route {ordinal}"
            );
        }
        let retained = invoke
            .find("return invoke_observed_keyboard(")
            .expect("return through retained route");
        assert!(invoke.contains("snapshot_identity.expect(\"element has identity\")"));
        assert!(
            invoke
                .find("unavailable_chromium_background(pid, delivery)")
                .unwrap()
                < retained
        );
        if start != concat!("impl ", "Tool for TypeTextTool {") {
            for refusal in [
                "unavailable_webkit_keyboard_background",
                "unavailable_gtk_keyboard_background",
                "unavailable_wayland_focused_input_background",
            ] {
                assert!(
                    invoke.find(refusal).unwrap() < retained,
                    "{start}: {refusal} precedes mutation"
                );
            }
        } else {
            assert!(
                invoke
                    .find("unavailable_webkit_keyboard_background")
                    .unwrap()
                    < retained
            );
        }
        assert!(retained < invoke.find("if hyprland_foreground(delivery)").unwrap());
        // Overlay/legacy pixel helpers may not receive snapshot indices even
        // if someone later accidentally moves them above the early return.
        let compact: String = invoke.split_whitespace().collect();
        assert!(!compact.contains("xid,resolved_elem"));
    }
    for (start, end) in [
        (
            concat!("impl ", "ObservedKeyboardAction {"),
            concat!("async fn ", "focus_by_pixel("),
        ),
        (
            concat!("async fn ", "focus_hyprland_foreground("),
            concat!("#[cfg(", "test)]"),
        ),
        (
            concat!("async fn ", "focus_nested_inject_target("),
            concat!("// ── ", "type_text"),
        ),
    ] {
        let helper = source
            .rsplit_once(start)
            .unwrap()
            .1
            .split_once(end)
            .unwrap()
            .0;
        for ordinal in forbidden {
            assert!(
                !helper.contains(ordinal),
                "{start} contains ordinal route {ordinal}"
            );
        }
    }
    let helper = source
        .rsplit_once(concat!("async fn ", "invoke_observed_keyboard("))
        .unwrap()
        .1
        .split_once(concat!("async fn ", "focus_by_pixel("))
        .unwrap()
        .0;
    assert!(helper.contains("acquire_observed_mutation(snapshot_identity, index)"));
    assert!(helper.contains("resolve_observed_target(pid, index, xid, &identity, proof)"));
    assert!(helper.contains("Some(crate::wayland::establish_exact_target(pid, xid)?)"));
    assert!(helper.contains("Ok(Some((Arc::new(permit), target)))"));
    assert!(helper.contains("let _permit = permit;"));
    assert!(helper.contains("if activation[\"ok\"] != true"));
    assert!(helper.contains("return Ok(activation);"));
    assert!(helper.contains("let _mutation = &permit;"));
    assert!(helper.contains("with_x11_foreground_permit("));
    assert!(helper.contains("Some(permit.clone())"));
    assert!(helper.contains("target.type_into_editable(text)?"));
    assert!(!helper.contains("unwrap_or(false)"));
    for call in [
        "action.send_nested(xid)?",
        "action.send_wayland(validate)",
        "action.send_x11(xid, true)",
        "action.send_x11(xid, false)?",
    ] {
        let before = helper.split_once(call).unwrap().0;
        assert!(
            before.trim_end().ends_with("target.verify_live()?;"),
            "{call} revalidates before injection"
        );
        assert!(before.rsplit_once("target.focus()?;").is_some());
    }
}

#[cfg(test)]
#[test]
fn keyboard_retained_errors_preserve_stale_refusal() {
    let stale = observed_keyboard_error(
        anyhow::anyhow!("stale_element_token: object disappeared")
            .context("observed editable refresh"),
    );
    assert_eq!(
        stale.structured_content.unwrap()["refusal"]["code"],
        "stale_element_token"
    );
    let indeterminate =
        observed_keyboard_error(anyhow::anyhow!("write timed out; refusing replay"));
    assert_eq!(indeterminate.is_error, Some(true));
    assert!(indeterminate.structured_content.is_none());
}

#[cfg(test)]
#[test]
fn type_text_routes_isolated_background_before_generic_wayland_refusal() {
    let source = include_str!("impl_.rs");
    let invoke = source
        .rsplit_once("impl Tool for TypeTextTool {")
        .unwrap()
        .1
        .split_once("impl Tool for PressKeyTool {")
        .unwrap()
        .0
        .split_once("async fn invoke")
        .unwrap()
        .1;
    let isolated = invoke
        .find("isolated_hyprland_background(delivery)")
        .expect("type_text must select isolated Hyprland background delivery");
    let generic = invoke
        .find("unavailable_wayland_focused_input_background")
        .expect("type_text must retain the generic Wayland refusal");
    assert!(
        isolated < generic,
        "the plugin route must run before the generic focused-input refusal"
    );
    assert!(invoke.contains("execute_background_text"));
    // Multi-key progress must reach the caller (see
    // isolated_text_reports_every_acknowledged_key_before_a_refusal).
    assert!(invoke.contains("Ok(result) => isolated_hyprland_text_result(result)"));
    assert!(!invoke.contains("Ok(result) => isolated_hyprland_result(result)"));
}

#[cfg(test)]
#[test]
fn coordinate_click_keeps_wayland_background_refusal_ahead_of_screenshot_context() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let resolved = AtomicBool::new(false);
    let native_refusal = crate::input::delivery::background_unavailable_error(
        crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
    );
    let result = coordinate_click_context(Some(native_refusal), || {
        resolved.store(true, Ordering::SeqCst);
        Ok(CoordinateContext::Screenshot(7.35))
    })
    .expect_err("unsupported Wayland background delivery must refuse");

    assert_eq!(
        result.structured_content.as_ref().unwrap()["code"],
        "background_unavailable"
    );
    assert!(
        !resolved.load(Ordering::SeqCst),
        "snapshot context must not be consulted before the native refusal"
    );
}

#[cfg(test)]
#[test]
fn coordinate_drag_keeps_wayland_background_refusal_ahead_of_screenshot_context() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let resolved = AtomicBool::new(false);
    let native_refusal = crate::input::delivery::background_unavailable_error(
        crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
    );
    let result = coordinate_drag_context(Some(native_refusal), || {
        resolved.store(true, Ordering::SeqCst);
        Ok(CoordinateContext::Screenshot(7.35))
    })
    .expect_err("unsupported Wayland background delivery must refuse drag");

    assert_eq!(
        result.structured_content.as_ref().unwrap()["code"],
        "background_unavailable"
    );
    assert!(
        !resolved.load(Ordering::SeqCst),
        "drag snapshot context must not be consulted before the native refusal"
    );
}

#[cfg(test)]
#[test]
fn coordinate_scroll_keeps_wayland_background_refusal_ahead_of_screenshot_context() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let resolved = AtomicBool::new(false);
    let native_refusal = crate::input::delivery::background_unavailable_error(
        crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
    );
    let result = coordinate_scroll_scale(Some(native_refusal), || {
        resolved.store(true, Ordering::SeqCst);
        Err(ToolResult::error(
            "screenshot context must not be consulted",
        ))
    })
    .expect_err("unsupported Wayland background delivery must refuse pixel scroll");

    assert_eq!(
        result.structured_content.as_ref().unwrap()["code"],
        "background_unavailable"
    );
    assert!(
        !resolved.load(Ordering::SeqCst),
        "scroll snapshot context must not be consulted before the native refusal"
    );
    assert_eq!(coordinate_scroll_scale(None, || Ok(1.5)).unwrap(), 1.5);

    let source = include_str!("impl_.rs");
    let invoke = source
        .rsplit_once("impl Tool for ScrollTool {")
        .unwrap()
        .1
        .split_once("async fn invoke")
        .unwrap()
        .1;
    let (before_scale, _) = invoke
        .split_once("coordinate_scroll_scale(native_refusal")
        .expect("pixel scroll must resolve its frame through the refusal-first helper");
    assert!(
        before_scale.contains("unavailable_wayland_focused_input_background(delivery, true)"),
        "pixel scroll must compute the native Wayland refusal before its frame"
    );
    assert!(
        !before_scale.contains("screenshot_scale("),
        "pixel scroll must not consult the screenshot frame before the native refusal"
    );
}

#[cfg(test)]
#[test]
fn missing_screenshot_frame_refusal_matches_the_click_form() {
    use BackgroundClickForm::{ExactPoint, ForegroundOnly, Modified};
    let native = || {
        Some(crate::input::delivery::background_unavailable_error(
            crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
        ))
    };
    let fields = |result: &ToolResult| result.structured_content.clone().unwrap();
    let text = |result: &ToolResult| {
        serde_json::to_value(result).unwrap()["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|content| content["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let missing = || {
        ToolResult::error("no screenshot").with_structured(
            json!({"code": "screenshot_context_missing", "pid": 7, "window_id": 9}),
        )
    };
    let zoom =
        || ToolResult::error("no zoom").with_structured(json!({"code": "zoom_context_missing"}));

    // Unmodified single left click: a capture enables the exact-point route.
    assert_eq!(background_click_form(1, 1, false), ExactPoint);
    let refusal = missing_frame_click_refusal(native(), missing(), ExactPoint);
    assert_eq!(refusal.is_error, Some(true));
    let exact = fields(&refusal);
    assert_eq!(exact["code"], "background_unavailable", "{exact}");
    assert_eq!(exact["cause"], "screenshot_context_missing", "{exact}");
    assert_eq!((&exact["pid"], &exact["window_id"]), (&json!(7), &json!(9)));
    assert!(
        exact.get("escalation").is_none(),
        "no foreground escalation: {exact}"
    );
    assert!(exact["suggestion"]
        .as_str()
        .unwrap()
        .contains("get_window_state"));
    let message = text(&refusal);
    assert!(message.contains("get_window_state"), "{message}");
    assert!(
        message.contains("delivery_mode:\"background\""),
        "{message}"
    );
    assert!(!message.contains("foreground"), "{message}");

    // Right, middle, double and triple clicks have no background route even
    // with a frame: foreground plus the screenshot prerequisite, no capture-
    // then-background promise.
    for (button, count) in [(3, 1), (2, 1), (1, 2), (1, 3)] {
        assert_eq!(background_click_form(button, count, false), ForegroundOnly);
        let refusal = missing_frame_click_refusal(native(), missing(), ForegroundOnly);
        let other = fields(&refusal);
        assert_eq!(other["code"], "background_unavailable", "{other}");
        assert_eq!(other["cause"], "screenshot_context_missing", "{other}");
        assert_eq!(other["escalation"]["recommended"], "foreground", "{other}");
        assert_eq!((&other["pid"], &other["window_id"]), (&json!(7), &json!(9)));
        let message = text(&refusal);
        assert!(message.contains("get_window_state"), "{message}");
        assert!(
            !message.contains("delivery_mode:\"background\""),
            "{message}"
        );
    }

    // Modified clicks: no mode can deliver them on native Wayland.
    assert_eq!(background_click_form(1, 1, true), Modified);
    assert_eq!(background_click_form(3, 2, true), Modified);
    let refusal = missing_frame_click_refusal(native(), missing(), Modified);
    let modified = fields(&refusal);
    assert_eq!(modified["code"], "background_unavailable", "{modified}");
    assert_eq!(modified["reason"], "modified_pixel_click_unsupported");
    assert!(modified.get("escalation").is_none(), "{modified}");
    assert!(modified.get("suggestion").is_none(), "{modified}");
    let message = text(&refusal);
    assert!(message.contains("modifier"), "{message}");
    assert!(!message.contains("get_window_state"), "{message}");

    for form in [ExactPoint, ForegroundOnly, Modified] {
        assert_eq!(
            fields(&missing_frame_click_refusal(native(), zoom(), form))["code"],
            "zoom_context_missing"
        );
        assert_eq!(
            fields(&missing_frame_click_refusal(None, missing(), form))["code"],
            "screenshot_context_missing"
        );
    }
}

#[cfg(test)]
#[test]
fn pixel_click_resolves_frame_before_native_background_refusal() {
    let source = include_str!("impl_.rs");
    let click_impl = source
        .rsplit_once(concat!("impl Tool for ", "ClickTool {"))
        .unwrap()
        .1;
    let click_impl = &click_impl[..click_impl.find("\n}\n").expect("end of the ClickTool impl")];
    let invoke = click_impl.split_once("async fn invoke").unwrap().1;
    let (before_frame, after_frame) = invoke
        .split_once(concat!("coordinate_click_context(", "None"))
        .expect("pixel click must resolve its frame before any native refusal");
    assert!(
        !before_frame.contains(concat!("unavailable_wayland_focused_", "input_background(")),
        "a native refusal before the frame would cut off the exact-point AT-SPI route"
    );
    let (frame_match, rest) = after_frame
        .split_once(concat!("crate::overlay::send_", "command_for("))
        .unwrap();
    assert!(
        frame_match.contains(concat!(
            "background_click_",
            "form(button, count, !modifiers.is_empty())"
        )),
        "the missing-frame refusal must follow the requested click form"
    );
    assert!(
        frame_match.contains(concat!(
            "missing_frame_click_",
            "refusal(native_refusal, refusal, form)"
        )),
        "a missing frame must report the contextual background refusal"
    );
    let (native_route, _) = rest
        .split_once(concat!(
            "if !delivery.is_foreground() ",
            "&& button == 1 && count == 1 {"
        ))
        .expect("the native Wayland background point route must remain after the frame")
        .1
        .split_once(concat!("if state_for_task.", "wayland_inject_mode()"))
        .expect("the point route must precede pointer injection");
    assert!(
        native_route.contains(concat!("state_for_task.", "point_action(")),
        "the native Wayland background route must try the exact-point AT-SPI action"
    );
}

#[cfg(test)]
#[test]
fn coordinate_less_mouse_button_up_survives_snapshot_replacement() {
    let state = ToolState::new();
    let cursor_id = "held-after-replacement";
    let hold = MouseHoldState {
        pid: std::process::id(),
        xid: 7,
        button: 1,
        x: 120.0,
        y: 80.0,
    };
    state.snapshots.publish_for_session(
        hold.pid as i32,
        hold.xid,
        crate::atspi::snapshot::AtspiSnapshot::from_nodes(&[]),
        Some("press-owner"),
        Some(2.0),
    );
    state
        .mouse_hold
        .lock()
        .unwrap()
        .insert(cursor_id.to_owned(), hold.clone());

    state.snapshots.publish_for_session(
        hold.pid as i32,
        hold.xid,
        crate::atspi::snapshot::AtspiSnapshot::from_nodes(&[]),
        Some("replacement-owner"),
        Some(1.0),
    );

    let release_args = json!({"_session_id": "press-owner"});
    assert_eq!(
        mouse_button_up_coordinates(&state, &release_args, &hold).unwrap(),
        (hold.x, hold.y),
        "coordinate-less release must use the stored native hold point"
    );

    let stale_coordinate_args = json!({"_session_id": "press-owner", "x": 60.0, "y": 40.0});
    let refusal = mouse_button_up_coordinates(&state, &stale_coordinate_args, &hold)
        .expect_err("coordinate-bearing release must still reject a replaced snapshot");
    assert_eq!(
        refusal.structured_content.as_ref().unwrap()["code"],
        "screenshot_context_missing"
    );

    clear_mouse_hold_after_release(&state, cursor_id);
    assert!(state
        .mouse_hold
        .lock()
        .unwrap()
        .insert(cursor_id.to_owned(), hold)
        .is_none());
}

#[cfg(test)]
#[test]
fn session_end_releases_only_its_hold_before_discarding_state() {
    let state = ToolState::new();
    let session_id = "ending-session";
    let hold = MouseHoldState {
        pid: 42,
        xid: 7,
        button: 1,
        x: 10.0,
        y: 20.0,
    };
    let other_hold = MouseHoldState {
        pid: 43,
        xid: 8,
        button: 3,
        x: 30.0,
        y: 40.0,
    };
    {
        let mut holds = state.mouse_hold.lock().unwrap();
        holds.insert(session_id.to_owned(), hold.clone());
        holds.insert("other-session".to_owned(), other_hold.clone());
    }

    let mut released = Vec::new();
    release_mouse_hold_for_session(&state, session_id, |cursor_id, held| {
        assert!(state.mouse_hold.lock().unwrap().contains_key(cursor_id));
        released.push((cursor_id.to_owned(), held.button));
        Ok(())
    })
    .unwrap();

    assert_eq!(released, vec![(session_id.to_owned(), hold.button)]);
    let mut holds = state.mouse_hold.lock().unwrap();
    assert!(!holds.contains_key(session_id));
    assert_eq!(
        holds.get("other-session").unwrap().button,
        other_hold.button
    );
    assert!(holds.insert(session_id.to_owned(), hold).is_none());
}

#[cfg(test)]
#[test]
fn failed_session_end_release_remains_retryable_and_bounded() {
    let state = ToolState::new();
    let session_id = "failed-release-session";
    let hold = MouseHoldState {
        pid: 42,
        xid: 7,
        button: 1,
        x: 10.0,
        y: 20.0,
    };
    state
        .mouse_hold
        .lock()
        .unwrap()
        .insert(session_id.to_owned(), hold.clone());

    let mut attempts = 0;
    let failure = release_mouse_hold_for_session(&state, session_id, |_, _| {
        attempts += 1;
        anyhow::bail!("compositor disconnected")
    })
    .expect_err("a failed compositor release must fail session cleanup");
    assert!(failure.contains("compositor disconnected"));
    assert_eq!(
        attempts, 1,
        "one cleanup pass must make one bounded attempt"
    );
    assert_eq!(
        state
            .mouse_hold
            .lock()
            .unwrap()
            .get(session_id)
            .unwrap()
            .button,
        hold.button,
        "failed cleanup must retain the hold for the next session-end retry"
    );

    release_mouse_hold_for_session(&state, session_id, |cursor_id, held| {
        attempts += 1;
        assert_eq!(cursor_id, session_id);
        assert_eq!(held.button, hold.button);
        Ok(())
    })
    .unwrap();
    assert_eq!(attempts, 2);
    assert!(state
        .mouse_hold
        .lock()
        .unwrap()
        .insert(session_id.to_owned(), hold)
        .is_none());
}

#[cfg(test)]
#[test]
fn isolated_hyprland_refused_partial_and_unknown_outcomes_stay_distinct() {
    let busy = isolated_hyprland_result(Err(crate::wayland::hyprland_input::LaneBusy.into()));
    let content = busy.structured_content.as_ref().unwrap();
    assert_eq!(content["reason"], "lane_busy");
    assert_eq!(content["effect"], "refused");
    assert!(content.get("delivery").is_none());
    assert!(busy
        .action_record
        .unwrap()
        .public_result()
        .unwrap()
        .delivery
        .is_none());

    let refused = isolated_hyprland_result(Ok(
        json!({"ok":false,"code":"stale_target","detail":"stale_target"}),
    ));
    assert_eq!(
        refused.structured_content.as_ref().unwrap()["effect"],
        "refused"
    );
    let public = refused.action_record.unwrap().public_result().unwrap();
    assert!(public.delivery.is_none());
    public.validate_invariants().unwrap();

    let partial = isolated_hyprland_result(Ok(
        json!({"ok":false,"code":"cancelled","detail":"cancelled",
        "effect":"partial","delivery":{"mode":"background","delivered_count":1}}),
    ));
    let public = partial.action_record.unwrap().public_result().unwrap();
    assert_eq!(public.effect, cua_driver_contract::ActionEffect::Partial);
    assert_eq!(public.delivery.unwrap().delivered_count, Some(1));

    let unknown = isolated_hyprland_result(Err(anyhow::anyhow!("connection lost")));
    let public = unknown.action_record.unwrap().public_result().unwrap();
    assert_eq!(
        public.effect,
        cua_driver_contract::ActionEffect::Unverifiable
    );
    assert_eq!(
        public.delivery.as_ref().unwrap().mode,
        cua_driver_contract::ActionDeliveryMode::Unknown
    );
    assert_eq!(public.delivery.unwrap().delivered_count, None);

    let unknown_after_start = isolated_hyprland_result(Err(
        crate::wayland::hyprland_input::unknown_dispatch(anyhow::anyhow!("connection lost"), 1),
    ));
    let public = unknown_after_start
        .action_record
        .unwrap()
        .public_result()
        .unwrap();
    public.validate_invariants().unwrap();
    assert_eq!(public.effect, cua_driver_contract::ActionEffect::Partial);
    let delivery = public.delivery.unwrap();
    assert_eq!(
        delivery.mode,
        cua_driver_contract::ActionDeliveryMode::Unknown
    );
    assert_eq!(delivery.delivered_count, Some(1));
}

#[cfg(test)]
#[test]
fn isolated_text_reports_every_acknowledged_key_before_a_refusal() {
    // Two keys acknowledged, then the target goes stale: the real text
    // producer reports partial/2, and the text projector must keep it.
    let mut calls = 0;
    let reply = crate::wayland::hyprland_input::background_text_reply_for_test("abc", |_| {
        calls += 1;
        Ok(if calls < 3 {
            json!({"ok": true, "effect": "unverifiable", "route": "synthetic_events"})
        } else {
            json!({"ok": false, "code": "stale_target", "detail": "stale_target"})
        })
    })
    .unwrap();
    assert_eq!(calls, 3);
    assert_eq!(reply["effect"], "partial");

    let result = isolated_hyprland_text_result(Ok(reply.clone()));
    assert_eq!(result.is_error, Some(true));
    let value = result.structured_content.as_ref().unwrap();
    assert_eq!(value["effect"], "partial");
    assert_eq!(value["delivery"]["mode"], "background");
    assert_eq!(value["delivery"]["delivered_count"], 2);
    let public = result.action_record.unwrap().public_result().unwrap();
    public.validate_invariants().unwrap();
    assert_eq!(public.effect, cua_driver_contract::ActionEffect::Partial);
    let delivery = public.delivery.unwrap();
    assert_eq!(
        delivery.mode,
        cua_driver_contract::ActionDeliveryMode::Background
    );
    assert_eq!(delivery.delivered_count, Some(2));

    // A single action never acknowledges two phases; that stays a refusal.
    let single = isolated_hyprland_result(Ok(reply));
    let public = single.action_record.unwrap().public_result().unwrap();
    assert_eq!(public.effect, cua_driver_contract::ActionEffect::Refused);
    assert!(public.delivery.is_none());
}

#[cfg(test)]
#[test]
fn experimental_pending_grant_is_an_error_with_operator_context() {
    let result = isolated_hyprland_result(Ok(json!({
        "ok": false, "code": "permission_required", "detail": "external approval pending",
        "epoch": "epoch", "challenge": "challenge", "target": "target", "revision": 3
    })));
    assert_eq!(result.is_error, Some(true));
    let value = result.structured_content.unwrap();
    assert_eq!(value["code"], "background_unavailable");
    assert_eq!(value["reason"], "permission_required");
    assert_eq!(value["challenge"], "challenge");
    assert_eq!(value["target"], "target");
    assert_eq!(value["revision"], 3);
    assert_eq!(value["verified"], false);
}

#[cfg(test)]
#[test]
fn experimental_dispatch_does_not_claim_application_success() {
    let result = isolated_hyprland_result(Ok(json!({
        "ok": true, "effect": "unverifiable", "route": "synthetic_events"
    })));
    assert_ne!(result.is_error, Some(true));
    let public = result.action_record.unwrap().public_result().unwrap();
    public.validate_invariants().unwrap();
    let value = serde_json::to_value(public).unwrap();
    assert_eq!(value["effect"], "unverifiable");
    assert_eq!(value["route"], "synthetic_events");
    assert!(value.get("verified").is_none());
}

#[cfg(test)]
#[test]
fn foreground_hyprland_reports_foreground_and_preserves_partial_progress() {
    use cua_driver_core::action_record::{ActionTransport, RequestedDelivery};
    for (reply, expected_effect, count) in [
        (json!({"ok":true}), "unverifiable", None),
        (
            json!({"ok":false,"code":"stale_target","detail":"stale"}),
            "refused",
            None,
        ),
        (
            json!({"ok":false,"code":"cancelled","detail":"cancelled","effect":"partial",
            "delivery":{"mode":"foreground","delivered_count":3}}),
            "partial",
            Some(3),
        ),
    ] {
        let result = hyprland_input_result(Ok(reply), true);
        let record = result.action_record.unwrap();
        assert_eq!(
            record.transport,
            ActionTransport::LinuxHyprlandForegroundInput
        );
        assert_eq!(record.requested_delivery, RequestedDelivery::Foreground);
        let public = record.public_result().unwrap();
        public.validate_invariants().unwrap();
        let value = serde_json::to_value(public).unwrap();
        assert_eq!(value["route"], "global_input");
        assert_eq!(value["effect"], expected_effect);
        if expected_effect == "refused" {
            assert!(value.get("delivery").is_none());
        } else {
            assert_eq!(value["delivery"]["mode"], "foreground");
            assert_eq!(value["delivery"]["delivered_count"].as_u64(), count);
        }
    }
    for reply in [
        Err(crate::wayland::hyprland_input::unknown_dispatch(
            anyhow::anyhow!("lost"),
            2,
        )),
        Ok(json!({"ok":false,"code":"foreground_partial_unknown","detail":"focus changed"})),
    ] {
        let unknown = hyprland_input_result(reply, true);
        let value =
            serde_json::to_value(unknown.action_record.unwrap().public_result().unwrap()).unwrap();
        assert_eq!(value["delivery"]["mode"], "unknown");
        assert_ne!(value["effect"], "refused");
    }
}

#[cfg(test)]
#[test]
fn foreground_hyprland_chords_preserve_all_modifiers() {
    let action = foreground_hyprland_key("Ctrl+Shift+H".into(), vec!["alt".into()]).unwrap();
    match action {
        crate::wayland::hyprland_input::Action::Key { key, modifiers } => {
            assert_eq!(key, "H");
            assert_eq!(modifiers, ["alt", "ctrl", "shift"]);
        }
        _ => panic!("expected a key chord"),
    }
    assert!(foreground_hyprland_key("bogus+H".into(), vec![]).is_err());
}

#[cfg(test)]
#[test]
fn uinput_failure_has_a_stable_structured_code() {
    let result = linux_input_error(crate::input::uinput_unavailable("synthetic failure"));
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result
            .structured_content
            .as_ref()
            .and_then(|value| value.get("code"))
            .and_then(Value::as_str),
        Some(crate::input::UINPUT_UNAVAILABLE_CODE)
    );
}

fn x11_pixel_click_no_focus_steal_modifiers(
    xid: u64,
    lx: i32,
    ly: i32,
    button: u8,
    count: usize,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    crate::input::send_click_with_modifiers(xid, lx, ly, count, button, modifiers)
}

fn mouse_button_name(button: u8) -> &'static str {
    match button {
        3 => "right",
        2 => "middle",
        _ => "left",
    }
}

fn resolve_cursor_key(args: &Value) -> String {
    for key in ["session", "_session_id", "cursor_id"] {
        if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
            if !v.is_empty() {
                return v.to_owned();
            }
        }
    }
    "default".to_owned()
}

/// Return the cursor key only for a lifecycle-owned session. Cursor positioning
/// for keyboard actions deliberately does not opt anonymous calls or the
/// legacy cursor_id-only path into session semantics. The proxy-minted
/// `_session_id` is trusted lifecycle state and must behave like a public named
/// session for cursor ownership.
fn named_session_cursor_key(args: &Value) -> Option<String> {
    cursor_overlay::named_session_cursor_key(args)
}

fn mouse_hold_json(cursor_id: &str, hold: Option<&MouseHoldState>) -> Value {
    match hold {
        Some(hold) => json!({
            "cursor_id": cursor_id,
            "held": true,
            "pid": hold.pid,
            "window_id": hold.xid,
            "button": mouse_button_name(hold.button),
            "x": hold.x,
            "y": hold.y,
        }),
        None => json!({
            "cursor_id": cursor_id,
            "held": false,
            "pid": Value::Null,
            "window_id": Value::Null,
            "button": Value::Null,
            "x": Value::Null,
            "y": Value::Null,
        }),
    }
}

fn held_target_mismatch(
    args: &Value,
    cursor_id: &str,
    hold: &MouseHoldState,
) -> Option<ToolResult> {
    match args.opt_u32("pid") {
        Ok(Some(pid)) if pid != hold.pid => {
            return Some(
                ToolResult::error(format!(
                    "Cursor '{cursor_id}' is holding a button for pid {}, not pid {pid}.",
                    hold.pid
                ))
                .with_structured(mouse_hold_json(cursor_id, Some(hold))),
            );
        }
        Err(err) => return Some(err.with_structured(mouse_hold_json(cursor_id, Some(hold)))),
        _ => {}
    }

    match args.opt_u64("window_id") {
        Some(xid) if xid != hold.xid => Some(
            ToolResult::error(format!(
                "Cursor '{cursor_id}' is holding a button for window_id {}, not {xid}.",
                hold.xid
            ))
            .with_structured(mouse_hold_json(cursor_id, Some(hold))),
        ),
        _ => None,
    }
}

fn overlay_snap_to_for(cursor_id: &str, sx: f64, sy: f64, heading: Option<f64>) {
    crate::overlay::send_command_for(
        cursor_id.to_owned(),
        cursor_overlay::OverlayCommand::SnapTo {
            x: sx,
            y: sy,
            heading_radians: heading,
        },
    );
}

fn overlay_move_to_for(cursor_id: &str, sx: f64, sy: f64, heading: Option<f64>) {
    overlay_move_to_target_for(cursor_id, sx, sy, heading, None);
}

/// [`overlay_move_to_for`] with the targeted element's screen rect
/// `[x, y, width, height]` (same space as `sx`/`sy`).
fn overlay_move_to_target_for(
    cursor_id: &str,
    sx: f64,
    sy: f64,
    heading: Option<f64>,
    target: Option<[f64; 4]>,
) {
    crate::overlay::send_command_for(
        cursor_id.to_owned(),
        cursor_overlay::OverlayCommand::MoveTo {
            x: sx,
            y: sy,
            end_heading_radians: heading.unwrap_or(std::f64::consts::FRAC_PI_4),
            target,
        },
    );
}

async fn overlay_glide_to_for(cursor_id: &str, sx: f64, sy: f64) {
    overlay_glide_to_target_for(cursor_id, sx, sy, None).await;
}

/// [`overlay_glide_to_for`] with the targeted element's screen rect
/// `[x, y, width, height]` (same space as `sx`/`sy`); `None` for pixel actions.
async fn overlay_glide_to_target_for(cursor_id: &str, sx: f64, sy: f64, target: Option<[f64; 4]>) {
    // Explicit runtime disable persists across pointer and keyboard actions.
    if !crate::overlay::is_enabled_for(cursor_id) {
        return;
    }
    // Every Wayland backend owns its animation loop. Send one destination and
    // let the Linux overlay layer select the exact-target semantic helper,
    // layer-shell, or older helper fallback without a competing interpolation.
    if crate::wayland::is_wayland() {
        overlay_move_to_target_for(cursor_id, sx, sy, None, target);
        return;
    }
    if !crate::overlay::is_placed_for(cursor_id) {
        crate::overlay::send_command_for(
            cursor_id.to_owned(),
            cursor_overlay::OverlayCommand::ClickPulse { x: sx, y: sy },
        );
        return;
    }
    crate::overlay::animate_cursor_to_target_for(cursor_id.to_owned(), sx, sy, target).await;
}

/// The compositor snapshot a desktop-scope action is addressed in. One
/// snapshot converts the action's points, places the overlay and sizes the
/// virtual pointer, so they cannot disagree about the monitor layout.
async fn desktop_input_space() -> anyhow::Result<crate::wayland::DesktopInputSpace> {
    if !crate::wayland::wayland_input_enabled() {
        return Ok(crate::wayland::DesktopInputSpace::default());
    }
    cua_driver_core::blocking::spawn(crate::wayland::DesktopInputSpace::current)
        .await
        .map_err(|error| anyhow::anyhow!("task error: {error}"))?
}

/// Keep the logical cursor position in sync with every visibly targeted
/// pointer action. Overlay delivery is intentionally best-effort: registry
/// state is still updated when no renderer is running or its queue is closed.
async fn reveal_pointer_action_for(
    state: &ToolState,
    cursor_id: &str,
    sx: f64,
    sy: f64,
    click_pulse: bool,
) {
    reveal_pointer_action_for_target(state, cursor_id, sx, sy, click_pulse, None).await;
}

/// [`reveal_pointer_action_for`] with the targeted element's screen rect
/// `[x, y, width, height]` (same space as `sx`/`sy`).
async fn reveal_pointer_action_for_target(
    state: &ToolState,
    cursor_id: &str,
    sx: f64,
    sy: f64,
    click_pulse: bool,
    target: Option<[f64; 4]>,
) {
    if !sx.is_finite() || !sy.is_finite() {
        return;
    }
    state.cursor_registry.update_position(cursor_id, sx, sy);
    emit_cursor_hook(cursor_id, sx, sy, false);
    overlay_glide_to_target_for(cursor_id, sx, sy, target).await;
    if click_pulse {
        emit_cursor_hook(cursor_id, sx, sy, true);
        crate::overlay::send_command_for(
            cursor_id.to_owned(),
            cursor_overlay::OverlayCommand::ClickPulse { x: sx, y: sy },
        );
    }
}

/// Report a commanded cursor move (or a press) to the embedder's cursor
/// hook, with the same guards as macOS's `CursorRegistry::emit_cursor_event`:
/// nothing is built unless a hook is registered, and an ended session never
/// reaches it. Emitted from the logical cursor write path, so it works with
/// no overlay renderer (a headless Xvfb session).
pub(crate) fn emit_cursor_hook(cursor_id: &str, x: f64, y: f64, pressed: bool) {
    if !cua_driver_core::cursor_hook::cursor_hook_enabled() {
        return;
    }
    if cursor_id.is_empty() || cua_driver_core::session::is_session_ended(cursor_id) {
        return;
    }
    cua_driver_core::cursor_hook::push_cursor_event(
        cua_driver_core::cursor_hook::CursorHookEvent {
            cursor_id: cursor_id.to_owned(),
            x,
            y,
            pressed,
        },
    );
}

fn keyboard_window_center(xid: u64) -> Option<(f64, f64)> {
    if xid == 0 {
        return None;
    }
    if crate::wayland::is_wayland() {
        return crate::wayland::window_geometry(xid).and_then(|(x, y, width, height)| {
            (width > 0 && height > 0).then_some((
                f64::from(x) + f64::from(width) / 2.0,
                f64::from(y) + f64::from(height) / 2.0,
            ))
        });
    }
    window_screen_center(xid)
        .ok()
        .map(|(x, y)| (f64::from(x), f64::from(y)))
}

fn current_pointer_position() -> Option<(f64, f64)> {
    if crate::wayland::is_wayland() {
        return crate::wayland::last_synth_cursor_pos().map(|(x, y)| (f64::from(x), f64::from(y)));
    }

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::rust_connection::RustConnection;

    let (connection, screen_num) = RustConnection::connect(None).ok()?;
    let root = connection.setup().roots[screen_num].root;
    let reply = connection.query_pointer(root).ok()?.reply().ok()?;
    Some((f64::from(reply.root_x), f64::from(reply.root_y)))
}

fn explicit_keyboard_cursor_target(
    pid: u32,
    xid: u64,
    element_index: Option<usize>,
    pixel_target: Option<(f64, f64)>,
) -> Option<(f64, f64)> {
    if let Some(element_index) = element_index {
        let (sx, sy) = element_screen_center(pid, element_index, (xid != 0).then_some(xid)).ok()?;
        return Some((sx, sy));
    }

    let (x, y) = pixel_target?;
    if crate::wayland::wayland_input_enabled() {
        return crate::wayland::window_geometry(xid)
            .map(|(wx, wy, _, _)| (f64::from(wx) + x.round(), f64::from(wy) + y.round()));
    }
    window_local_to_screen(xid, x, y).ok()
}

/// Position and reveal a named session's cursor before keyboard/value input.
/// `preserve_legacy_element_visual` retains the existing element-only feedback
/// for type_text/set_value anonymous calls without adding session-style fallback
/// placement to them. Geometry and overlay failures are observational and never
/// affect the tool's actual input result.
/// Tell the cursor hook where an anonymous keyboard action types, before the
/// keystrokes: the element or pixel target, else the window's center. The
/// overlay is left alone (anonymous calls keep their legacy visuals); only an
/// embedder publishing presence learns the position, so a viewer sees the
/// agent's cursor go to the field it is about to type into.
async fn announce_keyboard_target(
    args: &Value,
    pid: u32,
    xid: u64,
    element_index: Option<usize>,
    pixel_target: Option<(f64, f64)>,
) {
    if !cua_driver_core::cursor_hook::cursor_hook_enabled() {
        return;
    }
    let cursor_id = resolve_cursor_key(args);
    let target = cua_driver_core::blocking::spawn(move || {
        explicit_keyboard_cursor_target(pid, xid, element_index, pixel_target)
            .or_else(|| (xid != 0).then(|| keyboard_window_center(xid)).flatten())
    })
    .await
    .ok()
    .flatten();
    if let Some((x, y)) = target {
        emit_cursor_hook(&cursor_id, x, y, false);
    }
}

async fn position_named_session_keyboard_cursor(
    state: &ToolState,
    args: &Value,
    pid: u32,
    xid: u64,
    element_index: Option<usize>,
    pixel_target: Option<(f64, f64)>,
    preserve_legacy_element_visual: bool,
) {
    #[cfg(test)]
    if state.production_route_backend.is_some() {
        return;
    }
    let named_cursor_id = named_session_cursor_key(args);
    let cursor_id = match named_cursor_id {
        Some(ref cursor_id) => cursor_id.clone(),
        None if preserve_legacy_element_visual && element_index.is_some() => {
            resolve_cursor_key(args)
        }
        None => {
            announce_keyboard_target(args, pid, xid, element_index, pixel_target).await;
            return;
        }
    };

    let remembered = named_cursor_id.as_ref().and_then(|_| {
        state
            .cursor_registry
            .get(&cursor_id)
            .and_then(|cursor| cursor.x.zip(cursor.y))
    });
    let explicit = cua_driver_core::blocking::spawn(move || {
        explicit_keyboard_cursor_target(pid, xid, element_index, pixel_target)
    })
    .await
    .ok()
    .flatten();

    let fallback = if named_cursor_id.is_some()
        && explicit.is_none()
        && cursor_overlay::keyboard_cursor_target(None, remembered, None, None).is_none()
    {
        cua_driver_core::blocking::spawn(move || {
            let center = keyboard_window_center(xid);
            let pointer = center.is_none().then(current_pointer_position).flatten();
            (center, pointer)
        })
        .await
        .unwrap_or((None, None))
    } else {
        (None, None)
    };

    let Some((sx, sy)) =
        cursor_overlay::keyboard_cursor_target(explicit, remembered, fallback.0, fallback.1)
    else {
        return;
    };
    if xid != 0 {
        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
    }
    reveal_pointer_action_for(state, &cursor_id, sx, sy, false).await;
}

async fn track_overlay_drag_for(
    cursor_id: String,
    from: (f64, f64),
    to: (f64, f64),
    duration_ms: u64,
    steps: usize,
) {
    if !crate::overlay::is_enabled_for(&cursor_id) {
        return;
    }
    track_overlay_drag_with(
        |command| crate::overlay::send_command_for(cursor_id.clone(), command),
        from,
        to,
        duration_ms,
        steps,
    )
    .await;
}

async fn track_overlay_drag_with(
    send: impl Fn(cursor_overlay::OverlayCommand),
    from: (f64, f64),
    to: (f64, f64),
    duration_ms: u64,
    steps: usize,
) {
    let _pressed = cursor_overlay::PressedVisualGuard::new(&send);
    let steps = steps.max(1);
    let step_delay = std::time::Duration::from_millis(duration_ms / steps as u64);
    for index in 0..=steps {
        let t = index as f64 / steps as f64;
        let x = from.0 + (to.0 - from.0) * t;
        let y = from.1 + (to.1 - from.1) * t;
        send(cursor_overlay::track_pointer_command(x, y));
        if index < steps && !step_delay.is_zero() {
            tokio::time::sleep(step_delay).await;
        }
    }
}

#[cfg(test)]
#[tokio::test]
async fn cancelled_overlay_drag_releases_visual_press_without_native_commands() {
    use std::{cell::RefCell, future::Future, task::Context};
    let commands = RefCell::new(Vec::new());
    let mut drag = Box::pin(track_overlay_drag_with(
        |command| commands.borrow_mut().push(command),
        (10.0, 20.0),
        (30.0, 40.0),
        2000,
        20,
    ));
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(drag.as_mut().poll(&mut context).is_pending());
    assert!(matches!(
        commands.borrow()[0],
        cursor_overlay::OverlayCommand::SetPressed(true)
    ));
    drop(drag);
    let commands = commands.borrow();
    assert_eq!(commands.len(), 3);
    assert!(matches!(
        commands.last(),
        Some(cursor_overlay::OverlayCommand::SetPressed(false))
    ));
}

#[cfg(test)]
#[tokio::test]
async fn completed_overlay_drag_balances_visual_press_once() {
    use std::cell::RefCell;
    let pressed = RefCell::new(Vec::new());
    track_overlay_drag_with(
        |command| {
            if let cursor_overlay::OverlayCommand::SetPressed(value) = command {
                pressed.borrow_mut().push(value);
            }
        },
        (10.0, 20.0),
        (30.0, 40.0),
        0,
        2,
    )
    .await;
    assert_eq!(*pressed.borrow(), vec![true, false]);
}

fn process_name(pid: u32) -> Option<String> {
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let first = String::from_utf8_lossy(&cmdline)
        .split('\0')
        .next()
        .unwrap_or("")
        .trim()
        .to_owned();
    if !first.is_empty() {
        return std::path::Path::new(&first)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .or(Some(first));
    }

    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find(|l| l.starts_with("Name:"))
        .map(|l| l[5..].trim().to_owned())
}

fn is_terminal_process(pid: u32) -> bool {
    // Canonical list lives in `crate::terminal::TERMINAL_PROCESS_NAMES`
    // — keeps the additive contract centralised. Adding a new terminal
    // here means appending one string in `crate::terminal`.
    match process_name(pid).as_deref() {
        Some(name) => crate::terminal::is_terminal_process_name(name),
        None => false,
    }
}

fn terminal_descendant_ttys(pid: u32) -> Vec<PathBuf> {
    let mut parent_to_children: std::collections::HashMap<u32, Vec<u32>> =
        std::collections::HashMap::new();
    let proc_dir = std::path::Path::new("/proc");
    let entries = match fs::read_dir(proc_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    for entry in entries.flatten() {
        let pid_str = entry.file_name();
        let pid_str = pid_str.to_string_lossy();
        let child_pid: u32 = match pid_str.parse() {
            Ok(pid) => pid,
            Err(_) => continue,
        };
        let status = match fs::read_to_string(proc_dir.join(&*pid_str).join("status")) {
            Ok(status) => status,
            Err(_) => continue,
        };
        let parent_pid = status
            .lines()
            .find(|l| l.starts_with("PPid:"))
            .and_then(|l| l[5..].trim().parse::<u32>().ok());
        if let Some(parent_pid) = parent_pid {
            parent_to_children
                .entry(parent_pid)
                .or_default()
                .push(child_pid);
        }
    }

    let mut descendants = Vec::new();
    let mut queue = std::collections::VecDeque::from([pid]);
    while let Some(current) = queue.pop_front() {
        if let Some(children) = parent_to_children.get(&current) {
            for &child in children {
                descendants.push(child);
                queue.push_back(child);
            }
        }
    }
    descendants.sort_unstable();

    let mut ttys = Vec::new();
    for child in descendants {
        let tty = match fs::read_link(format!("/proc/{child}/fd/0")) {
            Ok(path) => path,
            Err(_) => continue,
        };
        if tty.starts_with("/dev/pts/") {
            ttys.push(tty);
        }
    }
    ttys
}

fn terminal_tty_for_window(pid: u32, xid: u64) -> Option<PathBuf> {
    if !is_terminal_process(pid) {
        return None;
    }
    let mut windows = crate::x11::list_windows(Some(pid));
    windows.sort_by_key(|w| w.xid);
    let window_index = windows.iter().position(|w| w.xid == xid)?;
    let ttys = terminal_descendant_ttys(pid);
    ttys.get(window_index).cloned()
}

/// True when `key` names the Enter key in the shared X keysym vocabulary
/// (`key_name_to_keysym`). The terminal pty short-circuit below applies to
/// every spelling of that physical key — `enter`, `return`, any case — so an
/// agent following the documented key names cannot silently lose the keypress
/// on a terminal window (terminals discard synthetic XSendEvent keys).
fn is_enter_key(key: &str) -> bool {
    crate::input::key_name_to_keysym(key).ok() == Some(0xFF0D)
}

/// Type into a terminal window without touching X focus. Resolves the window's
/// pty, then borrows the emulator's master fd and writes to it (see
/// `crate::tty`). Returns `Ok(false)` when the target isn't a terminal we can
/// reach this way so the caller falls back to the generic XSendEvent path.
fn inject_terminal_input(pid: u32, xid: u64, text: &str) -> anyhow::Result<bool> {
    let Some(tty) = terminal_tty_for_window(pid, xid) else {
        return Ok(false);
    };
    // tty is `/dev/pts/<N>`; the emulator (pid) holds the master for the same N.
    let Some(ptn) = tty
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return Ok(false);
    };
    crate::tty::inject_via_master(pid, ptn, text)
}

// ── click ─────────────────────────────────────────────────────────────────────
// Desktop-raw. Core dispatch waits for portal/libei, then takes the resource
// lease before this tool injects. Do not start a second seat-wide lease here.

fn element_ax_failure_may_fallback(exact_wayland_action: bool) -> bool {
    !exact_wayland_action
}

/// Only an affirmative, pre-dispatch result permits a native Wayland point
/// action to try another route: a miss (`None`), or a node that takes a real
/// pointer press (`ElementClickNeedsForeground`, refused before any action was
/// sent). Every other error includes failed or indeterminate delivery and must
/// propagate without replay.
fn exact_point_action_may_fallback(result: anyhow::Result<Option<String>>) -> anyhow::Result<bool> {
    match result {
        Ok(Some(_)) => Ok(false),
        Ok(None) => Ok(true),
        Err(error) if error.is::<crate::atspi::ElementClickNeedsForeground>() => Ok(true),
        Err(error) => Err(error),
    }
}

fn bounded_click_count_arg(args: &Value) -> Result<u32, ToolResult> {
    let count = match args.opt_u32("count") {
        Ok(Some(count)) => count,
        Ok(None) => 1,
        Err(err) => return Err(err),
    };
    if !(1..=3).contains(&count) {
        return Err(ToolResult::error(
            "click: count must be between 1 and 3".to_string(),
        ));
    }
    Ok(count)
}

#[path = "retained_click.rs"]
mod retained_click;

pub struct ClickTool {
    state: Arc<ToolState>,
}
static CLICK_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

impl ClickTool {
    /// Resolve the snapshot's retained object identity once. The current
    /// ordinal is never used to select a target after the observation.
    async fn click_indexed_x11(
        &self,
        pid: u32,
        idx: usize,
        xid_hint: Option<u64>,
        snapshot_identity: cua_driver_core::element_token::SnapshotIdentity,
        button: u8,
        count: usize,
        modifiers: Vec<String>,
        delivery: crate::input::delivery::DeliveryMode,
        cursor_id: String,
    ) -> ToolResult {
        if button != 1 || count != 1 || !modifiers.is_empty() {
            return retained_click::unqualified(
                "secondary element clicks require the retained pointer route",
            );
        }
        let Some(xid) = xid_hint.filter(|xid| *xid != 0) else {
            return ToolResult::error("Indexed click requires an observed exact X11 window");
        };
        let cache = self.state.snapshots.clone();
        let resolved = cua_driver_core::blocking::spawn(move || -> anyhow::Result<_> {
            let (permit, identity) = cache
                .acquire_observed_mutation(snapshot_identity, idx)
                .map_err(|error| anyhow::anyhow!("stale_element_token: {error}"))?;
            let target = crate::atspi::resolve_observed_click_target(pid, idx, xid, &identity)?;
            let center = target.screen_bounds().ok().map(|bounds @ (x, y, w, h)| {
                let (sx, sy) = (x as f64 + w as f64 / 2.0, y as f64 + h as f64 / 2.0);
                ((sx, sy), frame_target_rect(Some(bounds), sx, sy))
            });
            // Hand the admission through the overlay wait to the mutation worker.
            // If either waiter is cancelled, native work still owns its permit.
            Ok((permit, target, center))
        })
        .await;
        let (permit, target, center) = match resolved {
            Ok(Ok(target)) => target,
            Ok(Err(error)) => {
                return ToolResult::error(format!("AT-SPI element resolution failed: {error}"))
            }
            Err(error) => return ToolResult::error(format!("Task error: {error}")),
        };
        if let Some(((sx, sy), target_rect)) = center {
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::PinAbove(xid),
            );
            reveal_pointer_action_for_target(&self.state, &cursor_id, sx, sy, true, target_rect)
                .await;
        }
        let result = cua_driver_core::blocking::spawn(move || -> anyhow::Result<ToolResult> {
            let permit = std::sync::Arc::new(permit);
            target.verify_live()?;
            let action = target.perform_action(modifiers.is_empty() && button == 1 && count == 1);
            let (path, suspected_noop) = match action {
                Ok((_, suspected_noop)) => ("ax", suspected_noop),
                Err(error) if crate::atspi::click_error_allows_pointer_fallback(&error) => {
                    if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
                        return Ok(refusal);
                    }
                    // The background branch below sends XSendEvent, never the
                    // real pointer, so refuse toolkits that drop synthetic
                    // pointer input whatever pointer the host offers.
                    if let Some(refusal) = synthetic_pointer_fallback_refusal(pid, delivery) {
                        return Ok(refusal);
                    }
                    let local_center = || -> anyhow::Result<(f64, f64)> {
                        let (x, y, w, h) = target.screen_bounds()?;
                        let (ox, oy) = window_local_to_screen(xid, 0.0, 0.0)?;
                        target.verify_live()?;
                        Ok((
                            x as f64 + w as f64 / 2.0 - ox,
                            y as f64 + h as f64 / 2.0 - oy,
                        ))
                    };
                    let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    let (lx, ly) = local_center()?;
                    let path = if !delivery.is_foreground() && target.needs_foreground_pointer() {
                        return Ok(crate::input::delivery::background_unavailable_error(
                            crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                        ));
                    } else if delivery.is_foreground() {
                        crate::input::foreground::with_x11_foreground_permit(
                            xid,
                            crate::input::ForegroundOptions::from_settle_hint(80),
                            Some(permit.clone()),
                            || {
                                let (lx, ly) = local_center()?;
                                let (sx, sy) = window_local_to_screen(xid, lx, ly)?;
                                // Activation and coordinate reads may reparent the object.
                                target.verify_live()?;
                                crate::input::send_click_xtest_desktop_with_modifiers(
                                    sx.round() as i32,
                                    sy.round() as i32,
                                    button,
                                    count,
                                    &modifier_refs,
                                )
                            },
                        )?;
                        "x11_xtest_fg"
                    } else {
                        target.verify_live()?;
                        crate::input::send_click_with_modifiers(
                            xid,
                            lx.round() as i32,
                            ly.round() as i32,
                            count,
                            button,
                            &modifier_refs,
                        )?;
                        "x11_pixel"
                    };
                    (path, false)
                }
                Err(error) => return Err(error),
            };
            let mut structured = json!({
                "path": path,
                "verified": false,
                "effect": if suspected_noop { "suspected_noop" } else { "unverifiable" },
            });
            if suspected_noop {
                structured["escalation"] = non_ax_escalation();
            }
            Ok(
                ToolResult::text(format!("Clicked element [{idx}] (pid {pid})."))
                    .with_structured(structured),
            )
        })
        .await;
        match result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => ToolResult::error(format!("AT-SPI element click failed: {error}")),
            Err(error) => ToolResult::error(format!("Task error: {error}")),
        }
    }
}

#[async_trait]
impl Tool for ClickTool {
    fn def(&self) -> &ToolDef {
        CLICK_DEF.get_or_init(|| ToolDef {
            name: "click".into(),
            description: "Click against a target pid. **Prefer `element_token` over pixel \
                coordinates** — element_token works on backgrounded / hidden windows, surfaces \
                a stable handle, and tells you what you're clicking via the cached AT-SPI \
                element's role + label. Reach for `x, y` only when the target is a canvas / \
                custom-drawn surface that doesn't appear in the AT-SPI tree.\n\n\
                Provide either (window_id + x/y) or (pid + element_token). Routes via \
                XSendEvent (no focus steal). element_token cache is scoped per (pid, \
                window_id) and is replaced by the next get_window_state of the same window — \
                re-snapshot every turn before clicking.\n\n\
                After a zoom call, pass from_zoom=true to auto-translate zoom-image coords \
                back to full-window space.\n\n\
                button: \"left\" (default), \"right\", or \"middle\". Defaults to left so the \
                field is fully back-compat. X11: routes through XSendEvent ButtonPress/Release \
                with the matching button code. Native Wayland: only left-button is supported \
                via the virtual-pointer protocol — right/middle return an error rather than \
                silently degrading to left. `modifier` holds ctrl/shift/alt/super for the \
                click on X11. Native Wayland refuses modified pointer clicks until its input \
                protocol can carry keyboard modifier state.".into(),
            input_schema: json!({
                // `pid` is conditionally required (validated in code: needed for
                // window/element clicks, omitted for windowless scope="desktop"),
                // so it is NOT pinned in `required` — matches the click→[] canon
                // in cua_driver_core::tool_schema.
                "type":"object","required":[],"properties":{
                    "session": cua_driver_core::tool_schema::session_schema(),
                    "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
                    "pid":{"type":"integer","description":"Target process ID. Required unless scope is \"desktop\"."},
                    "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y or element_index; optional with element_token (the token carries it)."},
                    "x":{"type":"number","description":"Window-local pixel X of the target window's own get_window_state screenshot (0..screenshot_width). For get_desktop_state pixels pass scope:\"desktop\" (or coordinate_frame:\"desktop\")."},
                    "y":{"type":"number","description":"Window-local pixel Y of the target window's own get_window_state screenshot (0..screenshot_height); see x."},
                    "element_token": cua_driver_core::tool_schema::element_token_schema(),
                    "capture_id":{"type":"string","description":"Optional one-shot binding to the exact PNG returned by get_window_state or get_desktop_state. When present, x/y are interpreted in that capture and admitted before native dispatch."},
                    // Shape matches the shared button_schema() canon (string +
                    // [left,right,middle]); kept inline to carry the Linux/Wayland
                    // back-compat prose the click button-schema test asserts on.
                    "button":{"type":"string","enum":["left","right","middle"],"description":"Mouse button. Default: \"left\" (legacy back-compat). X11: routed via ButtonPress/Release with the matching evdev code. Native Wayland: only left-button is supported via the virtual-pointer protocol; right/middle return an error."},
                    "count":{"type":"integer","minimum":1,"maximum":3,"description":"Click count — 1 (single), 2 (double), or 3 (triple). Default 1."},
                    "modifier": cua_driver_core::tool_schema::modifier_schema(),
                    "from_zoom":{"type":"boolean","description":"Set true after a zoom call to auto-translate zoom-image pixel coordinates back to full-window space."},
                    "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Coordinate frame (default \"window\"). Pass \"desktop\" with x,y and no pid/window_id for a screen-absolute click in get_desktop_state coordinates."},
                    "coordinate_frame": coordinate_frame_schema(),
                    "delivery_mode": crate::input::delivery::delivery_mode_schema()
                },"additionalProperties":false
            }),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if args.get("capture_id").is_some() {
            return ToolResult::error("capture_id input routing is not yet qualified on the hardened Linux adapter; refresh get_window_state and use an element token or window coordinates")
                .with_structured(json!({"code":"capture_route_unqualified","effect":"refused"}));
        }

        let cursor_id = resolve_cursor_key(&args);
        let modifiers: Vec<String> = args.str_array("modifier");

        // ── Window-less screen-absolute branch (desktop target) ───────────────
        // x,y with NO pid and NO window_id → TRUE SCREEN pixels. Foreground,
        // desktop-scope click (the Linux peer of the Windows WindowFromPoint /
        // macOS global-HID path). The core registry has already enforced the
        // session policy; this branch validates the explicit action form.
        let has_pid = args.get("pid").map(|v| !v.is_null()).unwrap_or(false);
        let has_window_id = args.get("window_id").map(|v| !v.is_null()).unwrap_or(false);
        let has_xy = args.get("x").map(|v| v.is_number()).unwrap_or(false)
            && args.get("y").map(|v| v.is_number()).unwrap_or(false);
        if has_xy && !has_pid && !has_window_id {
            if args.get("element_token").is_some() || args.get("element_index").is_some() {
                return retained_click::unqualified(
                    "element targets cannot use windowless desktop coordinates",
                );
            }
            if args.get("scope").and_then(Value::as_str) != Some("desktop") {
                return ToolResult::error(
                    "click: x,y given with no pid/window_id requires scope=\"desktop\"; \
                     use get_desktop_state to read true screen pixels first."
                        .to_string(),
                )
                .with_structured(json!({
                    "code": "desktop_coordinate_scope_required",
                    "suggestion": "pass scope=desktop",
                }));
            }
            let input = match cua_driver_core::tool_args::parse_legacy_click_input(&args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let button = parse_mouse_button(input.button.unwrap_or(ClickButton::Left).as_str());
            let sx = input.x as i32;
            let sy = input.y as i32;
            let n = input.count.unwrap_or(1) as usize;
            if !(1..=3).contains(&n) {
                return ToolResult::error("click: count must be between 1 and 3")
                    .with_structured(json!({ "code": "invalid_arguments" }));
            }
            let space = match desktop_input_space().await {
                Ok(space) => space,
                Err(error) => {
                    return ToolResult::error(format!("desktop-scope click failed: {error}"))
                }
            };
            // Glide the agent-cursor overlay to the click point first (the macOS
            // / Windows desktop paths already do this). Without it the overlay
            // sits idle elsewhere while only the real pointer warps, so a viewer
            // sees the cursor "click somewhere else."
            // The overlay draws in layout coordinates, like the input below.
            let (overlay_x, overlay_y) = space.to_layout(sx, sy);
            reveal_pointer_action_for(
                &self.state,
                &cursor_id,
                f64::from(overlay_x),
                f64::from(overlay_y),
                true,
            )
            .await;
            let r = cua_driver_core::blocking::spawn(move || {
                if crate::wayland::wayland_input_enabled() {
                    if !modifiers.is_empty() {
                        anyhow::bail!(
                            "modified desktop clicks are unavailable on native Wayland: \
                             the virtual-pointer route cannot carry keyboard modifier state"
                        );
                    }
                    crate::wayland::click_desktop(&space, sx, sy, n as u32, button)
                } else {
                    let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    crate::input::send_click_xtest_desktop_with_modifiers(
                        sx,
                        sy,
                        button,
                        n,
                        &modifier_refs,
                    )
                }
            })
            .await;
            return match r {
                // Screen-absolute click — never driver-verifiable (no
                // read-back); the caller confirms via screenshot.
                Ok(Ok(())) => ToolResult::text(format!(
                    "✅ Sent screen-absolute click at ({sx},{sy}) (desktop scope)."
                ))
                .with_structured(
                    json!({ "path": if crate::wayland::wayland_input_enabled() { "wayland_desktop" } else { "xtest_desktop" }, "verified": false, "effect": "unverifiable" }),
                ),
                Ok(Err(e)) => ToolResult::error(format!("desktop-scope click failed: {e}")),
                Err(e) => ToolResult::error(format!("task error: {e}")),
            };
        }

        let pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        let count = match bounded_click_count_arg(&args) {
            Ok(count) => count as usize,
            Err(err) => return err,
        };
        // Surface 5: reject unknown buttons so a typo can't silently fall through
        // to a left-click. Empty string keeps back-compat with old clients.
        let button_str_raw = args.str_or("button", "left").to_lowercase();
        if !matches!(button_str_raw.as_str(), "" | "left" | "right" | "middle") {
            return ToolResult::error(format!(
                "click: unknown button \"{button_str_raw}\" — expected one of left, right, middle."
            ));
        }
        let button_str = if button_str_raw.is_empty() {
            "left"
        } else {
            button_str_raw.as_str()
        };
        let button = parse_mouse_button(button_str);
        // Argument qualification precedes every platform/capability branch.
        if args.get("element_token").is_some() || args.get("element_index").is_some() {
            if let Err(refusal) = retained_click::qualify(delivery, button, count, &modifiers) {
                return refusal;
            }
        }

        let isolated_background = isolated_hyprland_background(delivery);

        // Surface 6: element_token / element_index precedence resolution.
        // We resolve before the legacy `opt_u64("element_index")` branch
        // so a token-only call (no integer arg) still takes the element path.
        let element_token_arg = args.opt_str("element_token");
        let capture_id_arg = args.opt_str("capture_id");
        let window_id_arg = args.opt_u64("window_id");
        let resolved = match self
            .state
            .snapshots
            .resolve_for_tool(pid as i32, &args, "click")
        {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (elem_idx_resolved, snapshot_identity) = match &resolved {
            cua_driver_core::element_token::ResolvedElement::Element {
                element_index,
                snapshot_identity,
                ..
            } => (Some(*element_index), Some(*snapshot_identity)),
            cua_driver_core::element_token::ResolvedElement::None => (None, None),
        };
        let window_id_resolved: Option<u64> = match &resolved {
            cua_driver_core::element_token::ResolvedElement::Element { window_id, .. } => {
                Some(*window_id)
            }
            cua_driver_core::element_token::ResolvedElement::None => window_id_arg,
        };

        if let Some(idx) = elem_idx_resolved {
            if button != 1 || count != 1 || !modifiers.is_empty() {
                return retained_click::invoke(
                    &self.state,
                    pid,
                    window_id_resolved.expect("element has window"),
                    idx,
                    snapshot_identity.expect("element has identity"),
                    delivery,
                    button,
                    count,
                )
                .await;
            }
        }

        if crate::wayland::is_gnome_wayland_session() && window_id_resolved.is_none() {
            return ToolResult::error(
                "exact_target_required: click on host GNOME requires caller-approved pid and window_id",
            )
            .with_structured(json!({
                "code": "exact_target_required",
                "required": ["pid", "window_id"],
            }));
        }
        let exact_target_proof = if self.state.wayland_input_enabled() {
            let Some(exact_window_id) = window_id_resolved else {
                return ToolResult::error(
                    "exact_target_required: native Wayland click requires caller-approved pid and window_id",
                );
            };
            let state_for_target = self.state.clone();
            match cua_driver_core::blocking::spawn(move || {
                state_for_target.establish_exact_target(pid, exact_window_id)
            })
            .await
            {
                Ok(Ok(proof)) => Some(proof),
                Ok(Err(error)) => return ToolResult::error(error.to_string()),
                Err(error) => return ToolResult::error(format!("Task error: {error}")),
            }
        } else {
            None
        };

        if let Some(idx) = elem_idx_resolved {
            let snapshot_identity = snapshot_identity.expect("element has identity");
            if exact_target_proof.is_none() {
                return self
                    .click_indexed_x11(
                        pid,
                        idx,
                        window_id_resolved,
                        snapshot_identity,
                        1,
                        1,
                        Vec::new(),
                        delivery,
                        cursor_id,
                    )
                    .await;
            }
            let proof = exact_target_proof.expect("checked exact target");
            let task = match self.state.snapshots.spawn_observed_mutation(
                snapshot_identity,
                idx,
                move |_, identity| -> anyhow::Result<(String, bool)> {
                    let identity = identity.ok_or_else(|| {
                        anyhow::anyhow!("stale_element_token: native identity unavailable")
                    })?;
                    let target = crate::atspi::native::resolve_observed_target(
                        pid,
                        idx,
                        proof.window_id(),
                        &identity,
                        Some(proof),
                    )?;
                    target.perform_action(true)
                },
            ) {
                Ok(task) => task,
                Err(error) => return snapshot_publication_error(error),
            };
            return match task.await {
                Ok(Ok((_, suspected_noop))) => {
                    ToolResult::text(format!("Clicked element [{idx}] (pid {pid})."))
                        .with_structured(json!({"path":"ax", "verified":false,
                        "effect":if suspected_noop {"suspected_noop"} else {"unverifiable"}}))
                }
                Ok(Err(error)) => {
                    ToolResult::error(format!("exact Wayland element action refused: {error}"))
                }
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }

        if !isolated_background {
            if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
                return refusal;
            }
        }

        // Coordinate-based path.
        let xid = match args.opt_u64("window_id") {
            Some(v) => v,
            None => return ToolResult::error("Provide either element_token or window_id + x/y."),
        };
        let from_zoom = args.bool_or("from_zoom", false);
        let mut x = args.f64_or("x", 0.0);
        let mut y = args.f64_or("y", 0.0);
        // Resolve the frame first: with a frame, a native Wayland background
        // click can still be delivered through the exact-point AT-SPI route.
        let context = coordinate_click_context(None, || {
            if from_zoom {
                self.state
                    .zoom_context(&args, pid, Some(xid))
                    .map(CoordinateContext::Zoom)
            } else {
                screenshot_scale(&self.state, &args, pid, Some(xid))
                    .map(CoordinateContext::Screenshot)
            }
        });
        match context {
            Ok(CoordinateContext::Zoom(context)) => (x, y) = context.zoom_to_window(x, y),
            Ok(CoordinateContext::Screenshot(scale)) => {
                x *= scale;
                y *= scale;
            }
            Err(refusal) => {
                let native_refusal = (!isolated_background)
                    .then(|| unavailable_wayland_focused_input_background(delivery, true))
                    .flatten();
                let form = background_click_form(button, count, !modifiers.is_empty());
                return missing_frame_click_refusal(native_refusal, refusal, form);
            }
        }

        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        // Resolve the screen point the cursor glides to. Tool coordinates are
        // always window-local screenshot pixels; native Wayland translates
        // through compositor/AT-SPI geometry while X11 uses XTranslateCoordinates.
        let wayland_output_point = if self.state.wayland_input_enabled() {
            Some(
                self.state
                    .window_local_to_output(xid, x.round() as i32, y.round() as i32),
            )
        } else {
            None
        };
        let glide_target = if let Some((sx, sy)) = wayland_output_point {
            Some((sx as f64, sy as f64))
        } else {
            cua_driver_core::blocking::spawn(move || window_local_to_screen(xid, x, y))
                .await
                .ok()
                .and_then(|r| r.ok())
        };
        if let Some((sx, sy)) = glide_target {
            reveal_pointer_action_for(&self.state, &cursor_id, sx, sy, true).await;
        }

        let (xi, yi) = (x as i32, y as i32);
        let (output_x, output_y) = wayland_output_point.unwrap_or((xi, yi));
        if hyprland_foreground(delivery) {
            if !modifiers.is_empty() {
                return foreground_hyprland_refusal("modified clicks are unsupported");
            }
            if window_id_resolved.is_none() {
                return foreground_hyprland_refusal("an exact window_id is required");
            }
            return foreground_hyprland_action(
                &args,
                pid,
                xid,
                crate::wayland::hyprland_input::Action::Click {
                    x,
                    y,
                    button: u32::from(button),
                    count,
                },
            )
            .await;
        }
        if isolated_background {
            if !modifiers.is_empty() {
                return isolated_hyprland_refusal(
                    "modified clicks are unsupported by isolated input",
                );
            }
            if button == 1 && count == 1 {
                let Some(proof) = exact_target_proof.clone() else {
                    return isolated_hyprland_refusal("exact target proof is required");
                };
                let state_for_semantic = self.state.clone();
                let semantic = cua_driver_core::blocking::spawn(move || {
                    exact_point_action_may_fallback(
                        state_for_semantic.point_action(&proof, output_x, output_y),
                    )
                })
                .await;
                match semantic {
                    Ok(Ok(false)) => {
                        return ToolResult::text("Dispatched AT-SPI click.").with_structured(
                            json!({
                                "path": "wayland_atspi", "verified": false, "effect": "unverifiable"
                            }),
                        );
                    }
                    Ok(Ok(true)) => {} // Proven point miss: no semantic input was sent.
                    Ok(Err(error)) => {
                        return ToolResult::error(format!(
                            "exact Wayland point action failed or became indeterminate; refusing coordinate replay: {error}"
                        ));
                    }
                    Err(error) => return ToolResult::error(format!("Task error: {error}")),
                }
            }
            return isolated_hyprland_action(
                &args,
                pid,
                xid,
                crate::wayland::hyprland_input::Action::Click {
                    x,
                    y,
                    button: u32::from(button),
                    count,
                },
            )
            .await;
        }
        let cursor_id_for_task = cursor_id.clone();
        let modifiers_for_task = modifiers.clone();
        let exact_target_for_task = exact_target_proof.clone();
        let state_for_task = self.state.clone();
        // delivery_mode: background (default) = no-focus-steal injection;
        // foreground = activate the target window (EWMH) first, then inject,
        // then restore prior active. Mirrors macOS/Windows.
        let result = cua_driver_core::blocking::spawn(move || -> anyhow::Result<(
            &'static str,
            Option<crate::wayland::shell_helper::ForegroundTerminalOutcome>,
        )> {
            if state_for_task.wayland_input_enabled() {
                if !modifiers_for_task.is_empty() {
                    anyhow::bail!(
                        "modified coordinate clicks are unavailable on native Wayland: \
                         the pointer route cannot carry keyboard modifier state"
                    );
                }
                // Vision/pixel click on native Wayland. Mutter drops synthetic
                // virtual-pointer events (the `wayland::click` warp doesn't land),
                // so for a plain left single click resolve the screen pixel to the
                // covering accessible element and fire its action by
                // `element_index` — the coordinate-free path already verified
                // working. (x,y) are screen coords here, matching the frames in
                // `get_window_state`. Miss → fall through to the injection paths.
                if !delivery.is_foreground() && button == 1 && count == 1 {
                    let proof = exact_target_for_task.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("exact_target_required: native Wayland pixel action omitted proof")
                    })?;
                    match exact_point_action_may_fallback(state_for_task.point_action(
                        proof, output_x, output_y,
                    )) {
                        Ok(false) => return Ok(("wayland_atspi", None)),
                        Ok(true) => {}
                        Err(error) => {
                            return Err(anyhow::anyhow!(
                                "exact Wayland point action failed or became indeterminate; refusing coordinate replay: {error}"
                            ))
                        }
                    }
                }
                if state_for_task.wayland_inject_mode() {
                    // Never reconstruct authority from recyclable `(pid, xid)` here:
                    // carry the caller-established epoch/incarnation proof unchanged
                    // through the AT-SPI miss and into the injection boundary.
                    let target = exact_target_for_task.clone().ok_or_else(|| {
                        anyhow::anyhow!(
                            "exact_target_required: native Wayland click omitted proof"
                        )
                    })?;
                    let outcome = state_for_task.exact_click(
                        target,
                        output_x,
                        output_y,
                        count as u32,
                        button,
                    )?;
                    return Ok(("wayland_cua_compositor", outcome));
                }
                if !delivery.is_foreground() {
                    return Ok(("background_unavailable", None));
                }
                // Native Wayland: focus+raise the target toplevel
                // (foreign-toplevel `activate`), then drive `count` virtual-pointer
                // button events. Wayland injection routes to the compositor focus.
                let target = exact_target_for_task.clone().ok_or_else(|| {
                    anyhow::anyhow!("exact_target_required: native Wayland click omitted proof")
                })?;
                let outcome = state_for_task.exact_click(
                    target,
                    output_x,
                    output_y,
                    count as u32,
                    button,
                )?;
                return Ok(("wayland_activate", outcome));
            }
            // X11 injection. Tiered no-focus-steal delivery (background):
            //   1. Plain left single-click → AT-SPI doAction at that point.
            //   2. Right / middle / double click → real MPX uinput pointer + XI2
            //      shield grab (real Xorg only; skipped on Xvfb/Wayland).
            //   3. Fallback → synthetic XSendEvent.
            // Foreground skips the AT-SPI shortcut and does a real activated pixel
            // click (the agent's escalation when background didn't land).
            let inject = |fg: bool| -> anyhow::Result<&'static str> {
                if !fg && button == 1 && count == 1 && modifiers_for_task.is_empty() {
                    if let Ok(Some(_)) = crate::atspi::perform_action_at_point(pid, xi, yi) {
                        return Ok("x11_atspi");
                    }
                }
                if fg {
                    // Foreground: the window is already activated. Deliver a REAL
                    // XTest warp+button click. Synthetic XSendEvent button events
                    // are dropped by GTK/Qt/Chromium/Firefox, and the MPX uinput
                    // path needs /dev/uinput, which headless X servers
                    // (Xvfb/Xtigervnc) lack — so neither focuses the clicked
                    // widget. XTest is accepted as real input and gives the
                    // widget keyboard focus, so a following type lands.
                    if let Ok((sx, sy)) = window_local_to_screen(xid, xi as f64, yi as f64) {
                        let modifier_refs: Vec<&str> =
                            modifiers_for_task.iter().map(String::as_str).collect();
                        crate::input::send_click_xtest_desktop_with_modifiers(
                            sx.round() as i32,
                            sy.round() as i32,
                            button,
                            count,
                            &modifier_refs,
                        )?;
                        return Ok("x11_xtest_fg");
                    }
                }
                if modifiers_for_task.is_empty() {
                    x11_pixel_click_no_focus_steal(
                        &cursor_id_for_task,
                        xid,
                        xi,
                        yi,
                        button,
                        count,
                    )?;
                } else {
                    let modifier_refs: Vec<&str> =
                        modifiers_for_task.iter().map(String::as_str).collect();
                    x11_pixel_click_no_focus_steal_modifiers(
                        xid,
                        xi,
                        yi,
                        button,
                        count,
                        &modifier_refs,
                    )?;
                }
                Ok(if fg { "x11_pixel_fg" } else { "x11_pixel" })
            };
            let path = if delivery.is_foreground() {
                crate::input::with_x11_foreground(xid, 80, || inject(true))
            } else {
                inject(false)
            }?;
            Ok((path, None))
        })
        .await;
        let mode_label = if delivery.is_foreground() {
            "foreground"
        } else {
            "background"
        };
        match result {
            Ok(Ok(("background_unavailable", _))) => {
                crate::input::delivery::background_unavailable_error(
                    crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                )
            }
            Ok(Ok(("background_unavailable_pointer", _))) => {
                let mut refusal = crate::input::delivery::background_unavailable_error(
                    crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                );
                let hint = "No accessible control covers this point and the toolkit drops \
                     synthetic pointer events, so a background pixel click here would \
                     change nothing. Use get_window_state and click by element_token \
                     (AT-SPI action), or retry with delivery_mode='foreground'.";
                refusal
                    .content
                    .push(cua_driver_core::protocol::Content::text(hint));
                if let Some(structured) = refusal.structured_content.as_mut() {
                    structured["hint"] = json!(hint);
                    structured["path"] = json!("background_unavailable_pointer");
                }
                refusal
            }
            // A pixel/coordinate click is never driver-verifiable (no read-back) —
            // verified:false, effect:"unverifiable"; the caller confirms via
            // screenshot. path reports the rung taken.
            Ok(Ok((path, outcome))) => ToolResult::text(format!(
                "✅ Clicked at ({x:.1}, {y:.1}) × {count} (delivery_mode={mode_label})."
            ))
            .with_structured(with_foreground_diagnostics(
                json!({ "path": path, "verified": false, "effect": "unverifiable" }),
                outcome,
            )),
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── px-focus helper (keyboard family) ───────────────────────────────────────

/// A snapshot-bound keyboard action never rejoins the unaddressed editable,
/// terminal, pixel-focus, or focused-widget fallback ladders below.
#[derive(Clone)]
enum ObservedKeyboardAction {
    Text(String),
    Key { key: String, modifiers: Vec<String> },
}

impl ObservedKeyboardAction {
    fn send_wayland(
        &self,
        guard: &crate::wayland::ExactTargetInputGuard<'_>,
    ) -> anyhow::Result<()> {
        match self {
            Self::Text(text) => crate::wayland::type_text_focused_for_target(guard, text),
            Self::Key { key, modifiers } => match press_key_chord(modifiers, key) {
                Some(chord) => crate::wayland::hotkey_focused_for_target(guard, &chord),
                None => crate::wayland::press_key_focused_for_target(guard, key),
            },
        }
    }

    fn send_nested(&self, xid: u64) -> anyhow::Result<()> {
        match self {
            Self::Text(text) => crate::wayland::inject_type_text(xid, text),
            Self::Key { key, modifiers } => match press_key_chord(modifiers, key) {
                Some(chord) => crate::wayland::inject_hotkey(xid, &chord),
                None => crate::wayland::inject_press_key(xid, key),
            },
        }
    }

    fn send_x11(&self, xid: u64, foreground: bool) -> anyhow::Result<()> {
        match self {
            Self::Text(text) => {
                anyhow::ensure!(
                    foreground,
                    "background text requires a retained EditableText"
                );
                crate::input::send_type_text_xtest(text)
            }
            Self::Key { key, modifiers } => {
                let refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                if foreground {
                    crate::input::send_key_xtest(key, &refs)
                } else {
                    crate::input::send_key(xid, key, &refs)
                }
            }
        }
    }

    fn result(&self, path: &str, foreground: bool) -> ToolResult {
        match self {
            Self::Text(text) => ToolResult::text(format!(
                "Typed {} character(s) into the observed element.", text.chars().count(),
            )).with_structured(type_text_structured(path, text.chars().count(), false)),
            Self::Key { .. } => ToolResult::text("Sent keys to the observed element.")
                .with_structured(json!({"path":path, "verified":false,
                    "effect":"unverifiable", "delivery_mode": if foreground { "foreground" } else { "background" }})),
        }
    }
}

fn observed_keyboard_error(error: anyhow::Error) -> ToolResult {
    let message = format!("{error:#}");
    let result = ToolResult::error(message.clone());
    if message.contains("stale_element_token") {
        result.with_structured(json!({"status":"refused", "effect":"refused",
            "code":"stale_element_token", "refusal":{"code":"stale_element_token", "message":message}}))
    } else {
        result
    }
}

/// Resolve once under the snapshot's generation permit, then move BOTH the
/// retained object and permit into every native worker. Cancellation of an
/// async waiter cannot retire the generation while native input is in flight.
/// Cosmetic cursor positioning is deliberately skipped for this route: the
/// legacy overlay helpers re-walk indices and confer no mutation authority.
async fn invoke_observed_keyboard(
    state: &Arc<ToolState>,
    args: &Value,
    pid: u32,
    xid: u64,
    index: usize,
    snapshot_identity: cua_driver_core::element_token::SnapshotIdentity,
    delivery: crate::input::delivery::DeliveryMode,
    action: ObservedKeyboardAction,
) -> ToolResult {
    let snapshots = state.snapshots.clone();
    let attempt = action.clone();
    let resolved = cua_driver_core::blocking::spawn(move || -> anyhow::Result<_> {
        let (permit, identity) = snapshots
            .acquire_observed_mutation(snapshot_identity, index)
            .map_err(|error| anyhow::anyhow!("stale_element_token: {error}"))?;
        let proof = if crate::wayland::wayland_input_enabled() {
            Some(crate::wayland::establish_exact_target(pid, xid)?)
        } else {
            None
        };
        let target =
            crate::atspi::native::resolve_observed_target(pid, index, xid, &identity, proof)
                .map_err(|error| anyhow::anyhow!("stale_element_token: {error:#}"))?;
        if let ObservedKeyboardAction::Text(text) = &attempt {
            // Renderer bridges can echo EditableText without DOM input events.
            // Native editables retain their stronger focus-free addressed route.
            if !is_chromium_embedder(pid)
                && !is_webkitgtk_embedder(pid)
                && target.type_into_editable(text)?
            {
                return Ok(None);
            }
        }
        Ok(Some((Arc::new(permit), target)))
    })
    .await;
    let retained = match resolved {
        Ok(Ok(retained)) => retained,
        Ok(Err(error)) => return observed_keyboard_error(error),
        Err(error) => return ToolResult::error(format!("Task error: {error}")),
    };
    let Some((permit, target)) = retained else {
        let ObservedKeyboardAction::Text(text) = &action else {
            unreachable!()
        };
        return type_text_ax_result(pid, text.chars().count(), "via retained AT-SPI");
    };

    if hyprland_foreground(delivery) {
        if !crate::wayland::hyprland_input::enabled() {
            return foreground_hyprland_refusal("production Hyprland input plugin is unavailable");
        }
        // Preserve the Hyprland validation gates before any activation/focus.
        let key_action = match &action {
            ObservedKeyboardAction::Text(text) => {
                match crate::wayland::hyprland_input::text_actions(text) {
                    Ok(actions) if !actions.is_empty() => {}
                    Ok(_) => {
                        return foreground_hyprland_refusal("foreground text must not be empty")
                    }
                    Err(error) => return foreground_hyprland_refusal(error.to_string()),
                }
                None
            }
            ObservedKeyboardAction::Key { key, modifiers } => {
                if let Some(keys) = args.get("keys").and_then(Value::as_array) {
                    if keys.iter().any(|key| !key.is_string())
                        || keys
                            .iter()
                            .filter(|key| key.as_str().is_some_and(|key| !is_modifier(key)))
                            .count()
                            != 1
                    {
                        return foreground_hyprland_refusal(
                            "hotkeys require exactly one non-modifier key",
                        );
                    }
                }
                match foreground_hyprland_key(key.clone(), modifiers.clone()) {
                    Ok(action) => Some(action),
                    Err(error) => return error,
                }
            }
        };
        let owner = named_session_cursor_key(args);
        let (_cancellation, dispatch) = match spawn_isolated_hyprland(args, move |cancellation| {
            let _permit = permit;
            target.verify_live()?;
            let activation = crate::wayland::hyprland_input::execute_foreground(
                owner.clone(),
                pid,
                xid,
                crate::wayland::hyprland_input::Action::Activate,
                cancellation.clone(),
            )?;
            if activation["ok"] != true {
                return Ok(activation);
            }
            target.focus()?;
            target.verify_live()?;
            match action {
                ObservedKeyboardAction::Text(text) => {
                    crate::wayland::hyprland_input::execute_foreground_text(
                        owner,
                        pid,
                        xid,
                        &text,
                        cancellation,
                    )
                }
                ObservedKeyboardAction::Key { .. } => {
                    crate::wayland::hyprland_input::execute_foreground(
                        owner,
                        pid,
                        xid,
                        key_action.expect("validated key action"),
                        cancellation,
                    )
                }
            }
        }) {
            Ok(dispatch) => dispatch,
            Err(_) => {
                return foreground_hyprland_refusal("authenticated admitted lifecycle required")
            }
        };
        return match dispatch.await {
            Ok(Err(error)) if format!("{error:#}").contains("stale_element_token") => {
                observed_keyboard_error(error)
            }
            Ok(result) => hyprland_input_result(result, true),
            Err(error) => hyprland_input_result(
                Err(crate::wayland::hyprland_input::unknown_dispatch(
                    error.into(),
                    0,
                )),
                true,
            ),
        };
    }

    let foreground = delivery.is_foreground();
    let result = cua_driver_core::blocking::spawn(move || -> anyhow::Result<ToolResult> {
        // Own the permit through focus, all key injection, and restoration,
        // even when the caller drops the JoinHandle. X11 also shares it with
        // its admission-bounded activation/restoration continuations.
        let _mutation = &permit;
        if crate::wayland::is_inject_mode() {
            target.focus()?;
            target.verify_live()?;
            action.send_nested(xid)?;
            return Ok(action.result("key_events", foreground));
        }
        if crate::wayland::wayland_input_enabled() {
            if !foreground {
                return Ok(crate::input::delivery::background_unavailable_error(
                    crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                ));
            }
            crate::wayland::with_target_foreground(pid, xid, |validate| {
                target.focus()?;
                target.verify_live()?;
                action.send_wayland(validate)
            })?;
            return Ok(action.result("key_events", true));
        }
        if foreground {
            crate::input::foreground::with_x11_foreground_permit(
                xid,
                crate::input::ForegroundOptions::from_settle_hint(80),
                Some(permit.clone()),
                || {
                    target.focus()?;
                    target.verify_live()?;
                    action.send_x11(xid, true)
                },
            )?;
        } else {
            if matches!(action, ObservedKeyboardAction::Text(_)) {
                return Ok(crate::input::delivery::background_unavailable_error(
                    crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                ));
            }
            target.focus()?;
            target.verify_live()?;
            action.send_x11(xid, false)?;
        }
        Ok(action.result(
            if foreground {
                "key_events_fg"
            } else {
                "key_events"
            },
            foreground,
        ))
    })
    .await;
    match result {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => observed_keyboard_error(error),
        Err(error) => ToolResult::error(format!("Task error: {error}")),
    }
}

/// px-focus for the keyboard family (type_text / press_key / hotkey): pixel-click
/// at (x,y) to establish real renderer focus before a keystroke — the *element px
/// action* form of a keyboard tool. Reuses ClickTool's exact coordinate
/// translation + delivery_mode so it lands on the same pixel a px-click would.
/// `Ok(())` on success; `Err(ToolResult)` short-circuits the caller.
///
/// Retains the parent's authenticated session admission for the nested click.
/// `point` is the window-local pixel to click for focus.
async fn focus_by_pixel(
    state: &Arc<ToolState>,
    pid: u32,
    window_id: Option<u64>,
    (x, y): (f64, f64),
    foreground: bool,
    parent_args: &Value,
    from_zoom: bool,
) -> Result<(), ToolResult> {
    let mut click_args = json!({
        "pid": pid, "x": x, "y": y,
        "delivery_mode": if foreground { "foreground" } else { "background" },
    });
    if let Some(wid) = window_id {
        click_args["window_id"] = json!(wid);
    }
    for field in [
        "session",
        "_session_id",
        "_transport_session_id",
        "cursor_id",
    ] {
        if let Some(value) = parent_args.get(field) {
            click_args[field] = value.clone();
        }
    }
    if from_zoom {
        click_args["from_zoom"] = json!(true);
    }
    let focus = ClickTool {
        state: state.clone(),
    }
    .invoke(click_args)
    .await;
    if focus.is_error == Some(true) {
        return Err(focus);
    }
    // Brief settle so the renderer registers focus before the keystrokes.
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    Ok(())
}

/// Establish pixel-local focus inside a nested-compositor target without
/// changing the compositor's focused toplevel. Element routes instead focus
/// their retained Component in invoke_observed_keyboard.
async fn focus_nested_inject_target(
    pid: u32,
    window_id: u64,
    pixel: Option<(f64, f64)>,
) -> Result<(), ToolResult> {
    if let Some((x, y)) = pixel {
        return match cua_driver_core::blocking::spawn(move || {
            let target = crate::wayland::establish_exact_target(pid, window_id)?;
            let (output_x, output_y) = crate::wayland::window_local_to_output(
                target.window_id(),
                x.round() as i32,
                y.round() as i32,
            );
            crate::wayland::click(target, output_x, output_y, 1, 1)
        })
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(ToolResult::error(error.to_string())),
            Err(error) => Err(ToolResult::error(format!("Task error: {error}"))),
        };
    }
    Ok(())
}

// ── type_text ─────────────────────────────────────────────────────────────────

pub struct TypeTextTool {
    state: Arc<ToolState>,
}
static TYPE_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for TypeTextTool {
    fn def(&self) -> &ToolDef {
        TYPE_DEF.get_or_init(|| ToolDef {
            name: "type_text".into(),
            description: "Type text to a window via XSendEvent (KeyPress/KeyRelease). No focus steal.".into(),
            input_schema: json!({
                "type":"object","required":["text"],"properties":{
                    "session": cua_driver_core::tool_schema::session_schema(),
                    "pid":{"type":"integer","description":"Target process ID. Omit with scope \"desktop\" to type into the focused application."},
                    "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y; optional with element_token (the token carries it)."},
                    "text":{"type":"string","description":"Text to type."},
                    "element_token": cua_driver_core::tool_schema::element_token_schema(),
                    "x":{"type":"number","description":"Pixel X of the field to type into — the element px action form. Pass x,y (no element_token) and the tool pixel-clicks there to establish real renderer focus, then types. Use for Chromium/Electron inputs the AX path can't reach. Window-local pixels of the target window's own get_window_state screenshot by default (same convention as click); for get_desktop_state pixels pass scope:\"desktop\" (or coordinate_frame:\"desktop\") — a bare pid+x/y is otherwise misinterpreted as window-local and can focus the wrong widget."},
                    "y":{"type":"number","description":"Pixel Y of the field (see x)."},
                    "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Use \"desktop\" with no pid to type into the focused application. Default \"window\"."},
                    "coordinate_frame": coordinate_frame_schema(),
                    "delivery_mode": crate::input::delivery::delivery_mode_schema()
                },"additionalProperties":false
            }),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if args.get("capture_id").is_some() {
            return ToolResult::error("capture_id input routing is not yet qualified on the hardened Linux adapter; refresh get_window_state and use an element token or window coordinates")
                .with_structured(json!({"code":"capture_route_unqualified","effect":"refused"}));
        }

        use cua_driver_core::tool_args::ArgsExt;
        if args.opt_str("scope").as_deref() == Some("desktop") && args.get("pid").is_none() {
            let input = match parse_typed_projection::<TypeTextInput>("type_text", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let text =
                cua_driver_core::text_sanitize::strip_trailing_agent_protocol_tags(&input.text)
                    .into_owned();
            position_named_session_keyboard_cursor(&self.state, &args, 0, 0, None, None, false)
                .await;
            let wayland = crate::wayland::wayland_input_enabled();
            let path = if wayland { "wayland_focused" } else { "xtest" };
            let result = cua_driver_core::blocking::spawn(move || {
                if wayland {
                    crate::wayland::type_text_focused(&text)
                } else {
                    crate::input::send_type_text_xtest(&text)
                }
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text("Typed text into the focused desktop application.")
                    .with_structured(
                        json!({"scope":"desktop","path":path,"effect":"unverifiable"}),
                    ),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }
        let pid = args.u64_or("pid", 0) as u32;
        let text_raw = match args.require_str("text") {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Strip trailing agent-protocol closing tags — see
        // cua_driver_core::text_sanitize docs for rationale.
        let text = cua_driver_core::text_sanitize::strip_trailing_agent_protocol_tags(&text_raw)
            .into_owned();
        let resolved = match self
            .state
            .snapshots
            .resolve_for_tool(pid as i32, &args, "type_text")
        {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (resolved_elem_idx, resolved_window_id, snapshot_identity) = match &resolved {
            cua_driver_core::element_token::ResolvedElement::Element {
                element_index,
                window_id,
                snapshot_identity,
                ..
            } => (
                Some(*element_index),
                Some(*window_id),
                Some(*snapshot_identity),
            ),
            cua_driver_core::element_token::ResolvedElement::None => (None, None, None),
        };
        let xid_opt = resolved_window_id.or(args.opt_u64("window_id"));

        if crate::wayland::is_gnome_wayland_session() && xid_opt.is_none() {
            return ToolResult::error(
                "exact_target_required: type_text on host GNOME requires caller-approved pid and window_id",
            )
            .with_structured(json!({
                "code": "exact_target_required",
                "required": ["pid", "window_id"],
            }));
        }

        // Resolve XID: use window_id if given, else first window for pid.
        let xid = match xid_opt {
            Some(x) => x,
            None => {
                let windows =
                    cua_driver_core::blocking::spawn(move || crate::x11::list_windows(Some(pid)))
                        .await
                        .unwrap_or_default();
                match windows.first() {
                    Some(w) => w.xid,
                    None => {
                        return ToolResult::error(format!(
                            "No windows found for pid {pid}. Provide window_id."
                        ));
                    }
                }
            }
        };
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        if isolated_hyprland_background(delivery)
            && resolved_elem_idx.is_none()
            && args.get("x").is_none()
            && args.get("y").is_none()
        {
            if xid_opt.is_none() {
                return isolated_hyprland_refusal(
                    "an exact window_id is required for isolated text",
                );
            }
            match crate::wayland::hyprland_input::text_actions(&text) {
                Ok(actions) if !actions.is_empty() => {}
                Ok(_) => return isolated_hyprland_refusal("background text must not be empty"),
                Err(error) => return isolated_hyprland_refusal(error.to_string()),
            }
            position_named_session_keyboard_cursor(&self.state, &args, pid, xid, None, None, true)
                .await;
            let owner = named_session_cursor_key(&args);
            let (_cancellation, dispatch) =
                match spawn_isolated_hyprland(&args, move |cancellation| {
                    crate::wayland::hyprland_input::execute_background_text(
                        owner,
                        pid,
                        xid,
                        &text,
                        cancellation,
                    )
                }) {
                    Ok(dispatch) => dispatch,
                    Err(refusal) => return refusal,
                };
            return match dispatch.await {
                Ok(result) => isolated_hyprland_text_result(result),
                Err(error) => isolated_hyprland_task_error(error, false),
            };
        }
        if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
            return refusal;
        }
        if resolved_elem_idx.is_none() {
            if let Some(refusal) = unavailable_wayland_focused_input_background(delivery, true) {
                return refusal;
            }
        }

        let px = args.get("x").and_then(|value| value.as_f64());
        let py = args.get("y").and_then(|value| value.as_f64());
        if px.is_some() != py.is_some() {
            return ToolResult::error("Pass both x and y to type_text, or neither.");
        }
        if px.is_some() && resolved_elem_idx.is_some() {
            return ToolResult::error(
                "Pass either element_token (ax) or x,y (px) to type_text, not both.",
            );
        }

        if let Some(index) = resolved_elem_idx {
            if let Some(refusal) = unavailable_webkit_keyboard_background(pid, delivery) {
                return refusal;
            }
            return invoke_observed_keyboard(
                &self.state,
                &args,
                pid,
                xid,
                index,
                snapshot_identity.expect("element has identity"),
                delivery,
                ObservedKeyboardAction::Text(text),
            )
            .await;
        }

        if hyprland_foreground(delivery) {
            if xid_opt.is_none() {
                return foreground_hyprland_refusal("an exact window_id is required");
            }
            match crate::wayland::hyprland_input::text_actions(&text) {
                Ok(actions) if !actions.is_empty() => {}
                Ok(_) => return foreground_hyprland_refusal("foreground text must not be empty"),
                Err(error) => return foreground_hyprland_refusal(error.to_string()),
            }
            if let Err(error) =
                focus_hyprland_foreground(&self.state, &args, pid, xid, px.zip(py)).await
            {
                return error;
            }
            if named_session_cursor_key(&args).is_some() {
                if px.is_none() {
                    position_named_session_keyboard_cursor(
                        &self.state,
                        &args,
                        pid,
                        xid,
                        None,
                        None,
                        true,
                    )
                    .await;
                }
            } else {
                announce_keyboard_target(&args, pid, xid, None, px.zip(py)).await;
            }
            let owner = named_session_cursor_key(&args);
            let (_cancellation, dispatch) =
                match spawn_isolated_hyprland(&args, move |cancellation| {
                    crate::wayland::hyprland_input::execute_foreground_text(
                        owner,
                        pid,
                        xid,
                        &text,
                        cancellation,
                    )
                }) {
                    Ok(dispatch) => dispatch,
                    Err(_) => {
                        return foreground_hyprland_refusal(
                            "authenticated admitted lifecycle required",
                        )
                    }
                };
            return hyprland_input_result(
                match dispatch.await {
                    Ok(result) => result,
                    Err(error) => Err(crate::wayland::hyprland_input::unknown_dispatch(
                        error.into(),
                        0,
                    )),
                },
                true,
            );
        }

        position_named_session_keyboard_cursor(
            &self.state,
            &args,
            pid,
            xid,
            None,
            px.zip(py),
            true,
        )
        .await;

        let text_len = text.chars().count();
        // The private nested compositor can target the owning Wayland client
        // directly. Establish widget-local focus first, without changing the
        // compositor's focused toplevel, so keys reach the addressed control.
        if crate::wayland::is_inject_mode() {
            if let Err(error) = focus_nested_inject_target(pid, xid, px.zip(py)).await {
                return error;
            }
            let text_w = text.clone();
            let result = cua_driver_core::blocking::spawn(move || {
                crate::wayland::inject_type_text(xid, &text_w)
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Typed {text_len} character(s) (focus-free via cua-compositor)."
                ))
                .with_structured(type_text_structured(
                    "key_events",
                    text_len,
                    false,
                )),
                Ok(Err(e)) => ToolResult::error(e.to_string()),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        // ── px form: focus by pixel-click, then type into the now-focused element ──
        // Pass x,y (no element_token) for an *element px action*: pixel-click
        // the field to give the renderer the real keyboard focus the AT-SPI path
        // can't, then fall through to the focused-element type path below (it
        // escalates AT-SPI → key events and lands once focused). Reuses ClickTool's
        // exact coordinate translation + delivery_mode.
        if let (Some(cx), Some(cy)) = (px, py) {
            if let Some(refusal) = unavailable_webkit_keyboard_background(pid, delivery) {
                return refusal;
            }
            let from_zoom = args.bool_or("from_zoom", false);
            if let Err(e) = focus_by_pixel(
                &self.state,
                pid,
                Some(xid),
                (cx, cy),
                delivery.is_foreground(),
                &args,
                from_zoom,
            )
            .await
            {
                return e;
            }
            // resolved_elem_idx stays None → the type path below writes to the now-
            // focused element via the background key / AT-SPI rung.
        }

        // Native Wayland: keys go to the *focused* surface (no pid/window
        // targeting in the protocol). Type via the virtual-keyboard tool; pair
        // with a prior `click`/`activate` to focus the intended window.
        if crate::wayland::wayland_input_enabled() {
            if !delivery.is_foreground() {
                return crate::input::delivery::background_unavailable_error(
                    crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                );
            }
            let text_w = text.clone();
            let result =
                cua_driver_core::blocking::spawn(move || {
                    let target = crate::wayland::establish_exact_target(pid, xid)?;
                    crate::wayland::validate_exact_target(&target)?;
                    crate::wayland::with_target_foreground(pid, xid, |validate| {
                        crate::wayland::type_text_focused_for_target(validate, &text_w)
                    })?;
                    Ok::<
                        Option<crate::wayland::shell_helper::ForegroundTerminalOutcome>,
                        anyhow::Error,
                    >(None)
                })
                .await;
            return match result {
                Ok(Ok(outcome)) => ToolResult::text(format!(
                    "Typed {text_len} character(s) (via Wayland virtual-keyboard)."
                ))
                .with_structured(with_foreground_diagnostics(
                    type_text_structured("key_events", text_len, false),
                    outcome,
                )),
                Ok(Err(e)) => ToolResult::error(e.to_string()),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        // Terminal short-circuit: when the target window's WM_CLASS marks it
        // as a terminal emulator (Ghostty / Alacritty / kitty / …), skip the
        // AT-SPI path entirely. Terminals expose an editable text area that
        // AT-SPI `insertText` would aim at, but the write never reaches the
        // pty, so the user sees "type acknowledged but nothing appeared".
        // Try pty-master injection first (most reliable), then XTest key
        // synthesis. Either way the structured response reports
        // `path: "key_events"` so callers can verify the route taken.
        let pid_is_terminal = is_terminal_process(pid);
        let wm_class_is_terminal =
            cua_driver_core::blocking::spawn(move || crate::terminal::is_terminal_window(xid))
                .await
                .unwrap_or(false);
        if pid_is_terminal || wm_class_is_terminal {
            let text_len = text.chars().count();
            let text_t = text.clone();
            let foreground = delivery.is_foreground();
            let result =
                cua_driver_core::blocking::spawn(move || -> anyhow::Result<&'static str> {
                    // pty-master injection is preferred — it skips the X event
                    // queue entirely. Falls through to XTest if the terminal
                    // isn't reachable that way (descendant pty unresolvable).
                    if inject_terminal_input(pid, xid, &text_t)? {
                        return Ok("pty");
                    }
                    if foreground {
                        crate::input::with_x11_foreground(xid, 80, || {
                            crate::input::send_type_text_xtest(&text_t)
                        })?;
                        Ok("key_events_fg")
                    } else {
                        Ok("background_unavailable")
                    }
                })
                .await;
            return match result {
                Ok(Ok("background_unavailable")) => {
                    crate::input::delivery::background_unavailable_error(
                        crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                    )
                }
                Ok(Ok(path)) => ToolResult::text(format!(
                    "Typed {text_len} character(s) (terminal emulator: pty/XTest key events)."
                ))
                .with_structured(type_text_structured(path, text_len, false)),
                Ok(Err(e)) => ToolResult::error(e.to_string()),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }
        // Foreground means the caller explicitly permits activation. Chromium
        // and WebKitGTK can acknowledge an accessibility write without
        // producing the renderer input event, so web embedders use real XTest
        // key events. Native toolkits keep their verifiable AT-SPI path below.
        if delivery.is_foreground() && (is_chromium_embedder(pid) || is_webkitgtk_embedder(pid)) {
            let text_f = text.clone();
            let result = cua_driver_core::blocking::spawn(move || {
                crate::input::with_x11_foreground(xid, 80, || {
                    crate::input::send_type_text_xtest(&text_f)
                })
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Typed {text_len} character(s) (via X11, delivery_mode=foreground)."
                ))
                .with_structured(type_text_structured(
                    "key_events_fg",
                    text_len,
                    false,
                )),
                Ok(Err(e)) => ToolResult::error(e.to_string()),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        // Prefer the focused widget — the element the user just clicked. If a
        // NON-editable input holds keyboard focus (a spreadsheet cell, a
        // terminal, a canvas), the focus-free AT-SPI editable search below would
        // grab the wrong field (e.g. gnumeric's name box instead of the selected
        // cell, or skip a terminal entirely), so synth-type into the focused
        // widget instead: terminals via pty injection, everything else via
        // XSendEvent to the focused window. A focused *editable* (Some(true)) or
        // nothing focused (None) falls through to the existing AT-SPI-first flow.
        let focus_kind = cua_driver_core::blocking::spawn(move || {
            crate::atspi::focused_is_editable(pid).ok().flatten()
        })
        .await
        .ok()
        .flatten();
        if focus_kind == Some(false) {
            if !delivery.is_foreground() {
                return crate::input::delivery::background_unavailable_error(
                    crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                );
            }
            let text_f = text.clone();
            let result = cua_driver_core::blocking::spawn(move || {
                if inject_terminal_input(pid, xid, &text_f)? {
                    return Ok(());
                }
                // XTest (real input to the focused window), NOT XSendEvent: GTK/Qt
                // drop synthetic key events, so a spreadsheet cell / canvas would
                // stay empty. The click that gave this widget focus already put it
                // under the X input focus, so XTest-to-focus lands correctly.
                crate::input::with_x11_foreground(xid, 80, || {
                    crate::input::send_type_text_xtest(&text_f)
                })
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Typed {text_len} character(s) into the focused widget."
                ))
                .with_structured(type_text_structured(
                    "key_events",
                    text_len,
                    false,
                )),
                Ok(Err(e)) => ToolResult::error(e.to_string()),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        // Try AT-SPI EditableText first (focus-free, works for Qt6/GTK4).
        let text_clone = text.clone();
        let atspi_result = cua_driver_core::blocking::spawn(move || {
            crate::atspi::type_into_editable(pid, &text_clone)
        })
        .await;

        match atspi_result {
            Ok(Ok(())) => {
                // AT-SPI succeeded — focus-free typing worked (Qt6, GTK4, etc.)!
                // Electron/Chromium can echo this write without the renderer
                // observing it, so the confirm is suppressed there (mirrors macOS).
                return type_text_ax_result(pid, text_len, "via AT-SPI");
            }
            _ => {
                // AT-SPI failed (no editable exposed). Qt5 doesn't expose widgets
                // when unfocused, so try the synthetic-focus workaround.
            }
        }

        // Qt5 workaround: send synthetic FocusIn to make Qt5's AT-SPI bridge
        // expose the widget tree, type via AT-SPI, then send FocusOut.
        // This doesn't change the X11 active window, so the test's focus check passes.
        let text_clone2 = text.clone();
        let qt5_result = cua_driver_core::blocking::spawn(move || {
            // Send FocusIn to trigger Qt5's bridge
            crate::input::send_focus_in(xid)?;
            std::thread::sleep(std::time::Duration::from_millis(100));

            // Try AT-SPI again now that widgets should be exposed
            let result = crate::atspi::type_into_editable(pid, &text_clone2);

            // Restore state with FocusOut
            crate::input::send_focus_out(xid)?;

            result
        })
        .await;

        match qt5_result {
            Ok(Ok(())) => {
                return type_text_ax_result(pid, text_len, "via AT-SPI with focus workaround");
            }
            _ => {
                // AT-SPI still didn't work. Fall back to X11 XSendEvent.
            }
        }

        // Track which path the final fallback chain took, so the
        // structured response stays honest. The closure can't borrow
        // a local mutably across `spawn_blocking`, so funnel the
        // decision through the success type instead.
        // delivery_mode: background (default) = focus-free AT-SPI / XSendEvent;
        // foreground = activate the window (EWMH), then synthesize REAL key
        // events to it via XTest — the escalation when background didn't land
        // (e.g. a GTK dialog whose widget ignores synthetic XSendEvent keys).
        let result = cua_driver_core::blocking::spawn(move || -> anyhow::Result<&'static str> {
            // Terminals: write to the pty master (focus-free, below the toolkit).
            if inject_terminal_input(pid, xid, &text)? {
                return Ok("key_events");
            }
            // GUI apps: X11 only routes keystrokes to the *focused* toplevel's
            // focused widget, so background XSendEvent typing doesn't land. Fill
            // the editable field via AT-SPI instead — focus-free and toolkit-
            // agnostic. Fall back to Tk send or XSendEvent when no a11y field is exposed.
            if crate::atspi::insert_text(pid, &text).unwrap_or(false) {
                return Ok("ax");
            }
            // Tk apps: use Tk's `send` command (no AT-SPI bridge, so AT-SPI above
            // returned false). This is the Tk-specific override, like CDP for Chromium.
            if crate::input::inject_tk_send(&text).unwrap_or(false) {
                return Ok("key_events");
            }
            if delivery.is_foreground() {
                crate::input::with_x11_foreground(xid, 80, || {
                    crate::input::send_type_text_xtest(&text)
                })?;
                Ok("key_events_fg")
            } else {
                Ok("background_unavailable")
            }
        })
        .await;
        let mode_label = if delivery.is_foreground() {
            "foreground"
        } else {
            "background"
        };
        match result {
            // AT-SPI's boolean acknowledges the EditableText call; it is not a
            // fresh value readback. Keep the result unverifiable and apply the
            // stricter Chromium escalation where appropriate.
            Ok(Ok("ax")) => type_text_ax_result(
                pid,
                text_len,
                &format!("via X11, delivery_mode={mode_label}"),
            ),
            Ok(Ok("background_unavailable")) => {
                crate::input::delivery::background_unavailable_error(
                    crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                )
            }
            Ok(Ok(path)) => ToolResult::text(format!(
                "Typed {text_len} character(s) (via X11, delivery_mode={mode_label})."
            ))
            .with_structured(type_text_structured(path, text_len, false)),
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── press_key ─────────────────────────────────────────────────────────────────

pub struct PressKeyTool {
    state: Arc<ToolState>,
}
static PRESS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// Promote a modified single-key request to the chord expected by keyboard
/// backends that do not accept a separate modifier list.
fn press_key_chord(mods: &[String], key: &str) -> Option<Vec<String>> {
    if mods.is_empty() {
        return None;
    }
    let mut chord = mods.to_vec();
    chord.push(key.to_owned());
    Some(chord)
}

#[cfg(test)]
mod press_key_tests {
    use super::press_key_chord;

    #[test]
    fn unmodified_press_stays_on_the_single_key_route() {
        assert_eq!(press_key_chord(&[], "return"), None);
    }

    #[test]
    fn modifiers_are_promoted_to_a_chord_in_order() {
        assert_eq!(
            press_key_chord(&["ctrl".to_owned()], "s"),
            Some(vec!["ctrl".to_owned(), "s".to_owned()])
        );
        assert_eq!(
            press_key_chord(&["ctrl".to_owned(), "shift".to_owned()], "t"),
            Some(vec!["ctrl".to_owned(), "shift".to_owned(), "t".to_owned()])
        );
    }
}

#[async_trait]
impl Tool for PressKeyTool {
    fn def(&self) -> &ToolDef {
        PRESS_DEF.get_or_init(|| ToolDef {
            name: "press_key".into(),
            description: "Press a key via XSendEvent to a window. No focus steal.".into(),
            input_schema: json!({
                "type":"object","required":["key"],"properties":{
                    "session": cua_driver_core::tool_schema::session_schema(),
                    "pid":{"type":"integer","description":"Target process ID. Omit with scope \"desktop\" to send the key to the focused application."},
                    "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y or element_index; optional with element_token (the token carries it)."},
                    "key":{"type":"string","description":"Key name: enter/return, tab, escape, space, backspace, delete, insert, home, end, pageup, pagedown, up, down, left, right, f1-f12, or any single ASCII character."},
                    "modifiers":{"type":"array","items":{"type":"string"},"description":"Modifier keys held while the key is pressed: ctrl, shift, alt, super."},
                    "element_token": cua_driver_core::tool_schema::element_token_schema(),
                    "x":{"type":"number","description":"Pixel X — the element px action form: pixel-click there to focus, then send the key. Use when the key must go to a Chromium/Electron surface the AX path can't focus. Pass with y, no element_token. Window-local pixels by default (same convention as click); for get_desktop_state pixels pass scope:\"desktop\" (or coordinate_frame:\"desktop\")."},
                    "y":{"type":"number","description":"Pixel Y (see x)."},
                    "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Use \"desktop\" with no pid to send the key to the focused application. Default \"window\"."},
                    "coordinate_frame": coordinate_frame_schema(),
                    "delivery_mode": crate::input::delivery::delivery_mode_schema()
                },"additionalProperties":false
            }),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if args.get("capture_id").is_some() {
            return ToolResult::error("capture_id input routing is not yet qualified on the hardened Linux adapter; refresh get_window_state and use an element token or window coordinates")
                .with_structured(json!({"code":"capture_route_unqualified","effect":"refused"}));
        }

        use cua_driver_core::tool_args::ArgsExt;
        if args.opt_str("scope").as_deref() == Some("desktop") && args.get("pid").is_none() {
            let input = match parse_typed_projection::<PressKeyInput>("press_key", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let key = input.key;
            let display = key.clone();
            let modifiers = input.modifiers.unwrap_or_default();
            position_named_session_keyboard_cursor(&self.state, &args, 0, 0, None, None, false)
                .await;
            let wayland = crate::wayland::wayland_input_enabled();
            let path = if wayland { "wayland_focused" } else { "xtest" };
            let result = cua_driver_core::blocking::spawn(move || {
                if wayland && modifiers.is_empty() {
                    crate::wayland::press_key_focused(&key)
                } else if wayland {
                    let mut keys = modifiers;
                    keys.push(key);
                    crate::wayland::hotkey_focused(&keys)
                } else {
                    let refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    crate::input::send_key_xtest(&key, &refs)
                }
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!("Pressed desktop key '{display}'."))
                    .with_structured(
                        json!({"scope":"desktop","path":path,"effect":"unverifiable"}),
                    ),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }
        let pid = args.u64_or("pid", 0) as u32;
        let key = match args.require_str("key") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let mods: Vec<String> = args.str_array("modifiers");

        // Resolve the element token into both its owning window and exact
        // child. Foreground delivery establishes child focus inside the
        // verified top-level activation transaction; background XSendEvent
        // retains the historical direct-window path.
        let window_id_arg = args.opt_u64("window_id");
        let resolved = match self
            .state
            .snapshots
            .resolve_for_tool(pid as i32, &args, "press_key")
        {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (resolved_element_index, snapshot_identity) = match &resolved {
            cua_driver_core::element_token::ResolvedElement::Element {
                element_index,
                snapshot_identity,
                ..
            } => (Some(*element_index), Some(*snapshot_identity)),
            cua_driver_core::element_token::ResolvedElement::None => (None, None),
        };
        let xid_opt = match &resolved {
            cua_driver_core::element_token::ResolvedElement::Element { window_id, .. } => {
                Some(*window_id)
            }
            cua_driver_core::element_token::ResolvedElement::None => window_id_arg,
        };
        if crate::wayland::is_gnome_wayland_session() && xid_opt.is_none() {
            return ToolResult::error(
                "exact_target_required: press_key on host GNOME requires caller-approved pid and window_id",
            )
            .with_structured(json!({
                "code": "exact_target_required",
                "required": ["pid", "window_id"],
            }));
        }
        let xid = match xid_opt {
            Some(x) => x,
            None => {
                let windows =
                    cua_driver_core::blocking::spawn(move || crate::x11::list_windows(Some(pid)))
                        .await
                        .unwrap_or_default();
                match windows.first() {
                    Some(w) => w.xid,
                    None => {
                        return ToolResult::error(format!(
                            "No windows found for pid {pid}. Provide window_id."
                        ));
                    }
                }
            }
        };
        // Revalidate the exact PID/window pair at the mutation boundary. The
        // outer wrapper prevents malformed requests and stale explicit targets,
        // but ownership can change while element resolution or candidate lookup
        // runs. Never emit X11 or Wayland input from an earlier observation.
        let action_target_owned =
            cua_driver_core::blocking::spawn(move || explicit_window_belongs_to_pid(pid, xid))
                .await
                .unwrap_or(false);
        if !action_target_owned {
            return ToolResult::error(format!(
                "window_id {xid} is stale or no longer belongs to pid {pid}."
            ))
            .with_structured(json!({
                "code": "window_target_mismatch",
                "effect": "refused",
                "pid": pid,
                "window_id": xid,
            }));
        }

        // ── px form: pixel-click to focus, then the key goes to the focused element ──
        // Reuses click's translation + delivery_mode; after it, deliver via the plain
        // background path (the focus-click already handled fronting when fg). Pass x,y
        // (no element_token) for Chromium/Electron surfaces the AX path can't focus.
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        if isolated_hyprland_background(delivery) {
            if xid_opt.is_none() {
                return isolated_hyprland_refusal(
                    "an exact window_id is required for isolated keys",
                );
            }
            if resolved_element_index.is_some()
                || args.get("x").is_some()
                || args.get("y").is_some()
            {
                return isolated_hyprland_refusal(
                    "isolated keys address the exact top-level; first click the child explicitly",
                );
            }
            position_named_session_keyboard_cursor(&self.state, &args, pid, xid, None, None, false)
                .await;
            return isolated_hyprland_action(
                &args,
                pid,
                xid,
                crate::wayland::hyprland_input::Action::Key {
                    key,
                    modifiers: mods,
                },
            )
            .await;
        }
        if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
            return refusal;
        }

        if let Some(refusal) = unavailable_webkit_keyboard_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_gtk_keyboard_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_wayland_focused_input_background(delivery, true) {
            return refusal;
        }

        let px = args.get("x").and_then(|value| value.as_f64());
        let py = args.get("y").and_then(|value| value.as_f64());
        if px.is_some() != py.is_some() {
            return ToolResult::error("Pass both x and y to press_key, or neither.");
        }
        if px.is_some() && resolved_element_index.is_some() {
            return ToolResult::error(
                "Pass either element_token (ax) or x,y (px) to press_key, not both.",
            );
        }

        if let Some(index) = resolved_element_index {
            return invoke_observed_keyboard(
                &self.state,
                &args,
                pid,
                xid,
                index,
                snapshot_identity.expect("element has identity"),
                delivery,
                ObservedKeyboardAction::Key {
                    key,
                    modifiers: mods,
                },
            )
            .await;
        }

        if hyprland_foreground(delivery) {
            if xid_opt.is_none() {
                return foreground_hyprland_refusal("an exact window_id is required");
            }
            let action = match foreground_hyprland_key(key, mods) {
                Ok(action) => action,
                Err(error) => return error,
            };
            if let Err(error) =
                focus_hyprland_foreground(&self.state, &args, pid, xid, px.zip(py)).await
            {
                return error;
            }
            if px.is_none() {
                position_named_session_keyboard_cursor(
                    &self.state,
                    &args,
                    pid,
                    xid,
                    None,
                    None,
                    false,
                )
                .await;
            }
            return foreground_hyprland_action(&args, pid, xid, action).await;
        }

        position_named_session_keyboard_cursor(
            &self.state,
            &args,
            pid,
            xid,
            None,
            px.zip(py),
            false,
        )
        .await;

        // Nested cua-compositor addresses the owning Wayland client directly.
        // Preserve legacy modifiers by promoting the request to a chord.
        if crate::wayland::is_inject_mode() {
            if let Err(error) = focus_nested_inject_target(pid, xid, px.zip(py)).await {
                return error;
            }
            let result = if mods.is_empty() {
                let key_w = key.clone();
                cua_driver_core::blocking::spawn(move || {
                    crate::wayland::inject_press_key(xid, &key_w)
                })
                .await
            } else {
                let mut chord = mods.clone();
                chord.push(key.clone());
                cua_driver_core::blocking::spawn(move || crate::wayland::inject_hotkey(xid, &chord))
                    .await
            };
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Pressed key '{key}' (focus-free via cua-compositor)."
                )),
                Ok(Err(e)) => ToolResult::error(e.to_string()),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        let px_target = {
            if let (Some(cx), Some(cy)) = (px, py) {
                let from_zoom = args.bool_or("from_zoom", false);
                if let Err(e) = focus_by_pixel(
                    &self.state,
                    pid,
                    Some(xid),
                    (cx, cy),
                    delivery.is_foreground(),
                    &args,
                    from_zoom,
                )
                .await
                {
                    return e;
                }
                Some((cx.round() as i32, cy.round() as i32))
            } else {
                None
            }
        };

        // Native Wayland: send the key to the focused surface via virtual-keyboard.
        if crate::wayland::wayland_input_enabled() {
            let key_w = key.clone();
            let chord = press_key_chord(&mods, &key);
            let result =
                cua_driver_core::blocking::spawn(move || {
                    let target = crate::wayland::establish_exact_target(pid, xid)?;
                    crate::wayland::validate_exact_target(&target)?;
                    crate::wayland::with_target_foreground(pid, xid, |validate| match chord {
                        Some(keys) => crate::wayland::hotkey_focused_for_target(validate, &keys),
                        None => crate::wayland::press_key_focused_for_target(validate, &key_w),
                    })?;
                    Ok::<
                        Option<crate::wayland::shell_helper::ForegroundTerminalOutcome>,
                        anyhow::Error,
                    >(None)
                })
                .await;
            return match result {
                Ok(Ok(outcome)) => ToolResult::text(format!(
                    "Pressed key '{key}' (via Wayland virtual-keyboard)."
                ))
                .with_structured(with_foreground_diagnostics(json!({}), outcome)),
                Ok(Err(e)) => ToolResult::error(e.to_string()),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        let key_for_task = key.clone();
        // Foreground delivery is one atomic activate-and-XTest transaction.
        // A preceding PX click establishes internal widget focus, but that
        // click restores the prior top-level before returning.
        let deliver_fg = delivery.is_foreground();
        let result = cua_driver_core::blocking::spawn(move || -> anyhow::Result<()> {
            if resolved_element_index.is_none()
                && mods.is_empty()
                && is_enter_key(&key_for_task)
                && inject_terminal_input(pid, xid, "\n")?
            {
                return Ok(());
            }
            let m: Vec<&str> = mods.iter().map(String::as_str).collect();
            // foreground: activate the window first, then inject a REAL key via
            // XTest. Synthetic XSendEvent keys (`send_key`) are dropped by
            // GTK/Qt/Chromium/Firefox, so the foreground rung must use XTest —
            // it delivers to the now-focused window. background = direct
            // XSendEvent (no focus steal) for apps that accept it.
            if deliver_fg {
                return crate::input::with_x11_foreground(xid, 80, || {
                    crate::input::send_key_xtest(&key_for_task, &m)
                });
            }
            if let Some((x, y)) = px_target {
                crate::input::send_key_at(xid, x, y, &key_for_task, &m)
            } else {
                crate::input::send_key(xid, &key_for_task, &m)
            }
        })
        .await;
        let mode_label = if deliver_fg {
            "foreground"
        } else {
            "background"
        };
        match result {
            Ok(Ok(())) => {
                ToolResult::text(format!("Pressed key '{key}' (delivery_mode={mode_label})."))
                    .with_structured(json!({ "verified": false, "delivery_mode": mode_label }))
            }
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── hotkey ────────────────────────────────────────────────────────────────────

fn is_modifier(k: &str) -> bool {
    matches!(
        k.to_lowercase().as_str(),
        "ctrl"
            | "control"
            | "shift"
            | "alt"
            | "super"
            | "meta"
            | "cmd"
            | "command"
            | "win"
            | "windows"
    )
}

pub struct HotkeyTool {
    state: Arc<ToolState>,
}
static HOTKEY_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for HotkeyTool {
    fn def(&self) -> &ToolDef {
        HOTKEY_DEF.get_or_init(|| ToolDef {
            name: "hotkey".into(),
            description: "Press a combination of keys simultaneously, e.g. [\"ctrl\",\"c\"] for Copy. \
                Sent via XSendEvent directly to the target pid; target does NOT need to be frontmost.".into(),
            input_schema: json!({
                "type":"object","required":["keys"],"properties":{
                    "session": cua_driver_core::tool_schema::session_schema(),
                    "pid":{"type":"integer","description":"Target process ID. Omit with scope \"desktop\" to send the chord to the focused application."},
                    "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y or element_index; optional with element_token (the token carries it)."},
                    "keys":{"type":"array","items":{"type":"string"},"minItems":2,
                        "description":"Modifier(s) + one non-modifier key, e.g. [\"ctrl\",\"c\"]."},
                    "element_token": cua_driver_core::tool_schema::element_token_schema(),
                    "x":{"type":"number","description":"Screenshot-pixel X — the element px action form: pixel-click there to focus, then send the combo (so e.g. Ctrl+V pastes into that field). Pass with y. Use for Chromium/Electron surfaces the background combo can't reach."},
                    "y":{"type":"number","description":"Screenshot-pixel Y (see x)."},
                    "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Use \"desktop\" with no pid to send the chord to the focused application. Default \"window\"."},
                    "delivery_mode": crate::input::delivery::delivery_mode_schema()
                },"additionalProperties":false
            }),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if args.get("capture_id").is_some() {
            return ToolResult::error("capture_id input routing is not yet qualified on the hardened Linux adapter; refresh get_window_state and use an element token or window coordinates")
                .with_structured(json!({"code":"capture_route_unqualified","effect":"refused"}));
        }

        use cua_driver_core::tool_args::ArgsExt;
        if args.opt_str("scope").as_deref() == Some("desktop") && args.get("pid").is_none() {
            let input = match parse_typed_projection::<HotkeyInput>("hotkey", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let keys = input.keys;
            if keys.len() < 2 {
                return ToolResult::error("hotkey.keys must contain at least two keys.")
                    .with_structured(json!({ "code": "invalid_arguments" }));
            }
            let modifiers: Vec<String> = keys
                .iter()
                .filter(|key| is_modifier(key))
                .cloned()
                .collect();
            let Some(key) = keys.iter().rev().find(|key| !is_modifier(key)).cloned() else {
                return ToolResult::error("keys must include at least one non-modifier key.");
            };
            let display = keys.join("+");
            position_named_session_keyboard_cursor(&self.state, &args, 0, 0, None, None, false)
                .await;
            let wayland = crate::wayland::wayland_input_enabled();
            let path = if wayland { "wayland_focused" } else { "xtest" };
            let result = cua_driver_core::blocking::spawn(move || {
                if wayland {
                    crate::wayland::hotkey_focused(&keys)
                } else {
                    let refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    crate::input::send_key_xtest(&key, &refs)
                }
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!("Pressed desktop hotkey {display}."))
                    .with_structured(
                        json!({"scope":"desktop","path":path,"effect":"unverifiable"}),
                    ),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }
        let pid = args.u64_or("pid", 0) as u32;
        let window_id_arg = args.opt_u64("window_id");
        let resolved = match self
            .state
            .snapshots
            .resolve_for_tool(pid as i32, &args, "hotkey")
        {
            Ok(resolved) => resolved,
            Err(error) => return error,
        };
        let (resolved_element_index, snapshot_identity) = match &resolved {
            cua_driver_core::element_token::ResolvedElement::Element {
                element_index,
                snapshot_identity,
                ..
            } => (Some(*element_index), Some(*snapshot_identity)),
            cua_driver_core::element_token::ResolvedElement::None => (None, None),
        };
        let xid_opt = match &resolved {
            cua_driver_core::element_token::ResolvedElement::Element { window_id, .. } => {
                Some(*window_id)
            }
            cua_driver_core::element_token::ResolvedElement::None => window_id_arg,
        };

        if crate::wayland::is_gnome_wayland_session() && xid_opt.is_none() {
            return ToolResult::error(
                "exact_target_required: hotkey on host GNOME requires caller-approved pid and window_id",
            )
            .with_structured(json!({
                "code": "exact_target_required",
                "required": ["pid", "window_id"],
            }));
        }

        // Resolve XID: use window_id if given, else first window for pid.
        let xid = match xid_opt {
            Some(x) => x,
            None => {
                let windows =
                    cua_driver_core::blocking::spawn(move || crate::x11::list_windows(Some(pid)))
                        .await
                        .unwrap_or_default();
                match windows.first() {
                    Some(w) => w.xid,
                    None => {
                        return ToolResult::error(format!(
                            "No windows found for pid {pid}. Provide window_id."
                        ));
                    }
                }
            }
        };

        // Parse keys array (preferred) or fall back to legacy key+modifiers.
        let (key, mods) = if let Some(arr) = args.get("keys").and_then(|v| v.as_array()) {
            let keys: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            let modifiers: Vec<String> = keys.iter().filter(|k| is_modifier(k)).cloned().collect();
            let non_mods: Vec<String> = keys.iter().filter(|k| !is_modifier(k)).cloned().collect();
            if non_mods.is_empty() {
                return ToolResult::error("keys must include at least one non-modifier key.");
            }
            (non_mods.last().unwrap().clone(), modifiers)
        } else if let Some(k) = args.opt_str("key") {
            let mods: Vec<String> = args.str_array("modifiers");
            (k, mods)
        } else {
            return ToolResult::error(
                "Provide 'keys' array (e.g. [\"ctrl\",\"c\"]) or 'key'+'modifiers' parameters.",
            );
        };

        let key_display = format!("{}+{}", mods.join("+"), key);
        let key_for_wayland = key.clone();
        let mods_for_wayland = mods.clone();
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);

        if isolated_hyprland_background(delivery) {
            if xid_opt.is_none() {
                return isolated_hyprland_refusal(
                    "an exact window_id is required for isolated hotkeys",
                );
            }
            if resolved_element_index.is_some()
                || args.get("x").is_some()
                || args.get("y").is_some()
            {
                return isolated_hyprland_refusal("isolated hotkeys address the exact top-level; first click the child explicitly");
            }
            if let Some(keys) = args.get("keys").and_then(Value::as_array) {
                if keys
                    .iter()
                    .filter(|value| value.as_str().is_some_and(|key| !is_modifier(key)))
                    .count()
                    != 1
                    || keys.iter().any(|value| !value.is_string())
                {
                    return isolated_hyprland_refusal(
                        "isolated hotkeys require exactly one non-modifier key",
                    );
                }
            }
            position_named_session_keyboard_cursor(&self.state, &args, pid, xid, None, None, false)
                .await;
            return isolated_hyprland_action(
                &args,
                pid,
                xid,
                crate::wayland::hyprland_input::Action::Key {
                    key,
                    modifiers: mods,
                },
            )
            .await;
        }
        if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_webkit_keyboard_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_gtk_keyboard_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_wayland_focused_input_background(delivery, true) {
            return refusal;
        }

        let px = args.get("x").and_then(|value| value.as_f64());
        let py = args.get("y").and_then(|value| value.as_f64());
        if px.is_some() != py.is_some() {
            return ToolResult::error("Pass both x and y to hotkey, or neither.");
        }
        if px.is_some() && resolved_element_index.is_some() {
            return ToolResult::error(
                "Pass either element_token (ax) or x,y (px) to hotkey, not both.",
            );
        }

        if let Some(index) = resolved_element_index {
            return invoke_observed_keyboard(
                &self.state,
                &args,
                pid,
                xid,
                index,
                snapshot_identity.expect("element has identity"),
                delivery,
                ObservedKeyboardAction::Key {
                    key,
                    modifiers: mods,
                },
            )
            .await;
        }

        if hyprland_foreground(delivery) {
            if xid_opt.is_none() {
                return foreground_hyprland_refusal("an exact window_id is required");
            }
            if let Some(keys) = args.get("keys").and_then(Value::as_array) {
                if keys.iter().any(|key| !key.is_string())
                    || keys
                        .iter()
                        .filter(|key| key.as_str().is_some_and(|key| !is_modifier(key)))
                        .count()
                        != 1
                {
                    return foreground_hyprland_refusal(
                        "hotkeys require exactly one non-modifier key",
                    );
                }
            }
            let action = match foreground_hyprland_key(key, mods) {
                Ok(action) => action,
                Err(error) => return error,
            };
            if let Err(error) =
                focus_hyprland_foreground(&self.state, &args, pid, xid, px.zip(py)).await
            {
                return error;
            }
            if px.is_none() {
                position_named_session_keyboard_cursor(
                    &self.state,
                    &args,
                    pid,
                    xid,
                    None,
                    None,
                    false,
                )
                .await;
            }
            return foreground_hyprland_action(&args, pid, xid, action).await;
        }

        position_named_session_keyboard_cursor(
            &self.state,
            &args,
            pid,
            xid,
            None,
            px.zip(py),
            false,
        )
        .await;

        if crate::wayland::is_inject_mode() {
            if let Err(error) = focus_nested_inject_target(pid, xid, px.zip(py)).await {
                return error;
            }
            let mut chord = mods.clone();
            chord.push(key.clone());
            let result = cua_driver_core::blocking::spawn(move || {
                crate::wayland::inject_hotkey(xid, &chord)
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Pressed hotkey '{key_display}' (focus-free via cua-compositor)."
                )),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }

        // ── px form: pixel-click to focus, then the combo acts on the focused field ──
        // (e.g. Ctrl+V into a Chromium input). Reuses click's translation +
        // delivery_mode; after it, deliver the combo via the plain background path
        // (the focus-click already fronted when fg).
        let px_target = {
            if let (Some(cx), Some(cy)) = (px, py) {
                let from_zoom = args.bool_or("from_zoom", false);
                if let Err(e) = focus_by_pixel(
                    &self.state,
                    pid,
                    Some(xid),
                    (cx, cy),
                    delivery.is_foreground(),
                    &args,
                    from_zoom,
                )
                .await
                {
                    return e;
                }
                Some((cx.round() as i32, cy.round() as i32))
            } else {
                None
            }
        };
        let deliver_fg = delivery.is_foreground();

        let result = cua_driver_core::blocking::spawn(move || -> anyhow::Result<
            Option<crate::wayland::shell_helper::ForegroundTerminalOutcome>,
        > {
            if crate::wayland::wayland_input_enabled() {
                // Native Wayland: route the modifier combo through wtype's
                // -M/-k/-m sequence — the closest equivalent to the X11
                // state-mask path. window_id is irrelevant once focused.
                let mut combo: Vec<String> = mods_for_wayland.clone();
                combo.push(key_for_wayland.clone());
                let target = crate::wayland::establish_exact_target(pid, xid)?;
                return crate::wayland::hotkey_with_outcome(target, &combo);
            }
            let m: Vec<&str> = mods.iter().map(String::as_str).collect();
            // foreground: activate the target first, then inject the accelerator
            // as REAL key events via XTest. Synthetic XSendEvent keys
            // (`send_key`) are dropped by GTK/Qt/Chromium/Firefox, so the
            // foreground rung must use XTest, which reaches the focused window.
            if deliver_fg {
                return crate::input::with_x11_foreground(xid, 80, || {
                    crate::input::send_key_xtest(&key, &m)
                })
                .map(|()| None);
            }
            if let Some((x, y)) = px_target {
                crate::input::send_key_at(xid, x, y, &key, &m)
            } else {
                crate::input::send_key(xid, &key, &m)
            }?;
            Ok(None)
        })
        .await;
        let mode_label = if deliver_fg {
            "foreground"
        } else {
            "background"
        };
        match result {
            Ok(Ok(outcome)) => ToolResult::text(format!(
                "Pressed {key_display} on pid {pid} (delivery_mode={mode_label})."
            ))
            .with_structured(with_foreground_diagnostics(
                json!({ "verified": false, "delivery_mode": mode_label }),
                outcome,
            )),
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── set_value ─────────────────────────────────────────────────────────────────

pub struct SetValueTool {
    state: Arc<ToolState>,
}
static SV_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for SetValueTool {
    fn def(&self) -> &ToolDef {
        SV_DEF.get_or_init(|| ToolDef {
            name: "set_value".into(),
            description: "Set value of an AT-SPI element via SetValue action.".into(),
            input_schema: json!({
                "type":"object","required":["pid","value"],"properties":{
                    "session": cua_driver_core::tool_schema::session_schema(),
                    "pid":{"type":"integer","description":"Target process ID."},
                    "window_id":{"type":"integer","description":"Omit when element_token is supplied (the token carries it)."},
                    "element_token": cua_driver_core::tool_schema::element_token_schema(),
                    "value":{"type":"string","description":"New value for the element."}
                },"additionalProperties":false
            }),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if args.get("capture_id").is_some() {
            return ToolResult::error("capture_id input routing is not yet qualified on the hardened Linux adapter; refresh get_window_state and use an element token or window coordinates")
                .with_structured(json!({"code":"capture_route_unqualified","effect":"refused"}));
        }

        use cua_driver_core::tool_args::ArgsExt;
        let pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let value = match args.require_str("value") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let resolved = match self
            .state
            .snapshots
            .resolve_for_tool(pid as i32, &args, "set_value")
        {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (idx, resolved_window_id, snapshot_identity) = match &resolved {
            cua_driver_core::element_token::ResolvedElement::Element {
                element_index,
                window_id,
                snapshot_identity,
                ..
            } => (*element_index, Some(*window_id), *snapshot_identity),
            cua_driver_core::element_token::ResolvedElement::None => {
                return ToolResult::error(
                    "set_value requires element_token to address the target element.",
                )
            }
        };
        let exact_window_id = args.opt_u64("window_id").or(resolved_window_id);
        if crate::wayland::is_gnome_wayland_session() && exact_window_id.is_none() {
            return ToolResult::error(
                "exact_target_required: set_value on host GNOME requires caller-approved pid and window_id",
            )
            .with_structured(json!({
                "code": "exact_target_required",
                "required": ["pid", "window_id"],
            }));
        }
        let exact_target_proof = if self.state.wayland_input_enabled() {
            let Some(exact_window_id) = exact_window_id else {
                return ToolResult::error(
                    "exact_target_required: native Wayland set_value requires caller-approved pid and window_id",
                );
            };
            let state_for_target = self.state.clone();
            match cua_driver_core::blocking::spawn(move || {
                state_for_target.establish_exact_target(pid, exact_window_id)
            })
            .await
            {
                Ok(Ok(proof)) => Some(proof),
                Ok(Err(error)) => return ToolResult::error(error.to_string()),
                Err(error) => return ToolResult::error(format!("Task error: {error}")),
            }
        } else {
            None
        };
        let value_for_task = value.clone();
        let xid = exact_window_id.unwrap_or(0);
        position_named_session_keyboard_cursor(&self.state, &args, pid, xid, Some(idx), None, true)
            .await;
        let proof_for_value = exact_target_proof.clone();
        let state_for_value = self.state.clone();
        let value_task = match self.state.snapshots.spawn_observed_mutation(
            snapshot_identity,
            idx,
            move |snapshot_element_key, identity| match proof_for_value.as_ref() {
                Some(proof) => state_for_value.exact_set_value(
                    proof,
                    snapshot_element_key,
                    identity.as_ref(),
                    idx,
                    &value_for_task,
                ),
                None => {
                    let identity = identity.ok_or_else(|| {
                        anyhow::anyhow!("stale_element_token: native identity unavailable")
                    })?;
                    crate::atspi::resolve_observed_click_target(pid, idx, xid, &identity)?
                        .set_value(&value_for_task)
                }
            },
        ) {
            Ok(task) => task,
            Err(error) => {
                return ToolResult::error(format!(
                    "stale_element_token: snapshot generation is not mutable: {error}"
                ))
            }
        };
        let result = value_task.await;
        match result {
            Ok(Ok(())) => ToolResult::text(format!("Set value of element [{idx}] to '{value}'.")),
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── scroll ────────────────────────────────────────────────────────────────────

pub struct ScrollTool {
    state: Arc<ToolState>,
}

fn atspi_scroll_result(progress: crate::atspi::ScrollProgress, foreground: bool) -> ToolResult {
    use cua_driver_core::action_record::{
        ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
    };
    let effect = if !progress.complete && progress.acknowledged > 0 {
        ActionEffect::Partial
    } else {
        ActionEffect::Unverifiable
    };
    let mut record = ActionExecutionRecord::builder(
        effect,
        ActionTransport::LinuxAtSpiAction,
        if foreground {
            RequestedDelivery::Foreground
        } else {
            RequestedDelivery::Background
        },
    )
    .actual_delivery(if !progress.complete {
        ActualDelivery::Unknown
    } else if foreground {
        ActualDelivery::Foreground
    } else {
        ActualDelivery::Background
    });
    if progress.acknowledged > 0 {
        record = record.delivered_count(progress.acknowledged);
    }
    let text = if progress.complete {
        format!(
            "AT-SPI acknowledged {} scroll action(s); effect remains unverified.",
            progress.acknowledged
        )
    } else {
        format!("AT-SPI scroll stopped after {} acknowledged action(s): {}. Refresh state before another action.", progress.acknowledged, progress.detail.as_deref().unwrap_or("unknown outcome"))
    };
    let record = record
        .detail(text.clone())
        .build()
        .expect("AT-SPI scroll preserves acknowledged progress");
    let public = serde_json::to_value(record.public_result().unwrap()).unwrap();
    let result = if progress.complete {
        ToolResult::text(text)
    } else {
        ToolResult::error(text)
    };
    result.with_structured(public).with_action_record(record)
}

#[test]
fn atspi_scroll_outcomes_preserve_uncertainty_and_acknowledged_count() {
    use cua_driver_core::action_record::ActionEffect;
    for (complete, acknowledged, effect) in [
        (false, 0, ActionEffect::Unverifiable),
        (false, 1, ActionEffect::Partial),
        (true, 2, ActionEffect::Unverifiable),
    ] {
        let result = atspi_scroll_result(
            crate::atspi::ScrollProgress {
                acknowledged,
                complete,
                detail: None,
            },
            false,
        );
        let record = result.action_record.unwrap();
        assert_eq!(record.effect, effect);
        let public = serde_json::to_value(record.public_result().unwrap()).unwrap();
        assert_eq!(
            public["delivery"]["mode"],
            if complete { "background" } else { "unknown" }
        );
        assert_eq!(
            public["delivery"]["delivered_count"],
            if acknowledged == 0 {
                Value::Null
            } else {
                json!(acknowledged)
            }
        );
    }
}
#[cfg(test)]
#[path = "retained_scroll_route_tests.rs"]
mod retained_scroll_route_tests;

/// Narrow native seam: tests substitute only retained AT-SPI I/O, while the
/// public ScrollTool route, mutation permit and refusal policy stay real.
trait ObservedScrollTarget: Send {
    fn semantic_scroll(
        &self,
        direction: &str,
        amount: usize,
        by: cua_driver_contract::ScrollBy,
    ) -> anyhow::Result<crate::atspi::ScrollProgress>;
}

impl ObservedScrollTarget for crate::atspi::ObservedClickTarget {
    fn semantic_scroll(
        &self,
        direction: &str,
        amount: usize,
        by: cua_driver_contract::ScrollBy,
    ) -> anyhow::Result<crate::atspi::ScrollProgress> {
        crate::atspi::native::scroll_observed_element(self, direction, amount, by)
    }
}

fn observed_scroll_progress(
    progress: crate::atspi::ScrollProgress,
    foreground: bool,
) -> ToolResult {
    let stale = progress
        .detail
        .as_deref()
        .is_some_and(|detail| detail.contains("stale_element_token"));
    let mut result = atspi_scroll_result(progress, foreground);
    if stale {
        // Keep acknowledgement/partial-effect accounting; refuse the remainder
        // without replay, rather than disguising lost identity as an AX miss.
        if let Some(public) = result.structured_content.as_mut() {
            public["code"] = json!("stale_element_token");
            public["refusal"] = json!({"code":"stale_element_token"});
        }
    }
    result
}

/// Snapshot-bound scroll never rejoins the unaddressed or pixel ladders.
/// Cosmetic ordinal cursor placement is skipped, as for retained keyboard input.
async fn invoke_observed_scroll(
    state: &Arc<ToolState>,
    pid: u32,
    xid: u64,
    index: usize,
    snapshot_identity: cua_driver_core::element_token::SnapshotIdentity,
    delivery: crate::input::delivery::DeliveryMode,
    direction: String,
    amount: usize,
    by: cua_driver_contract::ScrollBy,
) -> ToolResult {
    let snapshots = state.snapshots.clone();
    #[cfg(test)]
    let backend = state.observed_scroll_backend.clone();
    let resolved = cua_driver_core::blocking::spawn(move || -> anyhow::Result<_> {
        let (permit, identity) = snapshots
            .acquire_observed_mutation(snapshot_identity, index)
            .map_err(|error| anyhow::anyhow!("stale_element_token: {error}"))?;
        #[cfg(test)]
        if let Some(backend) = backend {
            return Ok((
                Arc::new(permit),
                Box::new(backend) as Box<dyn ObservedScrollTarget>,
            ));
        }
        let proof = if crate::wayland::wayland_input_enabled() {
            Some(crate::wayland::establish_exact_target(pid, xid)?)
        } else {
            None
        };
        let target =
            crate::atspi::native::resolve_observed_target(pid, index, xid, &identity, proof)
                .map_err(|error| anyhow::anyhow!("stale_element_token: {error:#}"))?;
        Ok((
            Arc::new(permit),
            Box::new(target) as Box<dyn ObservedScrollTarget>,
        ))
    })
    .await;
    let (permit, target) = match resolved {
        Ok(Ok(retained)) => retained,
        Ok(Err(error)) => return observed_keyboard_error(error),
        Err(error) => return ToolResult::error(format!("Task error: {error}")),
    };
    let foreground = delivery.is_foreground();
    let direction_for_ax = direction.clone();
    // Semantic AT-SPI is allowed on Hyprland too. WebKitGTK's Wayland AX
    // acknowledgement remains a known silent no-op, so do not claim delivery.
    if !(crate::wayland::wayland_input_enabled() && is_webkitgtk_embedder(pid)) {
        let result = cua_driver_core::blocking::spawn(move || {
            crate::atspi::snapshot::with_retained_mutation(permit.clone(), || {
                let result = target.semantic_scroll(&direction_for_ax, amount, by);
                (permit, target, result)
            })
        })
        .await;
        match result {
            Ok((_, _, Ok(progress))) => return observed_scroll_progress(progress, foreground),
            Ok((_, _, Err(error))) if format!("{error:#}").contains("stale_element_token") => {
                return observed_keyboard_error(error);
            }
            Ok((_, _, Err(_))) => {}
            Err(error) => {
                return observed_scroll_progress(
                    crate::atspi::ScrollProgress {
                        acknowledged: 0,
                        complete: false,
                        detail: Some(error.to_string()),
                    },
                    foreground,
                )
            }
        }
    }
    // Element tokens authorize only the retained semantic route. An AX miss
    // never rejoins any input ladder, regardless of delivery mode or compositor.
    if !foreground {
        return crate::input::delivery::background_unavailable_error(
            crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
        );
    }
    ToolResult::error(
        "element_route_unqualified: the element exposes no accessible scroll action; retry scroll with x,y pixel coordinates instead of element_token",
    )
    .with_structured(json!({
        "code": "element_route_unqualified",
        "effect": "refused",
        "refusal": {"code": "element_route_unqualified"},
    }))
}

#[cfg(test)]
#[test]
fn retained_scroll_chromium_background_refuses_before_native_resolution() {
    cua_driver_core::tool::with_runtime_scope(
        format!("retained-scroll-background-{}", uuid::Uuid::new_v4()),
        || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    // A headless, owned process with Chromium's exact argv fingerprint. No
                    // native identity is stored: entering resolution would return stale, not
                    // the required background_unavailable, even without a desktop bus.
                    struct Child(std::process::Child);
                    impl Drop for Child {
                        fn drop(&mut self) {
                            let _ = self.0.kill();
                            let _ = self.0.wait();
                        }
                    }
                    let mut child = Child(
                        std::process::Command::new("/bin/sh")
                            .args(["-c", "printf ready; read unused", "--type=renderer"])
                            .stdin(std::process::Stdio::piped())
                            .stdout(std::process::Stdio::piped())
                            .spawn()
                            .unwrap(),
                    );
                    let mut ready = [0; 5];
                    std::io::Read::read_exact(child.0.stdout.as_mut().unwrap(), &mut ready)
                        .unwrap();
                    assert_eq!(&ready, b"ready");
                    let pid = child.0.id();
                    assert!(is_chromium_embedder(pid));
                    let state = ToolState::new();
                    let node = crate::atspi::AtspiNode {
                        element_index: Some(7),
                        element_key: 41,
                        identity: None,
                        role: "scroll pane".into(),
                        name: None,
                        value: None,
                        checked: None,
                        enabled: None,
                        selected: None,
                        description: None,
                        actions: vec!["scroll down".into()],
                        depth: 0,
                        parent_element_index: None,
                        in_web_content: false,
                        object_ref: None,
                    };
                    let id = state
                        .snapshots
                        .publish(state.snapshots.prepare(pid, 0x7f30_0105, &[node]).unwrap())
                        .unwrap();
                    let token = cua_driver_core::element_token::token_for(id, 7);
                    let result = ScrollTool { state }
        .invoke(json!({
            "pid":pid, "element_token":token, "direction":"down", "delivery_mode":"background"
        }))
        .await;
                    assert_eq!(
                        result.structured_content.unwrap()["code"],
                        "background_unavailable"
                    );
                });
        },
    );
}

#[cfg(test)]
#[test]
fn retained_scroll_stale_progress_keeps_acknowledgements_and_refuses_remainder() {
    let result = observed_scroll_progress(
        crate::atspi::ScrollProgress {
            acknowledged: 1,
            complete: false,
            detail: Some("stale_element_token: object left frame".into()),
        },
        false,
    );
    assert_eq!(result.is_error, Some(true));
    let public = result.structured_content.unwrap();
    assert_eq!(public["refusal"]["code"], "stale_element_token");
    assert_eq!(public["delivery"]["delivered_count"], 1);
    assert_eq!(
        result.action_record.unwrap().effect,
        cua_driver_core::action_record::ActionEffect::Partial
    );
}

static SCROLL_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for ScrollTool {
    fn def(&self) -> &ToolDef {
        SCROLL_DEF.get_or_init(|| ToolDef {
            name: "scroll".into(),
            description: "Scroll the target pid's focused region via XSendEvent Button4/5. \
                direction required; by defaults to line, amount defaults to 3.".into(),
            input_schema: json!({
                // `pid` is conditionally required (validated in code), so only
                // `direction` is pinned — matches the scroll→["direction"] canon
                // in cua_driver_core::tool_schema.
                "type":"object","required":["direction"],"properties":{
                    "session": cua_driver_core::tool_schema::session_schema(),
                    "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
                    "pid":{"type":"integer","description":"Target process ID. Required unless scope is \"desktop\"."},
                    "direction":{"type":"string","enum":["up","down","left","right"],"description":"Scroll direction."},
                    "by":{"type":"string","enum":["line","page"],"description":"Scroll granularity. Default: line."},
                    "amount":{"type":"integer","minimum":1,"maximum":50,"description":"Number of scroll steps. Default: 3."},
                    "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y; optional with element_token (the token carries it)."},
                    "element_token": cua_driver_core::tool_schema::element_token_schema(),
                    "x":{"type":"number","description":"Window-local screenshot-pixel X of the scroll target. Pass with y and without element_token."},
                    "y":{"type":"number","description":"Window-local screenshot-pixel Y of the scroll target. Pass with x and without element_token."},
                    "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Use \"desktop\" with x,y and no pid/window_id for get_desktop_state screen coordinates. Default \"window\"."},
                    "coordinate_frame": coordinate_frame_schema(),
                    "delivery_mode": crate::input::delivery::delivery_mode_schema()
                },"additionalProperties":false
            }),
            read_only: false, destructive: false, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if args.get("capture_id").is_some() {
            return ToolResult::error("capture_id input routing is not yet qualified on the hardened Linux adapter; refresh get_window_state and use an element token or window coordinates")
                .with_structured(json!({"code":"capture_route_unqualified","effect":"refused"}));
        }

        let cursor_id = resolve_cursor_key(&args);
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            let input = match parse_typed_projection::<ScrollInput>("scroll", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let direction = input.direction.as_str().to_owned();
            let x = input.x.round() as i32;
            let y = input.y.round() as i32;
            let amount = input.amount.unwrap_or(3).clamp(1, 50) as usize;
            let display = direction.clone();
            let wayland = crate::wayland::wayland_input_enabled();
            let path = if wayland { "wayland_desktop" } else { "xtest" };
            let space = match desktop_input_space().await {
                Ok(space) => space,
                Err(error) => return ToolResult::error(error.to_string()),
            };
            if named_session_cursor_key(&args).is_some() {
                let (overlay_x, overlay_y) = space.to_layout(x, y);
                reveal_pointer_action_for(
                    &self.state,
                    &cursor_id,
                    f64::from(overlay_x),
                    f64::from(overlay_y),
                    false,
                )
                .await;
            }
            let result = cua_driver_core::blocking::spawn(move || {
                if wayland {
                    crate::wayland::scroll_desktop(&space, x, y, &direction, amount as u32)
                } else {
                    crate::input::send_scroll_xtest_desktop(x, y, &direction, amount)
                }
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!("Scrolled desktop {display} × {amount}."))
                    .with_structured(
                        json!({"scope":"desktop","path":path,"effect":"unverifiable"}),
                    ),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }
        let pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let direction = match args.require_str("direction") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let amount = args.u64_or("amount", 3).clamp(1, 50) as usize;
        let by = match serde_json::from_value::<cua_driver_contract::ScrollBy>(
            args.get("by").cloned().unwrap_or(json!("line")),
        ) {
            Ok(by) => by,
            Err(error) => return ToolResult::error(format!("Invalid scroll by: {error}")),
        };
        // Element tokens leave the pixel/unaddressed ladders below entirely.
        // Their retained native object and permit travel through every mutation.
        let resolved = match self
            .state
            .snapshots
            .resolve_for_tool(pid as i32, &args, "scroll")
        {
            Ok(r) => r,
            Err(e) => return e,
        };
        let xid_opt: Option<u64> = match &resolved {
            cua_driver_core::element_token::ResolvedElement::Element { window_id, .. } => {
                Some(*window_id)
            }
            cua_driver_core::element_token::ResolvedElement::None => args.opt_u64("window_id"),
        };

        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        let isolated_background = isolated_hyprland_background(delivery);
        #[cfg(test)]
        let isolated_background = self
            .state
            .observed_scroll_backend
            .as_ref()
            .map_or(isolated_background, |backend| {
                backend.isolated_background(delivery)
            });
        // Chromium/Electron background refusal precedes even retained native
        // resolution, so a missing AT-SPI bridge cannot mask background_unavailable.
        if !isolated_background {
            if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
                return refusal;
            }
        }

        if crate::wayland::is_gnome_wayland_session() && xid_opt.is_none() {
            return ToolResult::error(
                "exact_target_required: scroll on host GNOME requires caller-approved pid and window_id",
            )
            .with_structured(json!({
                "code": "exact_target_required",
                "required": ["pid", "window_id"],
            }));
        }
        if crate::wayland::is_gnome_wayland_session() {
            let exact_window_id = xid_opt.expect("checked above");
            match cua_driver_core::blocking::spawn(move || {
                crate::wayland::establish_exact_target(pid, exact_window_id)
            })
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => return ToolResult::error(error.to_string()),
                Err(error) => return ToolResult::error(format!("Task error: {error}")),
            }
        }

        // Resolve XID: use window_id if given, else first window for pid.
        let xid = match xid_opt {
            Some(x) => x,
            None => {
                let windows =
                    cua_driver_core::blocking::spawn(move || crate::x11::list_windows(Some(pid)))
                        .await
                        .unwrap_or_default();
                match windows.first() {
                    Some(w) => w.xid,
                    None => {
                        return ToolResult::error(format!(
                            "No windows found for pid {pid}. Provide window_id."
                        ));
                    }
                }
            }
        };

        let native_refusal = if isolated_background {
            None
        } else {
            unavailable_wayland_focused_input_background(delivery, true)
        };
        let pixel_target = match (
            args.get("x").and_then(|value| value.as_f64()),
            args.get("y").and_then(|value| value.as_f64()),
        ) {
            (Some(x), Some(y)) => {
                // Pixel targets use the latest screenshot's coordinate frame.
                // Apply the same buffer-to-window ratio as click/drag before
                // positioning either the agent cursor or the input device.
                let ratio = match coordinate_scroll_scale(native_refusal, || {
                    screenshot_scale(&self.state, &args, pid, Some(xid))
                }) {
                    Ok(ratio) => ratio,
                    Err(refusal) => return refusal,
                };
                Some((x * ratio, y * ratio))
            }
            (None, None) => None,
            _ => return ToolResult::error("Pass both x and y to pixel-target scroll."),
        };
        if pixel_target.is_some()
            && matches!(
                resolved,
                cua_driver_core::element_token::ResolvedElement::Element { .. }
            )
        {
            return ToolResult::error(
                "Pass either element_token (ax) or x,y (px) to scroll, not both.",
            );
        }

        if let cua_driver_core::element_token::ResolvedElement::Element {
            element_index,
            snapshot_identity,
            ..
        } = resolved
        {
            return invoke_observed_scroll(
                &self.state,
                pid,
                xid,
                element_index,
                snapshot_identity,
                delivery,
                direction,
                amount,
                by,
            )
            .await;
        }

        if named_session_cursor_key(&args).is_some() {
            let visual_target = cua_driver_core::blocking::spawn(move || {
                explicit_keyboard_cursor_target(pid, xid, None, pixel_target)
                    .or_else(|| keyboard_window_center(xid))
            })
            .await
            .ok()
            .flatten();
            if let Some((sx, sy)) = visual_target {
                crate::overlay::send_command_for(
                    cursor_id.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(xid),
                );
                reveal_pointer_action_for(&self.state, &cursor_id, sx, sy, false).await;
            }
        }

        if hyprland_foreground(delivery) {
            if xid_opt.is_none() {
                return foreground_hyprland_refusal(
                    "an exact window_id or window-bound element token is required",
                );
            }
            let point = pixel_target;
            // `by: "page"` is the documented 3-detent approximation, not an
            // exact viewport; large requests become several wheel packets.
            let actions =
                match crate::wayland::hyprland_input::scroll_actions(point, &direction, amount, by)
                {
                    Ok(actions) => actions,
                    Err(error) => return foreground_hyprland_refusal(error.to_string()),
                };
            return foreground_hyprland_scroll(&args, pid, xid, actions).await;
        }
        if isolated_background {
            if xid_opt.is_none() {
                return isolated_hyprland_refusal(
                    "an exact window_id or window-bound element token is required",
                );
            }
            let owner = named_session_cursor_key(&args);
            let (_cancellation, dispatch) =
                match spawn_isolated_hyprland(&args, move |cancellation| {
                    let point = pixel_target;
                    let actions = crate::wayland::hyprland_input::scroll_actions(
                        point, &direction, amount, by,
                    )?;
                    crate::wayland::hyprland_input::execute_scroll(
                        owner,
                        pid,
                        xid,
                        actions,
                        cancellation,
                    )
                }) {
                    Ok(dispatch) => dispatch,
                    Err(refusal) => return refusal,
                };
            return match dispatch.await {
                Ok(result) => isolated_hyprland_result(result),
                Err(error) => isolated_hyprland_task_error(error, false),
            };
        }

        if crate::wayland::is_inject_mode() {
            let Some((x, y)) = pixel_target else {
                return crate::input::delivery::background_unavailable_error(
                    crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                );
            };
            let direction_for_inject = direction.clone();
            let result = cua_driver_core::blocking::spawn(move || {
                let target = crate::wayland::establish_exact_target(pid, xid)?;
                let output_point = crate::wayland::window_local_to_output(
                    target.window_id(),
                    x.round() as i32,
                    y.round() as i32,
                );
                crate::wayland::scroll_at(
                    target,
                    Some(output_point),
                    &direction_for_inject,
                    amount as u32,
                )
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Scrolled {direction} {amount} ticks (focus-free via cua-compositor)."
                ))
                .with_structured(json!({
                    "verified": false,
                    "delivery_mode": if delivery.is_foreground() { "foreground" } else { "background" },
                    "route": "cua_compositor_inject"
                })),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }

        if crate::wayland::wayland_input_enabled() {
            if let Some(refusal) = unavailable_wayland_focused_input_background(delivery, false) {
                return refusal;
            }
            let direction_for_wayland = direction.clone();
            let local_point = pixel_target;
            let output_point = local_point.map(|(x, y)| {
                crate::wayland::window_local_to_output(xid, x.round() as i32, y.round() as i32)
            });
            let result = cua_driver_core::blocking::spawn(move || {
                let target = crate::wayland::establish_exact_target(pid, xid)?;
                crate::wayland::scroll_at_with_outcome(
                    target,
                    output_point,
                    &direction_for_wayland,
                    amount as u32,
                )
            })
            .await;
            return match result {
                Ok(Ok(outcome)) => ToolResult::text(format!(
                    "Scrolled {direction} {amount} ticks (delivery_mode=foreground)."
                ))
                .with_structured(with_foreground_diagnostics(
                    json!({ "verified": false, "delivery_mode": "foreground" }),
                    outcome,
                )),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }

        if let Some(refusal) = unavailable_webkit_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_gtk_pointer_background(pid, delivery) {
            return refusal;
        }

        let element_point = pixel_target.map(|local| {
            let screen = window_local_to_screen(xid, local.0, local.1).ok();
            (local, screen)
        });

        // X11 scroll buttons: 4=up, 5=down, 6=left, 7=right
        // Note: "page" scroll is still per-click on X11; send more ticks for page.
        let button: u8 = match direction.as_str() {
            "up" => 4,
            "left" => 6,
            "right" => 7,
            _ => 5,
        };
        let cursor_id_for_task = cursor_id.clone();
        let direction_for_wayland = direction.clone();
        let amount_u32 = amount as u32;
        let result = cua_driver_core::blocking::spawn(move || -> anyhow::Result<
            Option<crate::wayland::shell_helper::ForegroundTerminalOutcome>,
        > {
            if crate::wayland::wayland_input_enabled() {
                let target = crate::wayland::establish_exact_target(pid, xid)?;
                return crate::wayland::scroll_with_outcome(
                    target,
                    &direction_for_wayland,
                    amount_u32,
                );
            }
            // foreground: activate the window, then scroll, then restore — for
            // surfaces that only route wheel events to the active window.
            let x11_scroll = || -> anyhow::Result<()> {
                // X11: synthetic Button4-7 XSendEvents are dropped by XInput2
                // toolkits (GTK never scrolls). On a real Xorg host, drive a real
                // wheel detent through the MPX uinput pointer over the window's
                // center — libinput turns it into the XI2 smooth-scroll GTK reads —
                // without stealing focus. Falls back to the legacy XSendEvent
                // Button4-7 path on Xvfb / unsupported servers.
                // Button → axis/sign: 4=up(+v) 5=down(-v) 6=left(-h) 7=right(+h).
                if crate::input::real_pointer_input_available() {
                    let pointer_point = element_point
                        .and_then(|(_, screen)| screen)
                        .map(|(x, y)| (x as i32, y as i32))
                        .or_else(|| window_screen_center(xid).ok());
                    if let Some((cx, cy)) = pointer_point {
                        let horizontal = matches!(button, 6 | 7);
                        let ticks = match button {
                            4 | 7 => amount as i32,
                            _ => -(amount as i32), // 5 (down) and 6 (left)
                        };
                        match crate::input::send_virtual_pointer_scroll(
                            &cursor_id_for_task,
                            &crate::input::VirtualPointerScroll {
                                target_window: xid,
                                x: cx,
                                y: cy,
                                horizontal,
                                ticks,
                            },
                        ) {
                            Ok(()) => return Ok(()),
                            Err(error) if crate::input::is_uinput_unavailable(&error) => {
                                return Err(error)
                            }
                            Err(e) => tracing::warn!("MPX scroll fell back to XSendEvent: {e}"),
                        }
                    }
                }
                let (local_x, local_y) =
                    element_point.map(|(local, _)| local).unwrap_or((0.0, 0.0));
                crate::input::send_click(xid, local_x as i32, local_y as i32, amount, button)
            };
            if delivery.is_foreground() {
                let point = element_point
                    .and_then(|(_, screen)| screen)
                    .or_else(|| {
                        window_screen_center(xid)
                            .ok()
                            .map(|(x, y)| (x as f64, y as f64))
                    })
                    .ok_or_else(|| anyhow::anyhow!("could not resolve foreground scroll point"))?;
                crate::input::with_x11_foreground(xid, 80, || {
                    crate::input::send_click_xtest_desktop(
                        point.0 as i32,
                        point.1 as i32,
                        button,
                        amount,
                    )
                })?;
            } else {
                x11_scroll()?
            }
            Ok(None)
        })
        .await;
        let mode_label = if delivery.is_foreground() {
            "foreground"
        } else {
            "background"
        };
        match result {
            Ok(Ok(outcome)) => ToolResult::text(format!(
                "Scrolled {direction} {amount} ticks (delivery_mode={mode_label})."
            ))
            .with_structured(with_foreground_diagnostics(
                json!({ "verified": false, "delivery_mode": mode_label }),
                outcome,
            )),
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// `ScreenshotTool` and `ScreenshotCompatTool` removed in PR #1692 — see the
// matching note in platform-windows/src/tools/impl_.rs. `get_window_state` is
// the canonical screenshot path (it always returns a screenshot now); the
// underlying capture machinery (XGetImage / `import` shell-out / etc.)
// stays reachable through GetWindowStateTool.

// ── double_click ──────────────────────────────────────────────────────────────

pub struct DoubleClickTool {
    state: Arc<ToolState>,
}
static DCLICK_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for DoubleClickTool {
    fn def(&self) -> &ToolDef {
        DCLICK_DEF.get_or_init(|| ToolDef {
            name: "double_click".into(),
            description: "Double-click at (x,y) or an element_token (AT-SPI bounds) via XSendEvent. \
                No focus steal. Provide either (window_id + x/y) or (pid + element_token). \
                After a zoom call, pass from_zoom=true to auto-translate zoom-image coords.".into(),
            input_schema: json!({"type":"object","required":["pid"],"properties":{
                "session": cua_driver_core::tool_schema::session_schema(),
                "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
                "pid":{"type":"integer","description":"Target process ID."},
                "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y or element_index; optional with element_token (the token carries it)."},
                "x":{"type":"number","description":"Window-local pixel X of the target window's own get_window_state screenshot (0..screenshot_width). For get_desktop_state pixels pass scope:\"desktop\" (or coordinate_frame:\"desktop\")."},
                "y":{"type":"number","description":"Window-local pixel Y of the target window's own get_window_state screenshot (0..screenshot_height); see x."},
                "coordinate_frame": coordinate_frame_schema(),
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "from_zoom":{"type":"boolean","description":"Set true after a zoom call to auto-translate zoom-image pixel coordinates back to full-window space."},
                "delivery_mode": crate::input::delivery::delivery_mode_schema()
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        if args.get("capture_id").is_some() {
            return ToolResult::error("capture_id input routing is not yet qualified on the hardened Linux adapter; refresh get_window_state and use an element token or window coordinates")
                .with_structured(json!({"code":"capture_route_unqualified","effect":"refused"}));
        }

        let cursor_id = resolve_cursor_key(&args);
        let pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        if let Err(refusal) = retained_click::qualify_args(&args, delivery, 1, 2) {
            return refusal;
        }
        if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_webkit_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_gtk_pointer_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_wayland_focused_input_background(delivery, true) {
            return refusal;
        }
        if args.get("element_index").is_some() || args.get("element_token").is_some() {
            let mut args = args;
            args["button"] = json!("left");
            args["count"] = json!(2);
            return ClickTool {
                state: self.state.clone(),
            }
            .invoke(args)
            .await;
        }
        if hyprland_foreground(delivery) {
            // Share exact-target validation and the admitted native click lifecycle.
            let mut args = args;
            args["button"] = json!("left");
            args["count"] = json!(2);
            return ClickTool {
                state: self.state.clone(),
            }
            .invoke(args)
            .await;
        }
        let resolved =
            match self
                .state
                .snapshots
                .resolve_for_tool(pid as i32, &args, "double_click")
            {
                Ok(r) => r,
                Err(e) => return e,
            };
        let window_id_resolved: Option<u64> = match &resolved {
            cua_driver_core::element_token::ResolvedElement::Element { window_id, .. } => {
                Some(*window_id)
            }
            cua_driver_core::element_token::ResolvedElement::None => args.opt_u64("window_id"),
        };
        if crate::wayland::is_gnome_wayland_session() && window_id_resolved.is_none() {
            return ToolResult::error(
                "exact_target_required: double_click on host GNOME requires caller-approved pid and window_id",
            )
            .with_structured(json!({
                "code": "exact_target_required",
                "required": ["pid", "window_id"],
            }));
        }
        let xid = match window_id_resolved {
            Some(v) => v,
            None => return ToolResult::error("Provide either element_token or window_id + x/y."),
        };
        let from_zoom = args.bool_or("from_zoom", false);
        let mut x = args.f64_or("x", 0.0);
        let mut y = args.f64_or("y", 0.0);
        if from_zoom {
            match self.state.zoom_context(&args, pid, Some(xid)) {
                Ok(ctx) => {
                    let (wx, wy) = ctx.zoom_to_window(x, y);
                    x = wx;
                    y = wy;
                }
                Err(refusal) => return refusal,
            }
        } else {
            let ratio = match screenshot_scale(&self.state, &args, pid, Some(xid)) {
                Ok(ratio) => ratio,
                Err(refusal) => return refusal,
            };
            x *= ratio;
            y *= ratio;
        }
        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        let wayland_output_point = if crate::wayland::wayland_input_enabled() {
            Some(crate::wayland::window_local_to_output(
                xid,
                x.round() as i32,
                y.round() as i32,
            ))
        } else {
            None
        };
        let glide_target = if let Some((sx, sy)) = wayland_output_point {
            Some((sx as f64, sy as f64))
        } else {
            cua_driver_core::blocking::spawn(move || window_local_to_screen(xid, x, y))
                .await
                .ok()
                .and_then(|r| r.ok())
        };
        if let Some((sx, sy)) = glide_target {
            reveal_pointer_action_for(&self.state, &cursor_id, sx, sy, true).await;
        }
        let (xi, yi) = (x as i32, y as i32);
        let cursor_id_for_task = cursor_id.clone();
        let result = cua_driver_core::blocking::spawn(move || -> anyhow::Result<
            Option<crate::wayland::shell_helper::ForegroundTerminalOutcome>,
        > {
            if crate::wayland::is_inject_mode() {
                let (output_x, output_y) = wayland_output_point.unwrap_or((xi, yi));
                let target = crate::wayland::establish_exact_target(pid, xid)?;
                return crate::wayland::click_with_outcome(target, output_x, output_y, 2, 1);
            }
            if crate::wayland::wayland_input_enabled() {
                let (output_x, output_y) = wayland_output_point.unwrap_or((xi, yi));
                let target = crate::wayland::establish_exact_target(pid, xid)?;
                return crate::wayland::click_with_outcome(target, output_x, output_y, 2, 1);
            }
            if delivery.is_foreground() {
                return crate::input::with_x11_foreground(xid, 80, || {
                    // Real XTest double-click at the screen point — synthetic
                    // XSendEvent button events are dropped by GTK/Qt and the MPX
                    // uinput path needs /dev/uinput (absent on Xvfb/Xtigervnc), so
                    // neither lands. Mirrors the single-click foreground path.
                    if let Ok((sx, sy)) = window_local_to_screen(xid, xi as f64, yi as f64) {
                        crate::input::send_click_xtest_desktop(
                            sx.round() as i32,
                            sy.round() as i32,
                            1,
                            2,
                        )?;
                        return Ok(());
                    }
                    x11_pixel_click_no_focus_steal(&cursor_id_for_task, xid, xi, yi, 1, 2)
                })
                .map(|()| None);
            }
            x11_pixel_click_no_focus_steal(&cursor_id_for_task, xid, xi, yi, 1, 2)
                .map(|()| None)
        })
        .await;
        let mode_label = if delivery.is_foreground() {
            "foreground"
        } else {
            "background"
        };
        match result {
            Ok(Ok(outcome)) => ToolResult::text(format!(
                "✅ Double-clicked at ({x:.1}, {y:.1}) (delivery_mode={mode_label})."
            ))
            .with_structured(with_foreground_diagnostics(
                json!({ "verified": false, "delivery_mode": mode_label }),
                outcome,
            )),
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── right_click ───────────────────────────────────────────────────────────────

pub struct RightClickTool {
    state: Arc<ToolState>,
}
static RCLICK_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for RightClickTool {
    fn def(&self) -> &ToolDef {
        RCLICK_DEF.get_or_init(|| ToolDef {
            name: "right_click".into(),
            description: "Right-click at (x,y) or an element_token (AT-SPI bounds) via XSendEvent. \
                No focus steal. Provide either (window_id + x/y) or (pid + element_token). \
                After a zoom call, pass from_zoom=true to auto-translate zoom-image coords.".into(),
            input_schema: json!({"type":"object","required":["pid"],"properties":{
                "session": cua_driver_core::tool_schema::session_schema(),
                "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
                "pid":{"type":"integer","description":"Target process ID."},
                "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y or element_index; optional with element_token (the token carries it)."},
                "x":{"type":"number","description":"Window-local pixel X of the target window's own get_window_state screenshot (0..screenshot_width). For get_desktop_state pixels pass scope:\"desktop\" (or coordinate_frame:\"desktop\")."},
                "y":{"type":"number","description":"Window-local pixel Y of the target window's own get_window_state screenshot (0..screenshot_height); see x."},
                "coordinate_frame": coordinate_frame_schema(),
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "modifier": cua_driver_core::tool_schema::modifier_schema(),
                "from_zoom":{"type":"boolean","description":"Set true after a zoom call to auto-translate zoom-image pixel coordinates back to full-window space."},
                "delivery_mode": crate::input::delivery::delivery_mode_schema()
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        if args.get("capture_id").is_some() {
            return ToolResult::error("capture_id input routing is not yet qualified on the hardened Linux adapter; refresh get_window_state and use an element token or window coordinates")
                .with_structured(json!({"code":"capture_route_unqualified","effect":"refused"}));
        }

        let cursor_id = resolve_cursor_key(&args);
        let pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        if let Err(refusal) = retained_click::qualify_args(&args, delivery, 3, 1) {
            return refusal;
        }
        if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_webkit_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_gtk_pointer_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_wayland_focused_input_background(delivery, true) {
            return refusal;
        }
        if args.get("element_index").is_some() || args.get("element_token").is_some() {
            let mut args = args;
            args["button"] = json!("right");
            args["count"] = json!(1);
            return ClickTool {
                state: self.state.clone(),
            }
            .invoke(args)
            .await;
        }
        if hyprland_foreground(delivery) {
            // Share exact-target validation and the admitted native click lifecycle.
            let mut args = args;
            args["button"] = json!("right");
            args["count"] = json!(1);
            return ClickTool {
                state: self.state.clone(),
            }
            .invoke(args)
            .await;
        }
        let resolved = match self
            .state
            .snapshots
            .resolve_for_tool(pid as i32, &args, "right_click")
        {
            Ok(r) => r,
            Err(e) => return e,
        };
        let window_id_resolved: Option<u64> = match &resolved {
            cua_driver_core::element_token::ResolvedElement::Element { window_id, .. } => {
                Some(*window_id)
            }
            cua_driver_core::element_token::ResolvedElement::None => args.opt_u64("window_id"),
        };
        if crate::wayland::is_gnome_wayland_session() && window_id_resolved.is_none() {
            return ToolResult::error(
                "exact_target_required: right_click on host GNOME requires caller-approved pid and window_id",
            )
            .with_structured(json!({
                "code": "exact_target_required",
                "required": ["pid", "window_id"],
            }));
        }
        let xid = match window_id_resolved {
            Some(v) => v,
            None => return ToolResult::error("Provide either element_token or window_id + x/y."),
        };
        let from_zoom = args.bool_or("from_zoom", false);
        let mut x = args.f64_or("x", 0.0);
        let mut y = args.f64_or("y", 0.0);
        if from_zoom {
            match self.state.zoom_context(&args, pid, Some(xid)) {
                Ok(ctx) => {
                    let (wx, wy) = ctx.zoom_to_window(x, y);
                    x = wx;
                    y = wy;
                }
                Err(refusal) => return refusal,
            }
        } else {
            let ratio = match screenshot_scale(&self.state, &args, pid, Some(xid)) {
                Ok(ratio) => ratio,
                Err(refusal) => return refusal,
            };
            x *= ratio;
            y *= ratio;
        }
        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        let wayland_output_point = if crate::wayland::wayland_input_enabled() {
            Some(crate::wayland::window_local_to_output(
                xid,
                x.round() as i32,
                y.round() as i32,
            ))
        } else {
            None
        };
        let glide_target = if let Some((sx, sy)) = wayland_output_point {
            Some((sx as f64, sy as f64))
        } else {
            cua_driver_core::blocking::spawn(move || window_local_to_screen(xid, x, y))
                .await
                .ok()
                .and_then(|r| r.ok())
        };
        if let Some((sx, sy)) = glide_target {
            reveal_pointer_action_for(&self.state, &cursor_id, sx, sy, true).await;
        }
        let (xi, yi) = (x as i32, y as i32);
        let cursor_id_for_task = cursor_id.clone();
        let result = cua_driver_core::blocking::spawn(move || -> anyhow::Result<
            Option<crate::wayland::shell_helper::ForegroundTerminalOutcome>,
        > {
            if crate::wayland::is_inject_mode() {
                let (output_x, output_y) = wayland_output_point.unwrap_or((xi, yi));
                let target = crate::wayland::establish_exact_target(pid, xid)?;
                return crate::wayland::click_with_outcome(target, output_x, output_y, 1, 3);
            }
            if crate::wayland::wayland_input_enabled() {
                let (output_x, output_y) = wayland_output_point.unwrap_or((xi, yi));
                let target = crate::wayland::establish_exact_target(pid, xid)?;
                return crate::wayland::click_with_outcome(target, output_x, output_y, 1, 3);
            }
            if delivery.is_foreground() {
                return crate::input::with_x11_foreground(xid, 80, || {
                    // Real XTest right-click at the screen point (synthetic
                    // XSendEvent is dropped by GTK/Qt; MPX needs /dev/uinput).
                    // Mirrors the single-click foreground path.
                    if let Ok((sx, sy)) = window_local_to_screen(xid, xi as f64, yi as f64) {
                        crate::input::send_click_xtest_desktop(
                            sx.round() as i32,
                            sy.round() as i32,
                            3,
                            1,
                        )?;
                        return Ok(());
                    }
                    x11_pixel_click_no_focus_steal(&cursor_id_for_task, xid, xi, yi, 3, 1)
                })
                .map(|()| None);
            }
            x11_pixel_click_no_focus_steal(&cursor_id_for_task, xid, xi, yi, 3, 1)
                .map(|()| None)
        })
        .await;
        let mode_label = if delivery.is_foreground() {
            "foreground"
        } else {
            "background"
        };
        match result {
            Ok(Ok(outcome)) => ToolResult::text(format!(
                "✅ Right-clicked at ({x:.1}, {y:.1}) (delivery_mode={mode_label})."
            ))
            .with_structured(with_foreground_diagnostics(
                json!({ "verified": false, "delivery_mode": mode_label }),
                outcome,
            )),
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── drag ─────────────────────────────────────────────────────────────────────

/// Typed refusal for a modified drag on native Wayland. The Wayland pointer
/// routes (virtual pointer, libei, and the nested cua-compositor socket) do not
/// hold keyboard modifier state across a pointer gesture, so the drag is
/// refused before any input is dispatched rather than degraded to an
/// unmodified drag. The stable `code` lets callers branch without parsing text.
fn wayland_modified_drag_refusal() -> ToolResult {
    const DETAIL: &str = "the pointer route cannot carry keyboard modifier state";
    ToolResult::error(format!(
        "modified drags are unavailable on native Wayland: {DETAIL}"
    ))
    .with_structured(json!({
        "code": "modified_pointer_unavailable",
        "effect": "refused",
        "verified": false,
        "detail": DETAIL,
    }))
}

pub struct DragTool {
    state: Arc<ToolState>,
}
static DRAG_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for DragTool {
    fn has_independent_input_lane(&self, args: &Value) -> bool {
        // This opt-in branch below either uses its own leased synthetic seat
        // or refuses. It never falls back to primary-seat input. All ordinary
        // Linux routes retain the common process-wide input coordinator.
        // Socket availability can change between admission and invocation.
        // Native Hyprland window drags remain fail-closed on their own lane.
        args.opt_str("scope").as_deref() != Some("desktop")
            && !crate::input::delivery::DeliveryMode::from_args(args).is_foreground()
            && crate::wayland::wayland_input_enabled()
            && crate::wayland::hyprland::is_session()
    }

    fn def(&self) -> &ToolDef {
        DRAG_DEF.get_or_init(|| ToolDef {
            name: "drag".into(),
            description: "Press-drag-release gesture from (from_x, from_y) to (to_x, to_y) in \
                          window-local screenshot pixels via XSendEvent (ButtonPress + MotionNotify × steps + ButtonRelease). \
                          duration_ms (default 500), steps (default 20). No focus steal.".into(),
            input_schema: json!({"type":"object","required":["from_x","from_y","to_x","to_y"],"properties":{
                "session": cua_driver_core::tool_schema::session_schema(),
                "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
                "pid":{"type":"integer","description":"Target process ID. Required unless scope is \"desktop\"."},
                "window_id":{"type":"integer","description":"Target window XID. Required."},
                "from_x":{"type":"number","description":"Drag start X: window-local pixels of the target window's own get_window_state screenshot; pass scope:\"desktop\" for get_desktop_state pixels."},
                "from_y":{"type":"number","description":"Drag start Y (same frame as from_x)."},
                "to_x":{"type":"number","description":"Drag end X (same frame as from_x)."},
                "to_y":{"type":"number","description":"Drag end Y (same frame as from_x)."},
                "duration_ms":{"type":"integer","minimum":0,"maximum":10000,"description":"Total drag duration. Default: 500."},
                "steps":{"type":"integer","minimum":1,"maximum":200,"description":"Intermediate MotionNotify events. Default: 20."},
                "modifier": cua_driver_core::tool_schema::modifier_schema(),
                "button": cua_driver_core::tool_schema::button_schema(),
                "from_zoom":{"type":"boolean","description":"Set true after a zoom call to auto-translate zoom-image pixel coordinates back to full-window space."},
                "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Use \"desktop\" with no pid/window_id for get_desktop_state screen coordinates. Default \"window\"."},
                "coordinate_frame": coordinate_frame_schema(),
                "delivery_mode": crate::input::delivery::delivery_mode_schema()
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        if args.get("capture_id").is_some() {
            return ToolResult::error("capture_id input routing is not yet qualified on the hardened Linux adapter; refresh get_window_state and use an element token or window coordinates")
                .with_structured(json!({"code":"capture_route_unqualified","effect":"refused"}));
        }

        let cursor_id = resolve_cursor_key(&args);
        let modifiers: Vec<String> = args.str_array("modifier");
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            let input = match parse_typed_projection::<DragInput>("drag", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let (from_x, from_y, to_x, to_y) = (input.from_x, input.from_y, input.to_x, input.to_y);
            let button = parse_mouse_button(input.button.unwrap_or(ClickButton::Left).as_str());
            let duration_ms = input.duration_ms.unwrap_or(500).min(10_000);
            let steps = input.steps.unwrap_or(20).clamp(1, 200) as usize;
            let modifiers = input.modifier.unwrap_or_default();
            let wayland = crate::wayland::wayland_input_enabled();
            if wayland && !modifiers.is_empty() {
                return wayland_modified_drag_refusal();
            }
            let path = if wayland { "wayland_desktop" } else { "xtest" };
            let space = match desktop_input_space().await {
                Ok(space) => space,
                Err(error) => return ToolResult::error(error.to_string()),
            };
            // The overlay draws in layout coordinates, like the input below.
            let overlay_point = |x: f64, y: f64| {
                let (lx, ly) = space.to_layout(x.round() as i32, y.round() as i32);
                (f64::from(lx), f64::from(ly))
            };
            let (overlay_from, overlay_to) =
                (overlay_point(from_x, from_y), overlay_point(to_x, to_y));
            let result = cua_driver_core::blocking::spawn(move || {
                if wayland {
                    crate::wayland::drag_desktop(
                        &space,
                        from_x.round() as i32,
                        from_y.round() as i32,
                        to_x.round() as i32,
                        to_y.round() as i32,
                        steps as u32,
                        duration_ms,
                        button,
                    )
                } else {
                    let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    crate::input::send_drag_xtest_desktop_with_modifiers(
                        from_x.round() as i32,
                        from_y.round() as i32,
                        to_x.round() as i32,
                        to_y.round() as i32,
                        button,
                        duration_ms,
                        steps,
                        &modifier_refs,
                    )
                }
            });
            let visual_drag = track_overlay_drag_for(
                cursor_id.clone(),
                overlay_from,
                overlay_to,
                duration_ms,
                steps,
            );
            let (result, ()) = tokio::join!(result, visual_drag);
            if matches!(&result, Ok(Ok(()))) {
                self.state
                    .cursor_registry
                    .update_position(&cursor_id, overlay_to.0, overlay_to.1);
            }
            return match result {
                Ok(Ok(())) => ToolResult::text("Dragged on the desktop.").with_structured(
                    json!({"scope":"desktop","path":path,"effect":"unverifiable"}),
                ),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }
        let pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let xid = match args.opt_u64("window_id") {
            Some(v) => v,
            None => return ToolResult::error("window_id is required on Linux."),
        };
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        let isolated_background = isolated_hyprland_background(delivery);
        if !isolated_background {
            if crate::wayland::wayland_input_enabled() && !modifiers.is_empty() {
                return wayland_modified_drag_refusal();
            }
            if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
                return refusal;
            }
            if let Some(refusal) = unavailable_webkit_background(pid, delivery) {
                return refusal;
            }
            if let Some(refusal) = unavailable_gtk_pointer_background(pid, delivery) {
                return refusal;
            }
            if let Some(refusal) = unavailable_wayland_focused_input_background(delivery, true) {
                return refusal;
            }
        }

        let coerce = |key: &str| -> Option<f64> {
            args.opt_f64(key)
                .or_else(|| args.opt_i64(key).map(|i| i as f64))
        };
        let mut from_x = match coerce("from_x") {
            Some(v) => v,
            None => return ToolResult::error("Missing: from_x"),
        };
        let mut from_y = match coerce("from_y") {
            Some(v) => v,
            None => return ToolResult::error("Missing: from_y"),
        };
        let mut to_x = match coerce("to_x") {
            Some(v) => v,
            None => return ToolResult::error("Missing: to_x"),
        };
        let mut to_y = match coerce("to_y") {
            Some(v) => v,
            None => return ToolResult::error("Missing: to_y"),
        };

        let duration_ms = args.u64_or("duration_ms", 500);
        let steps = args.u64_or("steps", 20) as usize;
        let button_str = args.str_or("button", "left");
        let button = parse_mouse_button(button_str.as_str());
        let from_zoom = args.bool_or("from_zoom", false);

        let context = coordinate_drag_context(None, || {
            if from_zoom {
                self.state
                    .zoom_context(&args, pid, Some(xid))
                    .map(CoordinateContext::Zoom)
            } else {
                screenshot_scale(&self.state, &args, pid, Some(xid))
                    .map(CoordinateContext::Screenshot)
            }
        });
        match context {
            Ok(CoordinateContext::Zoom(context)) => {
                (from_x, from_y) = context.zoom_to_window(from_x, from_y);
                (to_x, to_y) = context.zoom_to_window(to_x, to_y);
            }
            Ok(CoordinateContext::Screenshot(scale)) => {
                from_x *= scale;
                from_y *= scale;
                to_x *= scale;
                to_y *= scale;
            }
            Err(refusal) => return refusal,
        }

        if hyprland_foreground(delivery) {
            if button_str != "left" || args.get("modifier").is_some_and(|value| !value.is_null()) {
                return foreground_hyprland_refusal(
                    "foreground drag supports only an unmodified left button",
                );
            }
            if !crate::wayland::hyprland_input::enabled() {
                return foreground_hyprland_refusal(
                    "production Hyprland input plugin is unavailable",
                );
            }
            let owner = named_session_cursor_key(&args);
            let from = crate::wayland::window_local_to_output(
                xid,
                from_x.round() as i32,
                from_y.round() as i32,
            );
            let to = crate::wayland::window_local_to_output(
                xid,
                to_x.round() as i32,
                to_y.round() as i32,
            );
            let (started, acknowledged) = tokio::sync::oneshot::channel();
            let (_cancellation, mut dispatch) =
                match spawn_isolated_hyprland(&args, move |cancellation| {
                    crate::wayland::hyprland_input::execute_foreground_with_started(
                        owner,
                        pid,
                        xid,
                        crate::wayland::hyprland_input::Action::Drag {
                            from: (from_x, from_y),
                            to: (to_x, to_y),
                            duration_ms,
                        },
                        Some(started),
                        cancellation,
                    )
                }) {
                    Ok(dispatch) => dispatch,
                    Err(_) => {
                        return foreground_hyprland_refusal(
                            "authenticated admitted lifecycle required",
                        )
                    }
                };
            let acknowledged = acknowledged.await.is_ok();
            let result = if acknowledged {
                overlay_snap_to_for(&cursor_id, from.0 as f64, from.1 as f64, None);
                tokio::select! {
                    result = &mut dispatch => result,
                    () = track_overlay_drag_for(cursor_id.clone(), (from.0 as f64, from.1 as f64),
                        (to.0 as f64, to.1 as f64), duration_ms, steps) => dispatch.await,
                }
            } else {
                dispatch.await
            };
            return hyprland_input_result(
                match result {
                    Ok(result) => result,
                    Err(error) => Err(crate::wayland::hyprland_input::unknown_dispatch(
                        error.into(),
                        u32::from(acknowledged),
                    )),
                },
                true,
            );
        }

        if isolated_background {
            if button_str != "left" || args.get("modifier").is_some_and(|value| !value.is_null()) {
                return isolated_hyprland_refusal(
                    "isolated drag supports only an unmodified left button",
                );
            }
            let owner = named_session_cursor_key(&args);
            let from = crate::wayland::window_local_to_output(
                xid,
                from_x.round() as i32,
                from_y.round() as i32,
            );
            let to = crate::wayland::window_local_to_output(
                xid,
                to_x.round() as i32,
                to_y.round() as i32,
            );
            let (started, acknowledged) = tokio::sync::oneshot::channel();
            let (_cancellation, mut dispatch) =
                match spawn_isolated_hyprland(&args, move |cancellation| {
                    crate::wayland::hyprland_input::execute_with_started(
                        owner,
                        pid,
                        xid,
                        crate::wayland::hyprland_input::Action::Drag {
                            from: (from_x, from_y),
                            to: (to_x, to_y),
                            duration_ms,
                        },
                        Some(started),
                        cancellation,
                    )
                }) {
                    Ok(dispatch) => dispatch,
                    Err(refusal) => return refusal,
                };
            // Do not animate a drag that was refused. The compositor starts
            // the overlay clock only after accepting and pressing the button.
            let acknowledged = acknowledged.await.is_ok();
            if acknowledged {
                overlay_snap_to_for(&cursor_id, from.0 as f64, from.1 as f64, None);
                tokio::select! {
                    result = &mut dispatch => {
                        return match result { Ok(result) => isolated_hyprland_result(result), Err(error) => isolated_hyprland_task_error(error, acknowledged) };
                    }
                    () = track_overlay_drag_for(cursor_id.clone(), (from.0 as f64, from.1 as f64),
                        (to.0 as f64, to.1 as f64), duration_ms, steps) => {}
                }
            }
            return match dispatch.await {
                Ok(result) => isolated_hyprland_result(result),
                Err(error) => isolated_hyprland_task_error(error, acknowledged),
            };
        }

        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        let wayland_points = crate::wayland::wayland_input_enabled().then(|| {
            (
                crate::wayland::window_local_to_output(
                    xid,
                    from_x.round() as i32,
                    from_y.round() as i32,
                ),
                crate::wayland::window_local_to_output(
                    xid,
                    to_x.round() as i32,
                    to_y.round() as i32,
                ),
            )
        });
        let screen_from = if let Some((from, _)) = wayland_points {
            Some((from.0 as f64, from.1 as f64))
        } else {
            cua_driver_core::blocking::spawn(move || window_local_to_screen(xid, from_x, from_y))
                .await
                .ok()
                .and_then(|result| result.ok())
        };
        if let Some((sx_from, sy_from)) = screen_from {
            overlay_glide_to_for(&cursor_id, sx_from, sy_from).await;
            self.state
                .cursor_registry
                .update_position(&cursor_id, sx_from, sy_from);
            overlay_snap_to_for(&cursor_id, sx_from, sy_from, None);
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::ClickPulse {
                    x: sx_from,
                    y: sy_from,
                },
            );
        }
        // Native Wayland: emit press + interpolated motion + release as one
        // virtual-pointer (wlroots) or libei (GNOME/KDE) sequence, output-relative
        // coords. Returns early so we don't fall into the X11 XSendEvent loop below.
        if crate::wayland::wayland_input_enabled() {
            let steps_u32 = steps as u32;
            let ((from_output_x, from_output_y), (to_output_x, to_output_y)) = wayland_points
                .unwrap_or((
                    (from_x.round() as i32, from_y.round() as i32),
                    (to_x.round() as i32, to_y.round() as i32),
                ));
            let drag_result = cua_driver_core::blocking::spawn(move || {
                let target = crate::wayland::establish_exact_target(pid, xid)?;
                crate::wayland::drag_with_outcome(
                    target,
                    from_output_x,
                    from_output_y,
                    to_output_x,
                    to_output_y,
                    steps_u32,
                    duration_ms,
                    button,
                )
            });
            let visual_drag = track_overlay_drag_for(
                cursor_id.clone(),
                (f64::from(from_output_x), f64::from(from_output_y)),
                (f64::from(to_output_x), f64::from(to_output_y)),
                duration_ms,
                steps,
            );
            let (drag_result, ()) = tokio::join!(drag_result, visual_drag);
            if matches!(&drag_result, Ok(Ok(_))) {
                self.state.cursor_registry.update_position(
                    &cursor_id,
                    f64::from(to_output_x),
                    f64::from(to_output_y),
                );
            }
            return match drag_result {
                Ok(Ok(outcome)) => ToolResult::text(format!(
                    "✅ Posted drag ({button_str}) to pid {pid} \
                     from ({from_x:.0}, {from_y:.0}) → ({to_x:.0}, {to_y:.0}) \
                     in {duration_ms}ms / {steps} steps."
                ))
                .with_structured(with_foreground_diagnostics(json!({}), outcome)),
                Ok(Err(e)) => ToolResult::error(e.to_string()),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        if delivery.is_foreground() {
            let screen_points = cua_driver_core::blocking::spawn(move || {
                Ok::<_, anyhow::Error>((
                    window_local_to_screen(xid, from_x, from_y)?,
                    window_local_to_screen(xid, to_x, to_y)?,
                ))
            })
            .await;
            let ((screen_from_x, screen_from_y), (screen_to_x, screen_to_y)) = match screen_points {
                Ok(Ok(points)) => points,
                Ok(Err(e)) => return ToolResult::error(e.to_string()),
                Err(e) => return ToolResult::error(format!("Task error: {e}")),
            };
            let drag_result = cua_driver_core::blocking::spawn(move || {
                let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                crate::input::with_x11_foreground(xid, 80, || {
                    crate::input::send_drag_xtest_desktop_with_modifiers(
                        screen_from_x.round() as i32,
                        screen_from_y.round() as i32,
                        screen_to_x.round() as i32,
                        screen_to_y.round() as i32,
                        button,
                        duration_ms,
                        steps,
                        &modifier_refs,
                    )
                })
            });
            let visual_drag = track_overlay_drag_for(
                cursor_id.clone(),
                (screen_from_x, screen_from_y),
                (screen_to_x, screen_to_y),
                duration_ms,
                steps,
            );
            let (drag_result, ()) = tokio::join!(drag_result, visual_drag);
            if matches!(&drag_result, Ok(Ok(()))) {
                self.state
                    .cursor_registry
                    .update_position(&cursor_id, screen_to_x, screen_to_y);
            }
            return match drag_result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Dragged ({button_str}) to pid {pid} from ({from_x:.0}, {from_y:.0}) \
                     to ({to_x:.0}, {to_y:.0}) in {duration_ms}ms / {steps} steps \
                     (delivery_mode=foreground)."
                ))
                .with_structured(json!({
                    "path": "x11_xtest_fg",
                    "verified": false,
                    "delivery_mode": "foreground"
                })),
                Ok(Err(e)) => ToolResult::error(e.to_string()),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::SetPressed(true),
        );
        let press_result = cua_driver_core::blocking::spawn(move || {
            crate::input::send_button_down(
                xid,
                from_x.round() as i32,
                from_y.round() as i32,
                button,
            )
        })
        .await;
        let mut result: anyhow::Result<()> = match press_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(anyhow::anyhow!("Task error: {e}")),
        };

        if result.is_ok() {
            let step_delay_ms = if steps > 1 {
                duration_ms / steps as u64
            } else {
                duration_ms
            };
            for i in 1..=steps {
                let t = i as f64 / steps.max(1) as f64;
                let ix = from_x + (to_x - from_x) * t;
                let iy = from_y + (to_y - from_y) * t;
                let motion_result = cua_driver_core::blocking::spawn(move || {
                    crate::input::send_motion(
                        xid,
                        ix.round() as i32,
                        iy.round() as i32,
                        Some(button),
                    )
                })
                .await;
                match motion_result {
                    Ok(Ok(())) => {
                        if let Ok(Ok((sx, sy))) = cua_driver_core::blocking::spawn(move || {
                            window_local_to_screen(xid, ix, iy)
                        })
                        .await
                        {
                            self.state
                                .cursor_registry
                                .update_position(&cursor_id, sx, sy);
                            crate::overlay::send_command_for(
                                cursor_id.clone(),
                                cursor_overlay::track_pointer_command(sx, sy),
                            );
                        }
                        if step_delay_ms > 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(step_delay_ms))
                                .await;
                        }
                    }
                    Ok(Err(e)) => {
                        result = Err(e);
                        break;
                    }
                    Err(e) => {
                        result = Err(anyhow::anyhow!("Task error: {e}"));
                        break;
                    }
                }
            }
        }

        let release_result = cua_driver_core::blocking::spawn(move || {
            crate::input::send_button_up(xid, to_x.round() as i32, to_y.round() as i32, button)
        })
        .await;
        if result.is_ok() {
            result = match release_result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(anyhow::anyhow!("Task error: {e}")),
            };
        }
        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::SetPressed(false),
        );

        if result.is_ok() {
            if let Ok(Ok((sx_to, sy_to))) =
                cua_driver_core::blocking::spawn(move || window_local_to_screen(xid, to_x, to_y))
                    .await
            {
                crate::overlay::send_command_for(
                    cursor_id.clone(),
                    cursor_overlay::track_pointer_command(sx_to, sy_to),
                );
                self.state
                    .cursor_registry
                    .update_position(&cursor_id, sx_to, sy_to);
            }
        }

        match result {
            Ok(()) => ToolResult::text(format!(
                "✅ Posted drag ({button_str}) to pid {pid} \
                 from ({from_x:.0}, {from_y:.0}) → ({to_x:.0}, {to_y:.0}) \
                 in {duration_ms}ms / {steps} steps."
            )),
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}

// ── mouse_button_down / mouse_drag / mouse_button_up ────────────────────────

pub struct MouseButtonDownTool {
    state: Arc<ToolState>,
}
static MDOWN_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for MouseButtonDownTool {
    fn def(&self) -> &ToolDef {
        MDOWN_DEF.get_or_init(|| ToolDef {
            name: "mouse_button_down".into(),
            description: "Press and hold a mouse button at (x,y) via background X11 delivery. \
                Does not release the button; pair with mouse_drag / mouse_button_up. \
                Returns the current held-button state.".into(),
            input_schema: json!({"type":"object","required":["pid","window_id","x","y"],"properties":{
                "session": cua_driver_core::tool_schema::session_schema_with(
                    "When both are present, session takes precedence over cursor_id."
                ),
                "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
                "pid":{"type":"integer","description":"Target process ID."},
                "window_id":{"type":"integer","description":"Window id from list_windows."},
                "x":{"type":"number","description":"Window-local pixel X of the target window's own get_window_state screenshot (0..screenshot_width). For get_desktop_state pixels pass scope:\"desktop\" (or coordinate_frame:\"desktop\")."},
                "y":{"type":"number","description":"Window-local pixel Y of the target window's own get_window_state screenshot (0..screenshot_height); see x."},
                "coordinate_frame": coordinate_frame_schema(),
                "button": cua_driver_core::tool_schema::button_schema(),
                "from_zoom":{"type":"boolean","description":"Set true after a zoom call to auto-translate zoom-image pixel coordinates back to full-window space."}
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if args.get("capture_id").is_some() {
            return ToolResult::error("capture_id input routing is not yet qualified on the hardened Linux adapter; refresh get_window_state and use an element token or window coordinates")
                .with_structured(json!({"code":"capture_route_unqualified","effect":"refused"}));
        }

        let cursor_id = resolve_cursor_key(&args);
        if let Some(held) = self
            .state
            .mouse_hold
            .lock()
            .unwrap()
            .get(&cursor_id)
            .cloned()
        {
            return ToolResult::error(format!(
                "Cursor '{cursor_id}' already has a held mouse button. Call mouse_button_up first."
            ))
            .with_structured(mouse_hold_json(&cursor_id, Some(&held)));
        }

        let pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let xid = match args.opt_u64("window_id") {
            Some(v) => v,
            None => return ToolResult::error("window_id is required on Linux."),
        };
        let button_name = args.str_or("button", "left");
        let button = parse_mouse_button(button_name.as_str());
        let mut x = args.f64_or("x", 0.0);
        let mut y = args.f64_or("y", 0.0);
        if args.bool_or("from_zoom", false) {
            match self.state.zoom_context(&args, pid, Some(xid)) {
                Ok(ctx) => {
                    let (wx, wy) = ctx.zoom_to_window(x, y);
                    x = wx;
                    y = wy;
                }
                Err(refusal) => return refusal,
            }
        } else {
            let ratio = match screenshot_scale(&self.state, &args, pid, Some(xid)) {
                Ok(ratio) => ratio,
                Err(refusal) => return refusal,
            };
            x *= ratio;
            y *= ratio;
        }

        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        if let Ok(Ok((sx, sy))) =
            cua_driver_core::blocking::spawn(move || window_local_to_screen(xid, x, y)).await
        {
            overlay_glide_to_for(&cursor_id, sx, sy).await;
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::ClickPulse { x: sx, y: sy },
            );
        }

        let xi = x as i32;
        let yi = y as i32;
        // Native Wayland: route through the persistent virtual-pointer module
        // so the held button survives across tool calls; the X11 path keeps
        // the existing input::send_button_down behaviour.
        let result = if crate::wayland::is_wayland() {
            let cid = cursor_id.clone();
            cua_driver_core::blocking::spawn(move || {
                let target = crate::wayland::establish_exact_target(pid, xid)?;
                crate::wayland::persistent_vptr::press_exact(&cid, target, xi, yi, button)
            })
            .await
        } else {
            cua_driver_core::blocking::spawn(move || {
                crate::input::send_button_down(xid, xi, yi, button)
            })
            .await
        };
        match result {
            Ok(Ok(())) => {
                let hold = MouseHoldState {
                    pid,
                    xid,
                    button,
                    x,
                    y,
                };
                self.state
                    .mouse_hold
                    .lock()
                    .unwrap()
                    .insert(cursor_id.clone(), hold.clone());
                if let Ok(Ok((sx, sy))) =
                    cua_driver_core::blocking::spawn(move || window_local_to_screen(xid, x, y))
                        .await
                {
                    self.state
                        .cursor_registry
                        .update_position(&cursor_id, sx, sy);
                    overlay_snap_to_for(&cursor_id, sx, sy, None);
                    crate::overlay::send_command_for(
                        cursor_id.clone(),
                        cursor_overlay::OverlayCommand::SetPressed(true),
                    );
                }
                ToolResult::text(format!(
                    "✅ Cursor '{cursor_id}' held {} button down at ({x:.1}, {y:.1}).",
                    mouse_button_name(button),
                ))
                .with_structured(mouse_hold_json(&cursor_id, Some(&hold)))
            }
            Ok(Err(e)) => ToolResult::error(e.to_string()).with_structured(mouse_hold_json(
                &cursor_id,
                self.state.mouse_hold.lock().unwrap().get(&cursor_id),
            )),
            Err(e) => {
                ToolResult::error(format!("Task error: {e}")).with_structured(mouse_hold_json(
                    &cursor_id,
                    self.state.mouse_hold.lock().unwrap().get(&cursor_id),
                ))
            }
        }
    }
}

pub struct MouseDragTool {
    state: Arc<ToolState>,
}
static MDRAG_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for MouseDragTool {
    fn def(&self) -> &ToolDef {
        MDRAG_DEF.get_or_init(|| ToolDef {
            name: "mouse_drag".into(),
            description: "Move a previously-held mouse button to a new point via background X11 delivery. \
                Requires an active mouse_button_down state; does not release the button. \
                Returns the updated held-button state.".into(),
            input_schema: json!({"type":"object","required":["x","y"],"properties":{
                "session": cua_driver_core::tool_schema::session_schema_with(
                    "When both are present, session takes precedence over cursor_id."
                ),
                "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
                "pid":{"type":"integer","description":"Optional process ID; must match the held button's target."},
                "window_id":{"type":"integer","description":"Optional window id; must match the held button's target."},
                "x":{"type":"number","description":"Window-local pixel X of the target window's own get_window_state screenshot (0..screenshot_width). For get_desktop_state pixels pass scope:\"desktop\" (or coordinate_frame:\"desktop\")."},
                "y":{"type":"number","description":"Window-local pixel Y of the target window's own get_window_state screenshot (0..screenshot_height); see x."},
                "duration_ms":{"type":"integer","minimum":0,"maximum":10000,"description":"Total drag duration. Default: 500."},
                "steps":{"type":"integer","minimum":1,"maximum":200,"description":"Intermediate MotionNotify events. Default: 20."},
                "from_zoom":{"type":"boolean","description":"Set true after a zoom call to auto-translate zoom-image pixel coordinates back to full-window space."}
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if args.get("capture_id").is_some() {
            return ToolResult::error("capture_id input routing is not yet qualified on the hardened Linux adapter; refresh get_window_state and use an element token or window coordinates")
                .with_structured(json!({"code":"capture_route_unqualified","effect":"refused"}));
        }

        let cursor_id = resolve_cursor_key(&args);
        let Some(mut hold) = self
            .state
            .mouse_hold
            .lock()
            .unwrap()
            .get(&cursor_id)
            .cloned()
        else {
            return ToolResult::error(format!(
                "No mouse button is currently held for cursor '{cursor_id}'. Call mouse_button_down first."
            ))
            .with_structured(mouse_hold_json(&cursor_id, None));
        };
        if let Some(err) = held_target_mismatch(&args, &cursor_id, &hold) {
            return err;
        }

        let mut to_x = args.f64_or("x", 0.0);
        let mut to_y = args.f64_or("y", 0.0);
        if args.bool_or("from_zoom", false) {
            match self.state.zoom_context(&args, hold.pid, Some(hold.xid)) {
                Ok(ctx) => {
                    let (wx, wy) = ctx.zoom_to_window(to_x, to_y);
                    to_x = wx;
                    to_y = wy;
                }
                Err(refusal) => return refusal,
            }
        } else {
            let ratio = match screenshot_scale(&self.state, &args, hold.pid, Some(hold.xid)) {
                Ok(ratio) => ratio,
                Err(refusal) => return refusal,
            };
            to_x *= ratio;
            to_y *= ratio;
        }

        let xid = hold.xid;

        let from_x = hold.x;
        let from_y = hold.y;
        let duration_ms = args.u64_or("duration_ms", 500);
        let steps = args.u64_or("steps", 20).max(1) as usize;
        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        if let Ok(Ok((sx, sy))) =
            cua_driver_core::blocking::spawn(move || window_local_to_screen(xid, from_x, from_y))
                .await
        {
            overlay_glide_to_for(&cursor_id, sx, sy).await;
            self.state
                .cursor_registry
                .update_position(&cursor_id, sx, sy);
            overlay_snap_to_for(&cursor_id, sx, sy, None);
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::SetPressed(true),
            );
        }

        let button = hold.button;
        let step_delay_ms = if steps > 1 {
            duration_ms / steps as u64
        } else {
            duration_ms
        };
        let mut result: anyhow::Result<()> = Ok(());
        let mut prev_x = from_x;
        let mut prev_y = from_y;
        let is_wl = crate::wayland::is_wayland();
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let ix = from_x + (to_x - from_x) * t;
            let iy = from_y + (to_y - from_y) * t;
            // Native Wayland: route motion through the persistent virtual-
            // pointer so the held button stays held across the entire drag;
            // X11 keeps the existing input::send_motion path.
            let cid_inner = cursor_id.clone();
            let move_result = if is_wl {
                cua_driver_core::blocking::spawn(move || {
                    crate::wayland::persistent_vptr::move_to(
                        &cid_inner,
                        ix.round() as i32,
                        iy.round() as i32,
                    )
                })
                .await
            } else {
                cua_driver_core::blocking::spawn(move || {
                    crate::input::send_motion(
                        xid,
                        ix.round() as i32,
                        iy.round() as i32,
                        Some(button),
                    )
                })
                .await
            };
            match move_result {
                Ok(Ok(())) => {
                    if let Ok(Ok((sx, sy))) = cua_driver_core::blocking::spawn(move || {
                        window_local_to_screen(xid, ix, iy)
                    })
                    .await
                    {
                        let heading = if (ix - prev_x).abs() > f64::EPSILON
                            || (iy - prev_y).abs() > f64::EPSILON
                        {
                            Some((iy - prev_y).atan2(ix - prev_x))
                        } else {
                            None
                        };
                        self.state
                            .cursor_registry
                            .update_position(&cursor_id, sx, sy);
                        overlay_move_to_for(&cursor_id, sx, sy, heading);
                    }
                    prev_x = ix;
                    prev_y = iy;
                    if step_delay_ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(step_delay_ms)).await;
                    }
                }
                Ok(Err(e)) => {
                    result = Err(e);
                    break;
                }
                Err(e) => {
                    result = Err(anyhow::anyhow!("Task error: {e}"));
                    break;
                }
            }
        }

        match result {
            Ok(()) => {
                hold.x = to_x;
                hold.y = to_y;
                self.state
                    .mouse_hold
                    .lock()
                    .unwrap()
                    .insert(cursor_id.clone(), hold.clone());
                if let Ok(Ok((sx, sy))) = cua_driver_core::blocking::spawn(move || {
                    window_local_to_screen(xid, to_x, to_y)
                })
                .await
                {
                    self.state
                        .cursor_registry
                        .update_position(&cursor_id, sx, sy);
                    overlay_snap_to_for(
                        &cursor_id,
                        sx,
                        sy,
                        Some((to_y - from_y).atan2(to_x - from_x)),
                    );
                    crate::overlay::send_command_for(
                        cursor_id.clone(),
                        cursor_overlay::OverlayCommand::ClickPulse { x: sx, y: sy },
                    );
                }
                ToolResult::text(format!(
                    "✅ Cursor '{cursor_id}' dragged held {} button to ({to_x:.1}, {to_y:.1}).",
                    mouse_button_name(hold.button),
                ))
                .with_structured(mouse_hold_json(&cursor_id, Some(&hold)))
            }
            Err(e) => {
                // The Wayland owner terminally releases and drops its exact
                // focus/lease transaction on any move failure. Mirror that
                // terminal state in the tool registry rather than advertising
                // a held button that no longer exists.
                if is_wl {
                    self.state.mouse_hold.lock().unwrap().remove(&cursor_id);
                    crate::overlay::send_command_for(
                        cursor_id.clone(),
                        cursor_overlay::OverlayCommand::SetPressed(false),
                    );
                    ToolResult::error(e.to_string())
                        .with_structured(mouse_hold_json(&cursor_id, None))
                } else {
                    ToolResult::error(e.to_string())
                        .with_structured(mouse_hold_json(&cursor_id, Some(&hold)))
                }
            }
        }
    }
}

pub struct MouseButtonUpTool {
    state: Arc<ToolState>,
}
static MUP_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for MouseButtonUpTool {
    fn def(&self) -> &ToolDef {
        MUP_DEF.get_or_init(|| ToolDef {
            name: "mouse_button_up".into(),
            description: "Release a previously-held mouse button via background X11 delivery. \
                If x/y are omitted, releases at the last held position. Returns the current held-button state.".into(),
            input_schema: json!({"type":"object","properties":{
                "session": cua_driver_core::tool_schema::session_schema_with(
                    "When both are present, session takes precedence over cursor_id."
                ),
                "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
                "pid":{"type":"integer","description":"Optional process ID; must match the held button's target."},
                "window_id":{"type":"integer","description":"Optional window id; must match the held button's target."},
                "x":{"type":"number","description":"Window-local pixel X of the target window's own get_window_state screenshot (0..screenshot_width). For get_desktop_state pixels pass scope:\"desktop\" (or coordinate_frame:\"desktop\")."},
                "y":{"type":"number","description":"Window-local pixel Y of the target window's own get_window_state screenshot (0..screenshot_height); see x."},
                "coordinate_frame": coordinate_frame_schema(),
                "from_zoom":{"type":"boolean","description":"Set true after a zoom call to auto-translate zoom-image pixel coordinates back to full-window space."}
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if args.get("capture_id").is_some() {
            return ToolResult::error("capture_id input routing is not yet qualified on the hardened Linux adapter; refresh get_window_state and use an element token or window coordinates")
                .with_structured(json!({"code":"capture_route_unqualified","effect":"refused"}));
        }

        let cursor_id = resolve_cursor_key(&args);
        let Some(hold) = self
            .state
            .mouse_hold
            .lock()
            .unwrap()
            .get(&cursor_id)
            .cloned()
        else {
            return ToolResult::error(format!(
                "No mouse button is currently held for cursor '{cursor_id}'."
            ))
            .with_structured(mouse_hold_json(&cursor_id, None));
        };
        if let Some(err) = held_target_mismatch(&args, &cursor_id, &hold) {
            return err;
        }

        let xid = hold.xid;

        let (x, y) = match mouse_button_up_coordinates(&self.state, &args, &hold) {
            Ok(point) => point,
            Err(refusal) => return refusal,
        };

        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        if let Ok(Ok((sx, sy))) =
            cua_driver_core::blocking::spawn(move || window_local_to_screen(xid, x, y)).await
        {
            overlay_glide_to_for(&cursor_id, sx, sy).await;
        }

        let button = hold.button;
        let xi = x as i32;
        let yi = y as i32;
        // Native Wayland: release through the persistent virtual-pointer so
        // the same vptr device that emitted the press also emits the release
        // (single logical drag rather than a click pair).
        let result = if crate::wayland::is_wayland() {
            let cid = cursor_id.clone();
            cua_driver_core::blocking::spawn(move || {
                crate::wayland::persistent_vptr::release(&cid, button)
            })
            .await
        } else {
            cua_driver_core::blocking::spawn(move || {
                crate::input::send_button_up(xid, xi, yi, button)
            })
            .await
        };
        match result {
            Ok(Ok(())) => {
                if let Ok(Ok((sx, sy))) =
                    cua_driver_core::blocking::spawn(move || window_local_to_screen(xid, x, y))
                        .await
                {
                    self.state
                        .cursor_registry
                        .update_position(&cursor_id, sx, sy);
                    overlay_snap_to_for(&cursor_id, sx, sy, None);
                }
                crate::overlay::send_command_for(
                    cursor_id.clone(),
                    cursor_overlay::OverlayCommand::SetPressed(false),
                );
                self.state.mouse_hold.lock().unwrap().remove(&cursor_id);
                let cleared = mouse_hold_json(&cursor_id, None);
                ToolResult::text(format!(
                    "✅ Cursor '{cursor_id}' released held {} button at ({x:.1}, {y:.1}).",
                    mouse_button_name(button),
                ))
                .with_structured(cleared)
            }
            Ok(Err(e)) => ToolResult::error(e.to_string())
                .with_structured(mouse_hold_json(&cursor_id, Some(&hold))),
            Err(e) => ToolResult::error(format!("Task error: {e}"))
                .with_structured(mouse_hold_json(&cursor_id, Some(&hold))),
        }
    }
}

pub struct ParallelMouseDragTool {
    state: Arc<ToolState>,
}
static PMDRAG_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// cua-compositor path for parallel_mouse_drag: build window-local drag paths
/// and run them as concurrent multi-cursor injections over the control socket.
/// Coordinates stay window-local (the compositor maps them per app_id), so no
/// X11 geometry/MPX is needed — the X11 path's hard blocker on Wayland.
async fn parallel_drag_inject(args: &Value) -> ToolResult {
    let Some(items) = args.get("drags").and_then(|v| v.as_array()) else {
        return ToolResult::error("drags[] is required.");
    };
    if items.len() < 2 {
        return ToolResult::error("parallel_mouse_drag requires at least two drag items.");
    }
    let mut drags = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let Some(xid) = item.get("window_id").and_then(|v| v.as_u64()) else {
            return ToolResult::error("each drag item requires window_id.");
        };
        let local: Vec<(f64, f64)> = if let Some(pts) = item.get("path").and_then(|v| v.as_array())
        {
            pts.iter()
                .filter_map(|p| {
                    let a = p.as_array()?;
                    Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
                })
                .collect()
        } else {
            let g = |k: &str| item.get(k).and_then(|v| v.as_f64());
            match (g("from_x"), g("from_y"), g("to_x"), g("to_y")) {
                (Some(fx), Some(fy), Some(tx), Some(ty)) => vec![(fx, fy), (tx, ty)],
                _ => {
                    return ToolResult::error(
                        "each drag item requires path[] or from_x/from_y/to_x/to_y.",
                    );
                }
            }
        };
        if local.len() < 2 {
            return ToolResult::error("drag path needs at least 2 points.");
        }
        let steps = item
            .get("steps")
            .and_then(|v| v.as_u64())
            .unwrap_or(60)
            .clamp(1, 300) as usize;
        let x_button = parse_mouse_button(
            item.get("button")
                .and_then(|v| v.as_str())
                .unwrap_or("left"),
        ) as u32;
        let app = match cua_driver_core::blocking::spawn(move || {
            crate::wayland::inject_target_for_window(xid)
        })
        .await
        {
            Ok(Ok(target)) => target,
            Ok(Err(error)) => return ToolResult::error(error.to_string()),
            Err(error) => return ToolResult::error(format!("Task error: {error}")),
        };
        drags.push(crate::wayland::InjectDrag {
            app_id: app,
            idx: i,
            x_button,
            path: local,
            steps,
        });
    }
    let n = drags.len();
    match cua_driver_core::blocking::spawn(move || crate::wayland::inject_parallel_drags(&drags))
        .await
    {
        Ok(Ok(())) => ToolResult::text(format!(
            "Ran {n} concurrent drags (multi-cursor via cua-compositor)."
        )),
        Ok(Err(e)) => ToolResult::error(e.to_string()),
        Err(e) => ToolResult::error(format!("Task error: {e}")),
    }
}

#[async_trait]
impl Tool for ParallelMouseDragTool {
    fn def(&self) -> &ToolDef {
        PMDRAG_DEF.get_or_init(|| ToolDef {
            name: "parallel_mouse_drag".into(),
            description: "Run multiple mouse drag gestures concurrently via Linux MPX/XI2 virtual master pointers. \
                Each drag item runs on its own session-scoped master pointer (true same-window concurrent draws on X11). \
                Each item presses once, glides continuously through its whole path, and releases once — one smooth held \
                drag, not a chain of clicks. A path is given either as a straight segment (from_x/from_y → to_x/to_y) or \
                as a function `fn` = y(x) sampled over [x_from, x_to] in window-local pixels (e.g. fn:\"x\" is a diagonal, \
                fn:\"300+120*sin(x/40)\" a sine wave). Functions support + - * / ^, sin/cos/tan, sqrt, abs, exp, ln, pi, e.".into(),
            input_schema: json!({"type":"object","required":["drags"],"properties":{
                "drags":{"type":"array","minItems":2,"description":"Two or more drag gestures to run concurrently, each on its own virtual master pointer.","items":{"type":"object","required":["session","window_id"],"properties":{
                    "session":{"type":"string","description":"Session/cursor id; also keys the virtual master pointer."},
                    "window_id":{"type":"integer","description":"Window id from list_windows."},
                    "path":{"type":"array","items":{"type":"array","items":{"type":"number"}},"description":"Explicit window-local waypoints [[x,y],...] (>=2); pressed once, glided through, released once. Takes precedence over fn/from-to."},
                    "fn":{"type":"string","description":"Expression y(x) in window-local pixels; sampled over [x_from,x_to]. Mutually exclusive with from_x/to_x."},
                    "x_from":{"type":"number","description":"Domain start (window-local x) when `fn` is used."},
                    "x_to":{"type":"number","description":"Domain end (window-local x) when `fn` is used."},
                    "samples":{"type":"integer","minimum":2,"maximum":400,"description":"Waypoints sampled along `fn`. Default: 80."},
                    "from_x":{"type":"number","description":"Straight-segment start X (window-local pixels)."},
                    "from_y":{"type":"number","description":"Straight-segment start Y (window-local pixels)."},
                    "to_x":{"type":"number","description":"Straight-segment end X (window-local pixels)."},
                    "to_y":{"type":"number","description":"Straight-segment end Y (window-local pixels)."},
                    "button": cua_driver_core::tool_schema::button_schema(),
                    "duration_ms":{"type":"integer","minimum":0,"maximum":10000,"description":"Default: 1500 for fn paths, 500 for straight."},
                    "steps":{"type":"integer","minimum":1,"maximum":300,"description":"Motion sub-steps along the whole path. Default: scaled to path length."}
                },"additionalProperties":false}}
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if args.get("capture_id").is_some() {
            return ToolResult::error("capture_id input routing is not yet qualified on the hardened Linux adapter; refresh get_window_state and use an element token or window coordinates")
                .with_structured(json!({"code":"capture_route_unqualified","effect":"refused"}));
        }

        // Nested cua-compositor: run the drags as concurrent multi-cursor
        // injections (window-local, no X11 MPX/geometry needed).
        if crate::wayland::is_inject_mode() {
            return parallel_drag_inject(&args).await;
        }
        // Native Wayland without the inject socket: MPX/XI2 + uinput master
        // pointers don't exist on Wayland. Surface a typed error instead of
        // silently calling the X11 path that's guaranteed to fail.
        if crate::wayland::is_wayland() {
            return ToolResult::error(
                "parallel_mouse_drag requires the cua-compositor inject socket on Wayland \
                 (set CUA_INJECT_SOCKET to the cua-compositor control socket), \
                 or run the target under X11.",
            );
        }
        match cua_driver_core::blocking::spawn(crate::input::check_parallel_pointer_support).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return ToolResult::error(e.to_string()),
            Err(e) => return ToolResult::error(format!("Task error: {e}")),
        }

        let Some(items) = args.get("drags").and_then(|v| v.as_array()) else {
            return ToolResult::error("drags[] is required.");
        };
        if items.len() < 2 {
            return ToolResult::error("parallel_mouse_drag requires at least two drag items.");
        }

        let mut drags = Vec::with_capacity(items.len());
        for item in items {
            let Some(session) = item.get("session").and_then(|v| v.as_str()) else {
                return ToolResult::error("each drag item requires session.");
            };
            let Some(xid) = item.get("window_id").and_then(|v| v.as_u64()) else {
                return ToolResult::error("each drag item requires window_id.");
            };

            // Build the window-local waypoint path from one of: an explicit
            // `path` of [x,y] points, a function `fn` (y = f(x) sampled over
            // [x_from, x_to]), or a straight from→to segment.
            let is_fn = item.get("fn").and_then(|v| v.as_str()).is_some();
            let local: Vec<(f64, f64)> = if let Some(pts) =
                item.get("path").and_then(|v| v.as_array())
            {
                let mut out = Vec::with_capacity(pts.len());
                for p in pts {
                    let a = p.as_array();
                    let (Some(px), Some(py)) = (
                        a.and_then(|a| a.first()).and_then(|v| v.as_f64()),
                        a.and_then(|a| a.get(1)).and_then(|v| v.as_f64()),
                    ) else {
                        return ToolResult::error("each `path` entry must be [x, y].");
                    };
                    out.push((px, py));
                }
                if out.len() < 2 {
                    return ToolResult::error("`path` needs at least 2 points.");
                }
                out
            } else if let Some(expr_str) = item.get("fn").and_then(|v| v.as_str()) {
                let Some(x_from) = item.get("x_from").and_then(|v| v.as_f64()) else {
                    return ToolResult::error("`fn` requires x_from.");
                };
                let Some(x_to) = item.get("x_to").and_then(|v| v.as_f64()) else {
                    return ToolResult::error("`fn` requires x_to.");
                };
                let samples = item
                    .get("samples")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(80)
                    .clamp(2, 400);
                match crate::input::sample_function(expr_str, x_from, x_to, samples) {
                    Ok(pts) => pts,
                    Err(e) => return ToolResult::error(e.to_string()),
                }
            } else {
                let coerce = |k: &str| item.get(k).and_then(|v| v.as_f64());
                match (
                    coerce("from_x"),
                    coerce("from_y"),
                    coerce("to_x"),
                    coerce("to_y"),
                ) {
                    (Some(fx), Some(fy), Some(tx), Some(ty)) => vec![(fx, fy), (tx, ty)],
                    _ => {
                        return ToolResult::error(
                            "each drag item requires either `fn`+x_from+x_to, or from_x/from_y/to_x/to_y.",
                        );
                    }
                }
            };

            let button = parse_mouse_button(
                item.get("button")
                    .and_then(|v| v.as_str())
                    .unwrap_or("left"),
            );
            let duration_ms = item
                .get("duration_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(if is_fn { 1500 } else { 500 });

            // One translate gives the window origin; the path is a pure offset.
            let origin = match cua_driver_core::blocking::spawn(move || {
                window_local_to_screen(xid, 0.0, 0.0)
            })
            .await
            {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => return ToolResult::error(e.to_string()),
                Err(e) => return ToolResult::error(format!("Task error: {e}")),
            };
            let path: Vec<(i32, i32)> = local
                .iter()
                .map(|(lx, ly)| {
                    (
                        (origin.0 + lx).round() as i32,
                        (origin.1 + ly).round() as i32,
                    )
                })
                .collect();

            // Default sub-step count scaled to path length (smooth glide),
            // overridable via `steps`.
            let total_len: f64 = path
                .windows(2)
                .map(|w| {
                    (((w[1].0 - w[0].0) as f64).powi(2) + ((w[1].1 - w[0].1) as f64).powi(2)).sqrt()
                })
                .sum();
            let steps = item
                .get("steps")
                .and_then(|v| v.as_u64())
                .map(|s| (s as usize).clamp(1, 300))
                .unwrap_or_else(|| ((total_len / 3.0).round() as usize).clamp(24, 300));

            let start = path[0];
            self.state
                .cursor_registry
                .update_position(session, start.0 as f64, start.1 as f64);
            crate::overlay::send_command_for(
                session.to_owned(),
                cursor_overlay::OverlayCommand::PinAbove(xid),
            );
            crate::overlay::send_command_for(
                session.to_owned(),
                cursor_overlay::OverlayCommand::SnapTo {
                    x: start.0 as f64,
                    y: start.1 as f64,
                    heading_radians: None,
                },
            );
            crate::overlay::send_command_for(
                session.to_owned(),
                cursor_overlay::OverlayCommand::SetPressed(true),
            );

            drags.push((
                session.to_owned(),
                crate::input::VirtualPointerDrag {
                    target_window: xid,
                    button,
                    path,
                    duration_ms,
                    steps,
                },
            ));
        }

        let drags_for_task = drags.clone();
        let result = cua_driver_core::blocking::spawn(move || {
            crate::input::send_parallel_virtual_pointer_drags(&drags_for_task)
        })
        .await;
        match result {
            Ok(Ok(())) => {
                for (session, drag) in &drags {
                    let n = drag.path.len();
                    let end = drag.path[n - 1];
                    let prev = drag.path[n.saturating_sub(2)];
                    self.state
                        .cursor_registry
                        .update_position(session, end.0 as f64, end.1 as f64);
                    crate::overlay::send_command_for(
                        session.to_owned(),
                        cursor_overlay::OverlayCommand::SnapTo {
                            x: end.0 as f64,
                            y: end.1 as f64,
                            heading_radians: Some(
                                ((end.1 - prev.1) as f64).atan2((end.0 - prev.0) as f64),
                            ),
                        },
                    );
                    crate::overlay::send_command_for(
                        session.to_owned(),
                        cursor_overlay::OverlayCommand::SetPressed(false),
                    );
                    crate::overlay::send_command_for(
                        session.to_owned(),
                        cursor_overlay::OverlayCommand::ClickPulse {
                            x: end.0 as f64,
                            y: end.1 as f64,
                        },
                    );
                }
                ToolResult::text(format!(
                    "✅ Ran {} MPX drag gesture(s) concurrently.",
                    drags.len()
                ))
                .with_structured(json!({"count": drags.len()}))
            }
            Ok(Err(e)) => {
                for (session, _) in &drags {
                    crate::overlay::send_command_for(
                        session.to_owned(),
                        cursor_overlay::OverlayCommand::SetPressed(false),
                    );
                }
                linux_input_error(e)
            }
            Err(e) => {
                for (session, _) in &drags {
                    crate::overlay::send_command_for(
                        session.to_owned(),
                        cursor_overlay::OverlayCommand::SetPressed(false),
                    );
                }
                ToolResult::error(format!("Task error: {e}"))
            }
        }
    }
}

// ── get_screen_size ───────────────────────────────────────────────────────────

pub struct GetScreenSizeTool;
static GSS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetScreenSizeTool {
    fn def(&self) -> &ToolDef {
        GSS_DEF.get_or_init(|| ToolDef {
            name: "get_screen_size".into(),
            description: "Return the logical size of the main display in points plus its backing \
                scale factor. Agents click in points; Retina displays have scale_factor 2.0. \
                Requires no TCC permissions."
                .into(),
            input_schema: json!({"type":"object","properties":{
                "session": cua_driver_core::tool_schema::session_schema()
            },"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        if args.get("capture_id").is_some() {
            return ToolResult::error("capture_id input routing is not yet qualified on the hardened Linux adapter; refresh get_window_state and use an element token or window coordinates")
                .with_structured(json!({"code":"capture_route_unqualified","effect":"refused"}));
        }

        if let Err(result) = parse_typed_input::<GetScreenSizeInput>("get_screen_size", args) {
            return result;
        }
        let result = cua_driver_core::blocking::spawn(|| {
            if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
                // Shared manifest admission needs content-free display
                // metadata even when this native desktop has no X11 DISPLAY.
                // The adapter attests the IPC/Wayland compositor peer and
                // refuses layouts its logical frame cannot represent: no
                // powered output, or a rotated powered output.
                // Scaled outputs report logical pixels plus the output scale.
                // The size and the monitor list come from one snapshot.
                let (frame, monitors) = crate::wayland::hyprland::screen_report()?;
                return Ok((frame.width, frame.height, frame.scale, Some(monitors)));
            }
            // X11 reports pixel dimensions; scale factor on X11 is not
            // well-defined per-monitor, so report 1.0 (matches DPI-unaware
            // assumption). Other Wayland compositors retain their existing
            // limitation; do not infer native metadata support there.
            let (w, h) = x11_screen_size()?;
            Ok::<(u32, u32, f64, Option<Value>), anyhow::Error>((w, h, 1.0, None))
        })
        .await;
        match result {
            Ok(Ok((w, h, scale, Some(monitors)))) => {
                // Tell the agent which monitors are actually on: the desktop
                // frame covers powered monitors only and changes as they turn
                // on or off, so re-read it after any display change.
                let describe = |m: &Value| {
                    let name = m["name"].as_str().unwrap_or("?");
                    match (
                        m["powered"].as_bool(),
                        m["frame_x"].as_i64(),
                        m["frame_y"].as_i64(),
                    ) {
                        (Some(true), Some(x), Some(y)) => format!(
                            "{name} {}x{} on, at ({x},{y}) in the desktop frame",
                            m["width"], m["height"]
                        ),
                        _ => format!("{name} off (standby), not in the desktop frame"),
                    }
                };
                let lines: Vec<String> = monitors
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(describe)
                    .collect();
                ToolResult::text(format!(
                    "✅ Desktop frame: {w}x{h} points @ {scale}x, spanning every monitor that is on.\n{}",
                    lines.join("\n")
                ))
                .with_structured(json!({
                    "width": w, "height": h, "scale_factor": scale, "monitors": monitors,
                }))
            }
            // Matches Swift text format 1:1.
            Ok(Ok((w, h, scale, None))) => {
                ToolResult::text(format!("✅ Main display: {w}x{h} points @ {scale}x"))
                    .with_structured(json!({ "width": w, "height": h, "scale_factor": scale }))
            }
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// Read the true X11 root-window size in pixels: (width, height).
/// Shared by `get_screen_size` and `get_desktop_state`.
fn x11_screen_size() -> anyhow::Result<(u32, u32)> {
    use x11rb::connection::Connection;
    use x11rb::rust_connection::RustConnection;
    let (conn, screen_num) = RustConnection::connect(None)
        .map_err(|e| anyhow::anyhow!("{e}{}", crate::no_display_hint()))?;
    let setup = conn.setup();
    let screen = &setup.roots[screen_num];
    let w = screen.width_in_pixels as u32;
    let h = screen.height_in_pixels as u32;
    // WSLg / headless XWayland quirk: the X server connects but the
    // root screen advertises a 0-px geometry until a real output is
    // attached. Returning {width:0,height:0} here would propagate a
    // success with zero dimensions to the client, which then either
    // divides by zero when scaling or feeds the value into `int(...)`
    // after the missing key collapses to None. Fail loudly with an
    // actionable, typed error instead (never emit a 0/null where the
    // client expects a usable int). See issue #2005.
    if w == 0 || h == 0 {
        anyhow::bail!(
            "X11 connected but reports a 0x0 root screen — no usable \
             display geometry.{}",
            crate::no_display_hint()
        );
    }
    Ok((w, h))
}

/// Put the desktop image in the exact coordinate frame consumed by desktop
/// actions. Native Wayland capture buffers may use backing pixels while the
/// compositor's pointer protocol and reported screen geometry use logical
/// pixels. Returning the backing image alongside logical dimensions violates
/// the screenshot-to-action contract and makes every vision-grounded action
/// miss by the output scale.
fn normalize_desktop_capture_for_action_frame(
    png: Vec<u8>,
    action_width: u32,
    action_height: u32,
) -> anyhow::Result<(Vec<u8>, u32, u32, f64)> {
    if action_width == 0 || action_height == 0 {
        anyhow::bail!("desktop action frame is empty: {action_width}x{action_height}");
    }

    let (capture_width, capture_height) = crate::capture::png_dimensions_pub(&png)?;
    let scale_x = f64::from(capture_width) / f64::from(action_width);
    let scale_y = f64::from(capture_height) / f64::from(action_height);
    if (scale_x - scale_y).abs() > 0.01 {
        anyhow::bail!(
            "desktop capture {capture_width}x{capture_height} cannot be mapped uniformly to \
             action frame {action_width}x{action_height} (scale {scale_x:.4}x{scale_y:.4})"
        );
    }

    if capture_width == action_width && capture_height == action_height {
        return Ok((png, action_width, action_height, 1.0));
    }

    let image = image::load_from_memory_with_format(&png, image::ImageFormat::Png)?;
    let resized = image.resize_exact(
        action_width,
        action_height,
        image::imageops::FilterType::Lanczos3,
    );
    let mut encoded = std::io::Cursor::new(Vec::new());
    resized.write_to(&mut encoded, image::ImageFormat::Png)?;
    Ok((encoded.into_inner(), action_width, action_height, scale_x))
}

// ── get_desktop_state ─────────────────────────────────────────────────────────

pub struct GetDesktopStateTool {
    state: Arc<ToolState>,
}
static GDS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetDesktopStateTool {
    fn def(&self) -> &ToolDef {
        GDS_DEF.get_or_init(|| ToolDef {
            name: "get_desktop_state".into(),
            description: "Capture the full display in the desktop action coordinate frame. \
                Use the returned PNG directly as the coordinate source for actions whose target is \
                {kind:\"desktop\",display_id:\"primary\"}. No AT-SPI walk.".into(),
            input_schema: json!({"type":"object","properties":{
                "session":{"type":"string","description":"For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Omit it to use the authenticated transport's implicit lifecycle session."},
                "screenshot_out_file":{"type":"string","description":"Write PNG here instead of base64."},
                "max_image_dimension": cua_driver_core::tool_schema::desktop_max_image_dimension_schema()
            },"additionalProperties":false}),
            read_only: true, destructive: false, idempotent: false, open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let capture_args = args.clone();
        let input = match parse_typed_input::<GetDesktopStateInput>("get_desktop_state", args) {
            Ok(input) => input,
            Err(result) => return result,
        };
        let out_file = input.screenshot_out_file;
        let max_image_dimension = input.max_image_dimension;
        let capture_service = self.state.capture_service.clone();

        let result = cua_driver_core::blocking::spawn(move || -> anyhow::Result<_> {
            // Capture the full display at native size first. When the
            // compositor consumes logical input coordinates, normalize the
            // image below so screenshot pixels still land exactly.
            //
            // The agent reads this image, so the Driver's own cursor and
            // session pill are hidden around the grab (or the limitation is
            // reported) instead of being baked over the controls it reads.
            // Hyprland: read the desktop frame once. The screenshot, its action
            // size and the window geometry below all use it.
            let hyprland_plan =
                if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
                    Some(crate::wayland::hyprland::desktop_capture_plan()?)
                } else {
                    None
                };
            let (native_png, overlay_capture) =
                cursor_overlay::capture_exclusion::capture_excluding_overlays(
                    &crate::overlay_capture::OverlayExcluder,
                    |hidden| {
                        // A multi-monitor layout is composed from the frame
                        // read above, so the image is exactly that frame.
                        let png = match &hyprland_plan {
                            Some(plan) if plan.composite => {
                                crate::wayland::hyprland::compose_desktop_capture(&plan.frame)?
                            }
                            _ => crate::capture::screenshot_display_bytes()?,
                        };
                        Ok::<_, anyhow::Error>(crate::overlay_capture::verify_hidden_capture(
                            png, hidden,
                        ))
                    },
                )?;
            let (native_w, native_h) = crate::capture::png_dimensions_pub(&native_png)?;
            // True screen size. On a pure-Wayland session (native backend
            // opted in, no X11 DISPLAY) the capture above came from the
            // wlroots `zwlr_screencopy` cascade, whose full-display buffer is
            // the whole output at native (physical) pixels — so the PNG
            // dimensions ARE the true screen size. Querying the X11 root
            // window here would fail with "$DISPLAY variable not set" and
            // abort the tool even though the screenshot already succeeded.
            // Only fall back to the X11 root-window geometry off Wayland, so
            // the X11 / XWayland path is unchanged. See #2017 / Sway testing.
            let hyprland_frame = hyprland_plan.map(|plan| plan.frame);
            let (screen_w, screen_h) = if let Some(frame) = &hyprland_frame {
                (frame.width, frame.height)
            } else if crate::wayland::is_wayland() {
                crate::capture_action_frame::desktop_action_dimensions((native_w, native_h))?
            } else {
                x11_screen_size()?
            };
            let (png, shot_w, shot_h, scale_factor) =
                normalize_desktop_capture_for_action_frame(native_png, screen_w, screen_h)?;
            // Opt-in long-edge cap: the full-size capture is the desktop
            // action frame; a capped image is mapped back at dispatch
            // (cua_driver_core::desktop_capture_scale) and by its capture_id.
            let (png, shot_w, shot_h) = match max_image_dimension.filter(|cap| *cap > 0) {
                Some(cap) if shot_w.max(shot_h) > cap => {
                    let png = crate::capture::resize_png_if_needed(&png, cap)?;
                    let (w, h) = crate::capture::png_dimensions_pub(&png)?;
                    (png, w, h)
                }
                _ => (png, shot_w, shot_h),
            };
            // Optional: write PNG to disk instead of returning base64.
            let written = if let Some(path) = out_file.as_deref() {
                std::fs::write(path, &png)?;
                Some(path.to_string())
            } else {
                None
            };
            use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
            let b64 = if written.is_some() {
                None
            } else {
                Some(B64.encode(&png))
            };
            // Which pid owns which window is what the agent needs before its
            // first click; without it the first action guesses `pid: 1`.
            let mut windows = crate::wayland::list_windows_dispatch(None);
            windows
                .retain(|w| w.is_on_screen && w.pid.is_some_and(crate::proc_fs::is_process_live));
            if let Some(frame) = &hyprland_frame {
                // Window geometry is in layout coordinates; screenshot pixel
                // (0, 0) is the frame origin. Refuse rather than pair this
                // capture with a layout that changed meanwhile.
                anyhow::ensure!(
                    crate::wayland::hyprland::desktop_frame()? == *frame,
                    "the Hyprland monitor layout changed during get_desktop_state; call it again"
                );
                frame.rebase_windows(&mut windows);
            }
            let capture_id = crate::capture_action_frame::publish_desktop(
                &capture_service,
                &capture_args,
                &png,
                (shot_w, shot_h),
                (screen_w, screen_h),
            )?;
            Ok((
                b64,
                shot_w,
                shot_h,
                screen_w,
                screen_h,
                scale_factor,
                written,
                windows,
                capture_id,
                overlay_capture,
            ))
        })
        .await;

        match result {
            Ok(Ok((
                b64_opt,
                shot_w,
                shot_h,
                screen_w,
                screen_h,
                scale_factor,
                written,
                windows,
                capture_id,
                overlay_capture,
            ))) => {
                let frame_scale = if shot_w > 0 {
                    f64::from(screen_w) / f64::from(shot_w)
                } else {
                    1.0
                };
                let mut content = Vec::new();
                let mut structured = json!({
                    "platform": "linux",
                    "display": "primary",
                    "screenshot_width": shot_w,
                    "screenshot_height": shot_h,
                    "screen_width": screen_w,
                    "screen_height": screen_h,
                    "scale_factor": scale_factor,
                    "frame_scale": frame_scale,
                    "screenshot_mime_type": "image/png",
                    "windows": windows.iter().map(window_record_json).collect::<Vec<_>>(),
                    "capture_id": capture_id,
                    "agent_overlay_capture": overlay_capture,
                });
                if (frame_scale - 1.0).abs() > 0.001 {
                    // Capped: the uncapped capture is the action frame.
                    structured["screenshot_original_width"] = json!(screen_w);
                    structured["screenshot_original_height"] = json!(screen_h);
                }
                if let Some(b64) = b64_opt {
                    content.push(cua_driver_core::protocol::Content::image_png(b64));
                }
                let window_lines = desktop_window_lines(&windows, frame_scale);
                let mut frame_note = if (frame_scale - 1.0).abs() > 0.001 {
                    format!(
                        "; x/y for scope:\"desktop\" actions are pixels of THIS screenshot \
                         (mapped ×{frame_scale:.2} back to the screen automatically)"
                    )
                } else {
                    String::new()
                };
                cursor_overlay::capture_exclusion::append_summary_note(
                    &mut frame_note,
                    &overlay_capture,
                );
                if let Some(path) = written {
                    structured["screenshot_file_path"] = json!(path);
                    content.push(cua_driver_core::protocol::Content::text(format!(
                        "✅ Desktop screenshot {shot_w}x{shot_h} written to {path} (screen {screen_w}x{screen_h}{frame_note}){window_lines}"
                    )));
                } else {
                    content.push(cua_driver_core::protocol::Content::text(format!(
                        "✅ Desktop screenshot {shot_w}x{shot_h} (screen {screen_w}x{screen_h}{frame_note}){window_lines}"
                    )));
                }
                ToolResult {
                    content,
                    is_error: None,
                    structured_content: Some(structured),
                    action_record: None,
                }
            }
            Ok(Err(e)) => ToolResult::error(format!("Capture error: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// Text block naming every visible window with the `pid` / `window_id` pair
/// that every other tool takes, so the model's first action targets a real
/// process instead of guessing.
fn desktop_window_lines(windows: &[crate::x11::WindowInfo], frame_scale: f64) -> String {
    let px = |v: i32| (f64::from(v) / frame_scale).round() as i32;
    let sz = |v: u32| (f64::from(v) / frame_scale).round() as u32;
    if windows.is_empty() {
        return "\nVisible windows: none (use list_windows / launch_app).".to_owned();
    }
    let mut out =
        String::from("\nVisible windows (use these pid + window_id values in every tool call):");
    for w in windows {
        let title = if w.title.is_empty() {
            "(no title)".to_owned()
        } else {
            format!("\"{}\"", w.title)
        };
        out.push_str(&format!(
            "\n- pid={} window_id={} {} {}x{} at ({},{}){}",
            w.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
            w.xid,
            title,
            sz(w.width),
            sz(w.height),
            px(w.x),
            px(w.y),
            if w.app_name.is_empty() {
                String::new()
            } else {
                format!(" app={}", w.app_name)
            }
        ));
    }
    out.push_str("\n→ get_window_state(pid, window_id) lists clickable elements; click/type_text take the same pid (+ window_id for x,y).");
    out
}

// ── get_cursor_position ───────────────────────────────────────────────────────

pub struct GetCursorPositionTool;
static GCP_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetCursorPositionTool {
    fn def(&self) -> &ToolDef {
        GCP_DEF.get_or_init(|| ToolDef {
            name: "get_cursor_position".into(),
            description:
                "Return the current mouse cursor position in screen points (origin top-left)."
                    .into(),
            input_schema: json!({"type":"object","properties":{
                "session": cua_driver_core::tool_schema::session_schema()
            },"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        if let Err(result) =
            parse_typed_input::<GetCursorPositionInput>("get_cursor_position", args)
        {
            return result;
        }
        // Hyprland reports the real pointer over IPC; return it in the
        // desktop-frame coordinates that get_desktop_state and desktop actions use.
        if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
            let position = cua_driver_core::blocking::spawn(|| -> anyhow::Result<(i32, i32)> {
                let (x, y) = crate::wayland::hyprland::cursor_position()?;
                Ok(crate::wayland::hyprland::desktop_frame()?
                    .from_layout(x.round() as i32, y.round() as i32))
            })
            .await;
            // The synthetic registry below holds layout coordinates, not the
            // desktop frame, so a failed compositor query is an error here.
            return match position {
                Ok(Ok((x, y))) => {
                    ToolResult::text(format!("✅ Cursor at ({x}, {y}) in the desktop frame"))
                        .with_structured(json!({ "x": x, "y": y, "source": "compositor" }))
                }
                Ok(Err(e)) => ToolResult::error(format!(
                    "Could not read the Hyprland pointer position: {e:#}"
                )),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }
        // Native Wayland: there's no protocol for clients to query the real
        // global cursor position. Fall back to the synthetic registry that
        // records every `motion_absolute` this process emits.
        if crate::wayland::is_wayland() {
            return match crate::wayland::last_synth_cursor_pos() {
                Some((x, y)) => ToolResult::text(
                    format!("✅ Cursor at ({x}, {y}) (synthetic — last move_cursor in this process)")
                ).with_structured(json!({
                    "x": x, "y": y, "source": "synthetic"
                })),
                None => ToolResult::text(
                    "Cursor position unknown on Wayland — no move_cursor has been issued in this process yet.".to_string()
                ).with_structured(json!({ "source": "synthetic", "available": false })),
            };
        }
        let result = cua_driver_core::blocking::spawn(|| {
            use x11rb::connection::Connection;
            use x11rb::protocol::xproto::ConnectionExt as _;
            use x11rb::rust_connection::RustConnection;
            let (conn, screen_num) = RustConnection::connect(None)?;
            let root = conn.setup().roots[screen_num].root;
            let reply = conn.query_pointer(root)?.reply()?;
            Ok::<(i32, i32), anyhow::Error>((reply.root_x as i32, reply.root_y as i32))
        })
        .await;
        match result {
            // Text format matches Swift `GetCursorPositionTool` 1:1.
            Ok(Ok((x, y))) => ToolResult::text(format!("✅ Cursor at ({x}, {y})"))
                .with_structured(json!({ "x": x, "y": y, "source": "x11" })),
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── move_cursor ───────────────────────────────────────────────────────────────

pub struct MoveCursorTool {
    state: Arc<ToolState>,
}

static MCURSOR_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CursorControlScope {
    Agent,
    Desktop,
}

fn cursor_control_scope(args: &Value) -> CursorControlScope {
    match args.get("scope").and_then(Value::as_str) {
        Some("desktop") => CursorControlScope::Desktop,
        _ => CursorControlScope::Agent,
    }
}

#[async_trait]
impl Tool for MoveCursorTool {
    fn def(&self) -> &ToolDef {
        MCURSOR_DEF.get_or_init(|| ToolDef {
            name: "move_cursor".into(),
            description: "Move the synthetic agent cursor without changing the user's pointer. Only an explicit scope=desktop request moves the real OS pointer in get_desktop_state coordinates.".into(),
            input_schema: json!({"type":"object","required":["x","y"],"properties":{
                "x":{"type":"number","description":"Destination X. Window scope: window-local pixels of window_id when pid and window_id are given, otherwise screen coordinates of the agent cursor overlay. Desktop scope: get_desktop_state screenshot pixels."},
                "y":{"type":"number","description":"Destination Y, in the same space as x."},
                "pid":{"type":"integer","minimum":1,"description":"Window scope: process ID of the target window. Supply with window_id; required on GNOME Wayland and in Wayland inject mode."},
                "window_id":{"type":"integer","minimum":1,"description":"Window scope: target window id, supplied with pid. Makes x and y window-local."},
                "session": cua_driver_core::tool_schema::session_schema(),
                "cursor_id":{"type":"string","description":"Cursor instance to move. Default: 'default'."},
                "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"\"window\" (default) moves only the agent cursor overlay; \"desktop\" moves the real OS pointer."}
            },"additionalProperties":false}),
            read_only: false, destructive: false, idempotent: true, open_world: false,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        if cursor_control_scope(&args) == CursorControlScope::Desktop {
            let input = match parse_typed_projection::<MoveCursorInput>("move_cursor", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let (x, y) = (input.x, input.y);
            let xi = x.round() as i32;
            let yi = y.round() as i32;
            let wayland = crate::wayland::wayland_input_enabled();
            let path = if wayland {
                "wayland_desktop"
            } else {
                "xtest_desktop"
            };
            let result = if wayland {
                cua_driver_core::blocking::spawn(move || {
                    let space = crate::wayland::DesktopInputSpace::current()?;
                    crate::wayland::move_cursor_desktop(&space, xi, yi)
                })
                .await
            } else {
                cua_driver_core::blocking::spawn(move || {
                    crate::input::send_move_xtest_desktop(xi, yi)
                })
                .await
            };
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Moved the real desktop pointer to ({xi}, {yi})."
                ))
                .with_structured(
                    json!({"scope":"desktop","path":path,"x":xi,"y":yi,"effect":"unverifiable"}),
                ),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }
        let x = args.f64_or("x", 0.0);
        let y = args.f64_or("y", 0.0);
        let window_id = args.get("window_id").and_then(|v| v.as_u64());
        let pid = args.get("pid").and_then(|v| v.as_u64());
        if (crate::wayland::is_gnome_wayland_session() || crate::wayland::is_inject_mode())
            && (pid.is_none() || window_id.is_none())
        {
            return ToolResult::error(
                "exact_target_required: targeted Wayland pointer movement requires caller-approved pid and window_id",
            );
        }
        let target = match (pid, window_id) {
            (Some(pid), Some(window_id)) => {
                let pid = match u32::try_from(pid) {
                    Ok(pid) => pid,
                    Err(_) => return ToolResult::error("pid exceeds the supported range"),
                };
                match cua_driver_core::blocking::spawn(move || {
                    crate::wayland::establish_exact_target(pid, window_id)
                })
                .await
                {
                    Ok(Ok(target)) => Some(target),
                    Ok(Err(error)) => return ToolResult::error(error.to_string()),
                    Err(error) => return ToolResult::error(format!("Task error: {error}")),
                }
            }
            (None, None) => None,
            _ => {
                return ToolResult::error(
                    "exact_target_required: pid and window_id must be supplied together",
                );
            }
        };
        let cursor_id = resolve_cursor_key(&args);
        let (output_x, output_y) = if let Some(target) = target.as_ref() {
            let (output_x, output_y) = crate::wayland::window_local_to_output(
                target.window_id(),
                x.round() as i32,
                y.round() as i32,
            );
            (f64::from(output_x), f64::from(output_y))
        } else {
            (x, y)
        };
        self.state
            .cursor_registry
            .update_position(&cursor_id, output_x, output_y);
        if let Some(target) = target.as_ref() {
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::PinAbove(target.window_id()),
            );
        }
        // End pointing upper-left (45°) — matches Swift's
        // `AgentCursor.animateAndWait(endAngleDegrees: 45)` convention so the
        // overlay arrow settles to the natural macOS-style pose.
        // Use the acknowledged animation path so a first-ever move seeds and
        // displays the session cursor just as reliably as a coordinate click.
        reveal_pointer_action_for(&self.state, &cursor_id, output_x, output_y, false).await;
        ToolResult::text(format!(
            "Agent cursor '{cursor_id}' moved to ({output_x:.1}, {output_y:.1}); the user pointer was unchanged."
        ))
    }
}

// ── set_agent_cursor_enabled ──────────────────────────────────────────────────

pub struct SetAgentCursorEnabledV2Tool {
    state: Arc<ToolState>,
}

static CURSOR_ENABLED_V2_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for SetAgentCursorEnabledV2Tool {
    fn def(&self) -> &ToolDef {
        CURSOR_ENABLED_V2_DEF.get_or_init(|| canonical_cursor_def("set_agent_cursor_enabled"))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let enabled = match args.get("enabled").and_then(Value::as_bool) {
            Some(value) => value,
            None => return ToolResult::error("Missing required boolean field `enabled`."),
        };
        let session = resolve_cursor_key(&args);
        self.state.cursor_registry.set_enabled(&session, enabled);
        crate::overlay::send_command_for(
            session.clone(),
            cursor_overlay::OverlayCommand::SetEnabled(enabled),
        );
        ToolResult::text(format!(
            "Agent cursor for session '{session}' {}.",
            if enabled { "enabled" } else { "disabled" }
        ))
        .with_structured(json!({"session":session,"enabled":enabled}))
    }
}

pub struct SetAgentCursorMotionV2Tool;

static CURSOR_MOTION_V2_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn cursor_number(value: Option<&Value>) -> Option<f64> {
    value.and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_i64().map(|integer| integer as f64))
    })
}

#[async_trait]
impl Tool for SetAgentCursorMotionV2Tool {
    fn def(&self) -> &ToolDef {
        CURSOR_MOTION_V2_DEF.get_or_init(|| canonical_cursor_def("set_agent_cursor_motion"))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let session = resolve_cursor_key(&args);
        let current = crate::overlay::current_motion_for(&session);
        let motion = current.with_overrides(
            cursor_number(args.get("start_handle")),
            cursor_number(args.get("end_handle")),
            cursor_number(args.get("arc_size")),
            cursor_number(args.get("arc_flow")),
            cursor_number(args.get("spring")),
            cursor_number(args.get("glide_duration_ms")),
            cursor_number(args.get("dwell_after_click_ms")),
            cursor_number(args.get("idle_hide_ms")),
            None,
            cursor_number(args.get("turn_radius")),
        );
        let motion = match motion.with_style_args(&args) {
            Ok(motion) => motion,
            Err(message) => return ToolResult::error(message),
        };
        crate::overlay::send_command_for(
            session.clone(),
            cursor_overlay::OverlayCommand::SetMotion(motion.clone()),
        );
        ToolResult::text(format!(
            "Agent cursor motion updated for session '{session}'."
        ))
        .with_structured(json!({"session":session,"motion":motion.output_json()}))
    }
}

pub struct SetAgentCursorThemeTool {
    state: Arc<ToolState>,
}

static CURSOR_THEME_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn cursor_reduced_motion(value: Option<&str>) -> cursor_overlay::ReducedMotion {
    match value {
        Some("on") => cursor_overlay::ReducedMotion::On,
        Some("off") => cursor_overlay::ReducedMotion::Off,
        _ => cursor_overlay::ReducedMotion::Auto,
    }
}

#[async_trait]
impl Tool for SetAgentCursorThemeTool {
    fn def(&self) -> &ToolDef {
        CURSOR_THEME_DEF.get_or_init(|| canonical_cursor_def("set_agent_cursor_theme"))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let Some(theme_id) = args.get("theme_id").and_then(Value::as_str) else {
            return ToolResult::error("Missing required string field `theme_id`.");
        };
        let session = resolve_cursor_key(&args);
        let resolved_theme = match cursor_overlay::resolve_theme_selection(theme_id) {
            Ok(theme) => theme,
            Err(error) => {
                return ToolResult::error(format!(
                    "Cursor theme '{theme_id}' cannot be selected: {error}"
                ));
            }
        };
        let (version, profile) = resolved_theme
            .as_deref()
            .map(|theme| (theme.version.as_str(), theme.profile.as_str()))
            .unwrap_or((
                cursor_overlay::DEFAULT_THEME_VERSION,
                cursor_overlay::THEME_PROFILE,
            ));
        let reduced_motion =
            cursor_reduced_motion(args.get("reduced_motion").and_then(Value::as_str));
        self.state
            .cursor_registry
            .update_config(&session, |config| {
                config.theme_id = theme_id.to_owned();
                config.reduced_motion = reduced_motion;
            });
        crate::overlay::send_command_for(
            session.clone(),
            cursor_overlay::OverlayCommand::SetTheme {
                theme_id: theme_id.to_owned(),
                reduced_motion,
            },
        );
        ToolResult::text(format!(
            "Agent cursor theme for session '{session}' set to '{theme_id}'."
        ))
        .with_structured(json!({"session":session,"theme":{
            "id":theme_id,
            "version":version,
            "profile":profile,
            "reduced_motion":reduced_motion,
            "fallback":null
        }}))
    }
}

pub struct GetAgentCursorStateV2Tool {
    state: Arc<ToolState>,
}

static CURSOR_STATE_V2_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetAgentCursorStateV2Tool {
    fn def(&self) -> &ToolDef {
        CURSOR_STATE_V2_DEF.get_or_init(|| canonical_cursor_def("get_agent_cursor_state"))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let session = resolve_cursor_key(&args);
        let enabled = crate::overlay::is_enabled_for(&session);
        let motion = crate::overlay::current_motion_for(&session);
        let (theme_id, version, profile, fallback, visual) =
            crate::overlay::current_theme_state_for(&session).unwrap_or_else(|| {
                (
                    cursor_overlay::DEFAULT_THEME_ID.into(),
                    cursor_overlay::DEFAULT_THEME_VERSION.into(),
                    cursor_overlay::THEME_PROFILE.into(),
                    None,
                    cursor_overlay::CursorVisualState::default(),
                )
            });
        let modifiers: Vec<&str> = [
            visual.delivery.map(|value| value.as_str()),
            visual.target.map(|value| value.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect();
        // The last point this session's cursor was placed at, `null` until
        // it first moves (same registry source as macOS).
        let position = self
            .state
            .cursor_registry
            .get(&session)
            .and_then(|cursor| cursor.x.zip(cursor.y))
            .map(|(x, y)| json!({"x": x, "y": y}));
        ToolResult::text(format!("Agent cursor state for session '{session}'.")).with_structured(
            json!({
                "session":session,
                "enabled":enabled,
                "position":position,
                "theme":{
                    "id":theme_id,
                    "version":version,
                    "profile":profile,
                    "reduced_motion":visual.reduced_motion,
                    "fallback":fallback
                },
                "visual_state":{
                    "requested_action":visual.requested_action,
                    "resolved_action":visual.resolved_action,
                    "modifiers":modifiers,
                    "phase":visual.phase(),
                    "frame":visual.frame(),
                    "preempted_count":visual.preempted_count
                },
                "motion":motion.output_json()
            }),
        )
    }
}

fn canonical_cursor_def(name: &str) -> ToolDef {
    let contract = cua_driver_contract::tool_contract(name)
        .unwrap_or_else(|| panic!("missing canonical cursor contract for {name}"));
    ToolDef::from_contract(&contract)
}

// ── check_permissions ─────────────────────────────────────────────────────────

pub struct CheckPermissionsTool;
static PERMS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for CheckPermissionsTool {
    fn def(&self) -> &ToolDef {
        PERMS_DEF.get_or_init(|| ToolDef {
            name: "check_permissions".into(),
            description: "Check required permissions for cua-driver-rs on Linux.".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }
    async fn invoke(&self, _args: Value) -> ToolResult {
        // Check X11 connectivity (required for window enumeration and input injection).
        let x11_ok = cua_driver_core::blocking::spawn(|| {
            x11rb::rust_connection::RustConnection::connect(None).is_ok()
        })
        .await
        .unwrap_or(false);

        // Check AT-SPI: not merely "is there a session bus?" but "does
        // org.a11y.Bus actually answer on it?" — the previous env-var-or-
        // /run/user heuristic false-passed exactly the headless/container case
        // (/run/user exists, but no a11y bus → empty trees). Probe for real.
        let dbus_address = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok();
        let atspi_ok = cua_driver_core::blocking::spawn(crate::health_report::probe_a11y_bus)
            .await
            .unwrap_or(false);

        let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
        let atspi_status = if atspi_ok {
            match &dbus_address {
                Some(a) => format!("✅ org.a11y.Bus reachable (DBUS_SESSION_BUS_ADDRESS={a})"),
                None => "✅ org.a11y.Bus reachable".to_string(),
            }
        } else if dbus_address.is_none() {
            "❌ no session bus (DBUS_SESSION_BUS_ADDRESS unset and none auto-discovered) — \
             AT-SPI trees will be empty; start the daemon inside the desktop session"
                .to_string()
        } else {
            "❌ session bus present but org.a11y.Bus has no owner — enable accessibility \
             (gsettings set org.gnome.desktop.interface toolkit-accessibility true) / \
             start at-spi-bus-launcher"
                .to_string()
        };
        let status_text = format!(
            "X11 display: {}\nWayland: {}\nAT-SPI (D-Bus): {}\nXSendEvent injection: {}",
            if x11_ok {
                "✅ connected"
            } else {
                "❌ DISPLAY not set or X11 unavailable"
            },
            match &wayland_display {
                Some(s) if crate::wayland::wayland_enabled() => format!(
                    "✅ native Wayland session (WAYLAND_DISPLAY={s}) — experimental backend ENABLED"
                ),
                Some(s) => format!(
                    "⚠️  native Wayland session (WAYLAND_DISPLAY={s}) — experimental backend OFF; \
                     set {}=1 to enable it",
                    crate::wayland::ENABLE_WAYLAND_ENV
                ),
                None => "❌ not a Wayland session".to_string(),
            },
            atspi_status,
            if x11_ok {
                "✅ available"
            } else {
                "❌ requires X11"
            }
        );
        ToolResult::text(status_text)
            .with_structured(json!({ "x11": x11_ok, "wayland": wayland_display.is_some(), "wayland_enabled": crate::wayland::wayland_enabled(), "atspi": atspi_ok, "dbus_session_bus_address": dbus_address, "xsend_event": x11_ok }))
    }
}

// ── get_config ────────────────────────────────────────────────────────────────

pub struct GetConfigTool {
    state: Arc<ToolState>,
}
static GCFG_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetConfigTool {
    fn def(&self) -> &ToolDef {
        GCFG_DEF.get_or_init(|| ToolDef {
            name: "get_config".into(),
            description: "Return current cua-driver-rs configuration, including the agent_cursor.glide_duration_ms default. Explicit motion overrides are reported by get_agent_cursor_state.".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }
    async fn invoke(&self, _args: Value) -> ToolResult {
        let cfg = self.state.config.read().unwrap();
        let (pip_enabled, pip_geometry) = pip_preview::read_pip_keys_from_file();
        ToolResult::text("cua-driver-rs configuration").with_structured(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "source_sha": option_env!("CUA_DRIVER_SOURCE_SHA"),
            "platform": "linux",
            "capture_mode": cfg.capture_mode,
            "max_image_dimension": cfg.max_image_dimension,
            "agent_cursor": { "glide_duration_ms": cfg.agent_cursor_glide_duration_ms },
            "cursor": { "motion": cursor_overlay::motion_defaults::read_saved().config_json() },
            "experimental_pip": pip_enabled,
            "experimental_pip_geometry": pip_geometry
        }))
    }
}

// ── set_config ────────────────────────────────────────────────────────────────

fn with_cursor_motion_config_properties(mut schema: Value) -> Value {
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.extend(cursor_overlay::motion_defaults::config_schema_properties());
    }
    schema
}

pub struct SetConfigTool {
    state: Arc<ToolState>,
}
static SCFG_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for SetConfigTool {
    fn def(&self) -> &ToolDef {
        SCFG_DEF.get_or_init(|| ToolDef {
            name: "set_config".into(),
            description: "Update cua-driver-rs configuration. capture_mode / \
                max_image_dimension and agent_cursor_glide_duration_ms take effect immediately.\n\n\
                Two input shapes (both accepted, matching Windows/Swift):\n\
                - **{key, value}** (preferred): `{\"key\": \"max_image_dimension\", \"value\": 800}` \
                  — single leaf write.\n\
                - **Legacy per-field**: `{\"capture_mode\": \"som\", \"max_image_dimension\": 0}`.\n\n\
                The experimental_pip keys persist to ~/.cua-driver/config.json and apply on next \
                daemon restart (the PiP backend is initialised once at startup; \
                Linux ships only the trait stub today — see issue #1729).\n\n\
                `cursor.motion.style`, `cursor.motion.timing` and `cursor.motion.effects.<name>` \
                save the default cursor motion for sessions started afterwards.".into(),
            input_schema: with_cursor_motion_config_properties(json!({"type":"object","properties":{
                "key":{"type":"string","description":"Name of a single config field to write ({key, value} shape). Pair with `value`."},
                "value":{"description":"New value for `key`. JSON type depends on the key."},
                "capture_mode":{"type":"string","enum":["ax","vision"],"description":"Legacy per-field shape. Default capture mode for get_window_state. (\"som\"/\"screenshot\" still decode as deprecated aliases.)"},
                "agent_cursor_glide_duration_ms": cua_driver_core::agent_cursor::glide_duration_config_schema(),
                "max_image_dimension":{"type":"integer","description":"Legacy per-field shape. Max dimension for screenshot resizing (0 = no limit)."},
                "experimental_pip":{"type":"boolean","description":"Enable the experimental PiP preview window (applies next restart; Linux backend stubbed)."},
                "experimental_pip_geometry":{"type":"string","description":"PiP window size + optional position in `WxH` or `WxH+X+Y` form."}
            },"additionalProperties":false})),
            read_only: false, destructive: false, idempotent: true, open_world: false,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        if args.get("capture_scope").is_some()
            || args.get("key").and_then(Value::as_str) == Some("capture_scope")
        {
            return ToolResult::error(
                "config key 'capture_scope' is retired; select a window or desktop target on each action",
            )
            .with_structured(json!({
                "code": "config_key_retired",
                "key": "capture_scope",
                "replacement": "action.target",
            }));
        }
        let glide = match cua_driver_core::agent_cursor::glide_duration_config_arg(&args) {
            Ok(value) => value,
            Err(message) => return ToolResult::error(message),
        };
        let motion_keys = match cursor_overlay::motion_defaults::apply_config_args(&args) {
            Ok(keys) => keys,
            Err(message) => return ToolResult::error(message),
        };
        let mut cfg = self.state.config.write().unwrap();
        let mut parts: Vec<String> = motion_keys
            .iter()
            .map(|key| format!("{key} (applies to sessions started from now on)"))
            .collect();
        if let Some(glide) = glide {
            if let Err(error) = crate::overlay::set_default_glide_duration(None, glide) {
                return ToolResult::error(error.to_string()).with_structured(json!({
                    "code": "cursor_overlay_unavailable",
                }));
            }
            cfg.agent_cursor_glide_duration_ms = glide;
            if let Err(e) = pip_preview::write_config_key(
                cua_driver_core::agent_cursor::GLIDE_DURATION_CONFIG_KEY,
                json!(glide),
            ) {
                tracing::warn!("set_config: failed to persist agent_cursor_glide_duration_ms: {e}");
            }
            parts.push(format!("agent_cursor_glide_duration_ms={glide}"));
        }
        // {key, value} shape (what the Swift/macOS and Windows callers send).
        // Linux previously read only the legacy per-field keys below, so a
        // `{"key":"max_image_dimension","value":800}` write was silently
        // dropped (issue #1923). Dispatch on `key` to the same fields.
        if let (Some(key), Some(val)) =
            (args.get("key").and_then(|v| v.as_str()), args.get("value"))
        {
            match key {
                "agent_cursor_glide_duration_ms" => {}
                "capture_mode" => match val.as_str() {
                    Some(s) => {
                        cfg.capture_mode = s.to_owned();
                        if let Err(e) = pip_preview::write_config_key(
                            "capture_mode",
                            Value::String(s.to_owned()),
                        ) {
                            tracing::warn!("set_config: failed to persist capture_mode: {e}");
                        }
                        parts.push(format!("capture_mode={s}"));
                    }
                    None => {
                        return ToolResult::error(format!(
                            "`capture_mode` must be a string, got {val}."
                        ));
                    }
                },
                "max_image_dimension" => match val.as_u64() {
                    Some(n) => {
                        cfg.max_image_dimension = n as u32;
                        if let Err(e) =
                            pip_preview::write_config_key("max_image_dimension", Value::from(n))
                        {
                            tracing::warn!(
                                "set_config: failed to persist max_image_dimension: {e}"
                            );
                        }
                        parts.push(format!("max_image_dimension={n}"));
                    }
                    None => {
                        return ToolResult::error(format!(
                            "`max_image_dimension` must be an integer, got {val}."
                        ));
                    }
                },
                "experimental_pip" => match val.as_bool() {
                    Some(b) => {
                        if let Err(e) =
                            pip_preview::write_config_key("experimental_pip", Value::Bool(b))
                        {
                            return ToolResult::error(format!(
                                "failed to persist experimental_pip: {e}"
                            ));
                        }
                        parts.push(format!("experimental_pip={b} (next restart)"));
                    }
                    None => {
                        return ToolResult::error(format!(
                            "`experimental_pip` must be a boolean, got {val}."
                        ));
                    }
                },
                "experimental_pip_geometry" => match val.as_str() {
                    Some(s) => {
                        if pip_preview::PipGeometry::parse(s).is_none() {
                            return ToolResult::error(format!(
                                "experimental_pip_geometry `{s}` is not a valid WxH or WxH+X+Y string"
                            ));
                        }
                        if let Err(e) = pip_preview::write_config_key(
                            "experimental_pip_geometry",
                            Value::String(s.to_owned()),
                        ) {
                            return ToolResult::error(format!(
                                "failed to persist experimental_pip_geometry: {e}"
                            ));
                        }
                        parts.push(format!("experimental_pip_geometry={s} (next restart)"));
                    }
                    None => {
                        return ToolResult::error(format!(
                            "`experimental_pip_geometry` must be a string, got {val}."
                        ));
                    }
                },
                other if motion_keys.iter().any(|written| written == other) => {}
                other => return ToolResult::error(format!(
                    "Unknown config key `{other}`. Known: capture_mode, max_image_dimension, agent_cursor_glide_duration_ms, experimental_pip, experimental_pip_geometry, cursor.motion.style, cursor.motion.timing, cursor.motion.effects.<name>."
                )),
            }
        }
        // Legacy per-field shape.
        if let Some(mode) = args.opt_str("capture_mode") {
            if let Err(e) =
                pip_preview::write_config_key("capture_mode", Value::String(mode.clone()))
            {
                tracing::warn!("set_config: failed to persist capture_mode: {e}");
            }
            parts.push(format!("capture_mode={mode}"));
            cfg.capture_mode = mode;
        }
        if let Some(dim) = args.opt_u64("max_image_dimension") {
            cfg.max_image_dimension = dim as u32;
            if let Err(e) = pip_preview::write_config_key("max_image_dimension", Value::from(dim)) {
                tracing::warn!("set_config: failed to persist max_image_dimension: {e}");
            }
            parts.push(format!("max_image_dimension={dim}"));
        }
        if let Some(enabled) = args.get("experimental_pip").and_then(|v| v.as_bool()) {
            if let Err(e) = pip_preview::write_config_key("experimental_pip", Value::Bool(enabled))
            {
                return ToolResult::error(format!("failed to persist experimental_pip: {e}"));
            }
            parts.push(format!("experimental_pip={enabled} (next restart)"));
        }
        if let Some(geom) = args.opt_str("experimental_pip_geometry") {
            if pip_preview::PipGeometry::parse(&geom).is_none() {
                return ToolResult::error(format!(
                    "experimental_pip_geometry `{geom}` is not a valid WxH or WxH+X+Y string"
                ));
            }
            if let Err(e) = pip_preview::write_config_key(
                "experimental_pip_geometry",
                Value::String(geom.clone()),
            ) {
                return ToolResult::error(format!(
                    "failed to persist experimental_pip_geometry: {e}"
                ));
            }
            parts.push(format!("experimental_pip_geometry={geom} (next restart)"));
        }
        let msg = if parts.is_empty() {
            "Config unchanged (no known parameters).".to_owned()
        } else {
            format!("Config updated: {}", parts.join(", "))
        };
        let (pip_enabled, pip_geometry) = pip_preview::read_pip_keys_from_file();
        ToolResult::text(msg).with_structured(json!({
            "capture_mode": cfg.capture_mode,
            "max_image_dimension": cfg.max_image_dimension,
            "agent_cursor": { "glide_duration_ms": cfg.agent_cursor_glide_duration_ms },
            "experimental_pip": pip_enabled,
            "experimental_pip_geometry": pip_geometry
        }))
    }
}

// ── get_accessibility_tree ────────────────────────────────────────────────────

pub struct GetAccessibilityTreeTool;

static GAX_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetAccessibilityTreeTool {
    fn def(&self) -> &ToolDef {
        GAX_DEF.get_or_init(|| ToolDef {
            name: "get_accessibility_tree".into(),
            description: "Return a lightweight snapshot of the desktop: running processes and \
                on-screen visible X11 windows with their bounds and owner pid.\n\n\
                For the full AT-SPI subtree of a single window (with interactive element indices \
                you can click by), use get_window_state instead — this is a fast discovery read."
                .into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }
    async fn invoke(&self, _args: Value) -> ToolResult {
        let (procs, windows) = cua_driver_core::blocking::spawn(|| {
            (
                crate::proc_fs::list_processes(),
                crate::x11::list_windows(None),
            )
        })
        .await
        .unwrap_or_default();

        let mut lines = vec![format!(
            "{} running process(es), {} visible window(s)",
            procs.len(),
            windows.len()
        )];
        for p in &procs {
            let cmd = if p.cmdline.is_empty() {
                p.name.clone()
            } else {
                p.cmdline.clone()
            };
            lines.push(format!("- {} (pid {})", cmd, p.pid));
        }
        if !windows.is_empty() {
            lines.push(String::new());
            lines.push("Windows:".to_owned());
            for w in &windows {
                let title = if w.title.is_empty() {
                    "(no title)".to_owned()
                } else {
                    format!("\"{}\"", w.title)
                };
                lines.push(format!(
                    "- pid={} window_id={} {} {}x{}+{}+{}",
                    w.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
                    w.xid,
                    title,
                    w.width,
                    w.height,
                    w.x,
                    w.y
                ));
            }
            lines.push(
                "→ Call get_window_state(pid, window_id) to inspect a window's UI.".to_owned(),
            );
        }

        let structured = json!({
            "processes": procs.iter().map(|p| json!({"pid":p.pid,"name":p.name})).collect::<Vec<_>>(),
            "windows": windows.iter().map(|w| json!({
                "window_id": w.xid, "pid": w.pid, "title": w.title,
                "x": w.x, "y": w.y, "width": w.width, "height": w.height
            })).collect::<Vec<_>>()
        });
        ToolResult::text(lines.join("\n")).with_structured(structured)
    }
}

// ── zoom ──────────────────────────────────────────────────────────────────────

pub struct ZoomTool {
    state: Arc<ToolState>,
}
static ZOOM_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for ZoomTool {
    fn def(&self) -> &ToolDef {
        ZOOM_DEF.get_or_init(|| ToolDef {
            name: "zoom".into(),
            description: "Capture a cropped JPEG of a window region (x1,y1)–(x2,y2) in \
                screenshot pixels, with 20% padding. Output is at most 500 px wide.\n\n\
                After a zoom, pass from_zoom=true to click/type_text to auto-translate \
                coordinates back to full-window space. Coordinate actions return \
                `screenshot_context_missing` when no current snapshot contains a \
                screenshot owned by this session. `from_zoom` actions return \
                `zoom_context_missing` when the zoom was never created or was replaced; call \
                `get_window_state`, then `zoom`, again on the same connection.".into(),
            input_schema: json!({
                "type":"object","required":["window_id","x1","y1","x2","y2"],"properties":{
                    "window_id":{"type":"integer","description":"Window id of the window captured by get_window_state."},
                    "pid":{"type":"integer","description":"Optional target pid. When omitted, the driver resolves the unique current snapshot for this session and window."},
                    "x1":{"type":"number","description":"Left edge of the region in window-local screenshot pixels."},
                    "y1":{"type":"number","description":"Top edge of the region in window-local screenshot pixels."},
                    "x2":{"type":"number","description":"Right edge of the region in window-local screenshot pixels."},
                    "y2":{"type":"number","description":"Bottom edge of the region in window-local screenshot pixels."}
                },"additionalProperties":false
            }),
            read_only: true, destructive: false, idempotent: true, open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let xid = match args.require_u64("window_id") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let requested_pid = match args.get("pid") {
            None => None,
            Some(value) => match value.as_u64().and_then(|pid| i32::try_from(pid).ok()) {
                Some(pid) => Some(pid),
                None => return ToolResult::error("pid must be a positive 32-bit integer"),
            },
        };
        let session_id = args.opt_str("_session_id");
        let (pid, screenshot) = match self.state.snapshots.screenshot_context_for_zoom(
            requested_pid,
            xid,
            session_id.as_deref(),
        ) {
            Ok(context) => context,
            Err(refusal) => return refusal,
        };
        let x1 = match args.opt_f64("x1") {
            Some(v) => v,
            None => return ToolResult::error("Missing x1"),
        };
        let y1 = match args.opt_f64("y1") {
            Some(v) => v,
            None => return ToolResult::error("Missing y1"),
        };
        let x2 = match args.opt_f64("x2") {
            Some(v) => v,
            None => return ToolResult::error("Missing x2"),
        };
        let y2 = match args.opt_f64("y2") {
            Some(v) => v,
            None => return ToolResult::error("Missing y2"),
        };
        if x2 <= x1 || y2 <= y1 {
            return ToolResult::error("x2 must be > x1 and y2 must be > y1");
        }

        let (x1, y1, x2, y2) = (
            x1 * screenshot.scale,
            y1 * screenshot.scale,
            x2 * screenshot.scale,
            y2 * screenshot.scale,
        );
        let state = self.state.clone();
        let result = cua_driver_core::blocking::spawn(move || {
            // Route through the Wayland-aware window capture dispatcher so
            // pure-Wayland sessions surface a typed "per-window capture not
            // supported yet" error instead of accidentally calling the
            // X11-only path with a foreign-toplevel id.
            let png = crate::wayland::screenshot_window_dispatch(xid)?;
            cursor_overlay::capture_utils::crop_png_to_jpeg(&png, x1, y1, x2, y2, 500)
        })
        .await;

        match result {
            Ok(Ok(crop)) => {
                if let Err(refusal) = state.snapshots.set_zoom(
                    pid,
                    session_id.as_deref(),
                    ZoomContext {
                        screenshot,
                        origin_x: crop.origin_x,
                        origin_y: crop.origin_y,
                        scale_inv: crop.scale_inv,
                    },
                ) {
                    return refusal;
                }
                use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
                let b64 = B64.encode(&crop.jpeg_bytes);
                let (w, h) = (crop.out_w, crop.out_h);
                use cua_driver_core::protocol::Content;
                ToolResult {
                    content: vec![
                        Content::image_jpeg(b64),
                        Content::text(format!(
                            "Zoom ({x1:.0},{y1:.0})–({x2:.0},{y2:.0}) → {w}×{h} px JPEG."
                        )),
                    ],
                    is_error: None,
                    // Surface 7: `mime_type` mirrors the MCP image part's `mimeType`
                    // onto the structured payload (additive — `format` stays).
                    structured_content: Some(json!({
                        "width": w, "height": h, "format": "jpeg",
                        "mime_type": "image/jpeg"
                    })),
                    action_record: None,
                }
            }
            Ok(Err(e)) => ToolResult::error(format!("Zoom failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── type_text_chars ───────────────────────────────────────────────────────────

pub struct TypeTextCharsTool;
static TYPE_CHARS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for TypeTextCharsTool {
    fn def(&self) -> &ToolDef {
        TYPE_CHARS_DEF.get_or_init(|| ToolDef {
            name: "type_text_chars".into(),
            description: "Type text character-by-character with a configurable inter-character \
                delay (default 30 ms). Useful for apps that miss keystrokes. \
                Otherwise identical to type_text (XSendEvent, no focus steal).".into(),
            input_schema: json!({
                "type":"object","required":["pid","text"],"properties":{
                    "pid":{"type":"integer","description":"Target process ID."},
                    "window_id":{"type":"integer","description":"Window id from list_windows. Required with element_index."},
                    "text":{"type":"string","description":"Text to type, one character at a time."},
                    "delay_ms":{"type":"integer","description":"Milliseconds between chars (default 30)."},
                    "type_chars_only":{"type":"boolean","description":"Skip element focus, type directly. Default false."}
                },"additionalProperties":false
            }),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        #[allow(unused_assignments)]
        let pid = args.u64_or("pid", 0) as u32;
        let text_raw = match args.require_str("text") {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Same trailing-protocol-tag scrub as TypeTextTool — see
        // cua_driver_core::text_sanitize for rationale.
        let text = cua_driver_core::text_sanitize::strip_trailing_agent_protocol_tags(&text_raw)
            .into_owned();
        let delay_ms = args.u64_or("delay_ms", 30);
        let xid_opt = args.opt_u64("window_id");
        let xid = match xid_opt {
            Some(x) => x,
            None => {
                let windows = cua_driver_core::blocking::spawn(move || {
                    crate::x11::list_windows(if pid == 0 { None } else { Some(pid) })
                })
                .await
                .unwrap_or_default();
                // pid omitted: the keys go to the active window, like a
                // physical keyboard would, and the action adopts its pid.
                let chosen = if pid == 0 {
                    let active = crate::x11::active_window();
                    windows
                        .iter()
                        .find(|w| Some(w.xid) == active && w.pid.is_some())
                        .or_else(|| windows.iter().find(|w| w.is_on_screen && w.pid.is_some()))
                } else {
                    // The pid's active window, else its largest on-screen
                    // toplevel: LibreOffice and GIMP own hidden/utility
                    // toplevels that a plain `first()` could pick, and an
                    // unmapped window cannot take the virtual keyboard focus.
                    let active = crate::x11::active_window();
                    windows
                        .iter()
                        .find(|w| Some(w.xid) == active)
                        .or_else(|| {
                            windows
                                .iter()
                                .filter(|w| w.is_on_screen)
                                .max_by_key(|w| u64::from(w.width) * u64::from(w.height))
                        })
                        .or_else(|| windows.first())
                };
                match chosen {
                    Some(w) => w.xid,
                    None => {
                        return ToolResult::error(format!(
                            "No windows found for pid {pid}. Provide window_id."
                        ));
                    }
                }
            }
        };
        let text_len = text.chars().count();
        let result = cua_driver_core::blocking::spawn(move || {
            if crate::wayland::wayland_input_enabled() {
                // Per-char `wtype` loop with the requested delay — mirrors the
                // X11 XSendEvent per-char path. Sleeping here is fine because
                // we're inside spawn_blocking.
                let mut buf = [0u8; 4];
                for ch in text.chars() {
                    let s = ch.encode_utf8(&mut buf);
                    let target = crate::wayland::establish_exact_target(pid, xid)?;
                    crate::wayland::type_text(target, s)?;
                    if delay_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    }
                }
                return Ok(());
            }
            crate::input::send_type_text_with_delay(xid, &text, delay_ms)
        })
        .await;
        match result {
            Ok(Ok(())) => ToolResult::text(format!(
                "Typed {text_len} character(s) with {delay_ms}ms delay."
            )),
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── kill_app ──────────────────────────────────────────────────────────────────

pub struct KillAppTool;
static KILL_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for KillAppTool {
    fn def(&self) -> &ToolDef {
        KILL_DEF.get_or_init(|| ToolDef {
            name: "kill_app".into(),
            description: "Force-terminate a process by pid (kill -9 equivalent on Linux). \
                Use as escalation when the cooperative close path failed to make the process \
                exit. Unsaved state is lost — prefer the cooperative path first."
                .into(),
            input_schema: json!({"type":"object","required":["pid"],"properties":{
                "pid":{"type":"integer","description":"PID of the process to terminate."}
            },"additionalProperties":false}),
            read_only: false,
            destructive: true,
            idempotent: true,
            open_world: false,
        })
    }

    async fn protected_resource_scope(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> Result<Option<Value>, String> {
        if adapter_id != "process_control" {
            return Ok(None);
        }
        use cua_driver_core::browser::platform::BrowserPlatform;
        let pid = args
            .get("pid")
            .and_then(Value::as_i64)
            .filter(|pid| *pid > 0)
            .ok_or_else(|| "kill_app requires a positive integer pid".to_owned())?;
        let fingerprint = crate::browser_platform::LinuxBrowserPlatform::default()
            .process_fingerprint(pid)
            .await
            .map_err(|error| error.message)?;
        Ok(Some(json!({
            "kind": "process_instance",
            "fingerprint": fingerprint,
        })))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if let Some(expected) = args.get("_protected_process_fingerprint") {
            let current = match self
                .protected_resource_scope("process_control", &args)
                .await
            {
                Ok(Some(scope)) => scope["fingerprint"].clone(),
                Ok(None) => Value::Null,
                Err(message) => return kill_app_stale_process_refusal(message),
            };
            if current != *expected {
                return kill_app_stale_process_refusal(
                    "the process identity changed at the termination boundary".to_owned(),
                );
            }
        }
        let pid_i = match args.get("pid").and_then(|v| v.as_i64()) {
            Some(p) if p > 0 && p <= i32::MAX as i64 => p as i32,
            Some(_) => {
                return ToolResult::error("kill_app: `pid` must be a positive integer".to_string());
            }
            None => {
                return ToolResult::error(
                    "kill_app: missing required integer field `pid`".to_string(),
                );
            }
        };
        // Read the instance before signaling so the confirmation below can
        // tell its exit from a later process that reused the pid.
        let before = match crate::proc_fs::read_process_stat(pid_i as u32) {
            Ok(Some(before)) => before,
            Ok(None) => {
                return kill_app_failure(
                    pid_i,
                    "process_not_found",
                    format!("kill_app: pid {pid_i} does not exist; no signal was sent."),
                )
            }
            Err(error) => {
                return kill_app_failure(
                    pid_i,
                    "process_identity_unavailable",
                    format!(
                        "kill_app: cannot read /proc/{pid_i}/stat to verify termination: \
                         {error}; no signal was sent."
                    ),
                )
            }
        };
        // SAFETY: libc::kill is a thin syscall wrapper, no thread-safety concerns.
        let rc = unsafe { libc::kill(pid_i, libc::SIGKILL) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return kill_app_failure(
                pid_i,
                "signal_failed",
                format!(
                    "kill_app: kill(pid={pid_i}, SIGKILL) failed: {err}. \
                     The process may not exist, or the daemon lacks permission to signal it."
                ),
            );
        }
        // An accepted signal is not an exit: a sandbox (gVisor) can accept
        // SIGKILL for a process that keeps running. Report success only once
        // the same instance is gone.
        let deadline = tokio::time::Instant::now() + KILL_CONFIRM_TIMEOUT;
        loop {
            let now = crate::proc_fs::read_process_stat(pid_i as u32);
            if let Some(observation) = now
                .as_ref()
                .ok()
                .and_then(|now| crate::proc_fs::process_exit_observation(before, *now))
            {
                return ToolResult::text(format!(
                    "✅ Terminated pid {pid_i} with SIGKILL (confirmed: {observation})."
                ))
                .with_structured(json!({
                    "status": "terminated",
                    "effect": "confirmed",
                    "pid": pid_i,
                    "terminated": true,
                    "observation": observation,
                }));
            }
            if tokio::time::Instant::now() >= deadline {
                let detail = match now {
                    Err(error) => format!("/proc/{pid_i}/stat became unreadable: {error}"),
                    Ok(_) => format!(
                        "the same process was still alive {} ms later",
                        KILL_CONFIRM_TIMEOUT.as_millis()
                    ),
                };
                let mut result = kill_app_failure(
                    pid_i,
                    "termination_unconfirmed",
                    format!("kill_app: SIGKILL was accepted for pid {pid_i}, but {detail}."),
                );
                if let Some(structured) = result.structured_content.as_mut() {
                    structured["effect"] = json!("suspected_noop");
                }
                return result;
            }
            tokio::time::sleep(KILL_CONFIRM_INTERVAL).await;
        }
    }
}

/// How long `kill_app` waits for the signaled process to exit, and how often
/// it re-reads `/proc/<pid>/stat` meanwhile.
const KILL_CONFIRM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const KILL_CONFIRM_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

fn kill_app_failure(pid: i32, code: &str, message: String) -> ToolResult {
    ToolResult::error(message.clone()).with_structured(json!({
        "status": "failed",
        "pid": pid,
        "terminated": false,
        "code": code,
        "message": message,
    }))
}

fn kill_app_stale_process_refusal(message: String) -> ToolResult {
    ToolResult::error(message.clone()).with_structured(json!({
        "status": "refused",
        "refusal": {
            "code": "protected_resource_scope_stale",
            "message": message,
        }
    }))
}

// ── invoke_menu (Linux) ──────────────────────────────────────────────────────

pub struct InvokeMenuTool;

static INVOKE_MENU_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn normalized_menu_path(path: Vec<String>) -> Result<Vec<String>, String> {
    if path.is_empty() || path.len() > 16 {
        return Err("invoke_menu: path must contain between 1 and 16 segments".into());
    }
    path.into_iter()
        .enumerate()
        .map(|(index, segment)| {
            let segment = segment.trim();
            if segment.is_empty() {
                Err(format!("invoke_menu: path segment {index} is empty"))
            } else {
                Ok(segment.to_owned())
            }
        })
        .collect()
}

fn menu_refusal(message: String) -> ToolResult {
    ToolResult::error(message.clone()).with_structured(json!({
        "status": "refused",
        "refusal": { "code": "menu_path_unavailable", "message": message }
    }))
}

#[async_trait]
impl Tool for InvokeMenuTool {
    fn def(&self) -> &ToolDef {
        INVOKE_MENU_DEF.get_or_init(|| {
            let contract =
                cua_driver_contract::tool_contract("invoke_menu").expect("invoke_menu contract");
            ToolDef {
                name: contract.name,
                description: contract.description,
                input_schema: contract.input_schema,
                read_only: contract.annotations.read_only,
                destructive: contract.annotations.destructive,
                idempotent: contract.annotations.idempotent,
                open_world: contract.annotations.open_world,
            }
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::action_record::{
            ActionEffect, ActionEvidence, ActionExecutionRecord, ActionTransport, ActualDelivery,
            EvidenceKind, RequestedDelivery,
        };

        // Menu paths are resolved and fired through AT-SPI, which needs no
        // window activation on X11: the default is background (focus-free,
        // under the focus guard). `delivery_mode:"foreground"` is the explicit
        // escalation that activates the window first, as every other tool.
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        let mut args = args;
        if let Some(object) = args.as_object_mut() {
            // Not part of the closed InvokeMenuInput contract; consumed above.
            object.remove("delivery_mode");
        }
        let input: InvokeMenuInput = match parse_typed_input("invoke_menu", args) {
            Ok(input) => input,
            Err(result) => return result,
        };
        let path = match normalized_menu_path(input.path) {
            Ok(path) => path,
            Err(error) => return menu_refusal(error),
        };
        let pid = input.pid;
        let window_id = input.window_id;
        if !crate::wayland::list_windows_dispatch(Some(pid))
            .iter()
            .any(|window| window.xid == window_id && window.pid == Some(pid))
        {
            return menu_refusal("invoke_menu: window_id does not belong to pid".into());
        }

        let outcome = if crate::wayland::is_wayland() {
            cua_driver_core::blocking::spawn(move || {
                let target = crate::wayland::establish_exact_target(pid, window_id)?;
                let guard = crate::wayland::activate_window_for_input_target(&target)?;
                crate::wayland::validate_exact_target(&target)?;
                let result = crate::atspi::native::invoke_menu_path(&target, &path);
                drop(guard);
                result
            })
            .await
        } else {
            cua_driver_core::blocking::spawn(move || {
                let target = crate::wayland::establish_exact_target(pid, window_id)?;
                let prior = crate::input::x11_activate_window_persistent(window_id)?;
                let result = crate::atspi::native::invoke_menu_path(&target, &path);
                if let Some(prior_window) = prior {
                    let _ = crate::input::x11_activate_window_persistent(prior_window);
                }
                result
            })
            .await
        };

        match outcome {
            Ok(Ok(())) => ToolResult::text(
                "Resolved the live native menu path and dispatched its final accessibility action (delivery_mode=foreground); verify the command's semantic effect from fresh state.",
            )
            .with_structured(json!({
                "path": "ax",
                "verified": false,
                "effect": "unverifiable",
                "delivery_mode": "foreground",
            }))
            .with_action_record(
                ActionExecutionRecord::builder(
                    ActionEffect::Unverifiable,
                    ActionTransport::LinuxAtSpiAction,
                    RequestedDelivery::Foreground,
                )
                .actual_delivery(ActualDelivery::Foreground)
                .evidence(ActionEvidence {
                    kind: EvidenceKind::NativeApiResult,
                    detail: "Every menu hop resolved uniquely and AT-SPI accepted the final action"
                        .into(),
                })
                .build()
                .expect("invoke_menu record is valid"),
            ),
            Ok(Err(error)) => menu_refusal(format!("invoke_menu: {error}")),
            Err(error) => menu_refusal(format!("invoke_menu: blocking task failed: {error}")),
        }
    }
}

// ── set_window_frame (Linux) ─────────────────────────────────────────────────

pub struct SetWindowFrameTool;

static SET_WINDOW_FRAME_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for SetWindowFrameTool {
    fn def(&self) -> &ToolDef {
        SET_WINDOW_FRAME_DEF.get_or_init(|| {
            let contract = cua_driver_contract::tool_contract("set_window_frame")
                .expect("set_window_frame contract");
            ToolDef {
                name: contract.name,
                description: contract.description,
                input_schema: contract.input_schema,
                read_only: contract.annotations.read_only,
                destructive: contract.annotations.destructive,
                idempotent: contract.annotations.idempotent,
                open_world: contract.annotations.open_world,
            }
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_contract::SetWindowFrameInput;
        use cua_driver_core::action_record::{
            effect_from_value_readback, ActionEvidence, ActionExecutionRecord, ActionTransport,
            ActualDelivery, EvidenceKind, RequestedDelivery,
        };

        let input: SetWindowFrameInput =
            match cua_driver_core::tool_args::parse_typed_input("set_window_frame", args) {
                Ok(input) => input,
                Err(result) => return result,
            };
        if crate::wayland::is_wayland() {
            return ToolResult::error(
                "set_window_frame: this Wayland compositor exposes no protocol that permits a client to set another top-level window's geometry",
            );
        }
        let values = [input.x, input.y, input.width, input.height];
        if values.iter().any(|value| !value.is_finite())
            || input.width <= 0.0
            || input.height <= 0.0
        {
            return ToolResult::error(
                "set_window_frame: x/y must be finite and width/height must be finite positive numbers",
            );
        }
        let to_i32 = |name: &str, value: f64| -> Result<i32, String> {
            let rounded = value.round();
            if rounded < i32::MIN as f64 || rounded > i32::MAX as f64 {
                Err(format!("{name} is out of range for an X11 coordinate"))
            } else {
                Ok(rounded as i32)
            }
        };
        let to_u32 = |name: &str, value: f64| -> Result<u32, String> {
            let rounded = value.round();
            if rounded < 1.0 || rounded > u32::MAX as f64 {
                Err(format!("{name} is out of range for an X11 size"))
            } else {
                Ok(rounded as u32)
            }
        };
        let x = match to_i32("x", input.x) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(format!("set_window_frame: {error}")),
        };
        let y = match to_i32("y", input.y) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(format!("set_window_frame: {error}")),
        };
        let width = match to_u32("width", input.width) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(format!("set_window_frame: {error}")),
        };
        let height = match to_u32("height", input.height) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(format!("set_window_frame: {error}")),
        };
        let window_id = input.window_id;
        let pid = input.pid;
        let outcome = cua_driver_core::blocking::spawn(move || {
            let before = crate::x11::list_windows(Some(pid))
                .into_iter()
                .find(|window| window.xid == window_id)
                .map(|window| (window.x, window.y, window.width, window.height));
            let (observed, confirmed, mutation_error) =
                crate::x11::set_window_frame(window_id, pid, x, y, width, height)
                    .map_err(|error| error.to_string())?;
            let observed_frame =
                observed.map(|window| (window.x, window.y, window.width, window.height));
            let changed = observed_frame.is_some_and(|observed| before != Some(observed));
            Ok::<_, String>((observed_frame, confirmed, changed, mutation_error))
        })
        .await;
        let (observed, confirmed, changed, mutation_error) = match outcome {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => return ToolResult::error(format!("set_window_frame: {error}")),
            Err(error) => {
                return ToolResult::error(format!(
                    "set_window_frame: blocking task failed: {error}"
                ));
            }
        };
        let effect = effect_from_value_readback(confirmed, changed, observed.is_some());
        let requested = (x, y, width, height);
        let mut record = ActionExecutionRecord::builder(
            effect,
            ActionTransport::LinuxX11ConfigureWindow,
            RequestedDelivery::NotApplicable,
        )
        .actual_delivery(ActualDelivery::NotApplicable)
        .detail(format!(
            "requested={requested:?} observed={observed:?} mutation_error={mutation_error:?}"
        ));
        if observed.is_some() {
            record = record.evidence(ActionEvidence {
                kind: EvidenceKind::ValueReadback,
                detail: if confirmed {
                    "X11 geometry readback matched the requested frame".into()
                } else {
                    "X11 geometry readback did not match the requested frame".into()
                },
            });
        }
        ToolResult::text(if confirmed {
            "Set and verified the requested window frame."
        } else if observed.is_none() {
            "The native window mutation was attempted, but its resulting frame could not be read back."
        } else {
            "The window frame did not settle at the requested geometry."
        })
        .with_action_record(record.build().expect("set_window_frame record is valid"))
    }
}

// ── bring_to_front (Linux) ───────────────────────────────────────────────────

pub struct BringToFrontTool;

static BTF_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for BringToFrontTool {
    fn def(&self) -> &ToolDef {
        BTF_DEF.get_or_init(|| ToolDef {
            name: "bring_to_front".into(),
            description:
                "Persistently activate a window so subsequent input lands on it. \
                 X11: EWMH _NET_ACTIVE_WINDOW activation (the `wmctrl -a` equivalent, \
                 proper timestamp handling to beat focus-stealing prevention) — call \
                 it before `delivery_mode:\"foreground\"` input to avoid a per-call \
                 flash, or to escalate when background injection didn't land. \
                 Wayland: activates through a target-addressable compositor adapter \
                 (wlroots foreign-toplevel or the GNOME Shell helper) and refuses \
                 when the compositor offers no safe adapter. Matches the macOS / \
                 Windows bring_to_front rung."
                .into(),
            input_schema: serde_json::json!({
                "type":"object","required":["pid"],"properties":{
                    "pid":{"type":"integer","description":"Process ID of the app to activate."},
                    "window_id":{"type":"integer","description":"Exact window id to activate. Required when a Wayland process owns more than one toplevel."}
                },"additionalProperties":false
            }),
            read_only: false, destructive: false, idempotent: true, open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid = args.u64_or("pid", 0) as u32;
        if crate::wayland::is_wayland() {
            let window_id = match args.opt_u64("window_id") {
                Some(window_id) => window_id,
                None => match crate::wayland::list_windows_dispatch(Some(pid)).as_slice() {
                    [window] => window.xid,
                    [] => {
                        return ToolResult::error(format!(
                            "bring_to_front: no window_id given and no Wayland windows found for pid {pid}."
                        ));
                    }
                    windows => {
                        return ToolResult::error(format!(
                            "target_ambiguous: pid {pid} owns {} Wayland windows; provide an exact window_id.",
                            windows.len()
                        ));
                    }
                },
            };
            let result = cua_driver_core::blocking::spawn(move || {
                let target = crate::wayland::establish_exact_target(pid, window_id)?;
                crate::wayland::activate_window_for_input_target(&target)
                    .and_then(crate::wayland::ForegroundInputGuard::keep_focus)
            })
            .await;
            return match result {
                Ok(Ok(())) => {
                    ToolResult::text(format!("Brought Wayland window {window_id} to front."))
                        .with_structured(serde_json::json!({
                            "window_id": window_id,
                            "platform": "linux",
                            "session": "wayland",
                        }))
                }
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }

        // X11: resolve the target xid (window_id, else first window for pid).
        let xid = match args.opt_u64("window_id") {
            Some(x) => x,
            None => {
                let windows =
                    cua_driver_core::blocking::spawn(move || crate::x11::list_windows(Some(pid)))
                        .await
                        .unwrap_or_default();
                match windows.first() {
                    Some(w) => w.xid,
                    None => {
                        return ToolResult::error(format!(
                            "bring_to_front: no window_id given and no windows found for pid {pid}."
                        ));
                    }
                }
            }
        };
        let r = cua_driver_core::blocking::spawn(move || {
            crate::input::x11_activate_window_persistent(xid)
        })
        .await;
        match r {
            Ok(Ok(prior)) => ToolResult::text(format!(
                "✅ Brought window {xid} to front (X11 _NET_ACTIVE_WINDOW)."
            ))
            .with_structured(serde_json::json!({
                "window_id": xid,
                "prior_active": prior,
                "platform": "linux",
            })),
            Ok(Err(e)) => ToolResult::error(format!("bring_to_front failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── registry ─────────────────────────────────────────────────────────────────

pub fn build_registry_with_provider(
    compat: bool,
    provider: Option<std::sync::Arc<dyn cua_driver_core::consent::ProtectedConsentProvider>>,
) -> ToolRegistry {
    let mut r = ToolRegistry::new_with_protected_consent_provider(provider);
    let state = ToolState::new_with_capture_service(r.capture_service());
    if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
        let recording_state = Arc::downgrade(&state);
        r.recording
            .set_pixel_point_fn(move |args, window_id, pid, mut x, mut y| {
                let state = recording_state.upgrade()?;
                if let Some(pid) = pid {
                    let pid = u32::try_from(pid).ok()?;
                    // Match ClickTool's screenshot and zoom conversion before
                    // translating to the recording's full-output image.
                    if args.bool_or("from_zoom", false) {
                        (x, y) = state
                            .zoom_context(args, pid, args.opt_u64("window_id"))
                            .ok()?
                            .zoom_to_window(x, y);
                    } else if let Ok(ratio) =
                        screenshot_scale(&state, args, pid, args.opt_u64("window_id"))
                    {
                        x *= ratio;
                        y *= ratio;
                    }
                }
                crate::recording_hooks::hyprland_pixel_recording_point(window_id, pid, x, y)
            });
    }
    let cursor_outcome_reader = {
        let cursor_registry = state.cursor_registry.clone();
        cua_driver_core::session::register_scoped_cursor_outcome_reader(std::sync::Arc::new(
            move |session_id| {
                let state = cursor_registry.get(session_id);
                let motion_customized = state.is_some()
                    && crate::overlay::current_motion_for(session_id)
                        != cursor_overlay::MotionConfig::default();
                let active_cursor_count = cursor_registry
                    .all_states()
                    .iter()
                    .filter(|state| state.config.cursor_id != "default")
                    .count()
                    .max(1);
                match state {
                    Some(state) => cua_driver_core::session::bounded_cursor_outcome(
                        true,
                        state.config.enabled,
                        crate::overlay::is_visible_for_session(session_id),
                        Some(state.config.theme_id.as_str()),
                        motion_customized,
                        active_cursor_count,
                    ),
                    None => cua_driver_core::session::bounded_cursor_outcome(
                        false,
                        false,
                        false,
                        None,
                        false,
                        active_cursor_count,
                    ),
                }
            },
        ))
    };
    let session_end_hook = {
        let cursor_registry = state.cursor_registry.clone();
        let state_for_session_end = state.clone();
        cua_driver_core::session::register_scoped_fallible_session_end_hook(
            "linux_input",
            move |session_id| {
                // Retirement is terminal even if release-only native cleanup
                // fails. It must precede any fallible teardown operation.
                state_for_session_end
                    .capture_service
                    .retire_session_id(session_id);
                state_for_session_end
                    .snapshots
                    .retire_session_screenshots(session_id);
                cursor_registry.remove(session_id);
                crate::overlay::remove_cursor(session_id.to_owned());
                if crate::wayland::is_wayland() {
                    release_mouse_hold_for_session(
                        &state_for_session_end,
                        session_id,
                        |cursor_id, held| {
                            crate::wayland::persistent_vptr::release(cursor_id, held.button)
                        },
                    )?;
                } else {
                    clear_mouse_hold_after_release(&state_for_session_end, session_id);
                }

                crate::input::forget_master_pointer(session_id);
                crate::wayland::hyprland_input::cleanup_session(session_id);
                Ok(())
            },
        )
    };
    let session_revive_hook =
        cua_driver_core::session::register_scoped_session_revive_hook(move |session_id| {
            crate::overlay::revive_cursor(session_id.to_owned());
        });
    r.retain_cursor_outcome_reader(cursor_outcome_reader);
    r.retain_session_end_hook(session_end_hook);
    r.retain_session_revive_hook(session_revive_hook);
    if let Some(runtime_scope) = cua_driver_core::tool::current_dispatch_runtime_scope() {
        let prefix = format!("__cua_runtime_{runtime_scope}:");
        let cursor_registry = state.cursor_registry.clone();
        let capture_service = state.capture_service.clone();
        r.retain_runtime_cleanup(move || {
            crate::capture_action_frame::retire_runtime(&capture_service);
            crate::wayland::hyprland_input::cleanup_runtime(&prefix);
            for cursor in cursor_registry
                .all_states()
                .into_iter()
                .filter(|cursor| cursor.config.cursor_id.starts_with(&prefix))
            {
                cursor_registry.remove(&cursor.config.cursor_id);
                crate::overlay::remove_cursor(cursor.config.cursor_id);
            }
        });
    }
    r.register(Box::new(ListAppsTool));
    r.register(Box::new(ListWindowsTool));
    r.register(Box::new(GetWindowStateTool {
        state: state.clone(),
    }));
    r.register(Box::new(
        cua_driver_core::expectation::VerifyStateTool::new(std::sync::Arc::new(
            cua_driver_core::expectation::ToolObservationProvider::new(
                std::sync::Arc::new(ListWindowsTool),
                std::sync::Arc::new(GetWindowStateTool {
                    state: state.clone(),
                }),
            ),
        )),
    ));
    r.register(Box::new(LaunchAppTool));
    r.register(Box::new(KillAppTool));
    let pid_window_candidates: PidWindowGuardParts = (
        Arc::new(pid_window_target_candidates),
        desktop_point_window_resolver(),
        pid_fallback_window_resolver(),
        snapshot_window_resolver(state.clone()),
    );
    r.register(pid_window_guarded(BringToFrontTool, &pid_window_candidates));
    r.register(Box::new(SetWindowFrameTool));
    r.register(Box::new(InvokeMenuTool));
    r.register(pid_window_guarded(
        ClickTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        DoubleClickTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        RightClickTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        DragTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        MouseButtonDownTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        MouseDragTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        MouseButtonUpTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(Box::new(ParallelMouseDragTool {
        state: state.clone(),
    }));
    r.register(pid_window_guarded(
        TypeTextTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        PressKeyTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        HotkeyTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        SetValueTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        ScrollTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    cua_driver_core::clipboard::register_clipboard_tools(
        &mut r,
        Arc::new(crate::clipboard::LinuxClipboard::new()),
    );
    // `screenshot` removed - see the matching comment in
    // platform-windows/src/tools/impl_.rs::build_registry. Canonical
    // screenshot path is `get_window_state` (it always returns a screenshot now).
    let _ = compat;
    r.register(Box::new(GetScreenSizeTool));
    r.register(Box::new(GetDesktopStateTool {
        state: state.clone(),
    }));
    r.register(Box::new(GetCursorPositionTool));
    r.register(Box::new(MoveCursorTool {
        state: state.clone(),
    }));
    r.register(Box::new(SetAgentCursorEnabledV2Tool {
        state: state.clone(),
    }));
    r.register(Box::new(SetAgentCursorMotionV2Tool));
    r.register(Box::new(GetAgentCursorStateV2Tool {
        state: state.clone(),
    }));
    r.register(Box::new(SetAgentCursorThemeTool {
        state: state.clone(),
    }));
    r.register(Box::new(CheckPermissionsTool));
    // `health_report` — single-call cross-platform driver diagnostics.
    // Stable schema_version="1" contract for downstream consumers. Linux skips
    // tcc_* and bundle_identity with "not applicable on Linux".
    r.register(Box::new(
        cua_driver_core::health_report::HealthReportTool::new(std::sync::Arc::new(
            crate::health_report::LinuxHealthProvider,
        )),
    ));
    r.register(Box::new(GetConfigTool {
        state: state.clone(),
    }));
    r.register(Box::new(SetConfigTool {
        state: state.clone(),
    }));
    r.register(Box::new(GetAccessibilityTreeTool));
    r.register(Box::new(ZoomTool {
        state: state.clone(),
    }));
    // `type_text_chars` is a deprecated invoke-time alias for `type_text`.
    // Keep it out of tools/list, matching the macOS and Windows registries.
    let _: &TypeTextCharsTool = &TypeTextCharsTool;
    // Cross-platform `page` tool definition lives in mcp-server; Linux plugs
    // in its AT-SPI + CDP backend here.
    r.register(Box::new(cua_driver_core::page::PageTool::new(Arc::new(
        super::page::LinuxPageBackend::new(),
    ))));
    let browser_engine = cua_driver_core::browser::BrowserEngine::new_with_runtime_services(
        Arc::new(crate::browser_platform::LinuxBrowserPlatform::new(
            state.cursor_registry.clone(),
        )),
        r.approval_broker(),
        r.protected_resource_ownership(),
    );
    cua_driver_core::browser::register_browser_tools(&browser_engine, &mut r);
    r.register_recording_tools();
    r.register_session_tools();
    r
}

#[cfg(test)]
mod click_button_schema_tests {
    use super::{
        bounded_click_count_arg, chromium_background_must_refuse, element_ax_failure_may_fallback,
        exact_point_action_may_fallback, maps_indicate_gtk, ClickTool, ProductionRouteBackend,
        SetValueTool, ToolState,
    };
    use cua_driver_core::tool::Tool;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Surface 5: schema must advertise the three canonical button values and
    /// describe the back-compat default. Linux already routed button=middle/right
    /// pre-Surface-5; this freezes the schema shape so the contract can't drift.
    #[test]
    fn schema_advertises_button_enum_and_description() {
        let tool = ClickTool {
            state: super::ToolState::new(),
        };
        let d = tool.def();
        let props = d.input_schema.get("properties").expect("properties");
        let button = props.get("button").expect("button field present");
        assert_eq!(button.get("type").and_then(|v| v.as_str()), Some("string"));
        let enum_vals: Vec<&str> = button
            .get("enum")
            .and_then(|v| v.as_array())
            .expect("button.enum present")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        for need in ["left", "right", "middle"] {
            assert!(enum_vals.contains(&need), "missing {need} in button.enum");
        }
        let desc = button
            .get("description")
            .and_then(|v| v.as_str())
            .expect("button.description present");
        let lc = desc.to_ascii_lowercase();
        assert!(lc.contains("left"), "description should mention default");
        assert!(
            lc.contains("wayland"),
            "description should call out wayland fallback"
        );
        let count = props.get("count").expect("count field present");
        assert_eq!(count.get("minimum").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(count.get("maximum").and_then(|v| v.as_u64()), Some(3));
    }

    fn fixed_proof(pid: u32, xid: u64, target: &str) -> crate::wayland::ExactTargetProof {
        let epoch = target.split(':').next().expect("test target epoch");
        crate::wayland::ExactTargetProof::for_production_route_test(xid, pid, epoch, target)
    }

    fn backend_with(
        point_action: impl Fn(&crate::wayland::ExactTargetProof, i32, i32) -> anyhow::Result<Option<String>>
            + Send
            + Sync
            + 'static,
        click: impl Fn(
                crate::wayland::ExactTargetProof,
                i32,
                i32,
                u32,
                u8,
            )
                -> anyhow::Result<Option<crate::wayland::shell_helper::ForegroundTerminalOutcome>>
            + Send
            + Sync
            + 'static,
        set_value: impl Fn(&crate::wayland::ExactTargetProof, u64, &str) -> anyhow::Result<()>
            + Send
            + Sync
            + 'static,
    ) -> ProductionRouteBackend {
        ProductionRouteBackend {
            establish: Arc::new(|pid, xid| Ok(fixed_proof(pid, xid, "epoch-a:target-a"))),
            point_action: Arc::new(point_action),
            click: Arc::new(click),
            set_value: Arc::new(set_value),
        }
    }

    fn coordinate_click(state: Arc<ToolState>) -> ClickTool {
        ClickTool { state }
    }

    /// Pixel clicks need a screenshot this session captured. Publish one at
    /// scale 1.0 for this live test process (the registry refuses unknown
    /// pids) on a fresh window, and return `(pid, window_id, session)`.
    fn with_screenshot_context(state: &ToolState) -> (u32, u64, &'static str) {
        const SESSION: &str = "coordinate-click-test";
        let pid = std::process::id();
        let xid = NEXT_VALUE_XID.fetch_add(1, Ordering::Relaxed);
        state.snapshots.publish_for_session(
            pid as i32,
            xid,
            crate::atspi::snapshot::AtspiSnapshot::from_nodes(&[]),
            Some(SESSION),
            Some(1.0),
        );
        (pid, xid, SESSION)
    }

    #[tokio::test]
    async fn element_click_resolves_its_target_without_reentering_click() {
        // A 0.30.3 merge left double_click's delegation (button left, count 2,
        // invoke ClickTool again) at the top of ClickTool::invoke, keyed on
        // element_token/element_index. Every element click re-entered itself
        // until the stack overflowed and took the whole daemon down.
        let clicks = Arc::new(AtomicUsize::new(0));
        let clicks_for_backend = clicks.clone();
        let state = ToolState::new_with_production_route_backend(backend_with(
            |_proof, _, _| Ok(None),
            move |_proof, _, _, _, _| {
                clicks_for_backend.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            },
            |_, _, _| Ok(()),
        ));
        // (The registry refuses a bare element_index before any tool runs, so a
        // token is the element route that reaches invoke.)
        let (pid, xid, session) = with_screenshot_context(&state);
        for args in [
            serde_json::json!({"pid": pid, "element_token": "not-a-live-token", "_session_id": session}),
            serde_json::json!({"pid": pid, "window_id": xid, "element_token": "not-a-live-token",
                "_session_id": session}),
        ] {
            let result = coordinate_click(state.clone()).invoke(args).await;
            assert_eq!(result.is_error, Some(true), "{result:?}");
            assert_eq!(
                result.structured_content.as_ref().unwrap()["refusal"]["code"],
                "invalid_element_token",
                "{result:?}"
            );
        }
        assert_eq!(clicks.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn click_invoke_allows_only_a_point_miss_to_reach_coordinate_delivery() {
        let clicks = Arc::new(AtomicUsize::new(0));
        let clicks_for_backend = clicks.clone();
        let state = ToolState::new_with_production_route_backend(backend_with(
            |_proof, _, _| Ok(None),
            move |_proof, _, _, _, _| {
                clicks_for_backend.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            },
            |_, _, _| Ok(()),
        ));
        let (pid, xid, session) = with_screenshot_context(&state);
        let result = coordinate_click(state)
            .invoke(serde_json::json!({"pid": pid, "window_id": xid, "x": 12, "y": 14, "_session_id": session}))
            .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        assert_eq!(clicks.load(Ordering::SeqCst), 1);

        for error in [
            "point hit-test failed",
            "perform_action_at_screen_point timed out; delivery is indeterminate",
            "screen-point recipient changed at final mutation boundary",
        ] {
            let points = Arc::new(AtomicUsize::new(0));
            let points_for_backend = points.clone();
            let clicks = Arc::new(AtomicUsize::new(0));
            let clicks_for_backend = clicks.clone();
            let state = ToolState::new_with_production_route_backend(backend_with(
                move |_, _, _| {
                    points_for_backend.fetch_add(1, Ordering::SeqCst);
                    Err(anyhow::anyhow!(error))
                },
                move |_, _, _, _, _| {
                    clicks_for_backend.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                },
                |_, _, _| Ok(()),
            ));
            let (pid, xid, session) = with_screenshot_context(&state);
            let result = coordinate_click(state)
                .invoke(serde_json::json!({"pid": pid, "window_id": xid, "x": 1, "y": 2, "_session_id": session}))
                .await;
            assert_eq!(result.is_error, Some(true), "{error}");
            // The refusal must come from the point action, not an earlier gate.
            assert_eq!(points.load(Ordering::SeqCst), 1, "{error}: {result:?}");
            assert_eq!(clicks.load(Ordering::SeqCst), 0, "{error} replayed");
        }
    }

    #[tokio::test]
    async fn click_invoke_with_frame_delivers_background_point_action_without_pointer() {
        // With a session screenshot, a native Wayland background left click
        // reaches the exact-point AT-SPI action and sends no pointer input.
        let points = Arc::new(AtomicUsize::new(0));
        let points_for_backend = points.clone();
        let clicks = Arc::new(AtomicUsize::new(0));
        let clicks_for_backend = clicks.clone();
        let state = ToolState::new_with_production_route_backend(backend_with(
            move |_, _, _| {
                points_for_backend.fetch_add(1, Ordering::SeqCst);
                Ok(Some("click".into()))
            },
            move |_, _, _, _, _| {
                clicks_for_backend.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            },
            |_, _, _| Ok(()),
        ));
        let (pid, xid, session) = with_screenshot_context(&state);
        let result = coordinate_click(state)
            .invoke(
                serde_json::json!({"pid": pid, "window_id": xid, "x": 5, "y": 6,
                "delivery_mode": "background", "_session_id": session}),
            )
            .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["path"],
            "wayland_atspi",
            "{result:?}"
        );
        assert_eq!(points.load(Ordering::SeqCst), 1);
        assert_eq!(clicks.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn click_invoke_never_fires_the_action_of_a_node_that_takes_a_real_press() {
        // The point lands on a text field: the point route refuses before
        // firing (its `activate` is Enter), and the click goes to the real
        // pointer route this backend offers, exactly once.
        let points = Arc::new(AtomicUsize::new(0));
        let points_for_backend = points.clone();
        let clicks = Arc::new(AtomicUsize::new(0));
        let clicks_for_backend = clicks.clone();
        let state = ToolState::new_with_production_route_backend(backend_with(
            move |_, _, _| {
                points_for_backend.fetch_add(1, Ordering::SeqCst);
                Err(crate::atspi::ElementClickNeedsForeground.into())
            },
            move |_, _, _, _, _| {
                clicks_for_backend.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            },
            |_, _, _| Ok(()),
        ));
        let (pid, xid, session) = with_screenshot_context(&state);
        let result = coordinate_click(state)
            .invoke(
                serde_json::json!({"pid": pid, "window_id": xid, "x": 5, "y": 6,
                "delivery_mode": "background", "_session_id": session}),
            )
            .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["path"],
            "wayland_cua_compositor",
            "{result:?}"
        );
        assert_eq!(points.load(Ordering::SeqCst), 1);
        assert_eq!(clicks.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn click_invoke_carries_original_epoch_identity_across_point_miss() {
        let current = Arc::new(Mutex::new("epoch-a:target-a".to_owned()));
        let establish_calls = Arc::new(AtomicUsize::new(0));
        let current_at_establish = current.clone();
        let calls_at_establish = establish_calls.clone();
        let current_at_point = current.clone();
        let current_at_click = current.clone();
        let state = ToolState::new_with_production_route_backend(ProductionRouteBackend {
            establish: Arc::new(move |pid, xid| {
                calls_at_establish.fetch_add(1, Ordering::SeqCst);
                Ok(fixed_proof(pid, xid, &current_at_establish.lock().unwrap()))
            }),
            point_action: Arc::new(move |_, _, _| {
                *current_at_point.lock().unwrap() = "epoch-b:target-b".to_owned();
                Ok(None)
            }),
            click: Arc::new(move |proof, _, _, _, _| {
                let (_, target) = proof.production_route_test_identity();
                if target != Some(current_at_click.lock().unwrap().as_str()) {
                    anyhow::bail!("stale_target: immutable epoch/target identity was replaced");
                }
                Ok(None)
            }),
            set_value: Arc::new(|_, _, _| Ok(())),
        });
        let (pid, xid, session) = with_screenshot_context(&state);
        let result = coordinate_click(state)
            .invoke(serde_json::json!({"pid": pid, "window_id": xid, "x": 3, "y": 4, "_session_id": session}))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(establish_calls.load(Ordering::SeqCst), 1);
    }

    fn value_node(key: u64) -> crate::atspi::AtspiNode {
        crate::atspi::AtspiNode {
            element_index: Some(0),
            role: "text".into(),
            name: None,
            value: None,
            checked: None,
            enabled: Some(true),
            selected: None,
            description: None,
            actions: vec![],
            element_key: key,
            identity: None,
            depth: 0,
            parent_element_index: None,
            in_web_content: false,
            object_ref: None,
        }
    }

    static NEXT_VALUE_XID: AtomicU64 = AtomicU64::new(0x7f20_0000);

    /// Publish a one-node snapshot and return the token set_value requires.
    fn publish_value_snapshot(state: &ToolState, pid: u32, xid: u64, key: u64) -> String {
        let candidate = state
            .snapshots
            .prepare(pid, xid, &[value_node(key)])
            .unwrap();
        let snapshot_id = state.snapshots.publish(candidate).unwrap();
        cua_driver_core::element_token::format_token(snapshot_id, 0)
    }

    #[tokio::test]
    async fn set_value_invoke_reports_final_ancestry_race_without_mutation_fallback() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_backend = calls.clone();
        let state = ToolState::new_with_production_route_backend(backend_with(
            |_, _, _| Ok(None),
            |_, _, _, _, _| Ok(None),
            move |_, _, _| {
                calls_for_backend.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("exact window ancestry changed before mutation")
            },
        ));
        let pid = std::process::id();
        let xid = NEXT_VALUE_XID.fetch_add(1, Ordering::Relaxed);
        let token = publish_value_snapshot(&state, pid, xid, 0x41);
        let result = SetValueTool { state }
            .invoke(serde_json::json!({"pid": pid, "window_id": xid, "element_token": token, "value": "new"}))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{result:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_set_value_invoke_keeps_generation_permit_in_native_task() {
        let (started_tx, mut started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = Arc::new(Mutex::new(release_rx));
        let state = ToolState::new_with_production_route_backend(backend_with(
            |_, _, _| Ok(None),
            |_, _, _, _, _| Ok(None),
            move |_, key, _| {
                assert_eq!(key, 0x41);
                started_tx.lock().unwrap().take().unwrap().send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
                Ok(())
            },
        ));
        let pid = std::process::id();
        let xid = NEXT_VALUE_XID.fetch_add(1, Ordering::Relaxed);
        let token = publish_value_snapshot(&state, pid, xid, 0x41);
        let tool = SetValueTool {
            state: state.clone(),
        };
        {
            let invocation = tool.invoke(serde_json::json!({"pid": pid, "window_id": xid, "element_token": token, "value": "new"}));
            tokio::pin!(invocation);
            tokio::select! {
                started = &mut started_rx => started.unwrap(),
                result = &mut invocation => panic!("invoke completed before native mutation was released: {result:?}"),
                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => panic!("invoke did not reach native mutation"),
            }
        }

        let next = state
            .snapshots
            .prepare(pid, xid, &[value_node(0x99)])
            .unwrap();
        let (published_tx, published_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            published_tx
                .send(
                    cua_driver_core::element_token::global()
                        .publish(next, std::time::Duration::from_secs(2)),
                )
                .unwrap();
        });
        assert!(published_rx
            .recv_timeout(std::time::Duration::from_millis(40))
            .is_err());
        release_tx.send(()).unwrap();
        published_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap();
    }

    #[test]
    fn exact_wayland_ax_failure_never_allows_pointer_fallback() {
        assert!(!element_ax_failure_may_fallback(true));
        assert!(element_ax_failure_may_fallback(false));
    }

    #[test]
    fn exact_wayland_point_dispatch_errors_never_replay_coordinates() {
        assert!(exact_point_action_may_fallback(Err(anyhow::anyhow!("doAction failed"))).is_err());
        assert!(exact_point_action_may_fallback(Err(anyhow::anyhow!(
            "point action timed out; delivery indeterminate"
        )))
        .is_err());
    }

    #[test]
    fn only_pre_dispatch_point_miss_allows_coordinate_fallback() {
        assert!(exact_point_action_may_fallback(Ok(None)).unwrap());
        assert!(!exact_point_action_may_fallback(Ok(Some("click".into()))).unwrap());
    }

    #[test]
    fn a_point_on_a_node_that_takes_a_real_press_falls_back_to_the_pointer_route() {
        // The at-point route refuses editable text, focus-taking controls and
        // table cells before firing anything (a GTK entry's `activate` is
        // Enter). That refusal sent no input, so the pointer route may run.
        assert!(exact_point_action_may_fallback(Err(
            crate::atspi::ElementClickNeedsForeground.into()
        ))
        .unwrap());
    }

    #[test]
    fn click_count_parser_is_strict_and_bounded() {
        assert_eq!(bounded_click_count_arg(&serde_json::json!({})).unwrap(), 1);
        for count in 1..=3 {
            assert_eq!(
                bounded_click_count_arg(&serde_json::json!({ "count": count })).unwrap(),
                count
            );
        }
        for bad in [
            serde_json::json!(0),
            serde_json::json!(4),
            serde_json::json!(-1),
            serde_json::json!("2"),
        ] {
            assert!(bounded_click_count_arg(&serde_json::json!({ "count": bad })).is_err());
        }
    }

    #[test]
    fn chromium_background_requires_focus_free_inject_mode() {
        assert!(chromium_background_must_refuse(false, false, true));
        assert!(!chromium_background_must_refuse(false, true, true));
        assert!(!chromium_background_must_refuse(true, false, true));
        assert!(!chromium_background_must_refuse(false, false, false));
    }

    #[test]
    fn host_writes_are_classified_before_platform_injection() {
        use cua_driver_core::action_lease::{classify_tool, ActionClass};

        assert_eq!(classify_tool("click"), ActionClass::DesktopRaw);
        assert_eq!(classify_tool("set_value"), ActionClass::WindowSemantic);
        assert_eq!(classify_tool("get_window_state"), ActionClass::Observation);
    }

    #[test]
    fn private_pointer_tools_cannot_bypass_exact_target_wrappers() {
        let source = include_str!("impl_.rs");
        for primitive in ["click", "move", "drag", "scroll"] {
            let forbidden = format!("crate::wayland::inject_{primitive}(");
            assert!(
                !source.contains(&forbidden),
                "private {primitive} path bypasses immutable exact-target proof"
            );
        }
    }

    #[test]
    fn gtk_process_maps_are_detected_without_matching_unrelated_libraries() {
        assert!(maps_indicate_gtk(
            "7f00-7f01 r-xp /usr/lib/x86_64-linux-gnu/libgtk-3.so.0.2404.32"
        ));
        assert!(maps_indicate_gtk(
            "7f00-7f01 r-xp /nix/store/hash-gtk4/lib/libgtk-4.so.1"
        ));
        assert!(!maps_indicate_gtk(
            "7f00-7f01 r-xp /usr/lib/x86_64-linux-gnu/libgdk_pixbuf-2.0.so"
        ));
    }
}

#[cfg(test)]
mod pid_window_target_tests {
    use super::*;
    use cua_driver_core::window_target::{resolve_pid_window_target, PidWindowTargetResolution};

    fn window(xid: u64, pid: u32) -> crate::x11::WindowInfo {
        crate::x11::WindowInfo {
            xid,
            pid: Some(pid),
            app_name: "editor".into(),
            title: format!("Document {xid}"),
            is_on_screen: true,
            z_index: Some(1),
            x: 0,
            y: 0,
            width: 640,
            height: 480,
            native_window_id: None,
            target_id: None,
            helper_epoch: None,
            transient_for_window_id: None,
            transient_for_target_id: None,
            is_attached_dialog: None,
            is_modal: None,
            window_type: None,
            workspace_index: None,
            workspace_active: None,
            sticky: None,
            monitor: None,
            capture_current: None,
            identity_capabilities: None,
        }
    }

    #[test]
    fn same_pid_sibling_windows_are_ambiguous() {
        let candidates =
            window_target_candidates_for_pid([window(7, 42), window(8, 42), window(9, 99)], 42);
        assert!(matches!(
            resolve_pid_window_target(candidates),
            PidWindowTargetResolution::Ambiguous(windows)
                if windows.iter().map(|window| window.window_id).collect::<Vec<_>>() == [7, 8]
        ));
    }
}

#[cfg(test)]
mod window_capture_dimension_tests;

#[cfg(test)]
mod desktop_capture_frame_tests;

#[cfg(test)]
mod background_budget_tests;

#[cfg(test)]
mod background_keyboard_route_tests;

#[cfg(test)]
mod visibility_tests;

// Pure upstream routing classifications; delivery remains constrained by the retained-object adapters.
fn maps_indicate_synthetic_pointer_dropped(maps: &str) -> bool {
    maps_indicate_gtk(maps)
        || maps.contains("libvcl")
        || maps.contains("libmergedlo")
        || maps.contains("libQt5Gui")
        || maps.contains("libQt6Gui")
}

fn element_needs_real_click(role: &str) -> bool {
    crate::atspi::is_focus_taking_role(role)
}

fn element_is_menu_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "menu" | "menu item" | "check menu item" | "radio menu item"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WmChord {
    /// Alt+F4: close the target window (`_NET_CLOSE_WINDOW`).
    CloseWindow,
    /// Super+*, Alt+Tab, Ctrl+Alt+*, Alt+F7/F8/F10/space/Escape: a WM binding
    /// with no window-addressed equivalent.
    Unavailable,
}

fn wm_chord_kind(key: &str, modifiers: &[String]) -> Option<WmChord> {
    let mods: Vec<String> = modifiers
        .iter()
        .map(|m| m.trim().to_ascii_lowercase())
        .collect();
    let has = |names: &[&str]| mods.iter().any(|m| names.contains(&m.as_str()));
    let alt = has(&["alt", "alt_l", "alt_r", "option"]);
    let ctrl = has(&["ctrl", "control", "ctrl_l", "ctrl_r"]);
    let shift = has(&["shift", "shift_l", "shift_r"]);
    let super_ = has(&[
        "super", "super_l", "super_r", "meta", "win", "cmd", "command", "hyper",
    ]);
    let key_lower = key.trim().to_ascii_lowercase();
    if super_ {
        return Some(WmChord::Unavailable);
    }
    if alt && ctrl {
        return Some(WmChord::Unavailable);
    }
    if alt && !ctrl && key_lower == "f4" {
        return Some(WmChord::CloseWindow);
    }
    if alt
        && !ctrl
        && !shift
        && matches!(
            key_lower.as_str(),
            "tab" | "f5" | "f7" | "f8" | "f10" | "space" | "escape" | "esc"
        )
    {
        return Some(WmChord::Unavailable);
    }
    if alt && shift && key_lower == "tab" {
        return Some(WmChord::Unavailable);
    }
    None
}

fn split_key_combo(key: &str) -> (Vec<String>, String) {
    if key.len() <= 1 || !key.contains('+') {
        return (Vec::new(), key.to_owned());
    }
    let mut parts: Vec<&str> = key.split('+').collect();
    let last = parts.pop().unwrap_or("");
    let last = if last.is_empty() { "+" } else { last };
    let modifiers: Vec<String> = parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    if !modifiers.is_empty() && modifiers.iter().all(|m| is_modifier(m)) {
        (modifiers, last.to_owned())
    } else {
        (Vec::new(), key.to_owned())
    }
}

fn clear_mouse_hold_after_release(state: &ToolState, cursor_id: &str) {
    state.mouse_hold.lock().unwrap().remove(cursor_id);
}

fn release_mouse_hold_for_session<Release>(
    state: &ToolState,
    session_id: &str,
    mut release: Release,
) -> Result<(), String>
where
    Release: FnMut(&str, &MouseHoldState) -> anyhow::Result<()>,
{
    let hold = state.mouse_hold.lock().unwrap().get(session_id).cloned();
    let Some(hold) = hold else {
        return Ok(());
    };

    release(session_id, &hold).map_err(|error| error.to_string())?;
    state.mouse_hold.lock().unwrap().remove(session_id);
    Ok(())
}

#[cfg(test)]
mod cursor_hook_emission_tests {
    use super::emit_cursor_hook;
    use cua_driver_core::cursor_hook::{set_cursor_hook_fn, CursorHookEvent};
    use std::sync::{Arc, Mutex};

    /// Linux pointer actions report the agent cursor to an embedder's hook
    /// (cua-spacesd turns them into presence), moves and presses alike, and
    /// an anonymous cursor id never reaches it.
    #[test]
    fn pointer_actions_reach_the_cursor_hook() {
        let seen: Arc<Mutex<Vec<CursorHookEvent>>> = Arc::default();
        let sink = seen.clone();
        if !set_cursor_hook_fn(move |e| sink.lock().unwrap().push(e)) {
            // Another test in this binary owns the process-wide hook.
            return;
        }
        emit_cursor_hook("agent-7", 10.0, 20.0, false);
        emit_cursor_hook("agent-7", 10.0, 20.0, true);
        emit_cursor_hook("", 1.0, 1.0, false);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            (
                seen[0].cursor_id.as_str(),
                seen[0].x,
                seen[0].y,
                seen[0].pressed
            ),
            ("agent-7", 10.0, 20.0, false)
        );
        assert!(seen[1].pressed);
    }
}
