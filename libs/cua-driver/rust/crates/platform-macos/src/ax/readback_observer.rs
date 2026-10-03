//! Worker-local AXValueChanged observation for asynchronous text insertion.

use super::bindings::*;
use core_foundation::{
    base::{CFRelease, CFType, TCFType},
    runloop::{
        kCFRunLoopDefaultMode, CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRef,
        CFRunLoopRemoveSource, CFRunLoopRunInMode,
    },
    string::{CFString, CFStringRef},
};
use cua_driver_core::ax_readback::ReadbackEvent;
use std::{cell::Cell, ffi::c_void, time::Duration};

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
