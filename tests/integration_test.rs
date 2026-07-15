use chimney_post::config::{
    Config, LoggingConfig, MatrixConfig, MatrixCredentials, QueueConfig, SmtpConfig,
    DEFAULT_MESSAGE_TEMPLATE,
};
use chimney_post::queue::MessageStore;
use chimney_post::smtp::start_smtp_server;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn test_config(bind: &str, max_message_size: usize) -> Config {
    Config {
        smtp: SmtpConfig {
            bind: bind.to_string(),
            max_message_size,
            timeout: 5,
            max_connections: 100,
            max_session_seconds: 300,
        },
        matrix: MatrixConfig {
            homeserver: "https://example.org".to_string(),
            user_id: "@bot:example.org".to_string(),
            device_name: "chimney-post".to_string(),
            room_id: "!room:example.org".to_string(),
            store_path: "/tmp/matrix".to_string(),
            require_e2ee: true,
            message_template: DEFAULT_MESSAGE_TEMPLATE.to_string(),
            routes: Vec::new(),
            credentials: MatrixCredentials {
                password: Some("test".to_string()),
                access_token: None,
                device_id: None,
            },
        },
        logging: LoggingConfig {
            level: "info".to_string(),
            format: "pretty".to_string(),
        },
        queue: QueueConfig {
            max_retries: 5,
            retry_backoff: 60,
            db_path: ":memory:".to_string(),
            max_len: 0,
        },
    }
}

/// Read a full SMTP response (handles multi-line 250- responses).
async fn read_response(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> String {
    let mut response = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let done = line.len() < 4 || line.as_bytes()[3] == b' ';
        response.push_str(&line);
        if done {
            break;
        }
    }
    response
}

/// Spin up the SMTP server on a random port, return the bind address, the
/// backing store, and the server handle.
async fn start_test_server(
    config: Config,
) -> (
    String,
    MessageStore,
    tokio::task::JoinHandle<chimney_post::error::Result<()>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let bind = format!("127.0.0.1:{port}");
    let mut config = config;
    config.smtp.bind = bind.clone();
    let config = Arc::new(config);
    let store = MessageStore::open_with_max_len(":memory:", config.queue.max_len)
        .await
        .unwrap();
    let handle = tokio::spawn(start_smtp_server(config, store.clone()));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (bind, store, handle)
}

#[test]
fn config_rejects_both_credentials() {
    let mut config = test_config("127.0.0.1:2525", 1024);
    config.matrix.credentials.password = Some("pass".to_string());
    config.matrix.credentials.access_token = Some("token".to_string());
    config.matrix.credentials.device_id = Some("dev".to_string());
    let err = config.validate().unwrap_err();
    assert!(err.to_string().contains("not both"));
}

#[tokio::test]
async fn smtp_full_session_delivers_message() {
    let (bind, store, server_handle) = start_test_server(test_config("127.0.0.1:0", 10240)).await;

    let stream = TcpStream::connect(&bind).await.unwrap();
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Greeting
    let greeting = read_response(&mut reader).await;
    assert!(greeting.starts_with("220"));

    // EHLO -- should get multi-line response with SIZE
    writer.write_all(b"EHLO test\r\n").await.unwrap();
    let resp = read_response(&mut reader).await;
    assert!(resp.contains("250"));
    assert!(resp.contains("SIZE"));

    // MAIL FROM
    writer
        .write_all(b"MAIL FROM:<sender@example.com>\r\n")
        .await
        .unwrap();
    let resp = read_response(&mut reader).await;
    assert!(resp.starts_with("250"));

    // RCPT TO
    writer
        .write_all(b"RCPT TO:<recipient@example.com>\r\n")
        .await
        .unwrap();
    let resp = read_response(&mut reader).await;
    assert!(resp.starts_with("250"));

    // DATA
    writer.write_all(b"DATA\r\n").await.unwrap();
    let resp = read_response(&mut reader).await;
    assert!(resp.starts_with("354"));

    // Message body with headers
    writer
        .write_all(b"Subject: Test\r\n\r\nHello world\r\n.\r\n")
        .await
        .unwrap();
    let resp = read_response(&mut reader).await;
    assert!(resp.starts_with("250"));

    // QUIT
    writer.write_all(b"QUIT\r\n").await.unwrap();
    let resp = read_response(&mut reader).await;
    assert!(resp.starts_with("221"));

    // Verify the message was durably persisted to the outbox.
    let stored = store
        .claim_next_ready(now_secs())
        .await
        .unwrap()
        .expect("message should be persisted in the queue");
    let message = stored.message;
    assert_eq!(message.from.as_deref(), Some("sender@example.com"));
    assert_eq!(message.to, vec!["recipient@example.com"]);
    assert_eq!(message.subject.as_deref(), Some("Test"));
    assert!(message.body.contains("Hello world"));

    server_handle.abort();
}

