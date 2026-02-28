use crate::config::Config;
use crate::error::{ChimneyError, Result};
use crate::queue::MessageQueue;
use crate::smtp::parser::parse_data;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

pub async fn start_smtp_server(config: Arc<Config>, queue: MessageQueue) -> Result<()> {
    let bind_addr: SocketAddr =
        config.smtp.bind.parse().map_err(|_| {
            ChimneyError::Config("smtp.bind must be a valid SocketAddr".to_string())
        })?;

    if !bind_addr.ip().is_loopback() {
        return Err(ChimneyError::Config(
            "smtp.bind must be a loopback address".to_string(),
        ));
    }

    let listener = TcpListener::bind(bind_addr).await?;
    info!(bind = %bind_addr, "SMTP server listening");

    loop {
        let (stream, remote_addr) = listener.accept().await?;
        if !remote_addr.ip().is_loopback() {
            warn!(remote = %remote_addr, "Rejected non-local SMTP connection");
            continue;
        }

        let queue_clone = queue.clone();
        let config_clone = Arc::clone(&config);
        tokio::spawn(async move {
            if let Err(error) =
                handle_connection(stream, remote_addr, config_clone, queue_clone).await
            {
                warn!(error = %error, "SMTP connection failed");
            }
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    remote_addr: SocketAddr,
    config: Arc<Config>,
    queue: MessageQueue,
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
            reader.read_line(&mut buffer),
        )
        .await;
        let bytes = match read_result {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => return Err(error.into()),
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
        if upper.starts_with("QUIT") {
            writer.write_all(b"221 Bye\r\n").await?;
            break;
        } else if upper.starts_with("HELO") || upper.starts_with("EHLO") {
            writer.write_all(b"250 chimney-post\r\n").await?;
        } else if upper.starts_with("RSET") {
            from = None;
            recipients.clear();
            writer.write_all(b"250 OK\r\n").await?;
        } else if upper.starts_with("NOOP") {
            writer.write_all(b"250 OK\r\n").await?;
        } else if upper.starts_with("MAIL FROM:") {
            from = Some(
                line[10..]
                    .trim()
                    .trim_matches('<')
                    .trim_matches('>')
                    .to_string(),
            );
            writer.write_all(b"250 OK\r\n").await?;
        } else if upper.starts_with("RCPT TO:") {
            let recipient = line[8..]
                .trim()
                .trim_matches('<')
                .trim_matches('>')
                .to_string();
            recipients.push(recipient);
            writer.write_all(b"250 OK\r\n").await?;
        } else if upper.starts_with("DATA") {
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
            .await?;
            let message = parse_data(from.clone(), recipients.clone(), &data);
            queue.send(message).await?;
            writer.write_all(b"250 OK\r\n").await?;
            from = None;
            recipients.clear();
        } else {
            writer.write_all(b"250 OK\r\n").await?;
        }
    }

    Ok(())
}

async fn read_data(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    max_size: usize,
    timeout_seconds: u64,
) -> Result<String> {
    let mut data = String::new();
    loop {
        let mut line = String::new();
        let read_result = timeout(
            Duration::from_secs(timeout_seconds),
            reader.read_line(&mut line),
        )
        .await;
        let bytes = match read_result {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => return Err(ChimneyError::Smtp("SMTP data read timed out".to_string())),
        };

        if bytes == 0 {
            break;
        }

        if line == ".\r\n" || line == ".\n" {
            break;
        }

        if let Some(stripped) = line.strip_prefix("..") {
            line = format!(".{stripped}");
        }

        if data.len() + line.len() > max_size {
            return Err(ChimneyError::Smtp(
                "SMTP data exceeded max size".to_string(),
            ));
        }

        data.push_str(&line);
    }

    Ok(data)
}
