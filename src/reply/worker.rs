//! Drains the reply outbox: sends each queued answer by email, retrying
//! transient failures with backoff, and tells the person who asked how it went.

use super::mail::{build_email, MailTransport};
use crate::config::Config;
use crate::error::{ChimneyError, Result};
use crate::queue::{backoff_seconds, MessageStore, OutgoingReply, StoredReply};
use lettre::message::Mailbox;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::{error, info, warn};

/// Hard ceiling on the retry backoff, matching the email delivery worker.
const MAX_BACKOFF_SECONDS: u64 = 900;

/// How often expired reply targets and accepted events are pruned. Also the
/// longest the worker idles, so pruning happens even when nothing is sent.
const PRUNE_INTERVAL: Duration = Duration::from_secs(3600);

/// How long to wait after a transient storage error before retrying.
const STORAGE_ERROR_BACKOFF: Duration = Duration::from_secs(5);

/// A boxed, `Send` future with no result (see [`crate::queue::DeliveryFuture`]
/// for why it is boxed).
pub type FeedbackFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Tells the person who asked for an answer whether it went out. Best effort:
/// an implementation logs its own failures, since the answer's fate is already
/// settled by the time it is called.
pub trait ReplyFeedback: Send + Sync {
    fn sent<'a>(&'a self, reply: &'a OutgoingReply) -> FeedbackFuture<'a>;
    fn failed<'a>(&'a self, reply: &'a OutgoingReply, reason: &'a str) -> FeedbackFuture<'a>;
}

/// Settings for a [`ReplyWorker`].
#[derive(Clone, Debug)]
pub struct ReplyWorkerConfig {
    /// `From` of every answer.
    pub from: Mailbox,
    pub max_retries: u32,
    pub retry_backoff: u64,
    /// How long reply targets and accepted events are kept.
    pub max_age_seconds: i64,
}

impl ReplyWorkerConfig {
    /// The worker settings for `config`'s `[reply]` section, retrying like
    /// `[queue]` does.
    ///
    /// # Errors
    ///
    /// Returns [`ChimneyError::Config`] if `reply.from` is not an address.
    pub fn from_config(config: &Config) -> Result<Self> {
        let from = config
            .reply
            .from
            .parse()
            .map_err(|error| ChimneyError::Config(format!("invalid reply.from: {error}")))?;
        Ok(Self {
            from,
            max_retries: config.queue.max_retries,
            retry_backoff: config.queue.retry_backoff,
            max_age_seconds: config.reply.max_age_seconds(),
        })
    }
}

/// Sends queued answers through a [`MailTransport`].
pub struct ReplyWorker<T, F> {
    store: MessageStore,
    transport: T,
    feedback: F,
    config: ReplyWorkerConfig,
}

impl<T: MailTransport, F: ReplyFeedback> ReplyWorker<T, F> {
    pub fn new(store: MessageStore, transport: T, feedback: F, config: ReplyWorkerConfig) -> Self {
        Self {
            store,
            transport,
            feedback,
            config,
        }
    }

