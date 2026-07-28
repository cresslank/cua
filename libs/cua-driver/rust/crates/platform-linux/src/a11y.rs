//! Advertise Linux accessibility status without implicitly claiming a screen
//! reader.
//!
//! Chromium — and therefore every Electron, CEF, and Chrome-based app — ships
//! its accessibility tree disabled and only builds it once it believes an
//! assistive technology is listening. It decides that by watching the
//! freedesktop accessibility status published on the session bus: the
//! `org.a11y.Bus` service exposes an `org.a11y.Status` interface whose
//! `ScreenReaderEnabled` property is the signal. Until that property is true a
//! Chromium window registers either nothing or an empty application on the
//! AT-SPI registry, so [`crate::atspi`] walks an empty tree and
//! `get_window_state` reports the app as having no elements — it is invisible
//! to the driver. Electron apps shipped as AppImages behave identically; they
//! embed the same Chromium.
//!
//! A real screen reader turns the Chromium signal on. Doing that ourselves is
//! unsafe on GNOME: its settings daemon treats the signal as a user request and
//! launches Orca. Daemon launch environments can omit desktop identity, so Cua
//! defaults every desktop to only the generic `IsEnabled` signal. A caller that
//! deliberately needs the global Chromium signal can opt in explicitly with
//! `CUA_DRIVER_RS_A11Y_ADVERTISE_MODE=all`.
//!
//! Metadata-only runtimes do not touch session accessibility state. Action-
//! capable runtimes require this bounded preparation to produce a determinate
//! result before they become ready.

use std::sync::{Condvar, Mutex, OnceLock};

use anyhow::{anyhow, Context};
use atspi::zbus;

