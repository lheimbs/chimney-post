use chimney_post::config::{
    Config, LoggingConfig, MatrixConfig, MatrixCredentials, QueueConfig, SmtpConfig,
    DEFAULT_MESSAGE_TEMPLATE,
};
use chimney_post::queue::MessageQueue;
use chimney_post::smtp::start_smtp_server;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

fn test_config(bind: &str, max_message_size: usize) -> Config {
    Config {
        smtp: SmtpConfig {
            bind: bind.to_string(),
            max_message_size,
            timeout: 5,
        },
        matrix: MatrixConfig {
            homeserver: "https://example.org".to_string(),
            user_id: "@bot:example.org".to_string(),
            device_name: "chimney-post".to_string(),
            room_id: "!room:example.org".to_string(),
            store_path: "/tmp/matrix".to_string(),
            require_e2ee: true,
            message_template: DEFAULT_MESSAGE_TEMPLATE.to_string(),
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
            capacity: 10,
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

/// Spin up the SMTP server on a random port, return the bind address and server handle.
async fn start_test_server(
    config: Config,
) -> (
    String,
    MessageQueue,
    tokio::task::JoinHandle<chimney_post::error::Result<()>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let bind = format!("127.0.0.1:{port}");
    let mut config = config;
    config.smtp.bind = bind.clone();
    let config = Arc::new(config);
    let (queue, _receiver) = MessageQueue::new(config.queue.capacity);
    let queue_clone = queue.clone();
    let handle = tokio::spawn(start_smtp_server(config, queue_clone));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (bind, queue, handle)
}

#[test]
fn config_requires_loopback_bind() {
    let config = test_config("0.0.0.0:2525", 1024);
    assert!(config.validate().is_err());
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
    let base_config = test_config("127.0.0.1:0", 10240);
    let config = Arc::new(base_config.clone());
    let (queue, mut receiver) = MessageQueue::new(config.queue.capacity);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let bind = format!("127.0.0.1:{port}");
    let mut test_cfg = base_config;
    test_cfg.smtp.bind = bind.clone();
    let server_handle = tokio::spawn(start_smtp_server(Arc::new(test_cfg), queue));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

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

    // Verify message in queue
    let message = receiver.recv().await.unwrap();
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
