//! Regression guard for the UIA element-cache use-after-free fixed in
//! d95b89a1 (retain-under-lock / `RetainedElement`).
//!
//! THE RACE (what these tests model):
//!   Two concurrent sessions drive the same (pid, hwnd).
//!   - Session A: `get_window_state` → `ElementCache::update` → `core.insert`
//!     replaces the snapshot → old `CachedSnapshot::drop` → COM `Release` on
//!     every cached `IUIAutomationElement`.
//!   - Session B: `click` / `type_text` / `set_value` looked the element up out
//!     of the cache and is mid-action, dereferencing the same COM pointer.
//!   A bare pointer copy with no AddRef could be `Release`d to zero by A
//!   between the lookup and the dereference → use-after-free → daemon crash.
//!   This is the Windows analogue of the macOS #1796
//!   `AXUIElementCopyActionNames` fault.
//!
//! HOW WE REPRODUCE IT DETERMINISTICALLY WITHOUT A GUI:
//!   The cache only ever calls IUnknown vtable slots on the cached pointers
//!   (`AddRef` via `clone()`, `Release` via `drop`). So we feed it a real,
//!   independently-refcounted COM-ABI object of our own (`FakeObj`) with a
//!   hand-rolled IUnknown vtable. The real TokenRegistry payload, mutation
//!   admission, and COM retain/release implementation all run against it.
//!   Native-lifetime fixtures clone only the COM reference and release their
//!   permit before replacement; the cancellation fixture separately asserts
//!   that production-style admission blocks replacement until mutation ends.
//!   An AddRef/Release seen while the refcount is
//!   already <= 0 bumps a shared `uaf_hits` counter (a use-after-free that a
//!   sanitizer would flag); memory stays mapped so the assertion is
//!   deterministic and the test process never actually faults.
//!
//! `run_forced_interleave` pins the dangerous ordering (B looks up → A
//! replaces + releases → B dereferences) with channels instead of relying on
//! luck.

use super::{CachedSnapshot, ElementCache, RetainedElement, SnapshotKind};
use cua_driver_core::element_token::{token_for, ResolvedElement};
use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::UI::Accessibility::IUIAutomationElement;

#[repr(C)]
struct FakeVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct FakeObj {
    /// MUST be first: IUnknown ABI. The cache reads `*ptr` as the vtable.
    vtbl: *const FakeVtbl,
    refcount: AtomicIsize,
    /// Shared counter: bumped whenever AddRef/Release touches us while our
    /// refcount is already <= 0 — i.e. a use-after-free.
    uaf_hits: *const AtomicUsize,
    poison_on_zero: bool,
}

unsafe extern "system" fn fake_query_interface(
    _this: *mut c_void,
    _riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if !ppv.is_null() {
        *ppv = std::ptr::null_mut();
    }
    HRESULT(0x8000_4002u32 as i32) // E_NOINTERFACE
}

unsafe extern "system" fn fake_add_ref(this: *mut c_void) -> u32 {
    let obj = this as *mut FakeObj;
    let prev = (*obj).refcount.load(Ordering::SeqCst);
    if prev <= 0 {
        (*(*obj).uaf_hits).fetch_add(1, Ordering::SeqCst);
    }
    (*obj).refcount.fetch_add(1, Ordering::SeqCst);
    (prev + 1).max(1) as u32
}

unsafe extern "system" fn fake_release(this: *mut c_void) -> u32 {
    let obj = this as *mut FakeObj;
    let prev = (*obj).refcount.fetch_sub(1, Ordering::SeqCst);
    if prev <= 0 {
        (*(*obj).uaf_hits).fetch_add(1, Ordering::SeqCst);
    }
    let now = prev - 1;
    if now == 0 && (*obj).poison_on_zero {
        let vtbl_slot = std::ptr::addr_of_mut!((*obj).vtbl) as *mut usize;
        vtbl_slot.write(0xD15E_A5ED_DEAD_BEEFusize);
    }
    now.max(0) as u32
}

