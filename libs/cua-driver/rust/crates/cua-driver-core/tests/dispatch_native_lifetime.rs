//! Public dispatch owns session teardown until native release/restoration drains.
//! No platform registry, ambient desktop, or configured input policy is used.
use cua_driver_core::authorization::PermissionMode;
use cua_driver_core::dispatch_cancellation::{self, CancellationToken};
use cua_driver_core::protocol::ToolResult;
use cua_driver_core::session_authorization::{
    EffectiveAuthorizationContext, SessionAuthorizationRegistry, SessionModeCeiling,
};
use cua_driver_core::tool::{Tool, ToolDef, ToolRegistry};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

struct ParkedNative {
    def: ToolDef,
    channels: Mutex<
        Option<(
            tokio::sync::oneshot::Sender<String>,
            std::sync::mpsc::Receiver<()>,
            tokio::sync::oneshot::Sender<()>,
        )>,
    >,
    finished: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    continuation: bool,
}
#[async_trait::async_trait]
impl Tool for ParkedNative {
    fn def(&self) -> &ToolDef {
        &self.def
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let session = args["_session_id"].as_str().unwrap().to_owned();
        let (started, release, continuation_joined) = self.channels.lock().unwrap().take().unwrap();
        let finished = self.finished.clone();
        let continuation = self.continuation;
        cua_driver_core::blocking::spawn(move || {
            let work = move || {
                let _ = started.send(session);
                let _ = release.recv_timeout(Duration::from_secs(10));
                // Models the last key release and restoration, not reply receipt.
                finished.store(true, Ordering::SeqCst);
            };
            if continuation {
                // A native read times out while its independently owned native
                // continuation remains parked after the first worker exits.
                let outcome = cua_driver_core::blocking::run_native_with_deadline(
                    Duration::from_millis(25),
                    (),
                    move |_| work(),
                );
                assert!(matches!(
                    outcome,
                    Err(cua_driver_core::blocking::NativeDeadlineError::TimedOut)
                ));
            } else {
                work();
            }
        })
        .await
        .unwrap();
        if continuation {
            // The first worker has actually joined; only its timed-out native
            // continuation can now retain the session admission after cancel.
            let _ = continuation_joined.send(());
            std::future::pending::<()>().await;
        }
        ToolResult::text("finished")
    }
}
fn context() -> Arc<EffectiveAuthorizationContext> {
    SessionAuthorizationRegistry::with_ceiling(
        SessionModeCeiling::for_trusted_sessions(
            [PermissionMode::Unrestricted],
            true,
            Duration::from_secs(60),
            Duration::from_secs(30),
        )
        .unwrap(),
    )
    .compatibility_context(PermissionMode::Unrestricted, None)
    .unwrap()
}
async fn call(
    registry: &ToolRegistry,
    ctx: &Arc<EffectiveAuthorizationContext>,
    name: &str,
    args: Value,
) -> ToolResult {
    cua_driver_core::action_lease::InputRoute::AccessibilityOnly
        .scope(registry.invoke_with_context(name, args, ctx.clone()))
        .await
}

