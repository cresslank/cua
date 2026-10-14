//! Native-call seam: runs the real reveal worker with fake AX objects, never a desktop.
#![allow(non_snake_case, dead_code)]
extern crate self as core_foundation;
use std::cell::RefCell;
pub mod base {
    pub type CFTypeRef = *const std::ffi::c_void;
    pub unsafe fn CFRelease(p: CFTypeRef) {
        super::SCENE.with(|s| {
            let mut s = s.borrow_mut();
            let refs = &mut s.refs[p as usize - 1];
            assert!(*refs > 0, "releasing an unowned AX object");
            *refs -= 1;
        });
    }
    pub unsafe fn CFRetain(p: CFTypeRef) -> CFTypeRef {
        super::SCENE.with(|s| s.borrow_mut().refs[p as usize - 1] += 1);
        p
    }
}
#[derive(Clone)]
struct Scene {
    owners: [Option<u32>; 5],
    clamped: bool,
    mutations: usize,
    reparent: Option<usize>,
    inner: bool,
    inner_moves: bool,
    recipients: Vec<usize>,
    refs: [usize; 5],
}
thread_local! { static SCENE: RefCell<Scene> = const { RefCell::new(Scene { owners: [Some(7); 5], clamped: false, mutations: 0, reparent: None, inner: false, inner_moves: true, recipients: Vec::new(), refs: [0; 5] }) }; }
mod bindings {
    pub type AXUIElementRef = *const std::ffi::c_void;
    pub unsafe fn element_at_screen_position(_: i32, _: f64, _: f64) -> Option<AXUIElementRef> {
        super::base::CFRetain(1 as _);
        Some(1 as _)
    }
    pub unsafe fn copy_string_attr(p: AXUIElementRef, _: &str) -> Option<String> {
        if super::SCENE.with(|s| s.borrow().inner) {
            return Some(
                match p as usize {
                    2 => "AXGroup",
                    4 => "AXWebArea",
                    _ => "AXStaticText",
                }
                .into(),
            );
        }
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
        let parent = match p as usize {
            1 => Some(2 as _),
            2 if super::SCENE.with(|s| s.borrow().inner) => Some(4 as _),
            _ => None,
        };
        if let Some(parent) = parent {
            super::base::CFRetain(parent);
        }
        parent
    }
    pub unsafe fn copy_children(p: AXUIElementRef) -> Vec<AXUIElementRef> {
        if p as usize == 2 {
            super::SCENE.with(|s| {
                let mut s = s.borrow_mut();
                if let Some(index) = s.reparent {
                    s.owners[index] = Some(8);
                }
            });
            super::base::CFRetain(3 as _);
            vec![3 as _]
        } else if p as usize == 4 && super::SCENE.with(|s| s.borrow().inner) {
            super::base::CFRetain(5 as _);
            vec![5 as _]
        } else {
            vec![]
        }
    }
    pub unsafe fn element_screen_rect(p: AXUIElementRef) -> Option<[f64; 4]> {
        super::SCENE.with(|s| {
            let s = s.borrow();
            if s.inner {
                return Some(match p as usize {
                    2 | 4 => [0., 0., 100., 100.],
                    3 if s.inner_moves && s.recipients.contains(&3) => [0., 40., 50., 20.],
                    5 if s.recipients.contains(&5) => [0., 40., 50., 20.],
                    _ => [0., 120., 50., 20.],
                });
            }
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
    pub unsafe fn perform_action(p: AXUIElementRef, action: &str) -> bool {
        assert_eq!(action, "AXScrollToVisible");
        super::SCENE.with(|s| {
            let mut s = s.borrow_mut();
            s.mutations += 1;
            s.recipients.push(p as usize);
        });
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
            owners: [owners[0], owners[1], owners[2], Some(7), Some(7)],
            clamped,
            mutations: 0,
            reparent,
            inner: false,
            inner_moves: true,
            recipients: Vec::new(),
            refs: [0; 5],
        }
    });
    let outcome = unsafe {
        reveal_scroll::reveal_scroll(42, 7, (50., 50.), Some([0., 0., 100., 100.]), "down", 60.)
    };
    SCENE.with(|s| assert_eq!(s.borrow().refs, [0; 5], "all retained AX objects released"));
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

fn inner_case(inner_moves: bool, wrong_owner: Option<usize>) -> Vec<usize> {
    SCENE.with(|s| {
        let mut scene = s.borrow_mut();
        *scene = Scene {
            owners: [Some(7); 5],
            clamped: false,
            mutations: 0,
            reparent: None,
            inner: true,
            inner_moves,
            recipients: Vec::new(),
            refs: [0; 5],
        };
        if let Some(index) = wrong_owner {
            scene.owners[index] = Some(8);
        }
    });
    unsafe {
        reveal_scroll::reveal_scroll(42, 7, (50., 50.), Some([0., 0., 100., 100.]), "down", 60.);
    }
    SCENE.with(|s| {
        assert_eq!(s.borrow().refs, [0; 5], "all candidates and hit released");
        s.borrow().recipients.clone()
    })
}

#[test]
fn electron_inner_group_wins_before_outer_web_area() {
    assert_eq!(inner_case(true, None), vec![3]);
}

#[test]
fn electron_outer_fallback_revalidates_its_own_recipient() {
    assert_eq!(inner_case(false, None), vec![3, 5]);
    assert_eq!(inner_case(false, Some(3)), vec![3]);
    assert_eq!(inner_case(false, Some(4)), vec![3]);
    assert_eq!(inner_case(false, Some(0)), Vec::<usize>::new());
}
