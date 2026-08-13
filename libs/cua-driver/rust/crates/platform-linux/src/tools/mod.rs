//! Linux tool implementations.
//!
//! On Linux: delegates to real x11/atspi/input/capture implementations.
//! On other platforms: returns "not implemented" stubs so the crate compiles.

use cua_driver_core::tool::ToolRegistry;

#[cfg(target_os = "linux")]
mod impl_;
#[cfg(target_os = "linux")]
pub(crate) mod page;

#[cfg(not(target_os = "linux"))]
mod stubs;

pub fn build_registry(compat: bool) -> ToolRegistry {
    build_registry_with_provider(compat, None)
}

#[cfg(target_os = "linux")]
fn install_action_lease_hooks() {
    cua_driver_core::action_lease::install_raw_input_ready_hook(
        crate::wayland::ensure_raw_input_ready_for_tool,
    );
}

pub fn build_registry_with_provider(
    compat: bool,
    provider: Option<std::sync::Arc<dyn cua_driver_core::consent::ProtectedConsentProvider>>,
) -> ToolRegistry {
    #[cfg(target_os = "linux")]
    {
        install_action_lease_hooks();
        return impl_::build_registry_with_provider(compat, provider);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = compat;
        let _ = provider;
        stubs::build_registry()
    }
}

// Keep register_all as alias for backwards compat.
pub fn register_all() -> ToolRegistry {
    build_registry(false)
}
