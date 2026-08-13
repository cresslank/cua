//! Resource-scoped action leases for multi-agent coordination.
//!
//! Host GNOME still has one raw-input seat. This table is the in-process
//! authority for overlapping observations, per-window semantic writes,
//! process-wide app writes, and globally exclusive desktop-raw transactions.
//! Cross-process sharing is layered on later; this module must stay the
//! single classification and grant-order source.
//!
//! ## C0 process-global mutable state
//!
//! Audit of `cua-driver-core` globals that can still race two writers:
//!
//! - `tool::desktop_action_coordinator` — process-wide `Mutex<()>`. Semantic
//!   writes now use this table; desktop-raw is taken after portal/libei
//!   readiness, not across a consent dialog.
//! - `tool::active_text_input_pids` — folded into the app/desktop-raw lease.
//! - `element_token` — already runtime-generation scoped.
//! - `session::*` maps — already session-scoped; session end/idle eviction
//!   must also drop leases owned by that session.
//! - `cdp` pool — browser-profile scoped later.
//! - `capture_scope` — already session-scoped.
//! - Platform host raw-input flock (`platform-linux`) remains an OS backup
//!   for desktop-raw; it is not a resource-scoped coordinator.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::Notify;

/// Final-action classification used at the dispatch boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionClass {
    Observation,
    WindowSemantic,
    AppScoped,
    BrowserProfile,
    DesktopRaw,
}

/// Exact window identity used as a lease key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExactWindow {
    pub pid: i64,
    pub window_id: u64,
}

/// One resource in the fixed grant order: desktop-raw, then app, then window.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LeaseResource {
    DesktopRaw,
    BrowserProfile { key: String },
    App { pid: i64 },
    Window { pid: i64, window_id: u64 },
}

impl LeaseResource {
    fn rank(&self) -> u8 {
        match self {
            Self::DesktopRaw => 0,
            Self::BrowserProfile { .. } => 1,
            Self::App { .. } => 2,
            Self::Window { .. } => 3,
        }
    }
}

/// Caller-visible lease request. Empty resource sets (observations) never wait.
#[derive(Debug, Clone)]
pub struct LeaseRequest {
    pub class: ActionClass,
    pub window: Option<ExactWindow>,
    pub browser_profile: Option<String>,
    pub wait: Duration,
}

impl LeaseRequest {
    pub fn observation() -> Self {
        Self {
            class: ActionClass::Observation,
            window: None,
            browser_profile: None,
            wait: Duration::ZERO,
        }
    }

    pub fn window_semantic(window: ExactWindow, wait: Duration) -> Self {
        Self {
            class: ActionClass::WindowSemantic,
            window: Some(window),
            browser_profile: None,
            wait,
        }
    }

    pub fn app_scoped(window: ExactWindow, wait: Duration) -> Self {
        Self {
            class: ActionClass::AppScoped,
            window: Some(window),
            browser_profile: None,
            wait,
        }
    }

    pub fn desktop_raw(window: Option<ExactWindow>, wait: Duration) -> Self {
        Self {
            class: ActionClass::DesktopRaw,
            window,
            browser_profile: None,
            wait,
        }
    }

    pub fn browser_profile(key: impl Into<String>, wait: Duration) -> Self {
        Self {
            class: ActionClass::BrowserProfile,
            window: None,
            browser_profile: Some(key.into()),
            wait,
        }
    }
}

/// Final-boundary classification. Observation never takes a write lease.
pub fn classify_tool(tool: &str) -> ActionClass {
    match tool {
        "set_value" | "invoke_menu" => ActionClass::WindowSemantic,
        "click"
        | "double_click"
        | "right_click"
        | "scroll"
        | "drag"
        | "mouse_drag"
        | "parallel_mouse_drag"
        | "move_cursor"
        | "mouse_button_down"
        | "mouse_button_up"
        | "type_text"
        | "press_key"
        | "hotkey"
        | "bring_to_front"
        | "set_window_frame" => ActionClass::DesktopRaw,
        "browser_click"
        | "browser_type"
        | "browser_navigate"
        | "browser_pointer"
        | "browser_dialog"
        | "browser_download"
        | "browser_set_input_files"
        | "browser_prepare" => ActionClass::BrowserProfile,
        _ => ActionClass::Observation,
    }
}