#[tokio::test]
async fn smtp_returns_552_on_oversized_message() {
    let (bind, _queue, server_handle) = start_test_server(test_config("127.0.0.1:0", 32)).await;

    let stream = TcpStream::connect(&bind).await.unwrap();
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    read_response(&mut reader).await; // greeting

    writer.write_all(b"EHLO test\r\n").await.unwrap();
    read_response(&mut reader).await;

    writer.write_all(b"MAIL FROM:<a@b.com>\r\n").await.unwrap();
    read_response(&mut reader).await;

    writer.write_all(b"RCPT TO:<c@d.com>\r\n").await.unwrap();
    read_response(&mut reader).await;

    writer.write_all(b"DATA\r\n").await.unwrap();
    read_response(&mut reader).await;

    // Send data that exceeds the 32-byte limit
    writer
        .write_all(
            b"Subject: This is a very long subject line that exceeds the tiny limit\r\n.\r\n",
        )
        .await
        .unwrap();
    let resp = read_response(&mut reader).await;
    assert!(resp.starts_with("552"), "Expected 552, got: {resp}");

    server_handle.abort();
}

#[tokio::test]
async fn smtp_returns_451_when_queue_is_full() {
    let mut cfg = test_config("127.0.0.1:0", 10240);
    cfg.queue.max_len = 1;
    let (bind, _store, server_handle) = start_test_server(cfg).await;

    let stream = TcpStream::connect(&bind).await.unwrap();
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    read_response(&mut reader).await; // greeting

    // First message fills the single queue slot.
    let resp1 = submit_message(&mut reader, &mut writer, "one").await;
    assert!(resp1.starts_with("250"), "Expected 250, got: {resp1}");

    // Second message must be rejected with a temporary failure (451).
    let resp2 = submit_message(&mut reader, &mut writer, "two").await;
    assert!(resp2.starts_with("451"), "Expected 451, got: {resp2}");

    server_handle.abort();
}

/// Run one MAIL/RCPT/DATA cycle, returning the final response after the message
/// body. Assumes the greeting has already been consumed.
async fn submit_message(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    body: &str,
) -> String {
    writer.write_all(b"MAIL FROM:<a@b.com>\r\n").await.unwrap();
    read_response(reader).await;
    writer.write_all(b"RCPT TO:<c@d.com>\r\n").await.unwrap();
    read_response(reader).await;
    writer.write_all(b"DATA\r\n").await.unwrap();
    read_response(reader).await; // 354
    writer
        .write_all(format!("Subject: {body}\r\n\r\n{body}\r\n.\r\n").as_bytes())
        .await
        .unwrap();
    read_response(reader).await
}

