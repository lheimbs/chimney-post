use crate::config::Config;
use crate::error::{ChimneyError, Result};
use crate::queue::MessageStore;
use crate::smtp::parser::parse_data;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

/// Maximum length of a single SMTP command line, in bytes. RFC 5321 §4.5.3.1.4
/// mandates at least 512 octets; we allow more headroom for long EHLO/parameters
/// but still bound it so a client cannot exhaust memory with one endless line.
const MAX_COMMAND_LINE_BYTES: usize = 4096;

pub async fn start_smtp_server(config: Arc<Config>, store: MessageStore) -> Result<()> {
    let bind_addr: SocketAddr =
        config.smtp.bind.parse().map_err(|_| {
            ChimneyError::Config("smtp.bind must be a valid SocketAddr".to_string())
        })?;

    let listener = TcpListener::bind(bind_addr).await?;
    info!(bind = %bind_addr, "SMTP server listening");

    let connection_limiter = Arc::new(Semaphore::new(config.smtp.max_connections.max(1)));

    loop {
        let (mut stream, remote_addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(error) => {
                warn!(error = %error, "Failed to accept SMTP connection");
                continue;
            }
        };

        // Acquire a connection slot before doing any work. If the cap is
        // reached, reject with 421 and close so a flood cannot exhaust file
        // descriptors, tasks, or memory. The permit is held for the connection's
        // lifetime and released when the handler task ends.
        let permit = match Arc::clone(&connection_limiter).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                warn!(remote = %remote_addr, "connection limit reached; rejecting with 421");
                tokio::spawn(async move {
                    let _ = stream.write_all(b"421 Too many connections\r\n").await;
                });
                continue;
            }
        };

        let store_clone = store.clone();
        let config_clone = Arc::clone(&config);
        tokio::spawn(async move {
            let _permit = permit;
            let session_deadline =
                Duration::from_secs(config_clone.smtp.max_session_seconds.max(1));
            match timeout(
                session_deadline,
                handle_connection(stream, remote_addr, config_clone, store_clone),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => warn!(error = %error, "SMTP connection failed"),
                Err(_) => {
                    warn!(remote = %remote_addr, "SMTP session exceeded max duration; closing")
                }
            }
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    remote_addr: SocketAddr,
    config: Arc<Config>,
    store: MessageStore,
) -> Result<()> {
    info!(remote = %remote_addr, "SMTP connection opened");

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    writer.write_all(b"220 chimney-post ESMTP\r\n").await?;

    let mut from = None;
    let mut recipients = Vec::new();
    let mut buffer = String::new();

    loop {
        buffer.clear();
        let read_result = timeout(
            Duration::from_secs(config.smtp.timeout),
            read_line_limited(&mut reader, &mut buffer, MAX_COMMAND_LINE_BYTES),
        )
        .await;
        let bytes = match read_result {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(ChimneyError::SmtpLineTooLong)) => {
                writer.write_all(b"500 Line too long\r\n").await?;
                break;
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(ChimneyError::Smtp("SMTP session timed out".to_string()));
            }
        };

        if bytes == 0 {
            break;
        }

        let line = buffer.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }

        let upper = line.to_uppercase();
        let cmd = upper.split_whitespace().next().unwrap_or("");
        if cmd == "QUIT" {
            writer.write_all(b"221 Bye\r\n").await?;
            break;
        } else if cmd == "HELO" {
            writer.write_all(b"250 chimney-post\r\n").await?;
        } else if cmd == "EHLO" {
            let ehlo_response = format!(
                "250-chimney-post\r\n250 SIZE {}\r\n",
                config.smtp.max_message_size
            );
            writer.write_all(ehlo_response.as_bytes()).await?;
        } else if cmd == "RSET" {
            from = None;
            recipients.clear();
            writer.write_all(b"250 OK\r\n").await?;
        } else if cmd == "NOOP" {
            writer.write_all(b"250 OK\r\n").await?;
        } else if cmd == "MAIL" {
            match parse_mail_from(line) {
                Some(addr) => {
                    from = Some(addr);
                    writer.write_all(b"250 OK\r\n").await?;
                }
                None => {
                    writer
                        .write_all(b"501 Syntax error in MAIL FROM\r\n")
                        .await?;
                }
            }
        } else if cmd == "RCPT" {
            match parse_rcpt_to(line) {
                Some(addr) => {
                    recipients.push(addr);
                    writer.write_all(b"250 OK\r\n").await?;
                }
                None => {
                    writer.write_all(b"501 Syntax error in RCPT TO\r\n").await?;
                }
            }
        } else if cmd == "DATA" {
            if from.is_none() || recipients.is_empty() {
                writer
                    .write_all(b"503 Bad sequence of commands\r\n")
                    .await?;
                continue;
            }
            writer
                .write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n")
                .await?;
            let data = read_data(
                &mut reader,
                config.smtp.max_message_size,
                config.smtp.timeout,
            )
            .await;
            let data = match data {
                Ok(data) => data,
                Err(ChimneyError::SmtpSizeExceeded) => {
                    writer.write_all(b"552 Message size exceeded\r\n").await?;
                    from = None;
                    recipients.clear();
                    continue;
                }
                Err(error) => return Err(error),
            };
            let message = parse_data(from.clone(), recipients.clone(), &data);
            // Persist before acknowledging: only return 250 once the message is
            // durably queued, so an accepted email is never silently lost. On a
            // storage failure, return 451 so the sender retries instead.
            match store.enqueue(&message).await {
                Ok(_) => {
                    writer.write_all(b"250 OK\r\n").await?;
                }
                Err(error) => {
                    warn!(error = %error, "failed to persist message; rejecting with 451");
                    writer
                        .write_all(b"451 Requested action aborted: local error in processing\r\n")
                        .await?;
                }
            }
            from = None;
            recipients.clear();
        } else {
            writer.write_all(b"502 Command not implemented\r\n").await?;
        }
    }

    Ok(())
}