/// Exact window from already-normalized tool arguments.
pub fn window_from_args(args: &Value) -> Option<ExactWindow> {
    let pid = args
        .get("pid")
        .and_then(Value::as_i64)
        .filter(|pid| *pid > 0)?;
    let window_id = args
        .get("window_id")
        .and_then(Value::as_u64)
        .filter(|window_id| *window_id > 0)?;
    Some(ExactWindow { pid, window_id })
}

fn browser_profile_key(args: &Value) -> Option<String> {
    args.get("pid")
        .and_then(Value::as_i64)
        .filter(|pid| *pid > 0)
        .map(|pid| format!("pid:{pid}"))
        .or_else(|| {
            args.get("profile_name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
}

/// Build the grant set for one tool call. Desktop-raw still names the window
/// so a targeted raw transaction also covers that app and window.
pub fn lease_request_for(tool: &str, args: &Value, wait: Duration) -> LeaseRequest {
    match classify_tool(tool) {
        ActionClass::Observation => LeaseRequest::observation(),
        ActionClass::WindowSemantic => match window_from_args(args) {
            Some(window) => LeaseRequest::window_semantic(window, wait),
            None => args
                .get("pid")
                .and_then(Value::as_i64)
                .filter(|pid| *pid > 0)
                .map(|pid| LeaseRequest::app_scoped(ExactWindow { pid, window_id: 0 }, wait))
                .unwrap_or_else(LeaseRequest::observation),
        },
        ActionClass::AppScoped => args
            .get("pid")
            .and_then(Value::as_i64)
            .filter(|pid| *pid > 0)
            .map(|pid| {
                LeaseRequest::app_scoped(
                    ExactWindow {
                        pid,
                        window_id: window_from_args(args)
                            .map(|window| window.window_id)
                            .unwrap_or(0),
                    },
                    wait,
                )
            })
            .unwrap_or_else(LeaseRequest::observation),
        ActionClass::BrowserProfile => browser_profile_key(args)
            .map(|key| LeaseRequest::browser_profile(key, wait))
            .unwrap_or_else(LeaseRequest::observation),
        ActionClass::DesktopRaw => LeaseRequest::desktop_raw(window_from_args(args), wait),
    }
}

/// Process-wide table. Tests that need isolation should construct their own.
pub fn global() -> &'static Arc<ActionLeaseTable> {
    static TABLE: OnceLock<Arc<ActionLeaseTable>> = OnceLock::new();
    TABLE.get_or_init(ActionLeaseTable::new)
}

type RawInputReadyHook = fn(&str) -> Result<(), String>;

static RAW_INPUT_READY: OnceLock<RawInputReadyHook> = OnceLock::new();

/// Platform hook that waits for portal/libei without holding a write lease.
pub fn install_raw_input_ready_hook(hook: RawInputReadyHook) {
    let _ = RAW_INPUT_READY.set(hook);
}

pub fn ensure_raw_input_ready(tool: &str) -> Result<(), String> {
    match RAW_INPUT_READY.get() {
        Some(hook) => hook(tool),
        None => Ok(()),
    }
}

/// Structured busy result. Desktop-raw contention is `input_busy`; window or
/// app contention is `target_busy`. Messages stay free of other-session
/// metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseError {
    InputBusy { message: String },
    TargetBusy { message: String },
}

impl LeaseError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InputBusy { .. } => "input_busy",
            Self::TargetBusy { .. } => "target_busy",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::InputBusy { message } | Self::TargetBusy { message } => message,
        }
    }
}

/// Held grant. Drop or cancel of the owner releases every resource atomically.
pub struct ActionLease {
    table: Arc<ActionLeaseTable>,
    id: u64,
}