#[tokio::test]
async fn smtp_rejects_connections_over_the_limit() {
    let mut cfg = test_config("127.0.0.1:0", 10240);
    cfg.smtp.max_connections = 1;
    let (bind, _store, server_handle) = start_test_server(cfg).await;

    // Connection A holds the only permit (kept open for the rest of the test).
    let stream_a = TcpStream::connect(&bind).await.unwrap();
    let (read_a, _write_a) = stream_a.into_split();
    let mut reader_a = BufReader::new(read_a);
    assert!(read_response(&mut reader_a).await.starts_with("220"));

    // Connection B must be rejected with 421 because no permit is free.
    let stream_b = TcpStream::connect(&bind).await.unwrap();
    let (read_b, _write_b) = stream_b.into_split();
    let mut reader_b = BufReader::new(read_b);
    let resp_b = read_response(&mut reader_b).await;
    assert!(resp_b.starts_with("421"), "Expected 421, got: {resp_b}");

    // Once A releases its slot, a new connection is accepted again.
    drop(reader_a);
    drop(_write_a);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let stream_c = TcpStream::connect(&bind).await.unwrap();
    let (read_c, _write_c) = stream_c.into_split();
    let mut reader_c = BufReader::new(read_c);
    assert!(read_response(&mut reader_c).await.starts_with("220"));

    server_handle.abort();
}

#[tokio::test]
async fn smtp_closes_session_exceeding_max_duration() {
    let mut cfg = test_config("127.0.0.1:0", 10240);
    cfg.smtp.timeout = 30; // generous per-read timeout
    cfg.smtp.max_session_seconds = 1; // short overall session cap
    let (bind, _store, server_handle) = start_test_server(cfg).await;

    let stream = TcpStream::connect(&bind).await.unwrap();
    let (read_half, _writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    assert!(read_response(&mut reader).await.starts_with("220"));

    // Stay idle. The 1s session cap must close the connection well before the
    // 30s per-read timeout would fire; the next read should see EOF.
    let mut line = String::new();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_line(&mut line),
    )
    .await;
    match result {
        Ok(Ok(n)) => assert_eq!(n, 0, "expected EOF, got {n} bytes: {line:?}"),
        Ok(Err(_)) => {} // a reset is also an acceptable close
        Err(_) => panic!("connection not closed within 5s despite 1s session cap"),
    }

    server_handle.abort();
}

#[tokio::test]
async fn bind_smtp_rejects_an_invalid_address() {
    let cfg = test_config("not-a-socket-address", 1024);
    assert!(chimney_post::smtp::bind_smtp(&cfg).await.is_err());
}

#[tokio::test]
async fn bind_smtp_succeeds_on_a_valid_address() {
    let cfg = test_config("127.0.0.1:0", 1024);
    let listener = chimney_post::smtp::bind_smtp(&cfg).await.unwrap();
    // A real ephemeral port was assigned.
    assert!(listener.local_addr().unwrap().port() > 0);
}

#[tokio::test]
async fn smtp_discards_truncated_data_on_premature_disconnect() {
    let (bind, store, server_handle) = start_test_server(test_config("127.0.0.1:0", 10240)).await;

    let stream = TcpStream::connect(&bind).await.unwrap();
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    read_response(&mut reader).await; // greeting
    writer.write_all(b"MAIL FROM:<a@b.com>\r\n").await.unwrap();
    read_response(&mut reader).await;
    writer.write_all(b"RCPT TO:<c@d.com>\r\n").await.unwrap();
    read_response(&mut reader).await;
    writer.write_all(b"DATA\r\n").await.unwrap();
    read_response(&mut reader).await; // 354

    // Send a partial body and abruptly close WITHOUT the ".\r\n" terminator.
    writer
        .write_all(b"Subject: partial\r\n\r\nhalf a mess")
        .await
        .unwrap();
    drop(writer);
    drop(reader);

    // The truncated message must NOT be enqueued.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(store.len().await.unwrap(), 0);

    server_handle.abort();
}

#[tokio::test]
async fn smtp_returns_500_on_oversized_command_line() {
    let (bind, _store, server_handle) = start_test_server(test_config("127.0.0.1:0", 10240)).await;

    let stream = TcpStream::connect(&bind).await.unwrap();
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    read_response(&mut reader).await; // greeting

    // A single command line far larger than the command-line cap, with no
    // newline, must be rejected (500) rather than buffered unboundedly.
    let huge = vec![b'A'; 5000];
    writer.write_all(&huge).await.unwrap();

    let resp = read_response(&mut reader).await;
    assert!(resp.starts_with("500"), "Expected 500, got: {resp}");

    server_handle.abort();
}

