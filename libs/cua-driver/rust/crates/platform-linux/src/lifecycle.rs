//! Shared ownership across embedded SDK runtimes. A retiring SDK must not
//! disconnect another SDK's accessibility/compositor transports.
use anyhow::Result;
use std::sync::Mutex;

static OWNERS: Mutex<usize> = Mutex::new(0);

pub struct RuntimeLease {
    _private: (),
}

impl RuntimeLease {
    pub fn acquire() -> Result<Self> {
        // A retained shutdown owner may still be cancelling an unresponsive
        // transport. New admission must fail boundedly, not wait forever on it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut owners = loop {
            match OWNERS.try_lock() {
                Ok(owners) => break owners,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    anyhow::bail!("Linux platform retirement failed; ownership lock is poisoned")
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    anyhow::ensure!(
                        std::time::Instant::now() < deadline,
                        "Linux platform retirement is still in progress"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        };
        if *owners == 0 {
            crate::wayland::persistent_vptr::restart()?;
            crate::wayland::overlay::restart()?;
            crate::atspi::native::restart()?;
            crate::wayland::shell_helper::restart();
            #[cfg(feature = "portal-input")]
            crate::wayland::libei::restart();
        }
        *owners += 1;
        Ok(Self { _private: () })
    }
}

impl Drop for RuntimeLease {
    fn drop(&mut self) {
        // Serialize the last retirement with the next construction. Cleanup is
        // bounded; no transport/task acquires this SDK ownership mutex.
        let mut owners = OWNERS.lock().unwrap();
        *owners -= 1;
        if *owners != 0 {
            return;
        }
        if let Err(error) = crate::wayland::persistent_vptr::shutdown() {
            tracing::warn!(%error, "Wayland pointer retirement incomplete; ownership stays fenced");
        }
        if let Err(error) = crate::wayland::overlay::shutdown() {
            tracing::warn!(%error, "Wayland overlay retirement incomplete; continuing shutdown");
        }
        if let Err(error) = crate::atspi::native::shutdown() {
            tracing::warn!(%error, "AT-SPI teardown incomplete; continuing shutdown");
        }
        crate::wayland::shell_helper::shutdown();
        #[cfg(feature = "portal-input")]
        crate::wayland::libei::shutdown();
        cua_driver_core::element_token::release_global();
    }
}