/// Read one `\n`-terminated line into `buf` (raw bytes), consuming at most
/// `max_bytes` bytes from `reader`. Returns the number of bytes appended
/// (0 = EOF).
///
/// Returns [`ChimneyError::SmtpLineTooLong`] if the line would exceed
/// `max_bytes`, bounding per-line memory regardless of how the client behaves
/// (e.g. a never-ending line with no newline). Reading is done over `fill_buf`
/// so the cap is enforced incrementally, before the whole line is buffered.
async fn read_line_limited_bytes<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    let start = buf.len();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            break; // EOF
        }

        match available.iter().position(|&b| b == b'\n') {
            Some(idx) => {
                let take = idx + 1;
                if buf.len() - start + take > max_bytes {
                    return Err(ChimneyError::SmtpLineTooLong);
                }
                buf.extend_from_slice(&available[..take]);
                reader.consume(take);
                break;
            }
            None => {
                let take = available.len();
                if buf.len() - start + take > max_bytes {
                    return Err(ChimneyError::SmtpLineTooLong);
                }
                buf.extend_from_slice(available);
                reader.consume(take);
            }
        }
    }

    Ok(buf.len() - start)
}

/// Like [`read_line_limited_bytes`] but decodes the line as UTF-8. Used for SMTP
/// commands, which are ASCII; an invalid byte is an error.
async fn read_line_limited<R>(reader: &mut R, buf: &mut String, max_bytes: usize) -> Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    let mut raw: Vec<u8> = Vec::new();
    let bytes = read_line_limited_bytes(reader, &mut raw, max_bytes).await?;
    let text = String::from_utf8(raw)
        .map_err(|_| ChimneyError::Smtp("invalid UTF-8 in SMTP command".to_string()))?;
    buf.push_str(&text);
    Ok(bytes)
}