#[tokio::test]
async fn cancelled_composites_defer_session_hooks_until_native_and_continuations_unwind() {
    let composites = [
        "run_steps",
        #[cfg(feature = "script")]
        "run_script",
    ];
    for composite in composites {
        for continuation in [false, true] {
            for cancel_first in [false, true] {
                let (started, running) = tokio::sync::oneshot::channel();
                let (release, worker_release) = std::sync::mpsc::channel();
                let (continuation_joined, joined) = tokio::sync::oneshot::channel();
                let finished = Arc::new(AtomicBool::new(false));
                let calls = Arc::new(AtomicUsize::new(0));
                let mut registry = ToolRegistry::new();
                registry.register_session_tools();
                registry.register(Box::new(ParkedNative {
                    def: ToolDef {
                        name: "set_value".into(), description: "private native owner".into(),
                        input_schema: json!({"type":"object","properties":{"pid":{},"window_id":{},"value":{}}}),
                        read_only: true, destructive: false, idempotent: false, open_world: false,
                    }, channels: Mutex::new(Some((started, worker_release, continuation_joined))),
                    finished: finished.clone(), calls: calls.clone(), continuation,
                }));
                let registry = Arc::new(registry);
                registry.init_self_weak();
                let ctx = context();
                let args = if composite == "run_steps" {
                    json!({"session":"native-lifetime", "steps":[
                        {"set_value":{"pid":42,"window_id":7,"value":"first"}},
                        {"set_value":{"pid":42,"window_id":7,"value":"must not run"}}
                    ]})
                } else {
                    json!({"session":"native-lifetime", "script":"await cua.call('set_value',{pid:42,window_id:7,value:'first'}); await cua.call('set_value',{pid:42,window_id:7,value:'must not run'});"})
                };
                let token = CancellationToken::new();
                let mut invocation = Box::pin(dispatch_cancellation::scope(
                    token.clone(),
                    call(&registry, &ctx, composite, args),
                ));
                let id = tokio::select! {
                    id = tokio::time::timeout(Duration::from_secs(3), running) => id.unwrap().unwrap(),
                    result = &mut invocation => panic!("unexpected early completion: {result:?}"),
                };
                if continuation {
                    tokio::select! {
                        joined = tokio::time::timeout(Duration::from_secs(3), joined) => joined.unwrap().unwrap(),
                        result = &mut invocation => panic!("continuation returned before cancellation: {result:?}"),
                    }
                }
                let early_cleanup = Arc::new(AtomicBool::new(false));
                let early = early_cleanup.clone();
                let worker_finished = finished.clone();
                let (cleaned, mut cleanup) = tokio::sync::mpsc::unbounded_channel();
                let wanted = id.clone();
                let _hook =
                    cua_driver_core::session::register_scoped_session_end_hook(move |ended| {
                        if ended == wanted {
                            if !worker_finished.load(Ordering::SeqCst) {
                                early.store(true, Ordering::SeqCst);
                            }
                            let _ = cleaned.send(());
                        }
                    });
                if cancel_first {
                    token.cancel();
                } else {
                    cua_driver_core::session::end_session(&id);
                }
                let result = tokio::time::timeout(Duration::from_secs(1), &mut invocation)
                    .await
                    .unwrap();
                drop(invocation);
                if cancel_first {
                    cua_driver_core::session::end_session(&id);
                }
                // Collect the observations before releasing, but assert only
                // after draining so even the red control cannot strand a worker.
                let ended_while_active = cua_driver_core::session::is_session_ended(&id);
                let hook_while_active = cleanup.try_recv().is_ok();
                let active_at_reply = !finished.load(Ordering::SeqCst);
                let _ = release.send(());
                if !hook_while_active {
                    tokio::time::timeout(Duration::from_secs(3), cleanup.recv())
                        .await
                        .unwrap()
                        .unwrap();
                }
                assert!(
                    active_at_reply,
                    "fixture worker must remain parked at cancellation reply"
                );
                assert!(!ended_while_active, "{composite} continuation={continuation}: waiter drop completed session teardown");
                assert!(
                    !hook_while_active && !early_cleanup.load(Ordering::SeqCst),
                    "cleanup ran before native unwind"
                );
                assert!(finished.load(Ordering::SeqCst));
                assert!(cua_driver_core::session::is_session_ended(&id));
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    1,
                    "later action crossed cancellation fence"
                );
                assert_eq!(result.is_error, Some(true), "{result:?}");
                assert_eq!(
                    result.structured_content.as_ref().unwrap()["code"],
                    if cancel_first {
                        "request_cancelled"
                    } else {
                        "session_ended"
                    },
                    "{result:?}"
                );
            }
        }
    }
}
