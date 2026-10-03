//! Platform hook at the admitted mutation boundary. Uninstalled on platforms
//! without deferred focus protection; it does not change authorization.

use std::sync::OnceLock;

static BEFORE_MUTATION: OnceLock<fn()> = OnceLock::new();

/// Install the process-wide platform callback. Repeated installation is a no-op.
pub fn install_before_mutation_hook(hook: fn()) {
    let _ = BEFORE_MUTATION.set(hook);
}

pub(crate) fn notify_before_mutation() {
    if let Some(hook) = BEFORE_MUTATION.get() {
        hook();
    }
}
