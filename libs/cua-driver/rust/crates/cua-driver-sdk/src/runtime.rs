//! Private platform-runtime composition behind the public SDK boundary.
//!
//! This is deliberately not a second public interface. Both imported SDK
//! objects and the daemon host construct the same runtime here; transport
//! adapters are downstream consumers of `CuaDriver`.

use crate::{DriverActivityEvent, DriverActivityKind, DriverActivityObserver};
use cua_driver_core::{
    authorization::PermissionMode,
    protocol::ToolResult as CoreToolResult,
    session_authorization::{
        AuthenticatedActionConnection, DelegatedSessionRequest, EffectiveAuthorizationContext,
        SessionAuthorizationError, SessionAuthorizationRegistry, SessionModeCeiling,
        TrustedHostLease,
    },
    session_manifest::SessionManifest,
    tool::ToolRegistry,
};
use cursor_overlay::CursorConfig;
use serde_json::Value;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

const RECORDING_IDLE_TTL_SECS_DEFAULT: u64 = 300;
const SESSION_IDLE_TTL_SECS_DEFAULT: u64 = 300;
#[cfg(test)]
pub(crate) static TEST_RUNTIME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RuntimeCreateError {
    #[allow(dead_code)]
    #[error(
        "runtime_already_exists: one direct Cua Driver runtime is already active in this process"
    )]
    AlreadyExists,
    #[error("invalid runtime authorization configuration: {0}")]
    Authorization(String),
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    #[error("runtime_unavailable: {0}")]
    Unavailable(String),
}

#[derive(Clone)]
pub(crate) struct RuntimeOptions {
    pub cursor: CursorConfig,
    /// Whether the importing/embedding host owns macOS permission UX. Such a
    /// runtime may inspect TCC state but must never raise Cua-owned prompts.
    pub host_owns_permission_ux: bool,
    pub host_bundle_id: Option<String>,
    pub compatibility_mode: bool,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub prepare_desktop_environment: bool,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub require_atspi_listener: bool,
    pub register_host_tools: Option<fn(&mut ToolRegistry)>,
    pub authorization_ceiling: Option<SessionModeCeiling>,
    pub compatibility_authorization: Option<(PermissionMode, Option<Arc<SessionManifest>>)>,
    /// Constructor-only authorization host. This object is never reachable from
    /// public tool arguments or transport metadata.
    pub authorization_host: Option<Arc<dyn cua_driver_core::consent::ProtectedConsentProvider>>,
    pub activity_observer: Option<Arc<dyn DriverActivityObserver>>,
}

impl RuntimeOptions {
    pub(crate) fn embedded(compatibility_mode: bool) -> Self {
        Self {
            cursor: CursorConfig {
                enabled: false,
                ..CursorConfig::default()
            },
            host_owns_permission_ux: true,
            host_bundle_id: None,
            compatibility_mode,
            prepare_desktop_environment: true,
            require_atspi_listener: true,
            register_host_tools: None,
            authorization_ceiling: None,
            compatibility_authorization: None,
            authorization_host: None,
            activity_observer: None,
        }
    }

    pub(crate) fn embedded_with_ceiling(
        compatibility_mode: bool,
        authorization_ceiling: SessionModeCeiling,
        compatibility_permission_mode: PermissionMode,
        compatibility_manifest: Option<Arc<SessionManifest>>,
    ) -> Self {
        Self {
            authorization_ceiling: Some(authorization_ceiling),
            compatibility_authorization: Some((
                compatibility_permission_mode,
                compatibility_manifest,
            )),
            ..Self::embedded(compatibility_mode)
        }
    }
}

pub(crate) struct RuntimeSession {
    runtime: Arc<DriverRuntime>,
    authorization_registry: Arc<SessionAuthorizationRegistry>,
    host: TrustedHostLease,
    connection: AuthenticatedActionConnection,
    context: Arc<EffectiveAuthorizationContext>,
    public_session: String,
}

impl RuntimeSession {
    pub(crate) async fn invoke(&self, name: &str, mut args: Value) -> Option<CoreToolResult> {
        cua_driver_core::tool_args::sanitize_reserved_args(&mut args);
        let Some(arguments) = args.as_object_mut() else {
            return Some(permission_denied_result(
                "session-bound actions require an object argument".to_owned(),
            ));
        };
        if arguments
            .get("session")
            .and_then(Value::as_str)
            .is_some_and(|session| session != self.public_session)
        {
            return Some(permission_denied_result(
                "public session substitution does not match the bound authorization context"
                    .to_owned(),
            ));
        }
        arguments.insert(
            "session".to_owned(),
            Value::String(self.public_session.clone()),
        );
        let result = self
            .runtime
            .invoke_with_context(name, args, self.context.clone())
            .await;
        if name == "end_session"
            && result
                .as_ref()
                .is_some_and(|result| result.is_error != Some(true))
        {
            self.authorization_registry
                .revoke_connection(&self.connection);
        }
        result
    }
}

