//! Persistent state for answering forwarded emails from Matrix.
//!
//! Three pieces live next to the email outbox in the same database:
//!
//! - `reply_targets`: which Matrix event each forwarded email became, and where
//!   an answer to it goes. Recorded together with the email's removal from the
//!   outbox, so a delivered email is always answerable, and only answerable
//!   for `max_age` afterwards.
//! - `reply_events`: every Matrix event already accepted as an answer, so a
//!   replayed sync (e.g. after a restart) never sends the same answer twice,
//!   and the source of the rolling-hour rate limit.
//! - `reply_outbox` / `reply_dead_letter`: answers waiting to be sent by email,
//!   with the same retry and dead-letter handling as the email outbox.

use super::store::{unix_now, MessageStore};
use crate::error::Result;
use rusqlite::{params, OptionalExtension};

/// Where a forwarded email can be answered: the Matrix event it was delivered
/// as, and the email headers an answer needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyTarget {
    pub event_id: String,
    pub room_id: String,
    pub reply_to: String,
    pub message_id: Option<String>,
    pub subject: Option<String>,
}

/// An answer waiting to be sent by email.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingReply {
    /// The Matrix event that asked for the answer to be sent.
    pub event_id: String,
    pub room_id: String,
    pub to: String,
    pub subject: String,
    pub in_reply_to: Option<String>,
    pub body: String,
}

/// An answer as it lives in the reply outbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredReply {
    pub id: i64,
    pub reply: OutgoingReply,
    pub attempts: u32,
}

/// What became of a request to queue an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyQueued {
    Queued(i64),
    /// This Matrix event was already accepted once; nothing was queued.
    Duplicate,
    /// The rolling-hour limit is reached; nothing was queued.
    RateLimited,
}

const HOUR_SECONDS: i64 = 3600;

