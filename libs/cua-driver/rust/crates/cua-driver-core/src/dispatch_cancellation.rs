//! Cooperative cancellation shared by transport requests and nested dispatch.
//!
//! Cancellation closes admission for the next action; it does not revoke the
//! ownership of a running native worker or skip release/restoration. A session
//! scope must use the authorized runtime-private lifecycle id, never a public
//! label supplied by a client.

use std::future::Future;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<TokenState>);

#[derive(Default)]
struct TokenState {
    cancelled: AtomicBool,
    changed: tokio::sync::Notify,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.changed.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            // Register before checking, including on a multi-threaded executor.
            changed.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            changed.await;
        }
    }
}

/// Exact, content-free refusal reason. Session teardown takes precedence over
/// request cancellation when both have already been observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DispatchCancelled {
    #[error("the request was cancelled")]
    RequestCancelled,
    #[error("the session is ending or has ended")]
    SessionEnded,
}

impl DispatchCancelled {
    pub const fn code(self) -> &'static str {
        match self {
            Self::RequestCancelled => "request_cancelled",
            Self::SessionEnded => "session_ended",
        }
    }
}

/// Owned context for explicit task/thread continuations. Ordinary directly
/// awaited nested calls inherit the task-local automatically. `tokio::spawn`
/// does not: capture here, then use `context.scope(child).await` in that task.
#[derive(Clone, Default)]
pub struct CancellationContext {
    tokens: Vec<CancellationToken>,
    sessions: Vec<String>,
}

tokio::task_local! {
    static DISPATCH_CANCELLATION: CancellationContext;
}

impl CancellationContext {
    pub fn capture() -> Self {
        DISPATCH_CANCELLATION
            .try_with(Clone::clone)
            .unwrap_or_default()
    }

    pub fn check_cancelled(&self) -> Result<(), DispatchCancelled> {
        if self
            .sessions
            .iter()
            .any(|id| crate::session::is_session_ending(id))
        {
            Err(DispatchCancelled::SessionEnded)
        } else if self.tokens.iter().any(CancellationToken::is_cancelled) {
            Err(DispatchCancelled::RequestCancelled)
        } else {
            Ok(())
        }
    }

    /// Merge, rather than replace, the destination's inherited cancellation.
    pub async fn scope<F: Future>(&self, future: F) -> F::Output {
        let mut context = Self::capture();
        context.tokens.extend(self.tokens.iter().cloned());
        for session in &self.sessions {
            if !context.sessions.contains(session) {
                context.sessions.push(session.clone());
            }
        }
        DISPATCH_CANCELLATION.scope(context, future).await
    }

    /// Run a synchronous continuation under this context. Native cleanup still
    /// follows its own release-only scope; do not add cancellation checks to it.
    pub fn sync_scope<T>(&self, function: impl FnOnce() -> T) -> T {
        let mut context = Self::capture();
        context.tokens.extend(self.tokens.iter().cloned());
        for session in &self.sessions {
            if !context.sessions.contains(session) {
                context.sessions.push(session.clone());
            }
        }
        DISPATCH_CANCELLATION.sync_scope(context, function)
    }
}

pub fn check_cancelled() -> Result<(), DispatchCancelled> {
    DISPATCH_CANCELLATION
        .try_with(CancellationContext::check_cancelled)
        .unwrap_or(Ok(()))
}

pub async fn scope<F: Future>(token: CancellationToken, future: F) -> F::Output {
    struct CancelOnDrop(CancellationToken);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let guard = CancelOnDrop(token.clone());
    let mut context = CancellationContext::capture();
    context.tokens.push(token);
    let output = DISPATCH_CANCELLATION.scope(context, future).await;
    drop(guard);
    output
}

pub async fn scope_session<F: Future>(runtime_session: &str, future: F) -> F::Output {
    let mut context = CancellationContext::capture();
    if !context.sessions.iter().any(|id| id == runtime_session) {
        context.sessions.push(runtime_session.to_owned());
    }
    DISPATCH_CANCELLATION.scope(context, future).await
}

/// Stop a composite promptly even while it sleeps, polls a locator, or runs JS
/// without issuing another host call. Dropping the future activates existing
/// script/native cancellation guards; native release/restoration stays owned.
/// Polling the lifecycle fence does not require a Tokio interval or a new hook.
pub async fn run_composite<F>(future: F) -> crate::protocol::ToolResult
where
    F: Future<Output = crate::protocol::ToolResult>,
{
    tokio::pin!(future);
    loop {
        if let Err(cancelled) = check_cancelled() {
            return crate::input_policy::refusal(cancelled.code(), &cancelled.to_string());
        }
        tokio::select! {
            biased;
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {},
            result = &mut future => return result,
        }
    }
}