#[tokio::test]
async fn smtp_returns_552_on_oversized_data_line_without_newline() {
    let (bind, _store, server_handle) = start_test_server(test_config("127.0.0.1:0", 64)).await;

    let stream = TcpStream::connect(&bind).await.unwrap();
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    read_response(&mut reader).await; // greeting
    writer.write_all(b"MAIL FROM:<a@b.com>\r\n").await.unwrap();
    read_response(&mut reader).await;
    writer.write_all(b"RCPT TO:<c@d.com>\r\n").await.unwrap();
    read_response(&mut reader).await;
    writer.write_all(b"DATA\r\n").await.unwrap();
    read_response(&mut reader).await; // 354

    // A single DATA line with no newline, much larger than max_message_size,
    // must trip the size limit (552) without being buffered in full.
    let huge = vec![b'X'; 4000];
    writer.write_all(&huge).await.unwrap();

    let resp = read_response(&mut reader).await;
    assert!(resp.starts_with("552"), "Expected 552, got: {resp}");

    server_handle.abort();
}

#[tokio::test]
async fn smtp_returns_502_on_unknown_command() {
    let (bind, _queue, server_handle) = start_test_server(test_config("127.0.0.1:0", 10240)).await;

    let stream = TcpStream::connect(&bind).await.unwrap();
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    read_response(&mut reader).await; // greeting

    writer.write_all(b"BOGUSCOMMAND arg\r\n").await.unwrap();
    let resp = read_response(&mut reader).await;
    assert!(resp.starts_with("502"), "Expected 502, got: {resp}");

    writer.write_all(b"QUIT\r\n").await.unwrap();
    read_response(&mut reader).await;

    server_handle.abort();
}

#[tokio::test]
async fn smtp_rejects_malformed_mail_from() {
    let (bind, _queue, server_handle) = start_test_server(test_config("127.0.0.1:0", 10240)).await;

    let stream = TcpStream::connect(&bind).await.unwrap();
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    read_response(&mut reader).await; // greeting

    // "MAIL FROMAGE" should not be accepted as MAIL FROM
    writer
        .write_all(b"MAIL FROMAGE:<a@b.com>\r\n")
        .await
        .unwrap();
    let resp = read_response(&mut reader).await;
    assert!(
        resp.starts_with("501") || resp.starts_with("502"),
        "Expected 501 or 502, got: {resp}"
    );

    // Missing colon
    writer.write_all(b"MAIL FROM <a@b.com>\r\n").await.unwrap();
    let resp = read_response(&mut reader).await;
    assert!(resp.starts_with("501"), "Expected 501, got: {resp}");

    writer.write_all(b"QUIT\r\n").await.unwrap();
    read_response(&mut reader).await;

    server_handle.abort();
}

#[tokio::test]
async fn smtp_rejects_empty_rcpt_to() {
    let (bind, _queue, server_handle) = start_test_server(test_config("127.0.0.1:0", 10240)).await;

    let stream = TcpStream::connect(&bind).await.unwrap();
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    read_response(&mut reader).await; // greeting

    writer.write_all(b"MAIL FROM:<a@b.com>\r\n").await.unwrap();
    read_response(&mut reader).await;

    // Empty recipient should be rejected
    writer.write_all(b"RCPT TO:<>\r\n").await.unwrap();
    let resp = read_response(&mut reader).await;
    assert!(resp.starts_with("501"), "Expected 501, got: {resp}");

    writer.write_all(b"QUIT\r\n").await.unwrap();
    read_response(&mut reader).await;

    server_handle.abort();
}

#[tokio::test]
async fn smtp_accepts_null_sender() {
    let (bind, _queue, server_handle) = start_test_server(test_config("127.0.0.1:0", 10240)).await;

    let stream = TcpStream::connect(&bind).await.unwrap();
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    read_response(&mut reader).await; // greeting

    // Null sender (bounce message) is valid per RFC 5321
    writer.write_all(b"MAIL FROM:<>\r\n").await.unwrap();
    let resp = read_response(&mut reader).await;
    assert!(resp.starts_with("250"), "Expected 250, got: {resp}");

    writer.write_all(b"QUIT\r\n").await.unwrap();
    read_response(&mut reader).await;

    server_handle.abort();
}

