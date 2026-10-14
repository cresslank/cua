//! Hermetic admission regressions: no platform hooks or process-global table.
use super::{lease_refusal, ACTION_LEASE_WAIT, DISPATCH_ACTION_LEASE};
use crate::action_lease::{lease_request_for, ActionLeaseTable, ExactWindow, LeaseRequest};
use crate::blocking::ActionLeaseScope;
use futures_util::poll;
use serde_json::json;
use std::time::{Duration, Instant};

#[test]
fn admission_budget_covers_macos_cleanup_tail_with_scheduling_margin() {
    // key_focus_guard::TAIL is three seconds. The native background-counter
    // probe exhausted the former two-second admission wait before cleanup ended.
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