impl Drop for RuntimeSession {
    fn drop(&mut self) {
        self.authorization_registry
            .revoke_connection(&self.connection);
        self.authorization_registry.revoke_host(&self.host);
    }
}

pub(crate) struct DriverRuntime {
    registry: Arc<ToolRegistry>,
    authorization_registry: Arc<SessionAuthorizationRegistry>,
    compatibility_context: Arc<EffectiveAuthorizationContext>,
    shutdown: AtomicBool,
    last_activity: AtomicU64,
    /// Calls hold a read guard; shutdown takes the write guard after closing
    /// admission. Therefore shutdown is idempotent and does not return while a
    /// previously admitted operation is still executing.
    lifecycle: tokio::sync::RwLock<()>,
    activity_observer: Option<Arc<dyn DriverActivityObserver>>,
}

impl DriverRuntime {
    pub(crate) fn create(options: RuntimeOptions) -> Result<Arc<Self>, RuntimeCreateError> {
        #[cfg(target_os = "windows")]
        if let Err(reason) = platform_windows::diagnostics::interactive_desktop_check() {
            return Err(RuntimeCreateError::Unavailable(format!(
                "Cua Driver requires an interactive Windows user session: {reason}"
            )));
        }
        let authorization_registry = Arc::new(match options.authorization_ceiling.clone() {
            Some(ceiling) => SessionAuthorizationRegistry::with_ceiling(ceiling),
            None => SessionAuthorizationRegistry::process()
                .map_err(RuntimeCreateError::Authorization)?,
        });
        let compatibility_context = match options.compatibility_authorization.clone() {
            Some((mode, manifest)) => authorization_registry
                .compatibility_context(mode, manifest)
                .map_err(RuntimeCreateError::Authorization)?,
            None => authorization_registry
                .legacy_context()
                .map_err(RuntimeCreateError::Authorization)?,
        };
        let registry = Arc::new(cua_driver_core::tool::with_runtime_scope(
            compatibility_context.runtime_scope_key(),
            || build_registry(&options),
        )?);
        registry.init_self_weak();
        let runtime = Arc::new(Self {
            registry,
            authorization_registry,
            compatibility_context,
            shutdown: AtomicBool::new(false),
            last_activity: AtomicU64::new(now_unix_secs()),
            lifecycle: tokio::sync::RwLock::new(()),
            activity_observer: options.activity_observer.clone(),
        });
        spawn_lifecycle_maintenance(&runtime);
        Ok(runtime)
    }

    pub(crate) fn is_running(&self) -> bool {
        !self.shutdown.load(Ordering::Acquire)
    }

    pub(crate) fn runtime_scope_key(&self) -> String {
        self.compatibility_context.runtime_scope_key()
    }