static VTBL: FakeVtbl = FakeVtbl {
    query_interface: fake_query_interface,
    add_ref: fake_add_ref,
    release: fake_release,
};

/// Allocate a fake COM object with refcount 1 (the single ref the cache "owns",
/// matching the walker's clone()+forget() hand-off). Leaked on purpose: the
/// tests must keep the memory mapped to observe post-free touches.
fn make_fake(uaf_hits: &'static AtomicUsize, poison_on_zero: bool) -> usize {
    let obj = Box::new(FakeObj {
        vtbl: &VTBL as *const FakeVtbl,
        refcount: AtomicIsize::new(1),
        uaf_hits: uaf_hits as *const AtomicUsize,
        poison_on_zero,
    });
    Box::into_raw(obj) as usize
}

fn snapshot_with(ptrs: Vec<usize>) -> CachedSnapshot {
    CachedSnapshot {
        _descriptive: Vec::new(),
        elements: ptrs
            .into_iter()
            .map(|ptr| RetainedElement {
                ptr,
                kind: SnapshotKind::Uia,
                center: (0, 0),
                rect: None,
                msaa_role: None,
            })
            .collect(),
    }
}

/// Deliberately release the lookup retain before returning a bare pointer.
/// Models the pre-fix accessor so replacement can expose the lifetime bug.
fn old_get_element_ptr(cache: &ElementCache, snapshot: u32, idx: usize) -> Option<usize> {
    acquire(cache, snapshot, idx).map(|guard| guard.as_ptr())
}

fn acquire(cache: &ElementCache, snapshot: u32, idx: usize) -> Option<RetainedElement> {
    match cache
        .resolve_element_args(
            pid(),
            None,
            Some(&token_for(snapshot, idx)),
            None,
            Some(HWND),
            "test",
        )
        .ok()?
    {
        ResolvedElement::Element { element, .. } => Some((*element).clone()),
        ResolvedElement::None => None,
    }
}

/// The mid-action dereference a tool performs: reconstruct the interface from
/// the raw pointer and touch its vtable (AddRef then Release). On a live object
/// this is harmless; on a freed one it is the use-after-free.
unsafe fn touch_vtable(ptr: usize) {
    let elem: IUIAutomationElement = IUIAutomationElement::from_raw(ptr as *mut c_void);
    let dup = elem.clone(); // AddRef — reads the vtable
    std::mem::forget(elem); // don't Release the borrowed cache ref
    drop(dup); // Release — reads the vtable again
}

fn pid() -> i32 {
    i32::try_from(std::process::id()).expect("test process ID fits registry")
}

// These tests intentionally use the same lane to reproduce replacement. Keep
// distinct tests from superseding one another in the global registry.
use super::CACHE_TEST_LOCK as TEST_LOCK;
const HWND: u64 = 0x1234;

/// Forced interleave: B looks up the element, THEN A replaces+releases the
/// snapshot, THEN B dereferences through its retained guard. Returns the
/// number of use-after-free touches observed.
fn run_forced_interleave(use_retained: bool, poison_on_zero: bool) -> usize {
    let uaf_hits: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
    let cache = Arc::new(ElementCache::new());

    let ptr = make_fake(uaf_hits, poison_on_zero);
    let snapshot = cache.publish(pid(), HWND, snapshot_with(vec![ptr]));

    let (b_looked_up_tx, b_looked_up_rx) = mpsc::channel::<()>();
    let (a_replaced_tx, a_replaced_rx) = mpsc::channel::<()>();

    let cache_b = cache.clone();
    let worker_b = thread::spawn(move || {
        if use_retained {
            // FIXED path: AddRef under the lock; guard pins the object alive.
            let guard = acquire(&cache_b, snapshot, 0).expect("element present");
            b_looked_up_tx.send(()).unwrap();
            a_replaced_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap(); // A has now replaced + released
            unsafe { touch_vtable(guard.as_ptr()) }; // safe: guard holds +1
            drop(guard);
        } else {
            // PRE-FIX path: bare pointer, no AddRef.
            let raw = old_get_element_ptr(&cache_b, snapshot, 0).expect("element present");
            b_looked_up_tx.send(()).unwrap();
            a_replaced_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap(); // A has now replaced + released → freed
            unsafe { touch_vtable(raw) }; // USE-AFTER-FREE
        }
    });

    let cache_a = cache.clone();
    let worker_a = thread::spawn(move || {
        b_looked_up_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        // get_window_state on the same (pid, hwnd): replace the snapshot. The
        // old snapshot's Drop fires COM Release on `ptr`.
        cache_a.publish(pid(), HWND, snapshot_with(vec![]));
        a_replaced_tx.send(()).unwrap();
    });

    worker_a.join().unwrap();
    worker_b.join().unwrap();
    uaf_hits.load(Ordering::SeqCst)
}

