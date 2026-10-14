//! Native-call seam: runs the real reveal worker with fake AX objects, never a desktop.
#![allow(non_snake_case, dead_code)]
extern crate self as core_foundation;
use std::cell::RefCell;
pub mod base {
    pub type CFTypeRef = *const std::ffi::c_void;
    pub unsafe fn CFRelease(_: CFTypeRef) {}
    pub unsafe fn CFRetain(p: CFTypeRef) -> CFTypeRef {
        p
    }
}
#[derive(Clone)]
struct Scene {
    owners: [Option<u32>; 3],
    clamped: bool,
    mutations: usize,
    reparent: Option<usize>,
}
thread_local! { static SCENE: RefCell<Scene> = const { RefCell::new(Scene { owners: [Some(7); 3], clamped: false, mutations: 0, reparent: None }) }; }
mod bindings {
    pub type AXUIElementRef = *const std::ffi::c_void;
    pub unsafe fn element_at_screen_position(_: i32, _: f64, _: f64) -> Option<AXUIElementRef> {
        Some(1 as _)
    }
    pub unsafe fn copy_string_attr(p: AXUIElementRef, _: &str) -> Option<String> {
        Some(
            if p as usize == 2 {
                "AXScrollArea"
            } else {
                "AXStaticText"
            }
            .into(),
        )
    }
    pub unsafe fn copy_element_attr(p: AXUIElementRef, _: &str) -> Option<AXUIElementRef> {
        (p as usize == 1).then_some(2 as _)
    }
    pub unsafe fn copy_children(p: AXUIElementRef) -> Vec<AXUIElementRef> {
        if p as usize == 2 {
            super::SCENE.with(|s| {
                let mut s = s.borrow_mut();
                if let Some(index) = s.reparent {
                    s.owners[index] = Some(8);
                }
            });
            vec![3 as _]
        } else {
            vec![]
        }
    }
    pub unsafe fn element_screen_rect(p: AXUIElementRef) -> Option<[f64; 4]> {
        super::SCENE.with(|s| {
            let s = s.borrow();
            Some(if p as usize == 2 {
                [0., 0., 100., 100.]
            } else if s.mutations > 0 {
                [0., 40., 50., 20.]
            } else if s.clamped {
                [0., 99., 50., 1.]
            } else {
                [0., 120., 50., 20.]
            })
        })
    }
    pub unsafe fn perform_action(_: AXUIElementRef, action: &str) -> bool {
        assert_eq!(action, "AXScrollToVisible");
        super::SCENE.with(|s| s.borrow_mut().mutations += 1);
        true
    }
}
mod exact_target {
    pub unsafe fn element_window_id(p: super::bindings::AXUIElementRef) -> Option<u32> {
        super::SCENE.with(|s| s.borrow().owners[p as usize - 1])
    }
}
#[path = "../src/ax/reveal_scroll.rs"]
mod reveal_scroll;
fn run(owners: [Option<u32>; 3], clamped: bool) -> (bool, usize) {
    run_reparenting(owners, clamped, None)
}
fn run_reparenting(
    owners: [Option<u32>; 3],
    clamped: bool,
    reparent: Option<usize>,
) -> (bool, usize) {
    SCENE.with(|s| {
        *s.borrow_mut() = Scene {
            owners,
            clamped,
            mutations: 0,
            reparent,
        }
    });
    let outcome = unsafe {
        reveal_scroll::reveal_scroll(42, 7, (50., 50.), Some([0., 0., 100., 100.]), "down", 60.)
    };
    (outcome.is_some(), SCENE.with(|s| s.borrow().mutations))
}
#[test]
fn same_pid_covering_hit_is_refused() {
    assert_eq!(run([Some(8), Some(8), Some(8)], false), (false, 0));
}
#[test]
fn acquired_container_and_action_target_must_belong_to_exact_window() {
    for clamped in [false, true] {
        assert_eq!(run([Some(7), Some(8), Some(7)], clamped), (false, 0));
        assert_eq!(run([Some(7), Some(7), Some(8)], clamped), (false, 0));
    }
}
#[test]
fn unproven_ancestry_is_refused() {
    assert_eq!(run([None; 3], false), (false, 0));
}
#[test]
fn exact_target_can_reveal_in_both_branches() {
    for clamped in [false, true] {
        assert_eq!(run([Some(7); 3], clamped), (true, 1));
    }
}

#[test]
fn authority_is_rechecked_after_traversal_in_both_mutation_branches() {
    for clamped in [false, true] {
        for index in 0..3 {
            assert_eq!(
                run_reparenting([Some(7); 3], clamped, Some(index)),
                (false, 0)
            );
        }
    }
}