    pub(crate) async fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        let _drained = self.lifecycle.write().await;
        self.authorization_registry.revoke_all();
        let runtime_prefix = format!(
            "__cua_runtime_{}:",
            self.compatibility_context.runtime_scope_key()
        );
        cua_driver_core::session::revoke_sessions_with_prefix(&runtime_prefix);
        cua_driver_core::session::forget_ended_sessions_with_prefix(&runtime_prefix);
        cua_driver_core::session::forget_suspended_runtime_scope(
            &self.compatibility_context.runtime_scope_key(),
        );
        cua_driver_core::element_token::global()
            .clear_runtime_scope(&self.compatibility_context.runtime_scope_key());
        let recording = self.registry.recording.clone();
        let _ = tokio::task::spawn_blocking(move || recording.stop_owner(None)).await;
    }

    pub(crate) fn tools_list(&self) -> Option<Value> {
        self.is_running().then(|| self.registry.tools_list())
    }

    pub(crate) fn history(&self) -> Option<Arc<cua_driver_core::history::HistoryManager>> {
        self.is_running().then(|| self.registry.history()).flatten()
    }

    pub(crate) async fn invoke(&self, name: &str, args: Value) -> Option<CoreToolResult> {
        self.invoke_with_context(name, args, self.compatibility_context.clone())
            .await
    }

    pub(crate) async fn invoke_from_trusted_adapter(
        &self,
        name: &str,
        mut args: Value,
    ) -> Option<CoreToolResult> {
        let evidence =
            cua_driver_core::tool::TrustedInvocationEvidence::extract_from_adapter_args(&mut args);
        self.invoke_with_context_and_evidence(
            name,
            args,
            self.compatibility_context.clone(),
            evidence,
        )
        .await
    }

    async fn invoke_with_context(
        &self,
        name: &str,
        args: Value,
        context: Arc<EffectiveAuthorizationContext>,
    ) -> Option<CoreToolResult> {
        self.invoke_with_context_and_evidence(
            name,
            args,
            context,
            cua_driver_core::tool::TrustedInvocationEvidence::default(),
        )
        .await
    }

    async fn invoke_with_context_and_evidence(
        &self,
        name: &str,
        args: Value,
        context: Arc<EffectiveAuthorizationContext>,
        evidence: cua_driver_core::tool::TrustedInvocationEvidence,
    ) -> Option<CoreToolResult> {
        if !self.is_running() {
            return None;
        }
        self.last_activity.store(now_unix_secs(), Ordering::Relaxed);
        let _operation = self.lifecycle.read().await;
        if !self.is_running() {
            return None;
        }
        let ending_session = (name == "end_session")
            .then(|| {
                args.get("session")
                    .and_then(Value::as_str)
                    .map(|session| context.runtime_session_key(session))
            })
            .flatten();
        let public_session = args
            .get("session")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let risk = cua_driver_core::authorization::classify_tool_call(name, &args);
        let adapters = cua_driver_core::authorization::enforcement_adapters_for_call(name, &args)
            .into_iter()
            .map(|adapter| adapter.id.to_owned())
            .collect::<Vec<_>>();
        let result = self
            .registry
            .invoke_with_context_and_evidence(name, args, context, evidence)
            .await;
        if let Some(observer) = self.activity_observer.as_ref() {
            let refusal_code = result
                .structured_content
                .as_ref()
                .and_then(|value| value.pointer("/refusal/code"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let success = result.is_error != Some(true);
            observer.on_activity(DriverActivityEvent {
                kind: if success {
                    DriverActivityKind::AuthorizedAction
                } else if refusal_code.is_some() {
                    DriverActivityKind::AuthorizationRefused
                } else {
                    DriverActivityKind::ActionFailed
                },
                unix_ms: now_unix_ms(),
                tool_name: name.to_owned(),
                adapter_ids: adapters.clone(),
                risk_class: risk.class.as_str().to_owned(),
                public_session: public_session.clone(),
                refusal_code,
            });
            if success && name == "start_session" {
                observer.on_activity(activity_lifecycle_event(
                    DriverActivityKind::SessionStarted,
                    name,
                    public_session.clone(),
                ));
            }
            if success
                && adapters
                    .iter()
                    .any(|adapter| adapter == "browser_prepare.existing_profile")
            {
                observer.on_activity(activity_lifecycle_event(
                    DriverActivityKind::GrantIssued,
                    name,
                    public_session.clone(),
                ));
            }
            if success && name == "end_session" {
                observer.on_activity(activity_lifecycle_event(
                    DriverActivityKind::GrantRevoked,
                    name,
                    public_session.clone(),
                ));
                observer.on_activity(activity_lifecycle_event(
                    DriverActivityKind::SessionEnded,
                    name,
                    public_session,
                ));
            }
        }
        if let Some(session) = ending_session {
            // `end_session` is a lifecycle boundary: do not report completion
            // until any recording owned by the session has finalized.
            let recording = self.registry.recording.clone();
            let _ = tokio::task::spawn_blocking(move || recording.stop_owner(Some(&session))).await;
        }
        Some(result)
    }

    pub(crate) fn create_trusted_session(
        self: &Arc<Self>,
        request: DelegatedSessionRequest,
    ) -> Result<Arc<RuntimeSession>, SessionAuthorizationError> {
        if !self.is_running() {
            return Err(SessionAuthorizationError::RuntimeUnavailable);
        }
        let public_session = request.public_session.clone();
        let transport_session = request.transport_session.clone();
        let (host, connection) = self.authorization_registry.trusted_in_process_binding();
        self.authorization_registry
            .bind_delegated_session(&host, &connection, request)?;
        let context = self.authorization_registry.resolve_delegated(
            &connection,
            &public_session,
            &transport_session,
        )?;
        Ok(Arc::new(RuntimeSession {
            runtime: self.clone(),
            authorization_registry: self.authorization_registry.clone(),
            host,
            connection,
            context,
            public_session,
        }))
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn activity_lifecycle_event(
    kind: DriverActivityKind,
    tool_name: &str,
    public_session: Option<String>,
) -> DriverActivityEvent {
    DriverActivityEvent {
        kind,
        unix_ms: now_unix_ms(),
        tool_name: tool_name.to_owned(),
        adapter_ids: Vec::new(),
        risk_class: "r0".to_owned(),
        public_session,
        refusal_code: None,
    }
}

impl Drop for DriverRuntime {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.authorization_registry.revoke_all();
        let runtime_scope = self.compatibility_context.runtime_scope_key();
        let runtime_prefix = format!("__cua_runtime_{runtime_scope}:");
        cua_driver_core::session::revoke_sessions_with_prefix(&runtime_prefix);
        cua_driver_core::session::forget_ended_sessions_with_prefix(&runtime_prefix);
        cua_driver_core::session::forget_suspended_runtime_scope(&runtime_scope);
        cua_driver_core::element_token::global().clear_runtime_scope(&runtime_scope);
        // Explicit `shutdown()` drains work and finalizes recordings. Drop is
        // runtime-scoped and non-blocking so a retained binding cannot affect
        // another generation.
        let recording = self.registry.recording.clone();
        std::thread::spawn(move || {
            let _ = recording.stop_owner(None);
        });
    }
}

fn permission_denied_result(message: String) -> CoreToolResult {
    CoreToolResult::error(message.clone()).with_structured(serde_json::json!({
        "status": "refused",
        "refusal": {
            "code": "permission_denied",
            "message": message,
        }
    }))
}

fn configured_ttl(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn spawn_lifecycle_maintenance(runtime: &Arc<DriverRuntime>) {
    let runtime = Arc::downgrade(runtime);
    let recording_ttl = configured_ttl(
        "CUA_DRIVER_RS_RECORDING_IDLE_TTL_SECS",
        RECORDING_IDLE_TTL_SECS_DEFAULT,
    );
    let session_ttl = std::time::Duration::from_secs(configured_ttl(
        "CUA_DRIVER_RS_SESSION_IDLE_TTL_SECS",
        SESSION_IDLE_TTL_SECS_DEFAULT,
    ));
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        let Some(runtime) = runtime.upgrade() else {
            break;
        };
        if !runtime.is_running() {
            break;
        }
        let ended = cua_driver_core::session::evict_idle_with_prefix(
            session_ttl,
            &format!(
                "__cua_runtime_{}:",
                runtime.compatibility_context.runtime_scope_key()
            ),
        );
        if !ended.is_empty() {
            tracing::info!(
                count = ended.len(),
                "idle-TTL reclaimed runtime-owned sessions"
            );
        }
        let idle = now_unix_secs().saturating_sub(runtime.last_activity.load(Ordering::Relaxed));
        if idle >= recording_ttl && runtime.registry.recording.current_state().enabled {
            tracing::warn!("recording idle {idle}s ≥ {recording_ttl}s TTL; auto-stopping");
            let _ = runtime.registry.recording.stop_owner(None);
        }
    });
}

/// Build the canonical SDK tool inventory without acquiring runtime ownership.
///
/// This metadata-only path cannot dispatch actions and therefore remains
/// available when the host has no interactive desktop (for example Windows
/// Session 0). Finite CLI inspection commands use it to preserve their
/// desktop-free compatibility contract without weakening runtime admission.
pub(crate) fn tool_inventory(mut options: RuntimeOptions) -> Value {
    // Inventory construction is metadata-only even if the host's eventual
    // action runtime requests eager desktop preparation.
    options.prepare_desktop_environment = false;
    build_registry(&options)
        .expect("desktop-free tool inventory construction cannot fail")
        .tools_list()
}

fn build_registry(options: &RuntimeOptions) -> Result<ToolRegistry, RuntimeCreateError> {
    #[cfg(target_os = "macos")]
    let mut registry = {
        configure_macos_runtime();
        platform_macos::register_tools_with_cursor_and_provider(
            options.authorization_host.clone(),
            options.cursor.clone(),
            options.compatibility_mode,
            options.host_owns_permission_ux,
            options.host_bundle_id.clone(),
        )
    };

    #[cfg(target_os = "windows")]
    let mut registry = {
        configure_windows_runtime();
        platform_windows::register_tools_with_cursor_and_provider(
            options.authorization_host.clone(),
            options.cursor.clone(),
            options.compatibility_mode,
        )
    };

    #[cfg(target_os = "linux")]
    let mut registry = {
        configure_linux_runtime(
            options.prepare_desktop_environment,
            options.require_atspi_listener,
        )?;
        platform_linux::register_tools_with_cursor_and_provider(
            options.authorization_host.clone(),
            options.cursor.clone(),
            options.compatibility_mode,
        )
    };

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let mut registry = {
        let _ = options;
        ToolRegistry::new()
    };

    if let Some(register_host_tools) = options.register_host_tools {
        register_host_tools(&mut registry);
    }
    let recording = Arc::downgrade(&registry.recording);
    let recording_session_end = cua_driver_core::session::register_scoped_fallible_session_end_hook(
        "recording",
        move |session| {
            let Some(recording) = recording.upgrade() else {
                return Ok(());
            };
            recording
                .stop_owner(Some(session))
                .map_err(|error| error.to_string())
        },
    );
    registry.retain_session_end_hook(recording_session_end);
    Ok(registry)
}

#[cfg(target_os = "macos")]
fn configure_macos_runtime() {
    cua_driver_core::recording::set_screenshot_fn(|window_id, pid| {
        if let Some(window_id) = window_id {
            platform_macos::capture::screenshot_window_bytes(window_id as u32).ok()
        } else if let Some(pid) = pid {
            platform_macos::windows::resolve_main_window_id(pid as i32)
                .ok()
                .and_then(|window_id| {
                    platform_macos::capture::screenshot_window_bytes(window_id).ok()
                })
        } else {
            platform_macos::capture::screenshot_display_bytes().ok()
        }
    });
    cua_driver_core::recording::set_click_marker_fn(|png_bytes, x, y| {
        platform_macos::capture::crosshair_png_bytes(png_bytes, x, y).ok()
    });
    cua_driver_core::recording::set_ax_snapshot_fn(|window_id, pid| {
        platform_macos::recording_hooks::app_state_json_for(window_id, pid)
    });
    cua_driver_core::recording::set_element_bounds_fn(|window_id, pid, index| {
        platform_macos::recording_hooks::element_window_local_xy(window_id, pid, index)
    });
    cua_driver_core::video::set_video_backend_factory(Box::new(
        platform_macos::video_sckit::SckitVideoBackendFactory,
    ));
}

#[cfg(target_os = "windows")]
fn configure_windows_runtime() {
    cua_driver_core::recording::set_classified_screenshot_fn(|window_id, pid| {
        platform_windows::recording_hooks::screenshot_for_recording(window_id, pid)
    });
    cua_driver_core::recording::set_click_marker_fn(|png_bytes, x, y| {
        platform_windows::capture::crosshair_png_bytes(png_bytes, x, y).ok()
    });
    cua_driver_core::recording::set_ax_snapshot_fn(|window_id, pid| {
        platform_windows::recording_hooks::app_state_json_for(window_id, pid)
    });
    cua_driver_core::recording::set_element_bounds_fn(|window_id, pid, index| {
        platform_windows::recording_hooks::element_window_local_xy(window_id, pid, index)
    });
    cua_driver_core::video::set_video_backend_factory(Box::new(
        cua_driver_core::video_ffmpeg::FfmpegVideoBackendFactory,
    ));
}

#[cfg(target_os = "linux")]
fn acquire_linux_desktop_preparation_lock() -> Result<std::fs::File, String> {
    let effective_uid = unsafe { libc::geteuid() };
    let path = linux_desktop_preparation_lock_path(effective_uid);
    acquire_linux_desktop_preparation_lock_at(&path, effective_uid)
}

#[cfg(target_os = "linux")]
fn acquire_linux_desktop_preparation_lock_until(
    deadline: std::time::Instant,
) -> Result<std::fs::File, String> {
    let effective_uid = unsafe { libc::geteuid() };
    let path = linux_desktop_preparation_lock_path(effective_uid);
    acquire_linux_desktop_preparation_lock_at_until(&path, effective_uid, deadline)
}

#[cfg(target_os = "linux")]
fn linux_desktop_preparation_lock_path(effective_uid: u32) -> std::path::PathBuf {
    // Derive one host-wide location from the effective UID, never from ambient
    // XDG_RUNTIME_DIR or TMPDIR. Services, private workers, and manually
    // launched runtimes must serialize on the same inode even when their
    // environment discovery inputs differ.
    let runtime_dir = std::path::Path::new("/run/user").join(effective_uid.to_string());
    if runtime_dir.is_dir() {
        runtime_dir.join("cua-driver-desktop-preparation.lock")
    } else {
        std::path::Path::new("/tmp").join(format!(
            "cua-driver-desktop-preparation-{effective_uid}.lock"
        ))
    }
}

#[cfg(target_os = "linux")]
fn acquire_linux_desktop_preparation_lock_at(
    path: &std::path::Path,
    effective_uid: u32,
) -> Result<std::fs::File, String> {
    acquire_linux_desktop_preparation_lock_at_until(
        path,
        effective_uid,
        std::time::Instant::now() + std::time::Duration::from_secs(5),
    )
}

#[cfg(target_os = "linux")]
fn acquire_linux_desktop_preparation_lock_at_until(
    path: &std::path::Path,
    effective_uid: u32,
    deadline: std::time::Instant,
) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    // Reject a substituted non-regular, linked, group/world-accessible, or
    // foreign lock before trusting host-wide serialization.
    if !metadata.file_type().is_file()
        || metadata.uid() != effective_uid
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(format!(
            "refusing unsafe desktop preparation lock {}",
            path.display()
        ));
    }

    loop {
        // SAFETY: `file` owns this descriptor for the entire flock call.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(file);
        }
        let error = std::io::Error::last_os_error();
        let would_block = error
            .raw_os_error()
            .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN);
        if !would_block || std::time::Instant::now() >= deadline {
            return Err(format!(
                "desktop preparation lock {} unavailable: {error}",
                path.display()
            ));
        }
        std::thread::sleep(
            std::time::Duration::from_millis(20)
                .min(deadline.saturating_duration_since(std::time::Instant::now())),
        );
    }
}

