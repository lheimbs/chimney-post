use crate::error::{ChimneyError, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

/// A parsed email ready to be forwarded to Matrix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub from: Option<String>,
    pub to: Vec<String>,
    pub subject: Option<String>,
    pub body: String,
}

/// A message as it lives in the persistent outbox, with its row id and the
/// number of delivery attempts that have already failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredMessage {
    pub id: i64,
    pub message: Message,
    pub attempts: u32,
}

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
CREATE TABLE IF NOT EXISTS outbox (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    from_addr       TEXT,
    to_addrs        TEXT NOT NULL,
    subject         TEXT,
    body            TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_outbox_ready ON outbox(next_attempt_at, id);
CREATE TABLE IF NOT EXISTS dead_letter (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    from_addr       TEXT,
    to_addrs        TEXT NOT NULL,
    subject         TEXT,
    body            TEXT NOT NULL,
    attempts        INTEGER NOT NULL,
    created_at      INTEGER NOT NULL,
    failed_at       INTEGER NOT NULL,
    last_error      TEXT NOT NULL
);
";

/// A durable, SQLite-backed message outbox.
///
/// Messages are persisted before the SMTP server acknowledges them, and only
/// removed once delivery to Matrix succeeds (ack-after-delivery), so a crash,
/// restart, or shutdown never silently loses an accepted email. The store is
/// cheap to clone -- clones share the same database connection and new-message
/// notifier.
#[derive(Clone)]
pub struct MessageStore {
    conn: Arc<Mutex<Connection>>,
    notify: Arc<Notify>,
    /// Maximum number of messages allowed in the outbox; 0 means unlimited.
    max_len: usize,
}

impl MessageStore {
    /// Open (creating if needed) the outbox database at `path`, running
    /// migrations. Parent directories are created automatically. Pass
    /// `":memory:"` for an ephemeral in-process database (used by tests).
    /// The outbox is unbounded; use [`open_with_max_len`] to cap it.
    pub async fn open(path: &str) -> Result<Self> {
        Self::open_with_max_len(path, 0).await
    }

    /// Like [`open`], but reject `enqueue` with [`ChimneyError::QueueFull`] once
    /// the outbox holds `max_len` messages (`0` means unlimited).
    pub async fn open_with_max_len(path: &str, max_len: usize) -> Result<Self> {
        let path = path.to_string();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            if path != ":memory:" {
                if let Some(parent) = Path::new(&path).parent() {
                    if !parent.as_os_str().is_empty() {
                        std::fs::create_dir_all(parent)?;
                    }
                }
            }
            let conn = Connection::open(&path)?;
            conn.execute_batch(SCHEMA)?;
            Ok(conn)
        })
        .await
        .map_err(|error| ChimneyError::Queue(format!("storage task failed: {error}")))??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            notify: Arc::new(Notify::new()),
            max_len,
        })
    }

    /// Open an in-memory database. Convenience wrapper around [`open`].
    #[cfg(test)]
    pub async fn open_in_memory() -> Result<Self> {
        Self::open(":memory:").await
    }

    /// Persist a message to the outbox, returning its row id. Wakes the
    /// delivery worker. The returned future only resolves once the row is
    /// committed to disk, so callers may acknowledge to the sender afterwards.
    pub async fn enqueue(&self, message: &Message) -> Result<i64> {
        let from = message.from.clone();
        let to = encode_recipients(&message.to);
        let subject = message.subject.clone();
        let body = message.body.clone();
        let created_at = unix_now();
        let max_len = self.max_len;

        let id = self
            .with_conn(move |conn| {
                // Enforce the cap and insert under the same connection lock so the
                // count cannot race with a concurrent enqueue.
                if max_len > 0 {
                    let count: i64 =
                        conn.query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))?;
                    if count as usize >= max_len {
                        return Err(ChimneyError::QueueFull);
                    }
                }
                conn.execute(
                    "INSERT INTO outbox (from_addr, to_addrs, subject, body, attempts, next_attempt_at, created_at)
                     VALUES (?1, ?2, ?3, ?4, 0, 0, ?5)",
                    params![from, to, subject, body, created_at],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await?;

        self.notify.notify_one();
        Ok(id)
    }

    /// Fetch the earliest message whose `next_attempt_at` is due (`<= now`),
    /// ordered so that messages currently backing off are skipped in favour of
    /// ready ones. Does not remove the message; call [`mark_delivered`] or
    /// [`reschedule`] afterwards.
    pub async fn claim_next_ready(&self, now: i64) -> Result<Option<StoredMessage>> {
        self.with_conn(move |conn| {
            let stored = conn
                .query_row(
                    "SELECT id, from_addr, to_addrs, subject, body, attempts
                     FROM outbox
                     WHERE next_attempt_at <= ?1
                     ORDER BY next_attempt_at ASC, id ASC
                     LIMIT 1",
                    params![now],
                    row_to_stored,
                )
                .optional()?;
            Ok(stored)
        })
        .await
    }

    /// Remove a successfully delivered message from the outbox.
    pub async fn mark_delivered(&self, id: i64) -> Result<()> {
        self.remove(id).await
    }

    /// Move a message that has exhausted its retries out of the active outbox
    /// into the `dead_letter` table (recording the last error), so a permanently
    /// undeliverable alert is retained for inspection rather than lost. The
    /// insert and delete run in a single transaction so a crash between them
    /// cannot leave the message in both tables.
    pub async fn dead_letter(&self, id: i64, last_error: &str) -> Result<()> {
        let last_error = last_error.to_string();
        let failed_at = unix_now();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO dead_letter
                     (from_addr, to_addrs, subject, body, attempts, created_at, failed_at, last_error)
                 SELECT from_addr, to_addrs, subject, body, attempts, created_at, ?2, ?3
                 FROM outbox WHERE id = ?1",
                params![id, failed_at, last_error],
            )?;
            tx.execute("DELETE FROM outbox WHERE id = ?1", params![id])?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Number of messages in the dead-letter table.
    pub async fn dead_letter_len(&self) -> Result<usize> {
        self.with_conn(|conn| {
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM dead_letter", [], |row| row.get(0))?;
            Ok(count as usize)
        })
        .await
    }

    async fn remove(&self, id: i64) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute("DELETE FROM outbox WHERE id = ?1", params![id])?;
            Ok(())
        })
        .await
    }

    /// Record a failed attempt and schedule the next retry.
    pub async fn reschedule(&self, id: i64, attempts: u32, next_attempt_at: i64) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE outbox SET attempts = ?2, next_attempt_at = ?3 WHERE id = ?1",
                params![id, attempts, next_attempt_at],
            )?;
            Ok(())
        })
        .await
    }

    /// The earliest `next_attempt_at` strictly in the future, i.e. when the
    /// next backing-off message becomes due. `None` if the outbox is empty or
    /// every message is already due.
    pub async fn next_wakeup(&self, now: i64) -> Result<Option<i64>> {
        self.with_conn(move |conn| {
            let at: Option<i64> = conn.query_row(
                "SELECT MIN(next_attempt_at) FROM outbox WHERE next_attempt_at > ?1",
                params![now],
                |row| row.get(0),
            )?;
            Ok(at)
        })
        .await
    }

    /// Number of messages currently in the outbox.
    pub async fn len(&self) -> Result<usize> {
        self.with_conn(|conn| {
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))?;
            Ok(count as usize)
        })
        .await
    }

    /// Resolves the next time a message is enqueued.
    pub async fn notified(&self) {
        self.notify.notified().await;
    }

    /// Run a closure with exclusive access to the database connection on a
    /// blocking thread, keeping SQLite work off the async runtime.
    async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            // Recover from a poisoned mutex rather than propagating the panic:
            // the SQLite connection is still usable, so one panicking operation
            // must not permanently wedge every future enqueue and delivery.
            let guard = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            f(&guard)
        })
        .await
        .map_err(|error| ChimneyError::Queue(format!("storage task failed: {error}")))?
    }
}