#[tokio::test]
async fn smtp_returns_503_on_data_before_mail_from() {
    let (bind, _queue, server_handle) = start_test_server(test_config("127.0.0.1:0", 10240)).await;

    let stream = TcpStream::connect(&bind).await.unwrap();
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    read_response(&mut reader).await; // greeting

    writer.write_all(b"DATA\r\n").await.unwrap();
    let resp = read_response(&mut reader).await;
    assert!(resp.starts_with("503"), "Expected 503, got: {resp}");

    writer.write_all(b"QUIT\r\n").await.unwrap();
    read_response(&mut reader).await;

    server_handle.abort();
}

/// End-to-end durability check: a message accepted over SMTP must survive a
/// hard crash (no graceful shutdown, nothing delivered) and be redelivered
/// after a restart that reopens the same on-disk queue.
#[tokio::test]
async fn message_survives_crash_and_is_redelivered_on_restart() {
    use chimney_post::queue::{DeliveryFuture, DeliveryWorker, Message, MessageSink};
    use std::sync::Mutex;
    use tokio::sync::watch;

    /// Stands in for the real Matrix client; records what it would have sent.
    #[derive(Clone, Default)]
    struct RecordingSink {
        delivered: Arc<Mutex<Vec<String>>>,
    }
    impl MessageSink for RecordingSink {
        fn deliver<'a>(&'a self, message: &'a Message, _key: &'a str) -> DeliveryFuture<'a> {
            let delivered = Arc::clone(&self.delivered);
            let body = message.body.clone();
            Box::pin(async move {
                delivered.lock().unwrap().push(body);
                Ok(())
            })
        }
    }

    // --- first process lifetime: file-backed store + real SMTP server ---
    let dir = tempfile::tempdir().unwrap();
    let db_file = dir.path().join("queue.db");
    let db_path = db_file.to_str().unwrap().to_string();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let bind = format!("127.0.0.1:{port}");

    let mut cfg = test_config(&bind, 10240);
    cfg.queue.db_path = db_path.clone();
    let cfg = Arc::new(cfg);

    let store = MessageStore::open_with_max_len(&cfg.queue.db_path, cfg.queue.max_len)
        .await
        .unwrap();
    let server = tokio::spawn(start_smtp_server(Arc::clone(&cfg), store.clone()));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Accept a message over a real SMTP session.
    let stream = TcpStream::connect(&bind).await.unwrap();
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    read_response(&mut reader).await; // greeting
    let resp = submit_message(&mut reader, &mut writer, "survive me").await;
    assert!(resp.starts_with("250"), "Expected 250, got: {resp}");

    // It is durably persisted; no delivery has happened yet.
    assert_eq!(store.len().await.unwrap(), 1);

    // --- simulate a hard crash: stop the server and drop everything WITHOUT
    //     a graceful drain and without ever delivering. ---
    server.abort();
    drop(writer);
    drop(reader);
    drop(store);

    // --- restart: reopen the same database file ---
    let store2 = MessageStore::open_with_max_len(&db_path, cfg.queue.max_len)
        .await
        .unwrap();
    assert_eq!(
        store2.len().await.unwrap(),
        1,
        "the accepted message must still be queued after the restart"
    );

    // --- the delivery worker redelivers it on restart ---
    let sink = RecordingSink::default();
    let (tx, rx) = watch::channel(false);
    let worker = DeliveryWorker::new(store2.clone(), sink.clone(), 5, 0);
    let handle = tokio::spawn(worker.run(rx));

    let drained = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if store2.len().await.unwrap() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(drained.is_ok(), "worker did not redeliver within 5s");

    tx.send(true).unwrap();
    handle.await.unwrap();

    let delivered = sink.delivered.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1, "exactly one redelivery");
    assert!(
        delivered[0].contains("survive me"),
        "redelivered body mismatch: {:?}",
        delivered[0]
    );
}
