//! Admission control for native blocking calls.
//!
//! Timed-out platform work cannot always be cancelled (AX, UIA, AT-SPI, and
//! compositor IPC can remain inside an OS call after their async caller has
//! returned). Keep those calls on Tokio's blocking pool, but admit only a fixed
//! number at once. The permit is moved into the blocking closure, so dropping or
//! timing out the async waiter does not release capacity before the native call
//! actually exits. Unrelated SDK supervision and service shutdown work retain
//! access to the rest of Tokio's blocking pool.

use std::sync::Arc;

const MAX_CONCURRENT_NATIVE_CALLS: usize = 32;

fn native_call_limit() -> Arc<tokio::sync::Semaphore> {
    static LIMIT: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    LIMIT
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_NATIVE_CALLS)))
        .clone()
}

/// Spawn one potentially uncancellable native call under process-wide
/// admission control. The returned handle has the same output shape as
/// `tokio::task::spawn_blocking`, so callers retain ordinary join and panic
/// semantics.
pub fn spawn<F, R>(function: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    spawn_with_limit(native_call_limit(), function)
}

fn spawn_with_limit<F, R>(
    limit: Arc<tokio::sync::Semaphore>,
    function: F,
) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::spawn(async move {
        let permit = limit
            .acquire_owned()
            .await
            .expect("native blocking-call admission semaphore closed");
        match tokio::task::spawn_blocking(move || {
            let _permit = permit;
            function()
        })
        .await
        {
            Ok(value) => value,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => panic!("native blocking task was cancelled by runtime shutdown: {error}"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Condvar, Mutex,
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn permit_is_held_until_the_blocking_closure_exits() {
        let limit = Arc::new(tokio::sync::Semaphore::new(1));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let entered = Arc::new(AtomicUsize::new(0));

        let first_gate = gate.clone();
        let first_entered = entered.clone();
        let first = spawn_with_limit(limit.clone(), move || {
            first_entered.fetch_add(1, Ordering::SeqCst);
            let (lock, ready) = &*first_gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = ready.wait(released).unwrap();
            }
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while entered.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let second_entered = entered.clone();
        let second = spawn_with_limit(limit, move || {
            second_entered.fetch_add(1, Ordering::SeqCst);
        });
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert_eq!(entered.load(Ordering::SeqCst), 1);

        let (lock, ready) = &*gate;
        *lock.lock().unwrap() = true;
        ready.notify_all();
        first.await.unwrap();
        second.await.unwrap();
        assert_eq!(entered.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn native_panic_remains_a_join_error() {
        let handle = spawn_with_limit(Arc::new(tokio::sync::Semaphore::new(1)), || {
            panic!("native failure")
        });
        assert!(handle.await.unwrap_err().is_panic());
    }
}
