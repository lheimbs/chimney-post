use chimney_post::config::Config;
use chimney_post::error::{ChimneyError, Result};
use chimney_post::matrix::MatrixClient;
use chimney_post::queue::{DeliveryWorker, MessageStore};
use chimney_post::smtp::start_smtp_server;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
use tokio::signal::unix::{signal as unix_signal, SignalKind};
use tokio::sync::watch;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// How long to wait for the delivery worker to drain ready messages on
/// shutdown before giving up. Undelivered messages remain persisted regardless.
const SHUTDOWN_DRAIN_SECONDS: u64 = 30;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load_default()?;
    init_tracing(&config)?;

    let config = Arc::new(config);

    let store =
        MessageStore::open_with_max_len(&config.queue.db_path, config.queue.max_len).await?;
    let matrix = MatrixClient::connect(config.as_ref()).await?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let worker = DeliveryWorker::new(
        store.clone(),
        matrix,
        config.queue.max_retries,
        config.queue.retry_backoff,
    );
    let mut worker_handle = tokio::spawn(worker.run(shutdown_rx));
    let mut smtp_handle = tokio::spawn(start_smtp_server(Arc::clone(&config), store.clone()));

    let mut sigterm = unix_signal(SignalKind::terminate())?;

    tokio::select! {
        result = &mut smtp_handle => {
            // The SMTP server loops forever; any return is unexpected.
            return match result {
                Err(join_error) => Err(ChimneyError::Smtp(format!("SMTP task panicked: {join_error}"))),
                Ok(Err(smtp_error)) => Err(smtp_error),
                Ok(Ok(())) => Err(ChimneyError::Smtp("SMTP server exited unexpectedly".to_string())),
            };
        }
        result = &mut worker_handle => {
            return match result {
                Err(join_error) => Err(ChimneyError::Matrix(format!("delivery worker panicked: {join_error}"))),
                Ok(()) => Err(ChimneyError::Matrix("delivery worker exited unexpectedly".to_string())),
            };
        }
        _ = signal::ctrl_c() => info!("Shutdown signal received (SIGINT)"),
        _ = sigterm.recv() => info!("Shutdown signal received (SIGTERM)"),
    }

    // Graceful shutdown: stop accepting mail, then let the worker drain the
    // messages that are currently due. Anything still backing off stays in the
    // persistent queue and is retried on the next start.
    info!("draining delivery queue before exit");
    smtp_handle.abort();
    let _ = shutdown_tx.send(true);

    match tokio::time::timeout(Duration::from_secs(SHUTDOWN_DRAIN_SECONDS), worker_handle).await {
        Ok(Ok(())) => info!("delivery queue drained; exiting"),
        Ok(Err(join_error)) => warn!(%join_error, "delivery worker join error during shutdown"),
        Err(_) => warn!(
            "timed out draining delivery queue; undelivered messages remain persisted for retry"
        ),
    }

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
