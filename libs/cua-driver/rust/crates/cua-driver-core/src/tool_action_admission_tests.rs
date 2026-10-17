//! Hermetic admission regressions: no platform hooks or process-global table.

#[tokio::test]
async fn semantic_input_gnome_click_skips_readiness_but_keeps_lease_and_raw_ordering() {
    use crate::action_lease::InputRoute;
    let table = ActionLeaseTable::new();
    let args = json!({"pid":500,"window_id":5,"element_token":"s:1","delivery_mode":"background"});
    // The Linux selector supplies AccessibilityOnly for this GNOME shape;
    // exercise the real core admission body without global hook mutation.
    let semantic = super::admit_action_lease_with(
        "click",
        &args,
        InputRoute::AccessibilityOnly,
        &table,
        |_, _| panic!("semantic admission must not call readiness"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        table
            .acquire(LeaseRequest::desktop_raw(None, Duration::ZERO))
            .await
            .unwrap_err()
            .code(),
        "input_busy"
    );
    drop(semantic);
    let probe_table = table.clone();
    let coordinate = super::admit_action_lease_with(
        "click",
        &json!({"pid":500,"window_id":5,"x":1,"y":2}),
        InputRoute::Unrestricted,
        &table,
        move |tool, args| {
            assert_eq!(tool, "click");
            assert_eq!(args["x"], 1);
            // This would fail if the raw lease preceded readiness.
            let lease = probe_table
                .acquire_blocking(LeaseRequest::desktop_raw(None, Duration::ZERO))
                .unwrap();
            drop(lease);
            Err("fixture: EIS unavailable".into())
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        coordinate.structured_content.unwrap()["refusal"]["code"],
        "input_unavailable"
    );
    table
        .acquire(LeaseRequest::desktop_raw(None, Duration::ZERO))
        .await
        .unwrap();
}

#[tokio::test]
async fn semantic_input_scope_survives_native_continuations_cleanup_and_nested_dispatch() {
    use crate::action_lease::{
        current_input_route, input_route_for_tool, require_raw_input_allowed, InputRoute,
    };
    let scope = InputRoute::AccessibilityOnly
        .scope(async {
            assert_eq!(
                input_route_for_tool("press_key", &json!({})),
                InputRoute::AccessibilityOnly
            );
            InputRoute::Unrestricted
                .scope(async {
                    assert!(require_raw_input_allowed().is_err());
                })
                .await;
            crate::blocking::spawn(|| {
                assert!(require_raw_input_allowed().is_err());
                ActionLeaseScope::capture()
            })
            .await
            .unwrap()
        })
        .await;
    assert_eq!(current_input_route(), InputRoute::Unrestricted);
    std::thread::spawn(move || {
        scope.run(|| assert!(require_raw_input_allowed().is_err()));
        scope.run_cleanup(|| assert!(require_raw_input_allowed().is_err()));
    })
    .join()
    .unwrap();
}
use super::{lease_refusal, ACTION_LEASE_WAIT, DISPATCH_ACTION_LEASE};
use crate::action_lease::{lease_request_for, ActionLeaseTable, ExactWindow, LeaseRequest};
use crate::blocking::ActionLeaseScope;
use futures_util::poll;
use serde_json::json;
use std::time::{Duration, Instant};

#[test]
fn admission_budget_covers_macos_cleanup_tail_with_scheduling_margin() {
    // The focus guard's idle tail no longer holds the seat, but a cleanup
    // owner still can after its action returns: a hover reveal's three-second
    // reaper (move_cursor) and a deferred window-change read (up to its
    // CUA_DRIVER_WINDOW_CHANGE_TIMEOUT_MS bound). Wait out the longest plus
    // scheduling margin rather than refuse.
    let cleanup_tail = Duration::from_secs(3);
    let scheduling_margin = Duration::from_secs(1);
    assert!(ACTION_LEASE_WAIT >= cleanup_tail + scheduling_margin);
    assert_eq!(ACTION_LEASE_WAIT, Duration::from_secs(4));
}

#[tokio::test]
async fn returned_action_keeps_successor_excluded_until_last_cleanup_owner_drops() {
    let table = ActionLeaseTable::new();
    let args = json!({"pid": 100, "window_id": 1});
    let grant = table
        .acquire_for_dispatch(lease_request_for("click", &args, ACTION_LEASE_WAIT))
        .await
        .unwrap();
    let cleanup = DISPATCH_ACTION_LEASE
        .scope(Some(grant.clone()), async { ActionLeaseScope::capture() })
        .await;
    // The tool returned; only native cleanup retains the originating grant.
    drop(grant);
    let mut successor =
        Box::pin(table.acquire_for_dispatch(lease_request_for("click", &args, ACTION_LEASE_WAIT)));
    assert!(poll!(successor.as_mut()).is_pending());
    let last_cleanup = cleanup.run_cleanup(ActionLeaseScope::capture);
    drop(cleanup);
    assert!(poll!(successor.as_mut()).is_pending());
    // No sleeps or scheduler races decide ownership: release it explicitly.
    drop(last_cleanup);
    let successor = successor.await.expect("cleanup release admits successor");
    let err = table
        .acquire(LeaseRequest::desktop_raw(None, Duration::ZERO))
        .await
        .expect_err("successor must now own the seat");
    assert_eq!(err.code(), "input_busy");
    drop(successor);
    table
        .acquire(LeaseRequest::desktop_raw(None, Duration::ZERO))
        .await
        .expect("successor release restores availability");
}

#[tokio::test]
async fn admission_timeout_is_input_busy_without_releasing_cleanup_owner() {
    let table = ActionLeaseTable::new();
    let args = json!({"pid": 200, "window_id": 2});
    let grant = table
        .acquire_for_dispatch(lease_request_for("click", &args, ACTION_LEASE_WAIT))
        .await
        .unwrap();
    let cleanup = DISPATCH_ACTION_LEASE
        .scope(Some(grant.clone()), async { ActionLeaseScope::capture() })
        .await;
    drop(grant);
    let started = Instant::now();
    let err = tokio::time::timeout(
        Duration::from_secs(30),
        table.acquire_for_dispatch(lease_request_for("click", &args, ACTION_LEASE_WAIT)),
    )
    .await
    .expect("admission must remain bounded")
    .expect_err("cleanup is deliberately held beyond the whole admission budget");
    assert!(started.elapsed() >= ACTION_LEASE_WAIT);
    let refusal = serde_json::to_value(lease_refusal(err)).unwrap();
    assert_eq!(refusal["isError"], true);
    assert_eq!(
        refusal["structuredContent"]["refusal"]["code"],
        "input_busy"
    );
    assert_eq!(
        table
            .acquire(LeaseRequest::desktop_raw(None, Duration::ZERO))
            .await
            .expect_err("timeout cannot revoke cleanup ownership")
            .code(),
        "input_busy"
    );
    drop(cleanup);
    table
        .acquire_for_dispatch(lease_request_for("click", &args, ACTION_LEASE_WAIT))
        .await
        .expect("a fresh request succeeds after cleanup actually exits");
}

#[tokio::test]
async fn admission_preserves_desktop_exclusivity_and_exact_window_scoping() {
    let table = ActionLeaseTable::new();
    let args = json!({"pid": 300, "window_id": 3});
    let raw = table
        .acquire_for_dispatch(lease_request_for("click", &args, ACTION_LEASE_WAIT))
        .await
        .unwrap();
    assert_eq!(
        table
            .acquire(lease_request_for(
                "click",
                &json!({"pid": 400, "window_id": 4}),
                Duration::ZERO
            ))
            .await
            .expect_err("another desktop target still shares the seat")
            .code(),
        "input_busy"
    );
    assert_eq!(
        table
            .acquire(lease_request_for("set_value", &args, Duration::ZERO))
            .await
            .expect_err("raw grant also owns its exact target")
            .code(),
        "target_busy"
    );
    table.acquire(LeaseRequest::observation()).await.unwrap();
    drop(raw);
    let semantic = table
        .acquire_for_dispatch(lease_request_for("set_value", &args, ACTION_LEASE_WAIT))
        .await
        .unwrap();
    table
        .acquire(LeaseRequest::window_semantic(
            ExactWindow {
                pid: 300,
                window_id: 4,
            },
            Duration::ZERO,
        ))
        .await
        .expect("semantic grants do not serialize independent sibling windows");
    assert_eq!(
        table
            .acquire(lease_request_for("set_value", &args, Duration::ZERO))
            .await
            .expect_err("same-window semantic writers remain exclusive")
            .code(),
        "target_busy"
    );
    drop(semantic);
}