/// The retained resolution path survives the exact dangerous interleave with
/// zero use-after-free touches.
#[test]
fn fixed_path_survives_concurrent_replace() {
    let _serial = TEST_LOCK.lock().unwrap();
    let uaf = run_forced_interleave(true, false);
    assert_eq!(uaf, 0, "retained path must not touch a released element");
}

/// Negative control: dropping the lookup retain before replacement reproduces
/// the pre-fix use-after-free without crashing the ordinary suite.
#[test]
fn prefix_path_commits_use_after_free() {
    let _serial = TEST_LOCK.lock().unwrap();
    let uaf = run_forced_interleave(false, false);
    assert!(
        uaf > 0,
        "pre-fix path must hit the released element (the UAF)"
    );
}

/// Under real (unforced) concurrency: hammer retained resolution +
/// snapshot-replace from many threads in a tight loop; assert zero UAF.
#[test]
fn fixed_path_stress_no_uaf() {
    let _serial = TEST_LOCK.lock().unwrap();
    let uaf_hits: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
    let cache = Arc::new(ElementCache::new());

    // Seed a snapshot of several elements.
    let seed: Vec<usize> = (0..8).map(|_| make_fake(uaf_hits, false)).collect();
    let latest = Arc::new(std::sync::atomic::AtomicU32::new(cache.publish(
        pid(),
        HWND,
        snapshot_with(seed),
    )));

    const ITERS: usize = 4000;
    let publishers = Arc::new(std::sync::Mutex::new(()));
    let mut handles = Vec::new();

    // Replacer threads: continuously run `update` (snapshot replace → Release).
    for _ in 0..3 {
        let cache_r = cache.clone();
        let publishers = publishers.clone();
        let latest = latest.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..ITERS {
                let _publication = publishers.lock().unwrap();
                let fresh: Vec<usize> = (0..8).map(|_| make_fake(uaf_hits, false)).collect();
                let snapshot = cache_r.publish(pid(), HWND, snapshot_with(fresh));
                latest.store(snapshot, Ordering::SeqCst);
            }
        }));
    }

    // Actor threads: lookup-retain-deref-release, the click/type/set_value path.
    for _ in 0..3 {
        let cache_c = cache.clone();
        let latest = latest.clone();
        handles.push(thread::spawn(move || {
            for i in 0..ITERS {
                if let Some(guard) = acquire(&cache_c, latest.load(Ordering::SeqCst), i % 8) {
                    unsafe { touch_vtable(guard.as_ptr()) };
                }
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        uaf_hits.load(Ordering::SeqCst),
        0,
        "no element may be touched after Release under concurrent replace"
    );
}

// ---- Hard-crash demonstrations (run manually, never in the suite) -----------

/// BEFORE (real fault): the pre-fix path takes a genuine
/// STATUS_ACCESS_VIOLATION. Poison-on-zero makes the otherwise heap-reuse-
/// dependent UAF deterministic. Run with:
///   cargo test -p platform-windows -- --ignored --exact \
///     uia::cache::cache_uaf_repro::prefix_path_real_access_violation
/// EXPECTED: the test process crashes (exception 0xC0000005); it does NOT
/// print "test result: ok".
#[test]
#[ignore]
fn prefix_path_real_access_violation() {
    let _serial = TEST_LOCK.lock().unwrap();
    let uaf = run_forced_interleave(/* use_retained */ false, /* poison */ true);
    // Unreachable in practice — the dereference of the poisoned vtable faults.
    println!("SURVIVED unexpectedly (uaf_hits={uaf})");
}

/// AFTER (control): the same poison-on-zero setup, but the retained guard keeps
/// the refcount above zero during the dereference, so the vtable is never
/// poisoned while in use. EXPECTED: clean exit, prints the survival line.
#[test]
#[ignore]
fn fixed_path_no_access_violation() {
    let _serial = TEST_LOCK.lock().unwrap();
    let uaf = run_forced_interleave(/* use_retained */ true, /* poison */ true);
    assert_eq!(uaf, 0);
    println!("SURVIVED as expected (uaf_hits={uaf})");
}

#[test]
fn exact_snapshot_retains_matching_identity_and_geometry() {
    let _serial = TEST_LOCK.lock().unwrap();
    let hits = Box::leak(Box::new(AtomicUsize::new(0)));
    let cache = ElementCache::new();
    let ptr = make_fake(hits, false);
    let mut payload = snapshot_with(vec![ptr]);
    payload.elements[0].kind = SnapshotKind::Msaa;
    payload.elements[0].center = (31, 47);
    payload.elements[0].rect = Some((11, 27, 51, 67));
    payload.elements[0].msaa_role = Some(0x38);
    let first = cache.publish(pid(), HWND, payload);
    let guard = acquire(&cache, first, 0).unwrap();
    let second = cache.publish(pid(), HWND, snapshot_with(vec![make_fake(hits, false)]));
    assert!(acquire(&cache, first, 0).is_none());
    assert_eq!(guard.as_ptr(), ptr);
    assert_eq!(guard.kind, SnapshotKind::Msaa);
    assert_eq!(guard.center, (31, 47));
    assert_eq!(guard.rect, Some((11, 27, 51, 67)));
    assert_eq!(guard.msaa_role, Some(0x38));
    assert_eq!(
        guard
            .focus_element(HWND, std::process::id())
            .unwrap_err()
            .to_string(),
        "element is an MSAA element, not a UIA element"
    );
    assert_eq!(guard.element_has_keyboard_focus(), None);
    assert_eq!(acquire(&cache, second, 0).unwrap().center, (0, 0));
    let wrong_window = cache
        .resolve_element_args(
            pid(),
            None,
            Some(&token_for(second, 0)),
            None,
            Some(HWND + 1),
            "test",
        )
        .unwrap_err();
    assert_eq!(
        wrong_window.structured_content.unwrap()["refusal"]["code"],
        "conflicting_element_target"
    );
    let cloned = guard.clone();
    drop(guard);
    unsafe { touch_vtable(cloned.as_ptr()) };
    drop(cloned);
    assert_eq!(
        unsafe { (*(ptr as *const FakeObj)).refcount.load(Ordering::SeqCst) },
        0
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[test]
fn eviction_remove_and_clear_release_payload_but_not_acquired_guard() {
    let _serial = TEST_LOCK.lock().unwrap();
    let hits = Box::leak(Box::new(AtomicUsize::new(0)));
    let cache = ElementCache::new();
    let ptr = make_fake(hits, false);
    let first = cache.publish(pid(), HWND, snapshot_with(vec![ptr]));
    let guard = acquire(&cache, first, 0).unwrap();
    for offset in 1..=cua_driver_core::element_token::LRU_CAP_PER_PID {
        cache.publish(
            pid(),
            HWND + offset as u64,
            snapshot_with(vec![make_fake(hits, false)]),
        );
    }
    assert!(acquire(&cache, first, 0).is_none());
    unsafe { touch_vtable(guard.as_ptr()) };
    let removed_ptr = make_fake(hits, false);
    let removed = cache.publish(pid(), HWND, snapshot_with(vec![removed_ptr]));
    cache.remove(pid(), HWND);
    assert!(acquire(&cache, removed, 0).is_none());
    assert_eq!(
        unsafe {
            (*(removed_ptr as *const FakeObj))
                .refcount
                .load(Ordering::SeqCst)
        },
        0
    );
    assert_eq!(
        cache.clear(),
        cua_driver_core::element_token::LRU_CAP_PER_PID - 1
    );
    assert_eq!(cache.clear(), 0);
    drop(cache);
    unsafe { touch_vtable(guard.as_ptr()) };
    drop(guard);
    assert_eq!(
        unsafe { (*(ptr as *const FakeObj)).refcount.load(Ordering::SeqCst) },
        0
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn detached_worker_keeps_only_admitted_target_and_geometry() {
    let _serial = TEST_LOCK.lock().unwrap();
    let hits = Box::leak(Box::new(AtomicUsize::new(0)));
    let cache = ElementCache::new();
    let target = make_fake(hits, false);
    let sibling = make_fake(hits, false);
    let mut payload = snapshot_with(vec![target, sibling]);
    payload.elements[0].center = (71, 83);
    payload.elements[0].rect = Some((61, 73, 81, 93));
    let id = cache.publish(pid(), HWND, payload);
    let guard = acquire(&cache, id, 0).unwrap();
    let worker_guard = guard.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = tokio::task::spawn_blocking(move || {
        ready_tx.send(()).unwrap();
        resume_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert_eq!(worker_guard.as_ptr(), target);
        assert_eq!(worker_guard.center, (71, 83));
        assert_eq!(worker_guard.rect, Some((61, 73, 81, 93)));
        unsafe { touch_vtable(worker_guard.as_ptr()) };
        drop(worker_guard);
        done_tx.send(()).unwrap();
    });
    ready_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    worker.abort();
    drop(worker);
    drop(guard);
    cache.clear();
    assert_eq!(
        unsafe {
            (*(sibling as *const FakeObj))
                .refcount
                .load(Ordering::SeqCst)
        },
        0
    );
    assert_eq!(
        unsafe {
            (*(target as *const FakeObj))
                .refcount
                .load(Ordering::SeqCst)
        },
        1
    );
    resume_tx.send(()).unwrap();
    done_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    assert_eq!(
        unsafe {
            (*(target as *const FakeObj))
                .refcount
                .load(Ordering::SeqCst)
        },
        0
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[test]
fn recording_metadata_uses_snapshot_without_native_geometry() {
    let _serial = TEST_LOCK.lock().unwrap();
    cua_driver_core::tool::with_runtime_scope("windows-recording-metadata-test".into(), || {
        let hits = Box::leak(Box::new(AtomicUsize::new(0)));
        let cache = Arc::new(ElementCache::new());
        cua_driver_core::element_cache::register_runtime_cache(&cache);
        let ptr = make_fake(hits, false);
        let id = cache.publish(pid(), HWND, snapshot_with(vec![ptr]));
        let args = serde_json::json!({"element_token": token_for(id, 0)});
        assert_eq!(
            crate::recording_hooks::element_window_local_xy(i64::from(pid()), &args, false),
            Some((HWND, None))
        );
        cache.publish(pid(), HWND, snapshot_with(vec![]));
        assert_eq!(
            crate::recording_hooks::element_window_local_xy(i64::from(pid()), &args, false),
            None
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn null_native_pointer_is_not_an_actionable_member() {
    let _serial = TEST_LOCK.lock().unwrap();
    let cache = ElementCache::new();
    let snapshot = cache.publish(pid(), HWND, snapshot_with(vec![0]));
    assert!(acquire(&cache, snapshot, 0).is_none());
    let error = cache
        .resolve_element_args(
            pid(),
            None,
            Some(&token_for(snapshot, 0)),
            None,
            Some(HWND),
            "test",
        )
        .unwrap_err();
    assert_eq!(
        error.structured_content.unwrap()["refusal"]["code"],
        "stale_element_token"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_caller_keeps_the_blocking_workers_native_target_alive() {
    let _serial = TEST_LOCK.lock().unwrap();
    let hits = Box::leak(Box::new(AtomicUsize::new(0)));
    let ptr = make_fake(hits, false);
    let cache = ElementCache::new();
    let snapshot = cache.publish(pid(), HWND, snapshot_with(vec![ptr]));
    let guard = acquire(&cache, snapshot, 0).unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    let caller = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || {
            started_tx.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            unsafe { touch_vtable(guard.as_ptr()) };
            drop(guard);
            finished_tx.send(()).unwrap();
        })
        .await
        .unwrap();
    });
    started_rx.await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    cache.clear();
    assert_eq!(
        unsafe { (*(ptr as *const FakeObj)).refcount.load(Ordering::SeqCst) },
        1
    );
    release_tx.send(()).unwrap();
    finished_rx.await.unwrap();
    assert_eq!(
        unsafe { (*(ptr as *const FakeObj)).refcount.load(Ordering::SeqCst) },
        0
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[test]
fn unpublished_payload_releases_its_walker_retain() {
    let _serial = TEST_LOCK.lock().unwrap();
    let hits = Box::leak(Box::new(AtomicUsize::new(0)));
    let ptr = make_fake(hits, false);
    let descriptive = make_fake(hits, false);
    let mut payload = snapshot_with(vec![ptr]);
    payload._descriptive.push(RetainedElement {
        ptr: descriptive,
        kind: SnapshotKind::Uia,
        center: (0, 0),
        rect: None,
        msaa_role: None,
    });
    drop(payload);
    assert_eq!(
        unsafe { (*(ptr as *const FakeObj)).refcount.load(Ordering::SeqCst) },
        0
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        unsafe {
            (*(descriptive as *const FakeObj))
                .refcount
                .load(Ordering::SeqCst)
        },
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_admitted_worker_blocks_replacement_until_mutation_finishes() {
    let _serial = TEST_LOCK.lock().unwrap();
    let hits = Box::leak(Box::new(AtomicUsize::new(0)));
    let ptr = make_fake(hits, false);
    let cache = Arc::new(ElementCache::new());
    let snapshot = cache.publish(pid(), HWND, snapshot_with(vec![ptr]));
    let ResolvedElement::Element { element, .. } = cache
        .resolve_element_args(
            pid(),
            None,
            Some(&token_for(snapshot, 0)),
            None,
            Some(HWND),
            "test",
        )
        .unwrap()
    else {
        panic!("expected admitted element")
    };
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    let caller = tokio::spawn(async move {
        cua_driver_core::blocking::spawn(move || {
            started_tx.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            unsafe { touch_vtable(element.as_ptr()) };
            drop(element);
            finished_tx.send(()).unwrap();
        })
        .await
        .unwrap();
    });
    started_rx.await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let (published_tx, published_rx) = mpsc::channel();
    let replacement = cache.clone();
    let publisher = thread::spawn(move || {
        published_tx
            .send(replacement.try_publish(pid(), HWND, snapshot_with(vec![])))
            .unwrap();
    });
    assert_eq!(
        published_rx.recv_timeout(std::time::Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout),
        "publication must wait for the detached worker's mutation permit"
    );
    assert_eq!(
        unsafe { (*(ptr as *const FakeObj)).refcount.load(Ordering::SeqCst) },
        2
    );
    release_tx.send(()).unwrap();
    finished_rx.await.unwrap();
    published_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap()
        .unwrap();
    publisher.join().unwrap();
    assert_eq!(
        unsafe { (*(ptr as *const FakeObj)).refcount.load(Ordering::SeqCst) },
        0
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}