async fn read_data<R>(reader: &mut R, max_size: usize, timeout_seconds: u64) -> Result<String>
where
    R: AsyncBufRead + Unpin,
{
    // DATA is read as raw bytes so 8-bit / non-UTF-8 message bodies are accepted
    // (decoded lossily at the end) rather than killing the connection.
    let mut data: Vec<u8> = Vec::new();
    loop {
        let mut line: Vec<u8> = Vec::new();
        // Bound each line read to the remaining size budget (plus a little slack
        // for the terminator / dot-unstuffing) so peak memory stays ~max_size
        // even if a client never sends a newline.
        let remaining = max_size.saturating_sub(data.len()).saturating_add(4);
        let read_result = timeout(
            Duration::from_secs(timeout_seconds),
            read_line_limited_bytes(reader, &mut line, remaining),
        )
        .await;
        let bytes = match read_result {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(ChimneyError::SmtpLineTooLong)) => return Err(ChimneyError::SmtpSizeExceeded),
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(ChimneyError::Smtp("SMTP data read timed out".to_string())),
        };

        // A well-behaved session ends at the `.` terminator below; reaching EOF
        // here means the client disconnected mid-DATA, so the message is
        // incomplete and must be discarded rather than delivered truncated.
        if bytes == 0 {
            return Err(ChimneyError::SmtpDataIncomplete);
        }

        if matches!(line.as_slice(), b".\r\n" | b".\n") {
            break;
        }

        // Dot-unstuffing: a data line that began with ".." has one leading dot.
        if line.starts_with(b"..") {
            line.remove(0);
        }

        if data.len() + line.len() > max_size {
            return Err(ChimneyError::SmtpSizeExceeded);
        }

        data.extend_from_slice(&line);
    }

    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// Validate `MAIL FROM:` syntax and extract the sender address.
/// Returns `Some(address)` for valid commands, `None` for malformed syntax.
/// An empty address (from `MAIL FROM:<>`) is valid per RFC 5321 (null sender for bounces).
fn parse_mail_from(line: &str) -> Option<String> {
    let upper = line.to_uppercase();
    let rest = upper.strip_prefix("MAIL")?.trim_start();
    let rest = rest.strip_prefix("FROM")?.trim_start();
    if !rest.starts_with(':') {
        return None;
    }
    extract_address_after_colon(line)
}

/// Validate `RCPT TO:` syntax and extract the recipient address.
/// Returns `Some(address)` for valid commands with a non-empty recipient,
/// `None` for malformed syntax or empty addresses.
fn parse_rcpt_to(line: &str) -> Option<String> {
    let upper = line.to_uppercase();
    let rest = upper.strip_prefix("RCPT")?.trim_start();
    let rest = rest.strip_prefix("TO")?.trim_start();
    if !rest.starts_with(':') {
        return None;
    }
    let addr = extract_address_after_colon(line)?;
    if addr.is_empty() {
        return None;
    }
    Some(addr)
}