/// Extract only valid MCP cancellation notifications. Request IDs remain exact:
/// string `"1"` is not numeric `1`; malformed/null IDs never acquire authority.
/// Reasons and arbitrary metadata are neither retained nor interpreted.
pub fn cancelled_request_id(request: &crate::protocol::Request) -> Option<&serde_json::Value> {
    if request.jsonrpc != "2.0"
        || !request.is_notification()
        || request.method != "notifications/cancelled"
    {
        return None;
    }
    request
        .params
        .as_ref()?
        .get("requestId")
        .filter(|id| id.is_string() || id.as_i64().is_some() || id.as_u64().is_some())
}

/// JSON-RPC cancellation result, shared by all MCP adapters. This is a request
/// failure, not a fabricated successful tool result.
pub fn cancelled_response(id: serde_json::Value) -> crate::protocol::Response {
    crate::protocol::Response::error_with_data(
        id,
        -32800,
        "the request was cancelled",
        Some(serde_json::json!({"code": "request_cancelled"})),
    )
}

/// A bounded, FIFO MCP line reader which keeps reading control notifications
/// and EOF while the single current request runs. No detached reader task and
/// no concurrent action dispatch are introduced. Buffered ordinary requests
/// retain ordering and may themselves be cancelled before their first poll.
pub struct RequestReader<R> {
    reader: R,
    partial: Vec<u8>,
    queued: std::collections::VecDeque<QueuedLine>,
    current_cancelled: bool,
}

struct QueuedLine {
    line: String,
    id: Option<serde_json::Value>,
    cancelled: bool,
}

const MAX_MCP_LINE: usize = 1024 * 1024;
const MAX_QUEUED_MCP_REQUESTS: usize = 32;