#[cfg(target_os = "linux")]
fn trusted_private_worker_accessibility_bus_from_current_process(
) -> Result<Option<String>, RuntimeCreateError> {
    const PRIVATE_ATSPI_ROUTE: &str = "CUA_DRIVER_PRIVATE_AT_SPI_BUS_ADDRESS";

    match std::env::var(PRIVATE_ATSPI_ROUTE) {
        Ok(address) => {
            platform_linux::a11y::initialize_private_accessibility_bus(&address).map_err(
                |error| {
                    RuntimeCreateError::Unavailable(format!(
                        "could not validate the supervisor-owned private AT-SPI route: {error:#}"
                    ))
                },
            )?;
            Ok(Some(address))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(RuntimeCreateError::Unavailable(
            "supervisor-owned private AT-SPI route is not valid UTF-8".into(),
        )),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn prepare_private_worker_accessibility_route(
    deadline: std::time::Instant,
) -> Result<String, RuntimeCreateError> {
    if std::time::Instant::now() >= deadline {
        return Err(RuntimeCreateError::Unavailable(
            "private-worker desktop preparation deadline expired".into(),
        ));
    }
    if trusted_private_worker_accessibility_bus_from_current_process()?.is_none() {
        let preparation_lock = acquire_linux_desktop_preparation_lock_until(deadline)
            .map_err(RuntimeCreateError::Unavailable)?;
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(RuntimeCreateError::Unavailable(
                "private-worker desktop preparation exceeded its startup deadline".into(),
            ));
        }
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("cua-session-bus-discovery".into())
            .spawn(move || {
                let _ = sender.send(platform_linux::session_bus::session_bus_address());
            })
            .map_err(|error| {
                RuntimeCreateError::Unavailable(format!(
                    "could not start side-effect-free session-bus discovery: {error}"
                ))
            })?;
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(RuntimeCreateError::Unavailable(
                "session-bus discovery exceeded the private-worker startup deadline".into(),
            ));
        }
        let session_bus_address = receiver
            .recv_timeout(remaining)
            .map_err(|_| {
                RuntimeCreateError::Unavailable(
                    "session-bus discovery exceeded the private-worker startup deadline".into(),
                )
            })?
            .ok_or_else(|| {
                RuntimeCreateError::Unavailable(
                    "could not discover the desktop session bus without mutating the host environment"
                        .into(),
                )
            })?;
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(RuntimeCreateError::Unavailable(
                "private-worker accessibility preparation exceeded its startup deadline".into(),
            ));
        }
        platform_linux::a11y::ensure_accessibility_enabled_with_session_bus(
            preparation_lock,
            Some(session_bus_address),
            remaining,
        )
        .map_err(RuntimeCreateError::Unavailable)?;
    }

    platform_linux::a11y::trusted_accessibility_bus_address().map_err(|error| {
        RuntimeCreateError::Unavailable(format!(
            "could not resolve the prepared private-worker AT-SPI route: {error:#}"
        ))
    })
}