/// Extract the email address from after the colon in MAIL FROM: / RCPT TO: commands.
fn extract_address_after_colon(line: &str) -> Option<String> {
    let colon_pos = line.find(':')?;
    Some(
        line[colon_pos + 1..]
            .trim()
            .trim_matches('<')
            .trim_matches('>')
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::{read_data, read_line_limited};
    use crate::error::ChimneyError;
    use tokio::io::BufReader;

    /// Run `read_data` over an in-memory byte slice.
    async fn read_data_str(data: &[u8], max_size: usize) -> Result<String, ChimneyError> {
        let mut reader = BufReader::with_capacity(8, data);
        read_data(&mut reader, max_size, 5).await
    }

    #[tokio::test]
    async fn read_data_returns_the_block_up_to_the_terminator() {
        let data = read_data_str(b"Subject: hi\r\n\r\nbody\r\n.\r\n", 1024)
            .await
            .unwrap();
        assert_eq!(data, "Subject: hi\r\n\r\nbody\r\n");
    }

    #[tokio::test]
    async fn read_data_accepts_an_empty_body() {
        let data = read_data_str(b".\r\n", 1024).await.unwrap();
        assert_eq!(data, "");
    }

    #[tokio::test]
    async fn read_data_unstuffs_leading_dots() {
        let data = read_data_str(b"..dotted line\r\n.\r\n", 1024)
            .await
            .unwrap();
        assert_eq!(data, ".dotted line\r\n");
    }

    #[tokio::test]
    async fn read_data_errors_on_premature_eof_without_terminator() {
        // Body with no closing ".\r\n": the client disconnected mid-DATA.
        let err = read_data_str(b"Subject: x\r\n\r\nhalf a mess", 1024)
            .await
            .unwrap_err();
        assert!(matches!(err, ChimneyError::SmtpDataIncomplete));
    }

    #[tokio::test]
    async fn read_data_errors_on_immediate_eof() {
        let err = read_data_str(b"", 1024).await.unwrap_err();
        assert!(matches!(err, ChimneyError::SmtpDataIncomplete));
    }

    #[tokio::test]
    async fn read_data_enforces_the_size_limit() {
        let err = read_data_str(b"123456789\r\n.\r\n", 8).await.unwrap_err();
        assert!(matches!(err, ChimneyError::SmtpSizeExceeded));
    }

    #[tokio::test]
    async fn read_data_preserves_8bit_body_lossily() {
        // 0xE9 is 'é' in Latin-1, which is invalid UTF-8; it must not abort the
        // read but be decoded lossily so the message still gets forwarded.
        let data = read_data_str(b"caf\xe9 time\r\n.\r\n", 1024).await.unwrap();
        assert!(data.contains("caf"));
        assert!(data.contains("time"));
        assert!(
            data.contains('\u{FFFD}'),
            "invalid byte should become the replacement char: {data:?}"
        );
    }

    /// Read a single line from `data`, using a BufReader with the given internal
    /// `cap` (small values force multiple `fill_buf` chunks).
    async fn read_one(
        data: &[u8],
        cap: usize,
        max: usize,
    ) -> Result<(String, usize), ChimneyError> {
        let mut reader = BufReader::with_capacity(cap, data);
        let mut buf = String::new();
        let n = read_line_limited(&mut reader, &mut buf, max).await?;
        Ok((buf, n))
    }

    #[tokio::test]
    async fn reads_a_simple_line_including_newline() {
        let (line, n) = read_one(b"EHLO test\r\n", 64, 4096).await.unwrap();
        assert_eq!(line, "EHLO test\r\n");
        assert_eq!(n, 11);
    }

    #[tokio::test]
    async fn line_exactly_at_limit_is_accepted() {
        let (line, n) = read_one(b"abcd\n", 64, 5).await.unwrap();
        assert_eq!(line, "abcd\n");
        assert_eq!(n, 5);
    }

    #[tokio::test]
    async fn line_one_over_limit_is_rejected() {
        let err = read_one(b"abcde\n", 64, 5).await.unwrap_err();
        assert!(matches!(err, ChimneyError::SmtpLineTooLong));
    }

    #[tokio::test]
    async fn cap_is_enforced_across_chunks_for_a_line_without_newline() {
        // 100 bytes, no newline, tiny buffer: must error rather than buffer it all.
        let data = vec![b'A'; 100];
        let err = read_one(&data, 4, 16).await.unwrap_err();
        assert!(matches!(err, ChimneyError::SmtpLineTooLong));
    }

    #[tokio::test]
    async fn handles_a_line_split_across_many_chunks() {
        let (line, n) = read_one(b"hello world\n", 4, 4096).await.unwrap();
        assert_eq!(line, "hello world\n");
        assert_eq!(n, 12);
    }

    #[tokio::test]
    async fn eof_without_newline_returns_remaining_then_zero() {
        let mut reader = BufReader::with_capacity(8, &b"partial"[..]);
        let mut buf = String::new();
        let n = read_line_limited(&mut reader, &mut buf, 4096)
            .await
            .unwrap();
        assert_eq!(buf, "partial");
        assert_eq!(n, 7);

        let mut buf2 = String::new();
        let n2 = read_line_limited(&mut reader, &mut buf2, 4096)
            .await
            .unwrap();
        assert_eq!(n2, 0);
        assert!(buf2.is_empty());
    }

    #[tokio::test]
    async fn reads_multiple_sequential_lines() {
        let mut reader = BufReader::with_capacity(4, &b"one\r\ntwo\r\n"[..]);
        let mut a = String::new();
        read_line_limited(&mut reader, &mut a, 4096).await.unwrap();
        assert_eq!(a, "one\r\n");
        let mut b = String::new();
        read_line_limited(&mut reader, &mut b, 4096).await.unwrap();
        assert_eq!(b, "two\r\n");
    }

    #[tokio::test]
    async fn invalid_utf8_is_rejected() {
        let err = read_one(&[0xff, 0xfe, b'\n'], 64, 4096).await.unwrap_err();
        assert!(matches!(err, ChimneyError::Smtp(_)));
    }
}