/// Well-known session-bus name of the freedesktop accessibility-bus launcher,
/// which also carries the session's accessibility status.
const ACCESSIBILITY_BUS_SERVICE: &str = "org.a11y.Bus";
/// Object on [`ACCESSIBILITY_BUS_SERVICE`] exposing `org.a11y.Status`.
const ACCESSIBILITY_BUS_OBJECT: &str = "/org/a11y/bus";
/// Interface holding the session's accessibility-enabled / screen-reader flags.
const ACCESSIBILITY_STATUS_INTERFACE: &str = "org.a11y.Status";
const ACCESSIBILITY_BUS_INTERFACE: &str = "org.a11y.Bus";
/// Property Chromium watches to decide whether to build its AT-SPI tree.
const SCREEN_READER_ENABLED_PROPERTY: &str = "ScreenReaderEnabled";
/// Companion property GTK/Qt watch to load their AT-SPI bridges.
const ACCESSIBILITY_IS_ENABLED_PROPERTY: &str = "IsEnabled";
const ACCESSIBILITY_ADVERTISE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdvertiseMode {
    All,
    IsEnabledOnly,
    None,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TrustedAccessibilityBus {
    address: String,
    private_worker: bool,
}

static TRUSTED_ACCESSIBILITY_BUS: OnceLock<TrustedAccessibilityBus> = OnceLock::new();

/// Bind a private SDK worker to the exact post-policy AT-SPI route attested by
/// its parent. Ordinary/direct runtimes cannot reach this API through MCP.
pub fn initialize_private_accessibility_bus(address: &str) -> anyhow::Result<()> {
    let address = address.trim();
    if address.is_empty() {
        anyhow::bail!("trusted private AT-SPI bus address is empty");
    }
    let _: zbus::Address = address
        .parse()
        .context("validating the trusted private AT-SPI bus address")?;
    let expected = TrustedAccessibilityBus {
        address: address.to_owned(),
        private_worker: true,
    };
    if let Some(existing) = TRUSTED_ACCESSIBILITY_BUS.get() {
        if existing != &expected {
            anyhow::bail!("accessibility bus identity changed during process lifetime");
        }
        return Ok(());
    }
    TRUSTED_ACCESSIBILITY_BUS
        .set(expected)
        .map_err(|_| anyhow!("private accessibility bus initialization raced"))
}

/// Return the process-attested AT-SPI route after desktop preparation.
///
/// The SDK uses this narrow read-only seam to pass the exact prepared route to
/// an env-cleared private worker without inheriting an ambient caller value.
pub fn trusted_accessibility_bus_address() -> anyhow::Result<String> {
    TRUSTED_ACCESSIBILITY_BUS
        .get()
        .map(|bus| bus.address.clone())
        .ok_or_else(|| anyhow!("trusted accessibility bus was not initialized"))
}

/// Advertise generic accessibility to the session exactly once per daemon
/// process so GTK and Qt expose their trees to [`crate::atspi`]. The explicit
/// `all` mode also advertises a screen reader for Chromium/Electron. Idempotent
/// and fail-closed: action-capable runtimes reuse the first determinate result.
/// The host-wide preparation lock is moved into the bounded worker, so a timed-
/// out D-Bus mutation remains serialized until that worker actually exits.
pub fn ensure_accessibility_enabled(preparation_lock: std::fs::File) -> Result<(), String> {
    ensure_accessibility_enabled_with_session_bus(
        preparation_lock,
        None,
        ACCESSIBILITY_ADVERTISE_TIMEOUT,
    )
}

/// Prepare accessibility using an explicit session-bus address and caller
/// deadline. This path is safe for SDK constructors because it never mutates
/// the embedding process environment.
pub fn ensure_accessibility_enabled_with_session_bus(
    preparation_lock: std::fs::File,
    session_bus_address: Option<String>,
    timeout: std::time::Duration,
) -> Result<(), String> {
    #[derive(Default)]
    enum AdvertisementState {
        #[default]
        Idle,
        Running,
        Complete(Result<(), String>),
    }

    static ADVERTISED: OnceLock<(Mutex<AdvertisementState>, Condvar)> = OnceLock::new();
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "accessibility preparation timeout exceeds the platform clock".to_owned())?;
    let (state, ready) =
        ADVERTISED.get_or_init(|| (Mutex::new(AdvertisementState::Idle), Condvar::new()));
    let mut state_guard = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        match &*state_guard {
            AdvertisementState::Complete(result) => return result.clone(),
            AdvertisementState::Running => {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err("timed out waiting for concurrent accessibility preparation".into());
                }
                let (next_guard, wait) = ready
                    .wait_timeout(state_guard, remaining)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state_guard = next_guard;
                if wait.timed_out() && matches!(&*state_guard, AdvertisementState::Running) {
                    return Err("timed out waiting for concurrent accessibility preparation".into());
                }
            }
            AdvertisementState::Idle => {
                *state_guard = AdvertisementState::Running;
                drop(state_guard);
                break;
            }
        }
    }

    let operation = if TRUSTED_ACCESSIBILITY_BUS
        .get()
        .is_some_and(|bus| bus.private_worker)
    {
        drop(preparation_lock);
        Ok(())
    } else {
        let mode = advertise_mode_from(
            std::env::var_os("CUA_DRIVER_RS_DISABLE_A11Y_ADVERTISE").is_some(),
            std::env::var("CUA_DRIVER_RS_A11Y_ADVERTISE_MODE")
                .ok()
                .as_deref(),
        );
        advertise_accessibility_to_session(
            mode,
            preparation_lock,
            session_bus_address,
            deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(ACCESSIBILITY_ADVERTISE_TIMEOUT),
        )
        .map_err(|error| {
            format!("could not deterministically prepare session accessibility: {error:#}")
        })
        .and_then(|address| {
            let expected = TrustedAccessibilityBus {
                address,
                private_worker: false,
            };
            if let Some(existing) = TRUSTED_ACCESSIBILITY_BUS.get() {
                if existing != &expected {
                    return Err("accessibility bus identity changed during process lifetime".into());
                }
            } else {
                TRUSTED_ACCESSIBILITY_BUS
                    .set(expected)
                    .map_err(|_| "session accessibility bus initialization raced".to_owned())?;
            }
            Ok(())
        })
    };
    let mut state_guard = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *state_guard = AdvertisementState::Complete(operation.clone());
    ready.notify_all();
    operation
}

fn advertise_accessibility_to_session(
    mode: AdvertiseMode,
    preparation_lock: std::fs::File,
    session_bus_address: Option<String>,
    timeout: std::time::Duration,
) -> anyhow::Result<String> {
    // The daemon's tokio runtime is already driving this thread when the tool
    // registry is built, and `block_on` panics if called from within a runtime.
    // Run the one-shot bus work on a dedicated OS thread that owns a small
    // runtime of its own. The caller has a hard deadline; a wedged D-Bus call
    // cannot hold the cross-process desktop-preparation lock indefinitely.
    run_bounded_thread("cua-a11y-advertise", timeout, move || {
        let _preparation_lock = preparation_lock;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(advertise_accessibility(
            mode,
            session_bus_address.as_deref(),
        ))
    })
}

fn run_bounded_thread<T: Send + 'static>(
    name: &str,
    timeout: std::time::Duration,
    operation: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| anyhow!("accessibility startup timeout exceeds the platform clock range"))?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation))
                .map_err(|_| anyhow!("bounded accessibility operation panicked"))
                .and_then(|result| result);
            let _ = sender.send(result);
        })
        .context("spawning the accessibility-advertise thread")?;
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err(anyhow!(
            "accessibility advertisement exceeded its {:?} startup deadline",
            timeout
        ));
    }
    receiver
        .recv_timeout(remaining)
        .map_err(|error| match error {
            std::sync::mpsc::RecvTimeoutError::Timeout => anyhow!(
                "accessibility advertisement exceeded its {:?} startup deadline",
                timeout
            ),
            std::sync::mpsc::RecvTimeoutError::Disconnected => {
                anyhow!("accessibility-advertise thread exited without a result")
            }
        })?
}