fn row_to_stored(row: &rusqlite::Row) -> rusqlite::Result<StoredMessage> {
    let attempts: i64 = row.get(5)?;
    Ok(StoredMessage {
        id: row.get(0)?,
        message: Message {
            from: row.get(1)?,
            to: decode_recipients(&row.get::<_, String>(2)?),
            subject: row.get(3)?,
            body: row.get(4)?,
        },
        attempts: attempts as u32,
    })
}

/// Recipients are stored newline-joined; email addresses cannot contain a
/// newline, so this is unambiguous.
fn encode_recipients(to: &[String]) -> String {
    to.join("\n")
}

fn decode_recipients(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        Vec::new()
    } else {
        raw.split('\n').map(str::to_string).collect()
    }
}

/// Current wall-clock time in whole seconds since the Unix epoch.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(body: &str, to: &[&str]) -> Message {
        Message {
            from: Some("sender@example.com".to_string()),
            to: to.iter().map(|s| s.to_string()).collect(),
            subject: Some("subj".to_string()),
            body: body.to_string(),
        }
    }

    #[tokio::test]
    async fn enqueue_then_claim_roundtrips_all_fields() {
        let store = MessageStore::open_in_memory().await.unwrap();
        let id = store
            .enqueue(&msg("hello", &["a@x.com", "b@y.com"]))
            .await
            .unwrap();

        let claimed = store.claim_next_ready(unix_now()).await.unwrap().unwrap();
        assert_eq!(claimed.id, id);
        assert_eq!(claimed.attempts, 0);
        assert_eq!(claimed.message, msg("hello", &["a@x.com", "b@y.com"]));
    }

    #[tokio::test]
    async fn claim_returns_messages_in_fifo_order() {
        let store = MessageStore::open_in_memory().await.unwrap();
        store.enqueue(&msg("first", &["a@x.com"])).await.unwrap();
        store.enqueue(&msg("second", &["a@x.com"])).await.unwrap();

        let first = store.claim_next_ready(unix_now()).await.unwrap().unwrap();
        assert_eq!(first.message.body, "first");
    }

    #[tokio::test]
    async fn mark_delivered_removes_the_message() {
        let store = MessageStore::open_in_memory().await.unwrap();
        let id = store.enqueue(&msg("bye", &["a@x.com"])).await.unwrap();
        assert_eq!(store.len().await.unwrap(), 1);

        store.mark_delivered(id).await.unwrap();
        assert_eq!(store.len().await.unwrap(), 0);
        assert!(store.claim_next_ready(unix_now()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rescheduled_message_is_not_claimed_until_due() {
        let store = MessageStore::open_in_memory().await.unwrap();
        let id = store.enqueue(&msg("retry", &["a@x.com"])).await.unwrap();
        let now = unix_now();

        store.reschedule(id, 1, now + 100).await.unwrap();

        // Not yet due.
        assert!(store.claim_next_ready(now).await.unwrap().is_none());
        // The recorded attempt count and future due time are reflected.
        assert_eq!(store.next_wakeup(now).await.unwrap(), Some(now + 100));
        // Due in the future.
        let claimed = store.claim_next_ready(now + 100).await.unwrap().unwrap();
        assert_eq!(claimed.attempts, 1);
    }

    #[tokio::test]
    async fn ready_message_is_preferred_over_a_parked_one() {
        let store = MessageStore::open_in_memory().await.unwrap();
        let now = unix_now();
        let parked = store.enqueue(&msg("parked", &["a@x.com"])).await.unwrap();
        store.reschedule(parked, 1, now + 1000).await.unwrap();
        store.enqueue(&msg("ready", &["a@x.com"])).await.unwrap();

        let claimed = store.claim_next_ready(now).await.unwrap().unwrap();
        assert_eq!(claimed.message.body, "ready");
    }

    #[tokio::test]
    async fn next_wakeup_is_none_when_empty_or_all_due() {
        let store = MessageStore::open_in_memory().await.unwrap();
        let now = unix_now();
        assert_eq!(store.next_wakeup(now).await.unwrap(), None);

        store.enqueue(&msg("due", &["a@x.com"])).await.unwrap();
        // The message is due now (next_attempt_at = 0), so nothing is parked.
        assert_eq!(store.next_wakeup(now).await.unwrap(), None);
    }

    #[tokio::test]
    async fn messages_survive_reopening_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.db");
        let path = path.to_str().unwrap();

        {
            let store = MessageStore::open(path).await.unwrap();
            store
                .enqueue(&msg("durable", &["a@x.com", "b@y.com"]))
                .await
                .unwrap();
        } // store dropped, connection closed

        let store = MessageStore::open(path).await.unwrap();
        assert_eq!(store.len().await.unwrap(), 1);
        let claimed = store.claim_next_ready(unix_now()).await.unwrap().unwrap();
        assert_eq!(claimed.message, msg("durable", &["a@x.com", "b@y.com"]));
    }

    #[tokio::test]
    async fn open_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/sub/queue.db");
        let store = MessageStore::open(path.to_str().unwrap()).await.unwrap();
        store.enqueue(&msg("x", &["a@x.com"])).await.unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn enqueue_rejects_when_at_max_len() {
        let store = MessageStore::open_with_max_len(":memory:", 2)
            .await
            .unwrap();
        store.enqueue(&msg("a", &["x@x.com"])).await.unwrap();
        store.enqueue(&msg("b", &["x@x.com"])).await.unwrap();

        let err = store.enqueue(&msg("c", &["x@x.com"])).await.unwrap_err();
        assert!(matches!(err, ChimneyError::QueueFull));
        assert_eq!(store.len().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn removing_a_message_frees_a_slot() {
        let store = MessageStore::open_with_max_len(":memory:", 1)
            .await
            .unwrap();
        let id = store.enqueue(&msg("a", &["x@x.com"])).await.unwrap();
        assert!(matches!(
            store.enqueue(&msg("b", &["x@x.com"])).await.unwrap_err(),
            ChimneyError::QueueFull
        ));

        store.mark_delivered(id).await.unwrap();
        store.enqueue(&msg("b", &["x@x.com"])).await.unwrap();
        assert_eq!(store.len().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn max_len_zero_means_unlimited() {
        let store = MessageStore::open_with_max_len(":memory:", 0)
            .await
            .unwrap();
        for i in 0..50 {
            store
                .enqueue(&msg(&format!("m{i}"), &["x@x.com"]))
                .await
                .unwrap();
        }
        assert_eq!(store.len().await.unwrap(), 50);
    }

    #[tokio::test]
    async fn dead_letter_moves_message_out_of_the_outbox() {
        let store = MessageStore::open_in_memory().await.unwrap();
        let id = store.enqueue(&msg("doomed", &["x@x.com"])).await.unwrap();
        assert_eq!(store.len().await.unwrap(), 1);
        assert_eq!(store.dead_letter_len().await.unwrap(), 0);

        store.dead_letter(id, "permanent failure").await.unwrap();

        assert_eq!(store.len().await.unwrap(), 0);
        assert_eq!(store.dead_letter_len().await.unwrap(), 1);
        // It is no longer deliverable from the active queue.
        assert!(store.claim_next_ready(unix_now()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn dead_letter_on_unknown_id_is_a_noop() {
        let store = MessageStore::open_in_memory().await.unwrap();
        store.enqueue(&msg("keep", &["x@x.com"])).await.unwrap();

        // No such row: the transaction inserts nothing and deletes nothing.
        store.dead_letter(999, "no such row").await.unwrap();

        assert_eq!(store.len().await.unwrap(), 1);
        assert_eq!(store.dead_letter_len().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn recovers_from_a_poisoned_mutex() {
        let store = MessageStore::open_in_memory().await.unwrap();
        store.enqueue(&msg("before", &["x@x.com"])).await.unwrap();

        // Poison the connection mutex by panicking while holding it.
        let conn = Arc::clone(&store.conn);
        let _ = std::thread::spawn(move || {
            let _guard = conn.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();

        // Operations must still succeed despite the poisoned mutex.
        assert_eq!(store.len().await.unwrap(), 1);
        store.enqueue(&msg("after", &["x@x.com"])).await.unwrap();
        assert_eq!(store.len().await.unwrap(), 2);
    }

    #[test]
    fn recipient_encoding_roundtrips() {
        for case in [
            vec![],
            vec!["only@x.com".to_string()],
            vec!["a@x.com".to_string(), "b@y.com".to_string()],
        ] {
            assert_eq!(decode_recipients(&encode_recipients(&case)), case);
        }
    }
}
