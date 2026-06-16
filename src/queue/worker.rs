use super::store::{unix_now, Message, MessageStore, StoredMessage};
use crate::error::Result;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::{error, info, warn};

/// A boxed, `Send` future returning a delivery result. Used by [`MessageSink`]
/// so the worker can stay generic without requiring `async fn` in traits
/// (which is unavailable on the project's MSRV).
pub type DeliveryFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// Something that can deliver a message (e.g. the Matrix client). Abstracted so
/// the delivery worker can be tested without a live homeserver.
///
/// `idempotency_key` is stable for a given queued message across retries, so a
/// sink can deduplicate a redelivery (e.g. after a lost response).
pub trait MessageSink: Send + Sync {
    fn deliver<'a>(&'a self, message: &'a Message, idempotency_key: &'a str) -> DeliveryFuture<'a>;
}

/// Hard ceiling on the retry backoff, regardless of attempt count.
const MAX_BACKOFF_SECONDS: u64 = 900;

/// How long to sleep when the outbox is empty, relying on the enqueue
/// notification (or shutdown) to wake earlier.
const IDLE_SLEEP: Duration = Duration::from_secs(3600);

/// How long to wait after a transient storage error before retrying.
const STORAGE_ERROR_BACKOFF: Duration = Duration::from_secs(5);

/// Exponential backoff: `base * 2^(attempt-1)`, saturating and capped.
pub fn backoff_seconds(base: u64, attempt: u32, cap: u64) -> u64 {
    if attempt == 0 {
        return 0;
    }
    base.saturating_mul(2u64.saturating_pow(attempt - 1))
        .min(cap)
}

/// Drains the persistent outbox, delivering messages through a [`MessageSink`]
/// with non-blocking retries: a message that fails is rescheduled into the
/// future and the worker moves on to the next ready message, so one failing
/// message never blocks the rest of the queue.
pub struct DeliveryWorker<S> {
    store: MessageStore,
    sink: S,
    max_retries: u32,
    retry_backoff: u64,
}

impl<S: MessageSink> DeliveryWorker<S> {
    pub fn new(store: MessageStore, sink: S, max_retries: u32, retry_backoff: u64) -> Self {
        Self {
            store,
            sink,
            max_retries,
            retry_backoff,
        }
    }