async fn advertise_accessibility(
    mode: AdvertiseMode,
    session_bus_address: Option<&str>,
) -> anyhow::Result<String> {
    let session_bus = match session_bus_address {
        Some(address) => zbus::connection::Builder::address(address)?.build().await?,
        None => zbus::Connection::session().await?,
    };
    let bus = zbus::Proxy::new(
        &session_bus,
        ACCESSIBILITY_BUS_SERVICE,
        ACCESSIBILITY_BUS_OBJECT,
        ACCESSIBILITY_BUS_INTERFACE,
    )
    .await?;
    let address: String = bus.call("GetAddress", &()).await?;
    let _: zbus::Address = address
        .parse()
        .context("validating the session AT-SPI bus address")?;

    if mode == AdvertiseMode::None {
        tracing::debug!("accessibility advertisement disabled; leaving session status untouched");
        return Ok(address);
    }

    let status = zbus::Proxy::new(
        &session_bus,
        ACCESSIBILITY_BUS_SERVICE,
        ACCESSIBILITY_BUS_OBJECT,
        ACCESSIBILITY_STATUS_INTERFACE,
    )
    .await?;

    // Don't clobber a screen reader the user is already running: only write when
    // a flag is currently false, so an active Orca session stays authoritative
    // and we avoid emitting a redundant PropertiesChanged.
    if mode == AdvertiseMode::All && !is_flag_set(&status, SCREEN_READER_ENABLED_PROPERTY).await {
        status
            .set_property(SCREEN_READER_ENABLED_PROPERTY, true)
            .await?;
    }
    if !is_flag_set(&status, ACCESSIBILITY_IS_ENABLED_PROPERTY).await {
        status
            .set_property(ACCESSIBILITY_IS_ENABLED_PROPERTY, true)
            .await?;
    }
    Ok(address)
}

fn advertise_mode_from(disabled: bool, configured: Option<&str>) -> AdvertiseMode {
    if disabled {
        return AdvertiseMode::None;
    }
    match configured
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("all") => AdvertiseMode::All,
        Some("is_enabled_only") => AdvertiseMode::IsEnabledOnly,
        Some("none") => AdvertiseMode::None,
        Some(other) => {
            tracing::warn!(
                mode = other,
                "unknown CUA_DRIVER_RS_A11Y_ADVERTISE_MODE; using safe default"
            );
            AdvertiseMode::IsEnabledOnly
        }
        None => AdvertiseMode::IsEnabledOnly,
    }
}

/// Read a boolean status property, treating an unreadable property as unset so
/// the caller falls through to writing it.
async fn is_flag_set(status: &zbus::Proxy<'_>, property: &str) -> bool {
    status.get_property::<bool>(property).await.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{advertise_mode_from, run_bounded_thread, AdvertiseMode};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn default_does_not_claim_a_screen_reader() {
        assert_eq!(
            advertise_mode_from(false, None),
            AdvertiseMode::IsEnabledOnly
        );
    }

    #[test]
    fn screen_reader_claim_requires_explicit_all_mode() {
        assert_eq!(advertise_mode_from(false, Some("all")), AdvertiseMode::All);
    }

    #[test]
    fn explicit_safe_modes_override_the_default() {
        assert_eq!(
            advertise_mode_from(false, Some("is_enabled_only")),
            AdvertiseMode::IsEnabledOnly
        );
        assert_eq!(
            advertise_mode_from(false, Some("none")),
            AdvertiseMode::None
        );
    }

    #[test]
    fn unknown_mode_fails_closed() {
        assert_eq!(
            advertise_mode_from(false, Some("unexpected")),
            AdvertiseMode::IsEnabledOnly
        );
    }

    #[test]
    fn legacy_disable_wins_over_explicit_mode() {
        assert_eq!(advertise_mode_from(true, Some("all")), AdvertiseMode::None);
    }

    #[test]
    fn bounded_advertisement_does_not_wait_for_a_wedged_bus_call() {
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(Arc::clone(&dropped));
        let started = std::time::Instant::now();
        let result = run_bounded_thread(
            "cua-a11y-timeout-test",
            std::time::Duration::from_millis(20),
            move || {
                let _guard = guard;
                std::thread::sleep(std::time::Duration::from_millis(200));
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(started.elapsed() < std::time::Duration::from_millis(150));
        assert!(!dropped.load(Ordering::SeqCst));
        std::thread::sleep(std::time::Duration::from_millis(220));
        assert!(dropped.load(Ordering::SeqCst));
    }
}