impl MessageStore {
    /// Remove a delivered email from the outbox and, in the same transaction,
    /// record that it can be answered via `target`.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the transaction fails; then neither change
    /// is applied and the email stays queued.
    pub async fn mark_delivered_as(&self, id: i64, target: ReplyTarget) -> Result<()> {
        let created_at = unix_now();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            // A redelivery after a lost response gets the same event id back
            // from the homeserver, so replacing is idempotent.
            tx.execute(
                "INSERT OR REPLACE INTO reply_targets
                     (event_id, room_id, reply_to, message_id, subject, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    target.event_id,
                    target.room_id,
                    target.reply_to,
                    target.message_id,
                    target.subject,
                    created_at
                ],
            )?;
            tx.execute("DELETE FROM outbox WHERE id = ?1", params![id])?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// The answerable email delivered as `event_id`, unless it is older than
    /// `max_age_seconds` or unknown.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the query fails.
    pub async fn reply_target(
        &self,
        event_id: &str,
        max_age_seconds: i64,
    ) -> Result<Option<ReplyTarget>> {
        let event_id = event_id.to_string();
        let oldest = unix_now() - max_age_seconds;
        self.with_conn(move |conn| {
            let target = conn
                .query_row(
                    "SELECT event_id, room_id, reply_to, message_id, subject
                     FROM reply_targets
                     WHERE event_id = ?1 AND created_at >= ?2",
                    params![event_id, oldest],
                    |row| {
                        Ok(ReplyTarget {
                            event_id: row.get(0)?,
                            room_id: row.get(1)?,
                            reply_to: row.get(2)?,
                            message_id: row.get(3)?,
                            subject: row.get(4)?,
                        })
                    },
                )
                .optional()?;
            Ok(target)
        })
        .await
    }

    /// Queue `reply` for sending, unless its Matrix event was already accepted
    /// or `max_per_hour` answers were accepted in the last hour. Wakes the
    /// reply worker when something was queued.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the transaction fails; then nothing is
    /// recorded and the same event can be retried.
    pub async fn enqueue_reply(
        &self,
        reply: &OutgoingReply,
        max_per_hour: u32,
    ) -> Result<ReplyQueued> {
        let reply = reply.clone();
        let now = unix_now();
        let outcome = self
            .with_conn(move |conn| {
                let tx = conn.unchecked_transaction()?;
                let seen: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM reply_events WHERE event_id = ?1)",
                    params![reply.event_id],
                    |row| row.get(0),
                )?;
                if seen {
                    return Ok(ReplyQueued::Duplicate);
                }
                let last_hour: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM reply_events WHERE accepted_at > ?1",
                    params![now - HOUR_SECONDS],
                    |row| row.get(0),
                )?;
                if last_hour >= i64::from(max_per_hour) {
                    return Ok(ReplyQueued::RateLimited);
                }
                tx.execute(
                    "INSERT INTO reply_events (event_id, accepted_at) VALUES (?1, ?2)",
                    params![reply.event_id, now],
                )?;
                tx.execute(
                    "INSERT INTO reply_outbox
                         (event_id, room_id, to_addr, subject, in_reply_to, body, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        reply.event_id,
                        reply.room_id,
                        reply.to,
                        reply.subject,
                        reply.in_reply_to,
                        reply.body,
                        now
                    ],
                )?;
                let id = tx.last_insert_rowid();
                tx.commit()?;
                Ok(ReplyQueued::Queued(id))
            })
            .await?;

        if matches!(outcome, ReplyQueued::Queued(_)) {
            self.reply_notify.notify_one();
        }
        Ok(outcome)
    }

    /// The earliest queued answer that is due (`next_attempt_at <= now`).
    ///
    /// # Errors
    ///
    /// Returns a storage error if the query fails.
    pub async fn claim_next_reply(&self, now: i64) -> Result<Option<StoredReply>> {
        self.with_conn(move |conn| {
            let stored = conn
                .query_row(
                    "SELECT id, event_id, room_id, to_addr, subject, in_reply_to, body, attempts
                     FROM reply_outbox
                     WHERE next_attempt_at <= ?1
                     ORDER BY next_attempt_at ASC, id ASC
                     LIMIT 1",
                    params![now],
                    |row| {
                        let attempts: i64 = row.get(7)?;
                        Ok(StoredReply {
                            id: row.get(0)?,
                            reply: OutgoingReply {
                                event_id: row.get(1)?,
                                room_id: row.get(2)?,
                                to: row.get(3)?,
                                subject: row.get(4)?,
                                in_reply_to: row.get(5)?,
                                body: row.get(6)?,
                            },
                            attempts: attempts as u32,
                        })
                    },
                )
                .optional()?;
            Ok(stored)
        })
        .await
    }

    /// Remove a sent answer from the reply outbox.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the delete fails.
    pub async fn mark_reply_sent(&self, id: i64) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute("DELETE FROM reply_outbox WHERE id = ?1", params![id])?;
            Ok(())
        })
        .await
    }

    /// Record a failed send of an answer and schedule the next attempt.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the update fails.
    pub async fn reschedule_reply(
        &self,
        id: i64,
        attempts: u32,
        next_attempt_at: i64,
    ) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE reply_outbox SET attempts = ?2, next_attempt_at = ?3 WHERE id = ?1",
                params![id, attempts, next_attempt_at],
            )?;
            Ok(())
        })
        .await
    }

    /// Move an answer that exhausted its retries to `reply_dead_letter`.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the transaction fails; then the answer stays
    /// in the reply outbox.
    pub async fn dead_letter_reply(&self, id: i64, last_error: &str) -> Result<()> {
        let last_error = last_error.to_string();
        let failed_at = unix_now();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO reply_dead_letter
                     (event_id, room_id, to_addr, subject, in_reply_to, body,
                      attempts, created_at, failed_at, last_error)
                 SELECT event_id, room_id, to_addr, subject, in_reply_to, body,
                        attempts, created_at, ?2, ?3
                 FROM reply_outbox WHERE id = ?1",
                params![id, failed_at, last_error],
            )?;
            tx.execute("DELETE FROM reply_outbox WHERE id = ?1", params![id])?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// When the next backing-off answer becomes due, if any.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the query fails.
    pub async fn next_reply_wakeup(&self, now: i64) -> Result<Option<i64>> {
        self.with_conn(move |conn| {
            let at: Option<i64> = conn.query_row(
                "SELECT MIN(next_attempt_at) FROM reply_outbox WHERE next_attempt_at > ?1",
                params![now],
                |row| row.get(0),
            )?;
            Ok(at)
        })
        .await
    }

    /// Number of answers in the reply dead-letter table.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the query fails.
    pub async fn reply_dead_letter_len(&self) -> Result<usize> {
        self.with_conn(|conn| {
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM reply_dead_letter", [], |row| {
                    row.get(0)
                })?;
            Ok(count as usize)
        })
        .await
    }

    /// Forget reply targets and accepted reply events older than
    /// `max_age_seconds`. An event is always newer than the email it answers,
    /// so once an event's record is pruned its target is gone too, and a
    /// replay of that event cannot be accepted again.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the deletes fail.
    pub async fn prune_replies(&self, max_age_seconds: i64) -> Result<()> {
        let oldest = unix_now() - max_age_seconds;
        self.with_conn(move |conn| {
            conn.execute(
                "DELETE FROM reply_targets WHERE created_at < ?1",
                params![oldest],
            )?;
            conn.execute(
                "DELETE FROM reply_events WHERE accepted_at < ?1",
                params![oldest],
            )?;
            Ok(())
        })
        .await
    }

    /// Resolves the next time an answer is queued.
    pub async fn reply_notified(&self) {
        self.reply_notify.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::Message;

    fn target(event_id: &str) -> ReplyTarget {
        ReplyTarget {
            event_id: event_id.to_string(),
            room_id: "!room:example.org".to_string(),
            reply_to: "app@example.com".to_string(),
            message_id: Some("<1@example.com>".to_string()),
            subject: Some("ping".to_string()),
        }
    }

    fn reply(event_id: &str) -> OutgoingReply {
        OutgoingReply {
            event_id: event_id.to_string(),
            room_id: "!room:example.org".to_string(),
            to: "app@example.com".to_string(),
            subject: "Re: ping".to_string(),
            in_reply_to: Some("<1@example.com>".to_string()),
            body: "pong".to_string(),
        }
    }

    #[tokio::test]
    async fn delivery_records_an_answerable_target_atomically() {
        let store = MessageStore::open_in_memory().await.unwrap();
        let id = store.enqueue(&Message::default()).await.unwrap();

        store.mark_delivered_as(id, target("$e1")).await.unwrap();

        assert_eq!(store.len().await.unwrap(), 0);
        assert_eq!(
            store.reply_target("$e1", 3600).await.unwrap(),
            Some(target("$e1"))
        );
        assert_eq!(store.reply_target("$other", 3600).await.unwrap(), None);
    }

    #[tokio::test]
    async fn targets_expire_after_max_age() {
        let store = MessageStore::open_in_memory().await.unwrap();
        let id = store.enqueue(&Message::default()).await.unwrap();
        store.mark_delivered_as(id, target("$e1")).await.unwrap();

        // A negative age puts the cut-off in the future: everything is too old.
        assert_eq!(store.reply_target("$e1", -10).await.unwrap(), None);

        store.prune_replies(-10).await.unwrap();
        assert_eq!(store.reply_target("$e1", 3600).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_reply_event_is_queued_only_once() {
        let store = MessageStore::open_in_memory().await.unwrap();
        assert!(matches!(
            store.enqueue_reply(&reply("$r1"), 10).await.unwrap(),
            ReplyQueued::Queued(_)
        ));
        assert_eq!(
            store.enqueue_reply(&reply("$r1"), 10).await.unwrap(),
            ReplyQueued::Duplicate
        );

        // Still a duplicate after it was sent and left the outbox.
        let claimed = store.claim_next_reply(unix_now()).await.unwrap().unwrap();
        store.mark_reply_sent(claimed.id).await.unwrap();
        assert_eq!(
            store.enqueue_reply(&reply("$r1"), 10).await.unwrap(),
            ReplyQueued::Duplicate
        );
    }

    #[tokio::test]
    async fn replies_are_rate_limited_per_rolling_hour() {
        let store = MessageStore::open_in_memory().await.unwrap();
        for event in ["$a", "$b"] {
            assert!(matches!(
                store.enqueue_reply(&reply(event), 2).await.unwrap(),
                ReplyQueued::Queued(_)
            ));
        }
        assert_eq!(
            store.enqueue_reply(&reply("$c"), 2).await.unwrap(),
            ReplyQueued::RateLimited
        );
        // A rate-limited event is not recorded, so it is not a duplicate later.
        assert_eq!(
            store.enqueue_reply(&reply("$c"), 3).await.unwrap(),
            ReplyQueued::Queued(3)
        );
    }

    #[tokio::test]
    async fn reply_outbox_roundtrips_and_backs_off() {
        let store = MessageStore::open_in_memory().await.unwrap();
        store.enqueue_reply(&reply("$r1"), 10).await.unwrap();
        let now = unix_now();

        let claimed = store.claim_next_reply(now).await.unwrap().unwrap();
        assert_eq!(claimed.reply, reply("$r1"));
        assert_eq!(claimed.attempts, 0);

        store
            .reschedule_reply(claimed.id, 1, now + 60)
            .await
            .unwrap();
        assert!(store.claim_next_reply(now).await.unwrap().is_none());
        assert_eq!(store.next_reply_wakeup(now).await.unwrap(), Some(now + 60));
        let again = store.claim_next_reply(now + 60).await.unwrap().unwrap();
        assert_eq!(again.attempts, 1);
    }

    #[tokio::test]
    async fn dead_lettered_replies_leave_the_outbox() {
        let store = MessageStore::open_in_memory().await.unwrap();
        store.enqueue_reply(&reply("$r1"), 10).await.unwrap();
        let claimed = store.claim_next_reply(unix_now()).await.unwrap().unwrap();

        store
            .dead_letter_reply(claimed.id, "550 no such user")
            .await
            .unwrap();

        assert!(store.claim_next_reply(unix_now()).await.unwrap().is_none());
        assert_eq!(store.reply_dead_letter_len().await.unwrap(), 1);
    }
}
