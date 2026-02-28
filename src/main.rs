use chimney_post::config::Config;
use chimney_post::error::{ChimneyError, Result};
use chimney_post::matrix::MatrixClient;
use chimney_post::queue::MessageQueue;
use chimney_post::smtp::start_smtp_server;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
use tokio::time::sleep;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load_default()?;
    init_tracing(&config)?;

    let config = Arc::new(config);
    let (queue, mut receiver) = MessageQueue::new(config.queue.capacity);
    let matrix = MatrixClient::connect(config.as_ref()).await?;
    let max_retries = config.queue.max_retries;
    let retry_backoff = config.queue.retry_backoff;

    let matrix_task = tokio::spawn(async move {
        while let Some(message) = receiver.recv().await {
            let mut attempt = 0u32;

            loop {
                match matrix.send_message(&message).await {
                    Ok(()) => break,
                    Err(error) => {
                        if attempt >= max_retries {
                            error!(
                                error = %error,
                                attempts = attempt + 1,
                                "Matrix send failed after retries"
                            );
                            break;
                        }

                        attempt += 1;
                        let backoff_seconds = retry_backoff.saturating_mul(attempt as u64);
                        warn!(
                            error = %error,
                            attempt,
                            backoff_seconds,
                            "Matrix send failed, backing off"
                        );
                        sleep(Duration::from_secs(backoff_seconds)).await;
                    }
                }
            }
        }
    });

    let smtp_task = tokio::spawn(start_smtp_server(Arc::clone(&config), queue));

    tokio::select! {
        result = smtp_task => {
            if let Err(error) = result {
                return Err(ChimneyError::Smtp(format!("SMTP task failed: {error}")));
            }
        }
        _ = signal::ctrl_c() => {
            info!("Shutdown signal received");
        }
    }

    matrix_task.abort();
    Ok(())
}

fn init_tracing(config: &Config) -> Result<()> {
    let filter = EnvFilter::try_new(config.logging.level.clone())
        .map_err(|error| ChimneyError::Config(format!("invalid log level: {error}")))?;

    let builder = tracing_subscriber::fmt().with_env_filter(filter);

    if config.logging.format == "json" {
        builder.json().init();
    } else {
        builder.pretty().init();
    }

    Ok(())
}