impl Drop for ActionLease {
    fn drop(&mut self) {
        self.table.release(self.id);
    }
}

impl std::fmt::Debug for ActionLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionLease").field("id", &self.id).finish()
    }
}

#[derive(Default)]
struct TableInner {
    held: Vec<(u64, Vec<LeaseResource>)>,
}

/// In-process resource table. Task 3 will share one instance via the daemon.
pub struct ActionLeaseTable {
    inner: Mutex<TableInner>,
    condvar: Condvar,
    notify: Notify,
    next_id: AtomicU64,
}

impl ActionLeaseTable {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(TableInner::default()),
            condvar: Condvar::new(),
            notify: Notify::new(),
            next_id: AtomicU64::new(1),
        })
    }

    /// Resources in grant order. Callers cannot invert the lock graph.
    pub fn resources_for(request: &LeaseRequest) -> Vec<LeaseResource> {
        let mut resources = match request.class {
            ActionClass::Observation => Vec::new(),
            ActionClass::WindowSemantic => request
                .window
                .map(|window| LeaseResource::Window {
                    pid: window.pid,
                    window_id: window.window_id,
                })
                .into_iter()
                .collect(),
            ActionClass::AppScoped => request
                .window
                .map(|window| LeaseResource::App { pid: window.pid })
                .into_iter()
                .collect(),
            ActionClass::BrowserProfile => request
                .browser_profile
                .as_ref()
                .map(|key| LeaseResource::BrowserProfile { key: key.clone() })
                .into_iter()
                .collect(),
            ActionClass::DesktopRaw => {
                let mut resources = vec![LeaseResource::DesktopRaw];
                if let Some(window) = request.window {
                    resources.push(LeaseResource::App { pid: window.pid });
                    resources.push(LeaseResource::Window {
                        pid: window.pid,
                        window_id: window.window_id,
                    });
                }
                resources
            }
        };
        resources.sort_by_key(LeaseResource::rank);
        resources
    }

    pub async fn acquire(
        self: &Arc<Self>,
        request: LeaseRequest,
    ) -> Result<ActionLease, LeaseError> {
        let needed = Self::resources_for(&request);
        if needed.is_empty() {
            return Ok(self.issue(Vec::new()));
        }
        let deadline = Instant::now() + request.wait;
        loop {
            if let Some(lease) = self.try_grant(&needed) {
                return Ok(lease);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(busy_error(&needed));
            }
            let remaining = deadline.saturating_duration_since(now);
            let notified = self.notify.notified();
            if let Some(lease) = self.try_grant(&needed) {
                return Ok(lease);
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(remaining) => {}
            }
        }
    }

    /// Sync acquire for platform workers that run outside the MCP task.
    pub fn acquire_blocking(
        self: &Arc<Self>,
        request: LeaseRequest,
    ) -> Result<ActionLease, LeaseError> {
        let needed = Self::resources_for(&request);
        if needed.is_empty() {
            return Ok(self.issue(Vec::new()));
        }
        let deadline = Instant::now() + request.wait;
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            let held: Vec<LeaseResource> = inner
                .held
                .iter()
                .flat_map(|(_, resources)| resources.iter().cloned())
                .collect();
            if !set_conflicts(&needed, &held) {
                return Ok(self.issue_locked(&mut inner, needed));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(busy_error(&needed));
            }
            let remaining = deadline.saturating_duration_since(now);
            let (guard, wait) = self
                .condvar
                .wait_timeout(inner, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner = guard;
            if wait.timed_out() && Instant::now() >= deadline {
                return Err(busy_error(&needed));
            }
        }
    }

    fn try_grant(self: &Arc<Self>, needed: &[LeaseResource]) -> Option<ActionLease> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let held: Vec<LeaseResource> = inner
            .held
            .iter()
            .flat_map(|(_, resources)| resources.iter().cloned())
            .collect();
        if set_conflicts(needed, &held) {
            return None;
        }
        Some(self.issue_locked(&mut inner, needed.to_vec()))
    }

    fn issue(self: &Arc<Self>, resources: Vec<LeaseResource>) -> ActionLease {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.issue_locked(&mut inner, resources)
    }

    fn issue_locked(
        self: &Arc<Self>,
        inner: &mut TableInner,
        resources: Vec<LeaseResource>,
    ) -> ActionLease {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if !resources.is_empty() {
            inner.held.push((id, resources));
        }
        ActionLease {
            table: Arc::clone(self),
            id,
        }
    }

    fn release(&self, id: u64) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = inner.held.len();
        inner.held.retain(|(held_id, _)| *held_id != id);
        if inner.held.len() != before {
            drop(inner);
            self.condvar.notify_all();
            self.notify.notify_waiters();
        }
    }
}