    /// Run until `shutdown` flips to `true`. On shutdown the worker flushes
    /// every message that is currently due (graceful drain) without aborting an
    /// in-flight send, then exits; messages still backing off are left in the
    /// persistent outbox to be retried on the next start.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        loop {
            let now = unix_now();
            match self.store.claim_next_ready(now).await {
                Ok(Some(stored)) => self.deliver_one(stored, now).await,
                Ok(None) => {
                    if *shutdown.borrow() {
                        info!("delivery queue drained; worker stopping");
                        break;
                    }
                    let wait = self.idle_wait(now).await;
                    tokio::select! {
                        _ = self.store.notified() => {}
                        _ = sleep(wait) => {}
                        _ = shutdown.changed() => {}
                    }
                }
                Err(error) => {
                    error!(%error, "failed to read from the delivery queue; retrying shortly");
                    tokio::select! {
                        _ = sleep(STORAGE_ERROR_BACKOFF) => {}
                        _ = shutdown.changed() => {}
                    }
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    }

    /// How long to idle when nothing is ready: until the next backing-off
    /// message is due, or a long sleep if the outbox is empty.
    async fn idle_wait(&self, now: i64) -> Duration {
        match self.store.next_wakeup(now).await {
            Ok(Some(at)) => Duration::from_secs((at - now).max(0) as u64),
            Ok(None) => IDLE_SLEEP,
            Err(error) => {
                error!(%error, "failed to compute next retry time; backing off");
                STORAGE_ERROR_BACKOFF
            }
        }
    }

    async fn deliver_one(&self, stored: StoredMessage, now: i64) {
        let id = stored.id;
        // Stable across retries of this row, so the sink can dedupe a redelivery.
        let idempotency_key = format!("chimney-post-{id}");
        match self.sink.deliver(&stored.message, &idempotency_key).await {
            Ok(()) => match self.store.mark_delivered(id).await {
                Ok(()) => info!(id, "message delivered to Matrix"),
                Err(error) => error!(
                    id, %error,
                    "message delivered but could not be removed from the queue; it may be resent"
                ),
            },
            Err(error) => self.handle_failure(stored, now, error).await,
        }
    }

    async fn handle_failure(
        &self,
        stored: StoredMessage,
        now: i64,
        error: crate::error::ChimneyError,
    ) {
        let id = stored.id;
        let attempts = stored.attempts + 1;

        if attempts > self.max_retries {
            error!(
                id, attempts, %error,
                from = ?stored.message.from,
                subject = ?stored.message.subject,
                "giving up on message after exhausting retries; moving to dead-letter"
            );
            if let Err(dead_letter_error) = self.store.dead_letter(id, &error.to_string()).await {
                error!(id, error = %dead_letter_error, "failed to move message to dead-letter");
            }
            return;
        }

        let backoff = backoff_seconds(self.retry_backoff, attempts, MAX_BACKOFF_SECONDS);
        warn!(
            id, attempts, backoff_seconds = backoff, %error,
            "Matrix delivery failed; scheduling retry"
        );
        if let Err(error) = self
            .store
            .reschedule(id, attempts, now + backoff as i64)
            .await
        {
            error!(id, %error, "failed to reschedule message for retry");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ChimneyError;
    use crate::queue::store::unix_now;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct FakeInner {
        delivered: Mutex<Vec<Message>>,
        keys: Mutex<Vec<String>>,
        calls: AtomicUsize,
        fail_first: AtomicUsize,
        always_fail: bool,
        fail_if_body_contains: Option<String>,
    }

    #[derive(Clone, Default)]
    struct FakeSink {
        inner: Arc<FakeInner>,
    }

    impl FakeSink {
        fn failing_first(n: usize) -> Self {
            let inner = FakeInner {
                fail_first: AtomicUsize::new(n),
                ..Default::default()
            };
            Self {
                inner: Arc::new(inner),
            }
        }

        fn always_failing() -> Self {
            Self {
                inner: Arc::new(FakeInner {
                    always_fail: true,
                    ..Default::default()
                }),
            }
        }

        fn failing_bodies_containing(pattern: &str) -> Self {
            Self {
                inner: Arc::new(FakeInner {
                    fail_if_body_contains: Some(pattern.to_string()),
                    ..Default::default()
                }),
            }
        }

        fn calls(&self) -> usize {
            self.inner.calls.load(Ordering::SeqCst)
        }

        fn delivered_bodies(&self) -> Vec<String> {
            self.inner
                .delivered
                .lock()
                .unwrap()
                .iter()
                .map(|m| m.body.clone())
                .collect()
        }

        fn keys(&self) -> Vec<String> {
            self.inner.keys.lock().unwrap().clone()
        }
    }

    impl MessageSink for FakeSink {
        fn deliver<'a>(
            &'a self,
            message: &'a Message,
            idempotency_key: &'a str,
        ) -> DeliveryFuture<'a> {
            let inner = Arc::clone(&self.inner);
            let message = message.clone();
            let key = idempotency_key.to_string();
            Box::pin(async move {
                inner.calls.fetch_add(1, Ordering::SeqCst);
                inner.keys.lock().unwrap().push(key);

                if inner.always_fail {
                    return Err(ChimneyError::Matrix("forced failure".into()));
                }
                if let Some(pattern) = &inner.fail_if_body_contains {
                    if message.body.contains(pattern) {
                        return Err(ChimneyError::Matrix("forced failure".into()));
                    }
                }
                if inner.fail_first.load(Ordering::SeqCst) > 0 {
                    inner.fail_first.fetch_sub(1, Ordering::SeqCst);
                    return Err(ChimneyError::Matrix("forced failure".into()));
                }

                inner.delivered.lock().unwrap().push(message);
                Ok(())
            })
        }
    }

    fn msg(body: &str) -> Message {
        Message {
            from: Some("s@x.com".into()),
            to: vec!["r@y.com".into()],
            subject: Some("s".into()),
            body: body.into(),
        }
    }

    /// Spawn the worker and return a handle plus the shutdown sender.
    fn spawn_worker<S: MessageSink + 'static>(
        store: MessageStore,
        sink: S,
        max_retries: u32,
        retry_backoff: u64,
        shutdown_initial: bool,
    ) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
        let (tx, rx) = watch::channel(shutdown_initial);
        let worker = DeliveryWorker::new(store, sink, max_retries, retry_backoff);
        (tx, tokio::spawn(worker.run(rx)))
    }

    /// Poll `store.len()` until it reaches `target` or the timeout elapses.
    async fn wait_for_len(store: &MessageStore, target: usize) {
        let deadline = Duration::from_secs(5);
        let result = tokio::time::timeout(deadline, async {
            loop {
                if store.len().await.unwrap() == target {
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "queue length did not reach {target} within {deadline:?} (was {})",
            store.len().await.unwrap()
        );
    }

    #[test]
    fn backoff_grows_exponentially_and_caps() {
        assert_eq!(backoff_seconds(60, 0, 900), 0);
        assert_eq!(backoff_seconds(60, 1, 900), 60);
        assert_eq!(backoff_seconds(60, 2, 900), 120);
        assert_eq!(backoff_seconds(60, 3, 900), 240);
        // Capped.
        assert_eq!(backoff_seconds(60, 10, 900), 900);
        // A zero base stays zero (used for fast retries in tests).
        assert_eq!(backoff_seconds(0, 5, 900), 0);
    }

    #[tokio::test]
    async fn delivers_and_removes_a_message() {
        let store = MessageStore::open_in_memory().await.unwrap();
        store.enqueue(&msg("hello")).await.unwrap();
        let sink = FakeSink::default();

        let (tx, handle) = spawn_worker(store.clone(), sink.clone(), 5, 60, false);
        wait_for_len(&store, 0).await;

        tx.send(true).unwrap();
        handle.await.unwrap();

        assert_eq!(sink.delivered_bodies(), vec!["hello".to_string()]);
    }

    #[tokio::test]
    async fn retries_until_delivery_succeeds() {
        let store = MessageStore::open_in_memory().await.unwrap();
        store.enqueue(&msg("flaky")).await.unwrap();
        // Fail twice, then succeed; zero backoff so retries are immediate.
        let sink = FakeSink::failing_first(2);

        let (tx, handle) = spawn_worker(store.clone(), sink.clone(), 5, 0, false);
        wait_for_len(&store, 0).await;

        tx.send(true).unwrap();
        handle.await.unwrap();

        assert_eq!(sink.calls(), 3, "two failures then one success");
        assert_eq!(sink.delivered_bodies(), vec!["flaky".to_string()]);
    }

    #[tokio::test]
    async fn dead_letters_message_after_exhausting_retries() {
        let store = MessageStore::open_in_memory().await.unwrap();
        store.enqueue(&msg("doomed")).await.unwrap();
        let sink = FakeSink::always_failing();

        // max_retries=2 => initial + 2 retries = 3 attempts, then dead-letter.
        let (tx, handle) = spawn_worker(store.clone(), sink.clone(), 2, 0, false);
        wait_for_len(&store, 0).await;

        tx.send(true).unwrap();
        handle.await.unwrap();

        assert_eq!(sink.calls(), 3, "initial attempt plus two retries");
        assert_eq!(
            store.dead_letter_len().await.unwrap(),
            1,
            "exhausted message must be moved to dead-letter, not dropped"
        );
    }

    #[tokio::test]
    async fn a_failing_message_does_not_block_others() {
        let store = MessageStore::open_in_memory().await.unwrap();
        // The first message always fails and, with a large backoff, parks far
        // in the future. The second must still be delivered promptly.
        store.enqueue(&msg("bad")).await.unwrap();
        store.enqueue(&msg("good")).await.unwrap();
        let sink = FakeSink::failing_bodies_containing("bad");

        let (tx, handle) = spawn_worker(store.clone(), sink.clone(), 5, 1000, false);

        // The good message is delivered, leaving exactly the parked bad one.
        wait_for_len(&store, 1).await;
        assert_eq!(sink.delivered_bodies(), vec!["good".to_string()]);

        tx.send(true).unwrap();
        handle.await.unwrap();

        // The bad message is still persisted for a future retry.
        assert_eq!(store.len().await.unwrap(), 1);
        let remaining = store
            .claim_next_ready(unix_now() + 2000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(remaining.message.body, "bad");
    }

    #[tokio::test]
    async fn graceful_drain_flushes_all_ready_messages() {
        let store = MessageStore::open_in_memory().await.unwrap();
        for i in 0..5 {
            store.enqueue(&msg(&format!("m{i}"))).await.unwrap();
        }
        let sink = FakeSink::default();

        // Start already shutting down: the worker must still flush the backlog.
        let (_tx, handle) = spawn_worker(store.clone(), sink.clone(), 5, 60, true);
        handle.await.unwrap();

        assert_eq!(store.len().await.unwrap(), 0);
        assert_eq!(sink.delivered_bodies().len(), 5);
    }

    #[tokio::test]
    async fn shutdown_leaves_parked_messages_persisted() {
        let store = MessageStore::open_in_memory().await.unwrap();
        store.enqueue(&msg("retry-me")).await.unwrap();
        let sink = FakeSink::always_failing();

        // Large backoff so the message parks after its first failure.
        let (_tx, handle) = spawn_worker(store.clone(), sink.clone(), 5, 1000, true);
        handle.await.unwrap();

        // One delivery was attempted; the message remains for a future retry.
        assert_eq!(sink.calls(), 1);
        assert_eq!(store.len().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn reuses_a_stable_idempotency_key_across_retries() {
        let store = MessageStore::open_in_memory().await.unwrap();
        let id = store.enqueue(&msg("k")).await.unwrap();
        // Fail once, then succeed; zero backoff for an immediate retry.
        let sink = FakeSink::failing_first(1);

        let (tx, handle) = spawn_worker(store.clone(), sink.clone(), 5, 0, false);
        wait_for_len(&store, 0).await;
        tx.send(true).unwrap();
        handle.await.unwrap();

        let keys = sink.keys();
        assert_eq!(keys.len(), 2, "one failed attempt plus one success");
        assert_eq!(keys[0], keys[1], "the retry must reuse the same key");
        assert_eq!(keys[0], format!("chimney-post-{id}"));
    }

    #[tokio::test]
    async fn distinct_messages_get_distinct_idempotency_keys() {
        let store = MessageStore::open_in_memory().await.unwrap();
        let id1 = store.enqueue(&msg("a")).await.unwrap();
        let id2 = store.enqueue(&msg("b")).await.unwrap();
        let sink = FakeSink::default();

        let (tx, handle) = spawn_worker(store.clone(), sink.clone(), 5, 60, false);
        wait_for_len(&store, 0).await;
        tx.send(true).unwrap();
        handle.await.unwrap();

        let mut keys = sink.keys();
        keys.sort();
        assert_eq!(
            keys,
            vec![format!("chimney-post-{id1}"), format!("chimney-post-{id2}")]
        );
    }

    #[tokio::test]
    async fn idle_worker_stops_on_shutdown() {
        let store = MessageStore::open_in_memory().await.unwrap();
        let sink = FakeSink::default();
        let (tx, handle) = spawn_worker(store, sink, 5, 60, false);

        // Worker is idle (empty queue). Shutdown must wake and stop it.
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("worker did not stop on shutdown")
            .unwrap();
    }
}
