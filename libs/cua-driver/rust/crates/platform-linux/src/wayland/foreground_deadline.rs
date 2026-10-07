//! One input deadline for the whole GNOME foreground transaction, not each
//! move/click/scroll command. The helper can restore independently of the caller.
use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::{Duration, Instant};

// Keep in sync with wayland-helper/winrects@cua/extension.js FOREGROUND_TIMEOUT_MS.
// Stop writing five seconds before the helper can independently restore focus.
const HELPER_FOREGROUND_TIMEOUT: Duration = Duration::from_secs(30);
const FOREGROUND_SAFETY_MARGIN: Duration = Duration::from_secs(5);

thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[derive(Debug)]
pub(super) struct Scope {
    previous: Option<Instant>,
    // Restoring thread-local state must occur on the thread that installed it.
    _not_send: PhantomData<Rc<()>>,
}

impl Scope {
    pub(super) fn begin(start: Instant) -> Self {
        Self::until(start + HELPER_FOREGROUND_TIMEOUT - FOREGROUND_SAFETY_MARGIN)
    }

    pub(super) fn until(deadline: Instant) -> Self {
        let previous = DEADLINE.replace(Some(cap(deadline)));
        Self {
            previous,
            _not_send: PhantomData,
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        DEADLINE.set(self.previous);
    }
}

pub(super) fn cap(command_deadline: Instant) -> Instant {
    DEADLINE.get().map_or(command_deadline, |transaction| {
        command_deadline.min(transaction)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_foreground_timeout_matches_rust_budget() {
        let source = include_str!("../../../../../wayland-helper/winrects@cua/extension.js");
        let value = source
            .split("const FOREGROUND_TIMEOUT_MS = ")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .trim()
            .replace('_', "");
        assert_eq!(
            value.parse::<u64>().unwrap(),
            HELPER_FOREGROUND_TIMEOUT.as_millis() as u64
        );
        assert_eq!(
            HELPER_FOREGROUND_TIMEOUT - FOREGROUND_SAFETY_MARGIN,
            Duration::from_secs(25)
        );
    }

    #[test]
    fn foreground_scope_caps_nested_commands_and_restores_on_unwind() {
        let now = Instant::now();
        let ordinary = now + Duration::from_secs(20);
        assert_eq!(cap(ordinary), ordinary);
        let scope = Scope::begin(now);
        assert_eq!(cap(ordinary), ordinary);
        assert_eq!(
            cap(now + Duration::from_secs(40)),
            now + Duration::from_secs(25)
        );
        let _ = std::panic::catch_unwind(|| {
            let _nested = Scope::until(now + Duration::from_secs(1));
            assert_eq!(cap(ordinary), now + Duration::from_secs(1));
            let _later = Scope::until(now + Duration::from_secs(60));
            assert_eq!(cap(ordinary), now + Duration::from_secs(1));
            panic!("exercise RAII restoration");
        });
        assert_eq!(cap(ordinary), ordinary);
        drop(scope);
        assert_eq!(
            cap(now + Duration::from_secs(40)),
            now + Duration::from_secs(40)
        );
    }
}