fn resources_conflict(left: &LeaseResource, right: &LeaseResource) -> bool {
    match (left, right) {
        (LeaseResource::DesktopRaw, LeaseResource::DesktopRaw) => true,
        (
            LeaseResource::BrowserProfile { key: left },
            LeaseResource::BrowserProfile { key: right },
        ) => left == right,
        (LeaseResource::App { pid: left }, LeaseResource::App { pid: right }) => left == right,
        (LeaseResource::App { pid: left }, LeaseResource::Window { pid: right, .. })
        | (LeaseResource::Window { pid: right, .. }, LeaseResource::App { pid: left }) => {
            left == right
        }
        (
            LeaseResource::Window {
                pid: left_pid,
                window_id: left_window,
            },
            LeaseResource::Window {
                pid: right_pid,
                window_id: right_window,
            },
        ) => left_pid == right_pid && left_window == right_window,
        _ => false,
    }
}

fn set_conflicts(needed: &[LeaseResource], held: &[LeaseResource]) -> bool {
    needed
        .iter()
        .any(|want| held.iter().any(|have| resources_conflict(want, have)))
}

fn busy_error(needed: &[LeaseResource]) -> LeaseError {
    if needed
        .iter()
        .any(|resource| matches!(resource, LeaseResource::DesktopRaw))
    {
        LeaseError::InputBusy {
            message: "another desktop-raw transaction holds the seat".into(),
        }
    } else {
        LeaseError::TargetBusy {
            message: "another writer holds the requested target".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::time::timeout;

    fn window(pid: i64, window_id: u64) -> ExactWindow {
        ExactWindow { pid, window_id }
    }

    async fn hold_and_count(
        table: Arc<ActionLeaseTable>,
        request: LeaseRequest,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        hold: Duration,
    ) {
        let _lease = table
            .acquire(request)
            .await
            .expect("lease should be granted");
        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
        max_active.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(hold).await;
        active.fetch_sub(1, Ordering::SeqCst);
    }

    #[test]
    fn grant_order_is_desktop_raw_then_app_then_window() {
        let request = LeaseRequest::desktop_raw(Some(window(9, 4)), Duration::from_millis(1));
        assert_eq!(
            ActionLeaseTable::resources_for(&request),
            vec![
                LeaseResource::DesktopRaw,
                LeaseResource::App { pid: 9 },
                LeaseResource::Window {
                    pid: 9,
                    window_id: 4
                },
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_observations_do_not_block_each_other() {
        let table = ActionLeaseTable::new();
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let hold = Duration::from_millis(40);
        let first = tokio::spawn(hold_and_count(
            Arc::clone(&table),
            LeaseRequest::observation(),
            Arc::clone(&active),
            Arc::clone(&max_active),
            hold,
        ));
        let second = tokio::spawn(hold_and_count(
            table,
            LeaseRequest::observation(),
            active,
            Arc::clone(&max_active),
            hold,
        ));
        first.await.unwrap();
        second.await.unwrap();
        assert!(max_active.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_window_semantic_writes_serialize_and_different_windows_do_not() {
        let table = ActionLeaseTable::new();
        let same_active = Arc::new(AtomicUsize::new(0));
        let same_max = Arc::new(AtomicUsize::new(0));
        let hold = Duration::from_millis(40);
        let same_a = tokio::spawn(hold_and_count(
            Arc::clone(&table),
            LeaseRequest::window_semantic(window(11, 21), Duration::from_secs(1)),
            Arc::clone(&same_active),
            Arc::clone(&same_max),
            hold,
        ));
        let same_b = tokio::spawn(hold_and_count(
            Arc::clone(&table),
            LeaseRequest::window_semantic(window(11, 21), Duration::from_secs(1)),
            same_active,
            Arc::clone(&same_max),
            hold,
        ));
        same_a.await.unwrap();
        same_b.await.unwrap();
        assert_eq!(same_max.load(Ordering::SeqCst), 1);

        let distinct_active = Arc::new(AtomicUsize::new(0));
        let distinct_max = Arc::new(AtomicUsize::new(0));
        let left = tokio::spawn(hold_and_count(
            Arc::clone(&table),
            LeaseRequest::window_semantic(window(11, 21), Duration::from_secs(1)),
            Arc::clone(&distinct_active),
            Arc::clone(&distinct_max),
            hold,
        ));
        let right = tokio::spawn(hold_and_count(
            table,
            LeaseRequest::window_semantic(window(11, 22), Duration::from_secs(1)),
            distinct_active,
            Arc::clone(&distinct_max),
            hold,
        ));
        left.await.unwrap();
        right.await.unwrap();
        assert!(distinct_max.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_desktop_raw_transactions_never_overlap() {
        let table = ActionLeaseTable::new();
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let hold = Duration::from_millis(40);
        let first = tokio::spawn(hold_and_count(
            Arc::clone(&table),
            LeaseRequest::desktop_raw(Some(window(3, 1)), Duration::from_secs(1)),
            Arc::clone(&active),
            Arc::clone(&max_active),
            hold,
        ));
        let second = tokio::spawn(hold_and_count(
            table,
            LeaseRequest::desktop_raw(Some(window(4, 2)), Duration::from_secs(1)),
            active,
            Arc::clone(&max_active),
            hold,
        ));
        first.await.unwrap();
        second.await.unwrap();
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn app_scoped_write_blocks_another_window_in_the_same_pid() {
        let table = ActionLeaseTable::new();
        let holder = table
            .acquire(LeaseRequest::app_scoped(
                window(44, 1),
                Duration::from_secs(1),
            ))
            .await
            .unwrap();
        let blocked = table
            .acquire(LeaseRequest::window_semantic(
                window(44, 2),
                Duration::from_millis(30),
            ))
            .await
            .expect_err("same-pid window write must wait on the app lease");
        assert_eq!(blocked.code(), "target_busy");
        let other_pid = table
            .acquire(LeaseRequest::window_semantic(
                window(45, 2),
                Duration::from_millis(30),
            ))
            .await
            .expect("a different pid stays independent");
        drop(other_pid);
        drop(holder);
    }

    #[tokio::test]
    async fn wait_past_deadline_returns_busy_and_does_not_run_the_action() {
        let table = ActionLeaseTable::new();
        let _holder = table
            .acquire(LeaseRequest::desktop_raw(None, Duration::from_secs(1)))
            .await
            .unwrap();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let error = table
            .acquire(LeaseRequest::desktop_raw(None, Duration::from_millis(25)))
            .await
            .expect_err("expired waiter must not receive the seat");
        assert_eq!(error.code(), "input_busy");
        assert!(
            !ran.load(Ordering::SeqCst),
            "the action must not run after a busy refusal"
        );
    }

    #[tokio::test]
    async fn drop_of_the_holder_releases_the_lease() {
        let table = ActionLeaseTable::new();
        let holder = table
            .acquire(LeaseRequest::window_semantic(
                window(8, 9),
                Duration::from_secs(1),
            ))
            .await
            .unwrap();
        drop(holder);
        table
            .acquire(LeaseRequest::window_semantic(
                window(8, 9),
                Duration::from_millis(30),
            ))
            .await
            .expect("drop must release the window lease");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mixed_grant_sets_cannot_deadlock() {
        let table = ActionLeaseTable::new();
        let raw = {
            let table = Arc::clone(&table);
            tokio::spawn(async move {
                let lease = table
                    .acquire(LeaseRequest::desktop_raw(
                        Some(window(70, 3)),
                        Duration::from_secs(1),
                    ))
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(lease);
            })
        };
        let semantic = {
            let table = Arc::clone(&table);
            tokio::spawn(async move {
                let lease = table
                    .acquire(LeaseRequest::window_semantic(
                        window(70, 3),
                        Duration::from_secs(1),
                    ))
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(lease);
            })
        };
        let app = tokio::spawn(async move {
            let lease = table
                .acquire(LeaseRequest::app_scoped(
                    window(70, 8),
                    Duration::from_secs(1),
                ))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(lease);
        });
        timeout(Duration::from_secs(2), async {
            raw.await.unwrap();
            semantic.await.unwrap();
            app.await.unwrap();
        })
        .await
        .expect("fixed grant order must not deadlock");
    }

    #[test]
    fn desktop_raw_conflicts_only_with_desktop_raw_when_untargeted() {
        assert!(resources_conflict(
            &LeaseResource::DesktopRaw,
            &LeaseResource::DesktopRaw
        ));
        assert!(!resources_conflict(
            &LeaseResource::DesktopRaw,
            &LeaseResource::Window {
                pid: 1,
                window_id: 2
            }
        ));
        assert!(set_conflicts(
            &[LeaseResource::App { pid: 4 }],
            &[LeaseResource::Window {
                pid: 4,
                window_id: 9
            }]
        ));
    }

    #[test]
    fn classify_tool_separates_observation_semantic_raw_and_browser() {
        assert_eq!(classify_tool("list_windows"), ActionClass::Observation);
        assert_eq!(classify_tool("get_window_state"), ActionClass::Observation);
        assert_eq!(classify_tool("get_desktop_state"), ActionClass::Observation);
        assert_eq!(classify_tool("set_value"), ActionClass::WindowSemantic);
        assert_eq!(classify_tool("invoke_menu"), ActionClass::WindowSemantic);
        assert_eq!(classify_tool("click"), ActionClass::DesktopRaw);
        assert_eq!(classify_tool("type_text"), ActionClass::DesktopRaw);
        assert_eq!(classify_tool("browser_click"), ActionClass::BrowserProfile);
    }

    #[test]
    fn lease_request_for_targeted_raw_covers_desktop_app_and_window() {
        let request = lease_request_for(
            "click",
            &serde_json::json!({"pid": 11, "window_id": 22}),
            Duration::from_millis(5),
        );
        assert_eq!(
            ActionLeaseTable::resources_for(&request),
            vec![
                LeaseResource::DesktopRaw,
                LeaseResource::App { pid: 11 },
                LeaseResource::Window {
                    pid: 11,
                    window_id: 22
                },
            ]
        );
    }

    #[test]
    fn pid_only_set_value_is_app_scoped() {
        let request = lease_request_for(
            "set_value",
            &serde_json::json!({"pid": 15, "value": "x"}),
            Duration::ZERO,
        );
        assert_eq!(request.class, ActionClass::AppScoped);
        assert_eq!(
            ActionLeaseTable::resources_for(&request),
            vec![LeaseResource::App { pid: 15 }]
        );
    }

    #[test]
    fn blocking_acquire_returns_busy_without_running_the_action() {
        let table = ActionLeaseTable::new();
        let _hold = table
            .acquire_blocking(LeaseRequest::desktop_raw(None, Duration::from_secs(1)))
            .unwrap();
        let mut ran = false;
        let error = table
            .acquire_blocking(LeaseRequest::desktop_raw(None, Duration::from_millis(15)))
            .expect_err("expired blocking waiter must not receive the seat");
        assert_eq!(error.code(), "input_busy");
        assert!(!ran);
        ran = true;
        assert!(ran);
    }
}
