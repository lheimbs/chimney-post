use chimney_post::config::Config;
use chimney_post::error::{ChimneyError, Result};
use chimney_post::matrix::MatrixClient;
use chimney_post::queue::{run_with_reconnect, MessageStore};
use chimney_post::smtp::{bind_smtp, serve_smtp};
use sd_notify::NotifyState;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// How long to wait before retrying the Matrix connection at startup / after it
/// drops. SMTP keeps accepting and queuing mail throughout.
const RECONNECT_BACKOFF_SECONDS: u64 = 10;

/// How often the monitor reports status / pings the systemd watchdog. Keep this
/// at most half of the unit's WatchdogSec.
const MONITOR_INTERVAL_SECONDS: u64 = 60;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load_default()?;
    init_tracing(&config)?;

    let config = Arc::new(config);

    let store =
        MessageStore::open_with_max_len(&config.queue.db_path, config.queue.max_len).await?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Bind SMTP first: once it's listening the service can accept and queue mail,
    // which is the real readiness point -- so a homeserver outage at boot neither
    // refuses mail nor crash-loops the service. Signal systemd ready here, before
    // Matrix is connected.
    let listener = bind_smtp(&config).await?;
    let _ = sd_notify::notify(
        false,
        &[
            NotifyState::Ready,
            NotifyState::Status("accepting mail; connecting to Matrix"),
        ],
    );

    let mut smtp_handle = tokio::spawn(serve_smtp(listener, Arc::clone(&config), store.clone()));

    // Tracks whether Matrix has connected, for status reporting.
    let connected = Arc::new(AtomicBool::new(false));

    let worker_config = Arc::clone(&config);
    let worker_store = store.clone();
    let worker_connected = Arc::clone(&connected);
    let worker_shutdown = shutdown_rx.clone();
    let mut worker_handle = tokio::spawn(async move {
        let connect_config = Arc::clone(&worker_config);
        let connect = move || {
            let cfg = Arc::clone(&connect_config);
            let connected = Arc::clone(&worker_connected);
            async move {
                let client = MatrixClient::connect(cfg.as_ref()).await?;
                connected.store(true, Ordering::Relaxed);
                Ok(client)
            }
        };
        run_with_reconnect(
            worker_store,
            connect,
            worker_config.queue.max_retries,
            worker_config.queue.retry_backoff,
            Duration::from_secs(RECONNECT_BACKOFF_SECONDS),
            worker_shutdown,
        )
        .await;
    });

    // Periodically report queue depth to systemd (STATUS=), ping the watchdog,
    // and log when delivery is stalled or messages have dead-lettered -- so a
    // silent failure (Matrix down, queue backing up) is visible.
    let monitor_handle = tokio::spawn(run_monitor(
        store.clone(),
        Arc::clone(&connected),
        Duration::from_secs(MONITOR_INTERVAL_SECONDS),
        shutdown_rx,
    ));

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
    let _ = sd_notify::notify(false, &[NotifyState::Stopping]);
    smtp_handle.abort();
    let _ = shutdown_tx.send(true);

    match tokio::time::timeout(Duration::from_secs(SHUTDOWN_DRAIN_SECONDS), worker_handle).await {
        Ok(Ok(())) => info!("delivery queue drained; exiting"),
        Ok(Err(join_error)) => warn!(%join_error, "delivery worker join error during shutdown"),
        Err(_) => warn!(
            "timed out draining delivery queue; undelivered messages remain persisted for retry"
        ),
    }
    monitor_handle.abort();

    Ok(())
}

/// Periodically surface queue health: update the systemd `STATUS=` line, ping
/// the watchdog, and log when delivery is stalled or messages have dead-lettered.
async fn run_monitor(
    store: MessageStore,
    connected: Arc<AtomicBool>,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut last_dead_letter = 0usize;
    loop {
        // Ping the watchdog first so a slow status query can't starve it.
        let _ = sd_notify::notify(false, &[NotifyState::Watchdog]);

        let queued = store.len().await.unwrap_or(0);
        let dead_letter = store.dead_letter_len().await.unwrap_or(0);
        let is_connected = connected.load(Ordering::Relaxed);

        let status = format!(
            "{}; queued={queued} dead_letter={dead_letter}",
            if is_connected {
                "connected"
            } else {
                "connecting to Matrix"
            }
        );
        let _ = sd_notify::notify(false, &[NotifyState::Status(&status)]);

        if dead_letter > last_dead_letter {
            warn!(
                dead_letter,
                "messages have exhausted retries and are in the dead-letter table"
            );
        }
        if !is_connected && queued > 0 {
            warn!(queued, "not connected to Matrix; messages are queuing");
        }
        last_dead_letter = dead_letter;

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => return,
        }
    }
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
