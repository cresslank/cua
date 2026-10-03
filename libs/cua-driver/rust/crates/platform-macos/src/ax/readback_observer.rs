//! Worker-local AXValueChanged observation for asynchronous text insertion.

use super::bindings::*;
use core_foundation::{
    base::{CFEqual, CFRelease, CFType, TCFType},
    runloop::{
        kCFRunLoopDefaultMode, CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRef,
        CFRunLoopRemoveSource, CFRunLoopRunInMode,
    },
    string::{CFString, CFStringRef},
};
use cua_driver_core::ax_readback::ReadbackEvent;
use std::{cell::Cell, ffi::c_void, time::Duration};

/// An independent handle for read-back only. Never change the timeout on a
/// retained snapshot element: other workers may be reading the same handle.
pub(crate) struct ReadbackElement(CFType);

impl ReadbackElement {
    /// `element` must remain valid during construction. The remote token is
    /// copied from the exact element, never guessed or recovered by traversal.
    pub(crate) unsafe fn new(element: AXUIElementRef) -> Option<Self> {
        let token = _AXUIElementRemoteTokenCreate(element);
        if token.is_null() {
            return None;
        }
        let _token = CFType::wrap_under_create_rule(token as _);
        let copy = _AXUIElementCreateWithRemoteToken(token);
        if copy.is_null() {
            return None;
        }
        let owned = CFType::wrap_under_create_rule(copy as _);
        // Refuse an interned handle or a different identity; neither is safe
        // for independently tuning the native messaging timeout.
        if copy == element || CFEqual(copy as _, element as _) == 0 {
            return None;
        }
        Some(Self(owned))
    }

    pub(crate) fn value(&self, remaining: Duration) -> Option<String> {
        let started = std::time::Instant::now();
        let timeout = messaging_timeout(remaining)?;
        let element = self.0.as_CFTypeRef() as AXUIElementRef;
        unsafe {
            if AXUIElementSetMessagingTimeout(element, timeout) != kAXErrorSuccess
                || started.elapsed() >= remaining
            {
                return None;
            }
            copy_string_attr(element, "AXValue")
        }
    }
}

fn messaging_timeout(remaining: Duration) -> Option<f32> {
    // Zero resets AX's default timeout instead of disabling messaging. Round
    // down so conversion to native seconds cannot increase our allowance.
    let seconds = remaining.as_secs_f32().next_down();
    (seconds > 0.0).then_some(seconds)
}

/// Must be created, pumped and dropped on the same blocking worker. The boxed
/// callback context stays valid until the notification and source are removed.
pub(crate) struct ReadbackObserver {
    observer: AXObserverRef,
    element: CFType,
    notification: CFString,
    run_loop: CFRunLoopRef,
    notified: Box<Cell<bool>>,
    registered: bool,
}

extern "C" fn value_changed(
    _observer: AXObserverRef,
    _element: AXUIElementRef,
    _notification: CFStringRef,
    context: *mut c_void,
) {
    // AX invokes this only while this worker pumps its run loop. No native
    // reads or mutations in the callback; the waiter immediately re-reads.
    unsafe { &*context.cast::<Cell<bool>>() }.set(true);
}

impl ReadbackObserver {
    /// The caller keeps `element` valid for construction. This guard retains it
    /// through notification removal, including registration-failure cleanup.
    pub(crate) unsafe fn new(pid: i32, element: AXUIElementRef) -> Option<Self> {
        let mut observer = std::ptr::null_mut();
        let error = AXObserverCreate(pid, value_changed, &mut observer);
        if error != kAXErrorSuccess || observer.is_null() {
            if !observer.is_null() {
                CFRelease(observer as _);
            }
            return None;
        }
        let mut guard = Self {
            observer,
            element: CFType::wrap_under_get_rule(element as _),
            // kAXValueChangedNotification's string value.
            notification: CFString::new("AXValueChanged"),
            run_loop: CFRunLoopGetCurrent(),
            notified: Box::new(Cell::new(false)),
            registered: false,
        };
        if AXObserverAddNotification(
            observer,
            element,
            guard.notification.as_concrete_TypeRef(),
            (&*guard.notified as *const Cell<bool>).cast_mut().cast(),
        ) != kAXErrorSuccess
        {
            return None;
        }
        guard.registered = true;
        CFRunLoopAddSource(
            guard.run_loop,
            AXObserverGetRunLoopSource(observer),
            kCFRunLoopDefaultMode,
        );
        Some(guard)
    }

    pub(crate) fn wait(&self, slice: Duration) -> ReadbackEvent {
        if self.notified.replace(false) {
            return ReadbackEvent::Notified;
        }
        unsafe {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, slice.as_secs_f64(), 1);
        }
        if self.notified.replace(false) {
            ReadbackEvent::Notified
        } else {
            ReadbackEvent::Tick
        }
    }
}

impl Drop for ReadbackObserver {
    fn drop(&mut self) {
        unsafe {
            if self.registered {
                CFRunLoopRemoveSource(
                    self.run_loop,
                    AXObserverGetRunLoopSource(self.observer),
                    kCFRunLoopDefaultMode,
                );
                AXObserverRemoveNotification(
                    self.observer,
                    self.element.as_CFTypeRef() as AXUIElementRef,
                    self.notification.as_concrete_TypeRef(),
                );
            }
            CFRelease(self.observer as _);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readback_messaging_timeout_never_resets_default_or_exceeds_budget() {
        assert_eq!(messaging_timeout(Duration::ZERO), None);
        for budget in [
            Duration::from_nanos(1),
            Duration::from_millis(45),
            Duration::from_millis(250),
        ] {
            let seconds = messaging_timeout(budget).unwrap();
            assert!(seconds > 0.0);
            assert!(f64::from(seconds) <= budget.as_secs_f64());
        }
    }
}