impl<R: tokio::io::AsyncBufRead + Unpin> RequestReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            partial: Vec::new(),
            queued: Default::default(),
            current_cancelled: false,
        }
    }

    /// Read the next ordinary line. Notifications without a live request ID
    /// have no effect; in particular cancellation never poisons future ID reuse.
    pub async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        self.current_cancelled = false;
        if let Some(queued) = self.queued.pop_front() {
            self.current_cancelled = queued.cancelled;
            return Ok(Some(queued.line));
        }
        self.read_line().await
    }

    async fn read_line(&mut self) -> std::io::Result<Option<String>> {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        let mut bounded = (&mut self.reader).take((MAX_MCP_LINE + 1 - self.partial.len()) as u64);
        let n = bounded.read_until(b'\n', &mut self.partial).await?;
        if self.partial.len() > MAX_MCP_LINE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "MCP request exceeds size limit",
            ));
        }
        if n == 0 && self.partial.is_empty() {
            return Ok(None);
        }
        let bytes = std::mem::take(&mut self.partial);
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub fn into_inner(self) -> R {
        self.reader
    }

    /// Daemon/pipe counterpart: preserve pipelined lines, but close the active
    /// request on peer EOF. The per-call socket is itself the cancellation
    /// capability; clients cannot cancel another transport's work by guessing ID.
    pub async fn run_until_closed<F: Future>(
        &mut self,
        future: F,
    ) -> std::io::Result<Option<F::Output>> {
        let future = scope(CancellationToken::new(), future);
        tokio::pin!(future);
        loop {
            tokio::select! {
                biased;
                line = self.read_line() => {
                    let Some(line) = line? else { return Ok(None); };
                    if self.queued.len() >= MAX_QUEUED_MCP_REQUESTS {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "pending request limit exceeded",
                        ));
                    }
                    self.queued.push_back(QueuedLine {line, id: None, cancelled: false});
                }
                response = &mut future => return Ok(Some(response)),
            }
        }
    }

    /// Run one admitted request while servicing cancellation. EOF returns None
    /// and drops the request future, activating native/script ownership guards.
    /// Callers must then end only their transport-owned sessions.
    pub async fn run<F>(
        &mut self,
        id: serde_json::Value,
        future: F,
    ) -> std::io::Result<Option<crate::protocol::Response>>
    where
        F: Future<Output = crate::protocol::Response>,
    {
        if self.current_cancelled {
            return Ok(Some(cancelled_response(id)));
        }
        let token = CancellationToken::new();
        let future = scope(token.clone(), future);
        tokio::pin!(future);
        loop {
            tokio::select! {
                // Ready EOF/cancel wins over starting the next action unit.
                biased;
                line = self.read_line() => {
                    let Some(line) = line? else {
                        token.cancel();
                        return Ok(None);
                    };
                    if line.trim().is_empty() {
                        continue;
                    }
                    let parsed = serde_json::from_str::<crate::protocol::Request>(&line).ok();
                    if let Some(cancel_id) = parsed.as_ref().and_then(cancelled_request_id) {
                        if cancel_id == &id {
                            token.cancel();
                            return Ok(Some(cancelled_response(id)));
                        }
                        for queued in &mut self.queued {
                            if queued.id.as_ref() == Some(cancel_id) {
                                queued.cancelled = true;
                            }
                        }
                        continue;
                    }
                    if parsed.as_ref().is_some_and(|request| request.is_notification()) {
                        continue;
                    }
                    let queued_id = parsed.and_then(|request| request.id);
                    if queued_id.as_ref().is_some_and(|queued_id| {
                        queued_id == &id || self.queued.iter().any(|queued| queued.id.as_ref() == Some(queued_id))
                    }) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "duplicate active MCP request ID",
                        ));
                    }
                    if self.queued.len() >= MAX_QUEUED_MCP_REQUESTS {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "MCP pending request limit exceeded",
                        ));
                    }
                    self.queued.push_back(QueuedLine {line, id: queued_id, cancelled: false});
                }
                response = &mut future => return Ok(Some(response)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn nested_scope_cannot_replace_parent_cancellation() {
        let outer = CancellationToken::new();
        let inner = CancellationToken::new();
        scope(outer.clone(), async {
            scope(inner, async {
                assert_eq!(check_cancelled(), Ok(()));
                outer.cancel();
                assert_eq!(check_cancelled().unwrap_err().code(), "request_cancelled");
                let context = CancellationContext::capture();
                let child =
                    tokio::spawn(async move { context.scope(async { check_cancelled() }).await });
                assert_eq!(
                    child.await.unwrap().unwrap_err().code(),
                    "request_cancelled"
                );
            })
            .await;
        })
        .await;
        assert_eq!(check_cancelled(), Ok(()), "scope must not leak");
    }

    #[tokio::test]
    async fn token_wakes_all_waiters_and_remembers_prior_cancellation() {
        let token = CancellationToken::new();
        let a = token.clone();
        let b = token.clone();
        let first = tokio::spawn(async move { a.cancelled().await });
        let second = tokio::spawn(async move { b.cancelled().await });
        token.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            first.await.unwrap();
            second.await.unwrap();
            token.cancelled().await;
        })
        .await
        .expect("all waiters observe cancellation, including late ones");
    }

    #[tokio::test]
    async fn exact_private_session_end_is_visible_in_captured_context() {
        let id = format!("dispatch-cancellation-{}", uuid::Uuid::new_v4());
        let unrelated = format!("dispatch-cancellation-{}", uuid::Uuid::new_v4());
        scope_session(&id, async {
            let captured = CancellationContext::capture();
            assert_eq!(captured.check_cancelled(), Ok(()));
            crate::session::end_session(&unrelated);
            assert_eq!(captured.check_cancelled(), Ok(()));
            let admitted = crate::session::begin_session_dispatch(
                &id,
                Some("public-label"),
                &id,
                false,
                crate::session::SessionTransport::McpStdio,
                crate::session::SessionClientKind::Mcp,
            )
            .unwrap();
            crate::session::end_session(&id);
            assert!(
                !crate::session::is_session_ended(&id),
                "cleanup waits for admitted work"
            );
            assert_eq!(
                captured.check_cancelled().unwrap_err().code(),
                "session_ended"
            );
            drop(admitted);
            assert!(crate::session::is_session_ended(&id));
            let thread = std::thread::spawn(move || {
                captured.sync_scope(|| check_cancelled().unwrap_err().code())
            });
            assert_eq!(thread.join().unwrap(), "session_ended");
        })
        .await;
    }

    struct DropWitness(Arc<AtomicBool>);
    impl Drop for DropWitness {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    async fn parked_request(
        started: tokio::sync::oneshot::Sender<CancellationContext>,
        dropped: Arc<AtomicBool>,
    ) -> crate::protocol::Response {
        let _witness = DropWitness(dropped);
        let _ = started.send(CancellationContext::capture());
        std::future::pending().await
    }

    fn assert_cancelled(response: crate::protocol::Response, id: serde_json::Value) {
        let response = serde_json::to_value(response).unwrap();
        assert_eq!(response["id"], id);
        assert_eq!(response["error"]["code"], -32800);
        assert_eq!(response["error"]["data"]["code"], "request_cancelled");
    }

    #[tokio::test]
    async fn reader_cancels_live_and_queued_requests_without_starting_queued_work() {
        use tokio::io::AsyncWriteExt;
        let (server, mut peer) = tokio::io::duplex(8192);
        let mut reader = RequestReader::new(tokio::io::BufReader::new(server));
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let control = async move {
            let context = start_rx.await.unwrap();
            // String ID must not cancel numeric ID 1. The second request stays
            // FIFO queued and is independently cancelled before first dispatch.
            for value in [
                json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":"1"}}),
                json!({"jsonrpc":"2.0", "id":2, "method":"tools/list"}),
                json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":2}}),
                json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":1}}),
            ] {
                peer.write_all((value.to_string() + "\n").as_bytes())
                    .await
                    .unwrap();
            }
            (peer, context)
        };
        let (response, (mut peer, context)) =
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                tokio::join!(
                    reader.run(json!(1), parked_request(start_tx, dropped.clone())),
                    control,
                )
            })
            .await
            .expect("matching cancellation must finish the parked request");
        assert_cancelled(response.unwrap().unwrap(), json!(1));
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(
            context.check_cancelled().unwrap_err().code(),
            "request_cancelled"
        );
        let queued = reader.next_line().await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&queued).unwrap()["id"],
            2
        );
        let response = reader
            .run(json!(2), async { panic!("cancelled queued action ran") })
            .await;
        assert_cancelled(response.unwrap().unwrap(), json!(2));
        // Cancellation of an earlier request does not poison future ID reuse.
        peer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n")
            .await
            .unwrap();
        reader.next_line().await.unwrap().unwrap();
        let response = reader
            .run(json!(1), async {
                crate::protocol::Response::ok(json!(1), json!({}))
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::to_value(response).unwrap()["result"], json!({}));
    }

    #[tokio::test]
    async fn transport_eof_drops_inflight_request_and_cancels_captured_children() {
        let (server, peer) = tokio::io::duplex(1024);
        let mut reader = RequestReader::new(tokio::io::BufReader::new(server));
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let close = async move {
            let context = start_rx.await.unwrap();
            drop(peer);
            context
        };
        let (response, context) = tokio::join!(
            reader.run(json!(1), parked_request(start_tx, dropped.clone())),
            close,
        );
        assert!(response.unwrap().is_none());
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(
            context.check_cancelled().unwrap_err().code(),
            "request_cancelled"
        );
    }

    #[tokio::test]
    async fn partial_next_line_survives_request_completion() {
        use tokio::io::AsyncWriteExt;
        let (server, mut peer) = tokio::io::duplex(1024);
        let mut reader = RequestReader::new(tokio::io::BufReader::new(server));
        peer.write_all(b"{\"jsonrpc\"").await.unwrap();
        reader
            .run(json!(1), async {
                crate::protocol::Response::ok(json!(1), json!({}))
            })
            .await
            .unwrap();
        peer.write_all(b":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n")
            .await
            .unwrap();
        let line = reader.next_line().await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["id"],
            2
        );
    }

    #[test]
    fn cancellation_notification_requires_exact_method_and_valid_id() {
        for id in [json!(1), json!("1"), json!(u64::MAX)] {
            let request = serde_json::from_value(json!({
                "jsonrpc": "2.0", "method": "notifications/cancelled",
                "params": {"requestId": id, "reason": "not retained"}
            }))
            .unwrap();
            assert_eq!(cancelled_request_id(&request), Some(&id));
        }
        for raw in [
            json!({"method":"notifications/cancelled", "params":{"requestId":null}}),
            json!({"method":"notifications/cancelled", "params":{"requestId":1.5}}),
            json!({"method":"notifications/cancelled", "params":{"requestId":{}}}),
            json!({"method":"notifications/cancelled", "id":2, "params":{"requestId":1}}),
            json!({"method":"other", "params":{"requestId":1}}),
        ] {
            let mut raw = raw;
            raw["jsonrpc"] = json!("2.0");
            let request = serde_json::from_value(raw).unwrap();
            assert_eq!(cancelled_request_id(&request), None);
        }
    }
}