    /// Run until `shutdown` flips to `true`, then finish sending whatever is
    /// due and return. Answers still backing off stay queued for the next
    /// start.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut last_prune: Option<Instant> = None;
        loop {
            if last_prune.is_none_or(|at| at.elapsed() >= PRUNE_INTERVAL) {
                if let Err(error) = self.store.prune_replies(self.config.max_age_seconds).await {
                    error!(%error, "failed to prune expired reply records");
                }
                last_prune = Some(Instant::now());
            }

            let now = crate::queue::unix_now();
            match self.store.claim_next_reply(now).await {
                Ok(Some(stored)) => self.send_one(stored, now).await,
                Ok(None) => {
                    if *shutdown.borrow() {
                        break;
                    }
                    let wait = self.idle_wait(now).await;
                    tokio::select! {
                        _ = self.store.reply_notified() => {}
                        _ = sleep(wait) => {}
                        _ = shutdown.changed() => {}
                    }
                }
                Err(error) => {
                    error!(%error, "failed to read from the reply queue; retrying shortly");
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

    async fn idle_wait(&self, now: i64) -> Duration {
        match self.store.next_reply_wakeup(now).await {
            Ok(Some(at)) => Duration::from_secs((at - now).max(0) as u64).min(PRUNE_INTERVAL),
            Ok(None) => PRUNE_INTERVAL,
            Err(error) => {
                error!(%error, "failed to compute next reply retry time; backing off");
                STORAGE_ERROR_BACKOFF
            }
        }
    }

    async fn send_one(&self, stored: StoredReply, now: i64) {
        let id = stored.id;
        let email = match build_email(&self.config.from, &stored.reply) {
            Ok(email) => email,
            Err(error) => {
                // Nothing about a retry would change the outcome.
                return self.give_up(&stored, &error.to_string()).await;
            }
        };

        match self.transport.send(email).await {
            Ok(()) => {
                if let Err(error) = self.store.mark_reply_sent(id).await {
                    error!(id, %error, "answer sent but could not be removed from the queue; it may be resent");
                }
                info!(id, "answer sent by email");
                self.feedback.sent(&stored.reply).await;
            }
            Err(failure) if failure.permanent => self.give_up(&stored, &failure.message).await,
            Err(failure) => {
                let attempts = stored.attempts + 1;
                if attempts > self.config.max_retries {
                    return self.give_up(&stored, &failure.message).await;
                }
                let backoff =
                    backoff_seconds(self.config.retry_backoff, attempts, MAX_BACKOFF_SECONDS);
                warn!(
                    id, attempts, backoff_seconds = backoff, error = %failure.message,
                    "sending answer failed; scheduling retry"
                );
                if let Err(error) = self
                    .store
                    .reschedule_reply(id, attempts, now + backoff as i64)
                    .await
                {
                    error!(id, %error, "failed to reschedule answer for retry");
                }
            }
        }
    }

    async fn give_up(&self, stored: &StoredReply, reason: &str) {
        let id = stored.id;
        error!(id, attempts = stored.attempts + 1, error = %reason, "giving up on answer; moving to dead-letter");
        if let Err(error) = self.store.dead_letter_reply(id, reason).await {
            error!(id, %error, "failed to move answer to dead-letter");
        }
        self.feedback.failed(&stored.reply, reason).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::ReplyQueued;
    use crate::reply::mail::{SendError, SendFuture};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Answers each send with the next scripted outcome (success once the
    /// script runs out) and records what was sent.
    #[derive(Clone, Default)]
    struct FakeTransport {
        script: Arc<Mutex<VecDeque<Option<SendError>>>>,
        sent: Arc<Mutex<Vec<String>>>,
    }

    impl FakeTransport {
        fn failing(failures: Vec<SendError>) -> Self {
            Self {
                script: Arc::new(Mutex::new(failures.into_iter().map(Some).collect())),
                ..Default::default()
            }
        }
    }

    impl MailTransport for FakeTransport {
        fn send(&self, email: lettre::Message) -> SendFuture<'_> {
            Box::pin(async move {
                let outcome = self.script.lock().unwrap().pop_front().flatten();
                match outcome {
                    Some(error) => Err(error),
                    None => {
                        let text = String::from_utf8(email.formatted()).unwrap();
                        self.sent.lock().unwrap().push(text);
                        Ok(())
                    }
                }
            })
        }
    }

    #[derive(Clone, Default)]
    struct FakeFeedback {
        events: Arc<Mutex<Vec<String>>>,
    }

    impl ReplyFeedback for FakeFeedback {
        fn sent<'a>(&'a self, reply: &'a OutgoingReply) -> FeedbackFuture<'a> {
            Box::pin(async move {
                self.events
                    .lock()
                    .unwrap()
                    .push(format!("sent {}", reply.event_id));
            })
        }

        fn failed<'a>(&'a self, reply: &'a OutgoingReply, reason: &'a str) -> FeedbackFuture<'a> {
            Box::pin(async move {
                self.events
                    .lock()
                    .unwrap()
                    .push(format!("failed {}: {reason}", reply.event_id));
            })
        }
    }

    fn transient(message: &str) -> SendError {
        SendError {
            message: message.to_string(),
            permanent: false,
        }
    }

    fn reply(to: &str) -> OutgoingReply {
        OutgoingReply {
            event_id: "$answer".to_string(),
            room_id: "!room:example.org".to_string(),
            to: to.to_string(),
            subject: "Re: ping".to_string(),
            in_reply_to: None,
            body: "pong".to_string(),
        }
    }

    /// Queue `reply`, run a worker until it has nothing left due (retries use
    /// a zero backoff so they are due at once), and return the store.
    async fn run_once(
        reply: OutgoingReply,
        transport: FakeTransport,
        feedback: FakeFeedback,
        max_retries: u32,
    ) -> MessageStore {
        let store = MessageStore::open_in_memory().await.unwrap();
        assert!(matches!(
            store.enqueue_reply(&reply, 10).await.unwrap(),
            ReplyQueued::Queued(_)
        ));
        let config = ReplyWorkerConfig {
            from: "chimney-post@example.org".parse().unwrap(),
            max_retries,
            retry_backoff: 0,
            max_age_seconds: 3600,
        };
        // Shutdown already requested: the worker drains what is due and exits.
        let (_tx, rx) = watch::channel(true);
        ReplyWorker::new(store.clone(), transport, feedback, config)
            .run(rx)
            .await;
        store
    }

    #[tokio::test]
    async fn sends_an_answer_and_reports_success() {
        let transport = FakeTransport::default();
        let feedback = FakeFeedback::default();
        let store = run_once(
            reply("app@example.com"),
            transport.clone(),
            feedback.clone(),
            3,
        )
        .await;

        let sent = transport.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("To: app@example.com\r\n"));
        assert_eq!(*feedback.events.lock().unwrap(), vec!["sent $answer"]);
        assert!(store
            .claim_next_reply(crate::queue::unix_now())
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn retries_transient_failures() {
        let transport =
            FakeTransport::failing(vec![transient("421 try later"), transient("timeout")]);
        let feedback = FakeFeedback::default();
        run_once(
            reply("app@example.com"),
            transport.clone(),
            feedback.clone(),
            3,
        )
        .await;

        assert_eq!(transport.sent.lock().unwrap().len(), 1);
        assert_eq!(*feedback.events.lock().unwrap(), vec!["sent $answer"]);
    }

    #[tokio::test]
    async fn dead_letters_after_exhausting_retries() {
        let transport =
            FakeTransport::failing(vec![transient("one"), transient("two"), transient("three")]);
        let feedback = FakeFeedback::default();
        let store = run_once(
            reply("app@example.com"),
            transport.clone(),
            feedback.clone(),
            2,
        )
        .await;

        assert!(transport.sent.lock().unwrap().is_empty());
        assert_eq!(store.reply_dead_letter_len().await.unwrap(), 1);
        assert_eq!(
            *feedback.events.lock().unwrap(),
            vec!["failed $answer: three"]
        );
    }

    #[tokio::test]
    async fn gives_up_at_once_on_a_permanent_failure() {
        let transport = FakeTransport::failing(vec![SendError {
            message: "550 no such user".to_string(),
            permanent: true,
        }]);
        let feedback = FakeFeedback::default();
        let store = run_once(
            reply("app@example.com"),
            transport.clone(),
            feedback.clone(),
            5,
        )
        .await;

        assert_eq!(store.reply_dead_letter_len().await.unwrap(), 1);
        assert_eq!(
            *feedback.events.lock().unwrap(),
            vec!["failed $answer: 550 no such user"]
        );
    }

    #[tokio::test]
    async fn gives_up_at_once_on_an_unbuildable_email() {
        let transport = FakeTransport::default();
        let feedback = FakeFeedback::default();
        let store = run_once(
            reply("not an address"),
            transport.clone(),
            feedback.clone(),
            5,
        )
        .await;

        assert!(transport.sent.lock().unwrap().is_empty());
        assert_eq!(store.reply_dead_letter_len().await.unwrap(), 1);
        assert_eq!(feedback.events.lock().unwrap().len(), 1);
    }
}