#[cfg(target_os = "linux")]
fn configure_linux_runtime(
    prepare_desktop_environment: bool,
    require_atspi_listener: bool,
) -> Result<(), RuntimeCreateError> {
    if prepare_desktop_environment {
        if let Some(preparation_lock) = linux_preparation_lock_for_admission(
            require_atspi_listener,
            acquire_linux_desktop_preparation_lock(),
        )? {
            platform_linux::xauth::ensure_xauthority_discovered();
            platform_linux::session_bus::ensure_session_bus_discovered();
            finish_linux_accessibility_admission(
                require_atspi_listener,
                platform_linux::a11y::ensure_accessibility_enabled(preparation_lock),
                || {
                    platform_linux::atspi::ensure_listener_active()
                        .map_err(|error| error.to_string())
                },
            )?;
        }
    }
    cua_driver_core::recording::set_screenshot_fn(|window_id, pid| {
        platform_linux::recording_hooks::screenshot_for_recording(window_id, pid)
    });
    cua_driver_core::recording::set_click_marker_fn(|png_bytes, x, y| {
        platform_linux::capture::crosshair_png_bytes(png_bytes, x, y).ok()
    });
    cua_driver_core::recording::set_ax_snapshot_fn(|window_id, pid| {
        platform_linux::recording_hooks::app_state_json_for(window_id, pid)
    });
    cua_driver_core::recording::set_element_bounds_fn(|window_id, pid, index| {
        platform_linux::recording_hooks::element_window_local_xy(window_id, pid, index)
    });
    if platform_linux::wayland::is_wayland() {
        cua_driver_core::video::set_video_backend_factory(Box::new(
            platform_linux::video_wayland::WfRecorderVideoBackendFactory,
        ));
    } else {
        cua_driver_core::video::set_video_backend_factory(Box::new(
            cua_driver_core::video_ffmpeg::FfmpegVideoBackendFactory,
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_preparation_lock_for_admission<T>(
    require_atspi_listener: bool,
    lock: Result<T, String>,
) -> Result<Option<T>, RuntimeCreateError> {
    match lock {
        Ok(lock) => Ok(Some(lock)),
        Err(error) if require_atspi_listener => Err(RuntimeCreateError::Unavailable(error)),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "desktop preparation lock unavailable; continuing with degraded accessibility capability"
            );
            Ok(None)
        }
    }
}

#[cfg(target_os = "linux")]
fn finish_linux_accessibility_admission(
    require_atspi_listener: bool,
    preparation: Result<(), String>,
    ensure_listener: impl FnOnce() -> Result<(), String>,
) -> Result<(), RuntimeCreateError> {
    if let Err(error) = preparation {
        if require_atspi_listener {
            return Err(RuntimeCreateError::Unavailable(format!(
                "session accessibility preparation failed: {error}"
            )));
        }
        tracing::warn!(
            error = %error,
            "session accessibility unavailable; continuing with degraded accessibility capability"
        );
        return Ok(());
    }

    if let Err(error) = ensure_listener() {
        if require_atspi_listener {
            return Err(RuntimeCreateError::Unavailable(format!(
                "persistent AT-SPI listener is unavailable: {error}"
            )));
        }
        tracing::warn!(
            error = %error,
            "persistent AT-SPI listener unavailable; continuing with degraded accessibility capability"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use cua_driver_core::consent::{
        ConsentAction, ConsentRequest, ProtectedConsentProvider, ProviderDecision,
    };
    use std::time::Duration;

    struct TestProtectedHost;

    #[async_trait]
    impl ProtectedConsentProvider for TestProtectedHost {
        fn provider_id(&self) -> &'static str {
            "test.runtime-protected-host"
        }

        async fn request_consent(
            &self,
            request: &ConsentRequest,
        ) -> Result<ProviderDecision, String> {
            Ok(ProviderDecision {
                action: ConsentAction::Accept,
                request_digest: request.request_digest.clone(),
            })
        }
    }

    fn standard_options() -> RuntimeOptions {
        let ceiling = SessionModeCeiling::for_trusted_sessions(
            [PermissionMode::Standard],
            false,
            Duration::from_secs(60),
            Duration::from_secs(30),
        )
        .unwrap();
        let mut options =
            RuntimeOptions::embedded_with_ceiling(false, ceiling, PermissionMode::Standard, None);
        options.authorization_host = Some(Arc::new(TestProtectedHost));
        options
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strict_linux_admission_rejects_accessibility_preparation_failure() {
        let listener_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called = listener_called.clone();
        let error = finish_linux_accessibility_admission(
            true,
            Err("org.a11y.Status rejected setup".into()),
            move || {
                called.store(true, Ordering::SeqCst);
                Ok(())
            },
        )
        .expect_err("strict admission must reject preparation failures");

        assert!(matches!(error, RuntimeCreateError::Unavailable(_)));
        assert!(!listener_called.load(Ordering::SeqCst));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn desktop_preparation_lock_contention_obeys_linux_admission_policy() {
        assert!(matches!(
            linux_preparation_lock_for_admission::<()>(true, Err("lock deadline".into())),
            Err(RuntimeCreateError::Unavailable(_))
        ));
        assert_eq!(
            linux_preparation_lock_for_admission::<()>(false, Err("lock deadline".into()))
                .expect("best-effort direct MCP should preserve transport"),
            None
        );
        assert_eq!(
            linux_preparation_lock_for_admission(false, Ok(7)).unwrap(),
            Some(7)
        );

        let path = std::env::temp_dir().join(format!(
            "cua-sdk-lock-policy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let effective_uid = unsafe { libc::geteuid() };
        let held = acquire_linux_desktop_preparation_lock_at_until(
            &path,
            effective_uid,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .unwrap();
        let contended = acquire_linux_desktop_preparation_lock_at_until(
            &path,
            effective_uid,
            std::time::Instant::now(),
        );
        assert!(linux_preparation_lock_for_admission(false, contended)
            .expect("real contention should degrade for direct MCP")
            .is_none());
        drop(held);
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn best_effort_linux_admission_degrades_on_accessibility_preparation_failure() {
        finish_linux_accessibility_admission(
            false,
            Err("org.a11y.Bus is unavailable".into()),
            || panic!("listener must not run after failed preparation"),
        )
        .expect("best-effort admission should preserve direct MCP transport");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn listener_failure_obeys_linux_admission_policy() {
        let strict = finish_linux_accessibility_admission(true, Ok(()), || {
            Err("registration rejected".into())
        });
        assert!(matches!(strict, Err(RuntimeCreateError::Unavailable(_))));

        finish_linux_accessibility_admission(false, Ok(()), || Err("registration rejected".into()))
            .expect("best-effort admission should degrade after listener rejection");
    }

    #[tokio::test]
    async fn authorized_dispatch_refreshes_only_the_runtime_private_activity_key() {
        let _runtime_test = TEST_RUNTIME_LOCK.lock().unwrap();
        let runtime = DriverRuntime::create(standard_options()).unwrap();
        let public = "runtime-activity-refresh";
        let internal = runtime.compatibility_context.runtime_session_key(public);
        let prefix = format!(
            "__cua_runtime_{}:",
            runtime.compatibility_context.runtime_scope_key()
        );

        runtime
            .invoke(
                "start_session",
                serde_json::json!({"session": public, "capture_scope": "auto"}),
            )
            .await
            .unwrap();
        assert!(cua_driver_core::session::has_session_activity(&internal));
        assert!(!cua_driver_core::session::has_session_activity(public));

        std::thread::sleep(Duration::from_millis(20));
        let idle_before_refresh =
            cua_driver_core::session::session_idle_duration(&internal).unwrap();
        runtime
            .invoke("start_session", serde_json::json!({"session": public}))
            .await
            .unwrap();
        let idle_after_refresh =
            cua_driver_core::session::session_idle_duration(&internal).unwrap();
        assert!(
            idle_after_refresh < idle_before_refresh,
            "authorized traffic must reset the private idle clock: before={idle_before_refresh:?} after={idle_after_refresh:?} ended={}",
            cua_driver_core::session::is_session_ended(&internal)
        );
        let evicted =
            cua_driver_core::session::evict_idle_with_prefix(idle_before_refresh, &prefix);
        assert!(
            !evicted.contains(&internal),
            "continuous authorized traffic must refresh the private idle clock"
        );

        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn idle_eviction_finalizes_the_owning_runtime_recording() {
        let _runtime_test = TEST_RUNTIME_LOCK.lock().unwrap();
        let runtime = DriverRuntime::create(standard_options()).unwrap();
        let public = "runtime-recording-idle";
        let internal = runtime.compatibility_context.runtime_session_key(public);
        let prefix = format!(
            "__cua_runtime_{}:",
            runtime.compatibility_context.runtime_scope_key()
        );
        runtime
            .invoke(
                "start_session",
                serde_json::json!({"session": public, "capture_scope": "auto"}),
            )
            .await
            .unwrap();
        let output = tempfile::tempdir().unwrap();
        let started = runtime
            .invoke(
                "start_recording",
                serde_json::json!({
                    "session": public,
                    "output_dir": output.path(),
                    "record_video": false,
                }),
            )
            .await
            .unwrap();
        assert_ne!(started.is_error, Some(true));
        assert!(runtime.registry.recording.current_state().enabled);

        let evicted = cua_driver_core::session::evict_idle_with_prefix(Duration::ZERO, &prefix);
        assert!(evicted.contains(&internal));
        for _ in 0..100 {
            if !runtime.registry.recording.current_state().enabled {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !runtime.registry.recording.current_state().enabled,
            "session-end hook must finalize recording after idle eviction"
        );

        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn incomplete_end_keeps_trusted_authorization_live_for_cleanup_retry() {
        let _runtime_test = TEST_RUNTIME_LOCK.lock().unwrap();
        let runtime = DriverRuntime::create(standard_options()).unwrap();
        let public_session = "runtime-end-cleanup-retry";
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_hook = attempts.clone();
        let _hook = cua_driver_core::session::register_scoped_fallible_session_end_hook(
            "runtime-end-cleanup-retry-test",
            move |_| {
                if attempts_for_hook.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err("synthetic first-attempt failure".into())
                } else {
                    Ok(())
                }
            },
        );
        let session = runtime
            .create_trusted_session(DelegatedSessionRequest {
                public_session: public_session.into(),
                transport_session: "runtime-end-cleanup-retry-transport".into(),
                mode: PermissionMode::Standard,
                ttl: Duration::from_secs(60),
                idle_ttl: Duration::from_secs(30),
                capability_manifest: None,
            })
            .unwrap();

        let started = session
            .invoke("start_session", serde_json::json!({}))
            .await
            .unwrap();
        assert_ne!(started.is_error, Some(true));
        let runtime_prefix = format!(
            "__cua_runtime_{}:",
            runtime.compatibility_context.runtime_scope_key()
        );
        let started_sessions = cua_driver_core::session::list_session_snapshots_with_prefix(
            &runtime_prefix,
            Duration::from_secs(300),
        );
        assert_eq!(started_sessions.len(), 1, "{started_sessions:?}");

        let first_end = session
            .invoke("end_session", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            first_end.is_error,
            Some(true),
            "result={first_end:?} started={started_sessions:?} attempts={}",
            attempts.load(Ordering::SeqCst)
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        let retried_end = session
            .invoke("end_session", serde_json::json!({}))
            .await
            .unwrap();
        assert_ne!(retried_end.is_error, Some(true));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);

        let after_end = session
            .invoke("health_report", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            after_end
                .structured_content
                .as_ref()
                .and_then(|value| value.pointer("/refusal/code"))
                .and_then(Value::as_str),
            Some("authorization_revoked")
        );

        runtime.shutdown().await;
    }
}
