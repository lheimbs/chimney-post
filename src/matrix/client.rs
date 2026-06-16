use crate::config::Config;
use crate::error::{ChimneyError, Result};
use crate::matrix::format_message;
use crate::queue::{DeliveryFuture, Message, MessageSink};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::authentication::SessionTokens;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::encryption::CryptoStoreError;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::{OwnedRoomId, OwnedTransactionId, OwnedUserId};
use matrix_sdk::Client;
use matrix_sdk::SessionMeta;
use std::path::Path;
use tracing::{info, warn};

#[derive(Clone)]
pub struct MatrixClient {
    client: Client,
    room_id: OwnedRoomId,
    user_id: OwnedUserId,
    require_e2ee: bool,
    message_template: String,
}

/// Build a fresh client backed by the SQLite store at `store_path`.
async fn build_client(homeserver_url: &url::Url, store_path: &str) -> Result<Client> {
    Client::builder()
        .homeserver_url(homeserver_url.clone())
        .sqlite_store(store_path, None)
        .build()
        .await
        .map_err(|error| ChimneyError::Matrix(format!("Matrix client build failed: {error}")))
}

/// True if `error` is the crypto-store device/account mismatch, i.e. the store
/// holds keys for a different device than the one we are configured to use
/// (`CryptoStoreError::MismatchedAccount`).
///
/// The login/restore call wraps this deep inside other errors (the surfaced
/// message looks like "failed to read or write to the crypto store the account
/// in the store doesn't match the account in the constructor: ..."), so a
/// `matches!` on the top-level `matrix_sdk::Error` variant misses it. We walk
/// the error's `source()` chain and downcast to the concrete crypto-store
/// error; as a safety net we also match the distinctive message text, which the
/// wrappers include in their own `Display` output.
fn is_mismatched_account(error: &matrix_sdk::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(err) = source {
        if let Some(CryptoStoreError::MismatchedAccount { .. }) =
            err.downcast_ref::<CryptoStoreError>()
        {
            return true;
        }
        source = err.source();
    }

    message_indicates_mismatch(&error.to_string())
}

/// Matches the distinctive text of `CryptoStoreError::MismatchedAccount`. This
/// is far more specific than "doesn't match", which also appears in unrelated
/// crypto errors (sender, public-key, and room-id mismatches) that must not
/// trigger a crypto-store wipe.
fn message_indicates_mismatch(message: &str) -> bool {
    message.contains("doesn't match the account in the constructor")
}

/// Delete only the crypto-store database (plus its SQLite WAL/SHM sidecars) so
/// a fresh device identity can be created. The state, media, and event-cache
/// databases are left intact, and unrelated files are never touched -- so a
/// misconfigured `store_path` cannot wipe more than the crypto database.
fn reset_crypto_store(store_path: &str) -> Result<()> {
    // matrix-sdk-sqlite keeps the crypto account in this single file.
    const CRYPTO_DB: &str = "matrix-sdk-crypto.sqlite3";
    for suffix in ["", "-wal", "-shm"] {
        let path = Path::new(store_path).join(format!("{CRYPTO_DB}{suffix}"));
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ChimneyError::Matrix(format!(
                    "failed to clear crypto store file {}: {error}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

impl MatrixClient {
    pub async fn connect(config: &Config) -> Result<Self> {
        let homeserver_url: url::Url = config.matrix.homeserver.parse().map_err(|error| {
            ChimneyError::Config(format!("invalid matrix.homeserver URL: {error}"))
        })?;

        // Create store directory before the builder needs it
        std::fs::create_dir_all(&config.matrix.store_path).map_err(|error| {
            let path = &config.matrix.store_path;
            ChimneyError::Matrix(format!("failed to create store directory {path}: {error}"))
        })?;

        let mut client = build_client(&homeserver_url, &config.matrix.store_path).await?;

        let user_id: OwnedUserId =
            config.matrix.user_id.parse().map_err(|error| {
                ChimneyError::Config(format!("invalid matrix.user_id: {error}"))
            })?;
        let room_id: OwnedRoomId =
            config.matrix.room_id.parse().map_err(|error| {
                ChimneyError::Config(format!("invalid matrix.room_id: {error}"))
            })?;

        if let Some(access_token) = config.matrix.credentials.access_token.as_deref() {
            let device_id = config
                .matrix
                .credentials
                .device_id
                .as_deref()
                .ok_or_else(|| {
                    ChimneyError::Config(
                        "matrix.credentials.device_id is required when using access_token"
                            .to_string(),
                    )
                })?;

            let session = MatrixSession {
                meta: SessionMeta {
                    user_id: user_id.clone(),
                    device_id: device_id.to_string().into(),
                },
                tokens: SessionTokens {
                    access_token: access_token.to_string(),
                    refresh_token: None,
                },
            };

            if let Err(error) = client.restore_session(session.clone()).await {
                if is_mismatched_account(&error) {
                    warn!(
                        "crypto store has keys for a different device; \
                         clearing crypto store and retrying"
                    );
                    drop(client);
                    reset_crypto_store(&config.matrix.store_path)?;
                    client = build_client(&homeserver_url, &config.matrix.store_path).await?;
                    client.restore_session(session).await.map_err(|error| {
                        ChimneyError::Matrix(format!(
                            "Matrix access token login failed after store reset: {error}"
                        ))
                    })?;
                } else {
                    return Err(ChimneyError::Matrix(format!(
                        "Matrix access token login failed: {error}"
                    )));
                }
            }
        } else if let Some(password) = config.matrix.credentials.password.as_deref() {
            if config.matrix.credentials.device_id.is_none() {
                warn!(
                    "password auth without matrix.credentials.device_id: the homeserver \
                     issues a NEW device on every start, which orphans devices on your \
                     account and forces a crypto-store reset each restart. Set a stable \
                     device_id to reuse one device across restarts."
                );
            }

            let do_password_login = |client: &Client| {
                let auth = client.matrix_auth();
                let mut login_builder = auth
                    .login_username(&config.matrix.user_id, password)
                    .initial_device_display_name(&config.matrix.device_name);

                if let Some(device_id) = config.matrix.credentials.device_id.as_deref() {
                    login_builder = login_builder.device_id(device_id);
                }

                login_builder.send()
            };

            let response = match do_password_login(&client).await {
                Ok(response) => response,
                Err(error) if is_mismatched_account(&error) => {
                    warn!(
                        "crypto store has keys for a different device; \
                         clearing crypto store and retrying"
                    );
                    drop(client);
                    reset_crypto_store(&config.matrix.store_path)?;
                    client = build_client(&homeserver_url, &config.matrix.store_path).await?;
                    do_password_login(&client).await.map_err(|error| {
                        ChimneyError::Matrix(format!(
                            "Matrix login failed after store reset: {error}"
                        ))
                    })?
                }
                Err(error) => {
                    return Err(ChimneyError::Matrix(format!(
                        "Matrix login failed: {error}"
                    )));
                }
            };

            info!(
                device_id = %response.device_id,
                "password login successful; use this device_id in config \
                 when switching to access_token auth"
            );
        } else {
            return Err(ChimneyError::Config(
                "matrix credentials require password or access_token".to_string(),
            ));
        }

        // Perform initial sync to load encryption keys
        client
            .sync_once(SyncSettings::default())
            .await
            .map_err(|error| {
                ChimneyError::Matrix(format!("Matrix initial sync failed: {error}"))
            })?;

        // Bootstrap cross-signing if not already set up so the bot's
        // device appears as verified to other users.  Some homeservers
        // (e.g. matrix.org) require interactive auth (OAuth) for this
        // operation, so treat failure as non-fatal.
        let needs_bootstrap = match client.encryption().cross_signing_status().await {
            Some(status) => !status.is_complete(),
            None => true,
        };
        if needs_bootstrap {
            info!("cross-signing not set up, attempting bootstrap");
            match client.encryption().bootstrap_cross_signing(None).await {
                Ok(()) => info!("cross-signing bootstrap complete"),
                Err(error) => warn!(
                    %error,
                    "cross-signing bootstrap failed (homeserver may require interactive auth); \
                     verify the bot device manually from another client"
                ),
            }
        } else {
            info!("cross-signing already set up");
        }

        Ok(Self {
            client,
            room_id,
            user_id,
            require_e2ee: config.matrix.require_e2ee,
            message_template: config.matrix.message_template.clone(),
        })
    }

    /// Send `message` to the configured room. `idempotency_key` is used as the
    /// Matrix transaction id so that a redelivery of the same queued message
    /// (e.g. after a lost response) is deduplicated by the homeserver within the
    /// session rather than appearing twice.
    pub async fn send_message(&self, message: &Message, idempotency_key: &str) -> Result<()> {
        let formatted = format_message(message, &self.message_template)?;
        let transaction_id = OwnedTransactionId::from(idempotency_key);

        let room = match self.client.get_room(&self.room_id) {
            Some(room) => room,
            None => {
                self.client
                    .join_room_by_id(&self.room_id)
                    .await
                    .map_err(|error| {
                        ChimneyError::Matrix(format!("failed to join Matrix room: {error}"))
                    })?;

                self.client
                    .get_room(&self.room_id)
                    .ok_or_else(|| ChimneyError::Matrix("Matrix room not found".to_string()))?
            }
        };

        // Check room encryption state
        let mut encryption_state = room.encryption_state();
        if encryption_state.is_unknown() {
            room.request_encryption_state().await.map_err(|error| {
                ChimneyError::Matrix(format!("failed to query room encryption state: {error}"))
            })?;
            encryption_state = room.encryption_state();
        }

        if !encryption_state.is_encrypted() {
            if self.require_e2ee {
                return Err(ChimneyError::Matrix(
                    "room is not encrypted and matrix.require_e2ee is enabled; \
                     refusing to send unencrypted message"
                        .to_string(),
                ));
            }
            warn!(
                room_id = %self.room_id,
                "sending message to unencrypted room (matrix.require_e2ee = false)"
            );
        }

        let content = RoomMessageEventContent::text_plain(formatted);

        room.send(content)
            .with_transaction_id(transaction_id)
            .await
            .map_err(|error| ChimneyError::Matrix(format!("Matrix send failed: {error}")))?;

        info!(
            room_id = %self.room_id,
            user_id = %self.user_id,
            "Matrix message sent"
        );

        Ok(())
    }
}

impl MessageSink for MatrixClient {
    fn deliver<'a>(&'a self, message: &'a Message, idempotency_key: &'a str) -> DeliveryFuture<'a> {
        Box::pin(self.send_message(message, idempotency_key))
    }
}

#[cfg(test)]
mod tests {
    use super::message_indicates_mismatch;

    #[test]
    fn detects_wrapped_account_mismatch() {
        // The exact shape surfaced by a password login against a store holding
        // keys for a different device.
        let message = "failed to read or write to the crypto store the account in \
             the store doesn't match the account in the constructor: \
             expected @user:matrix.org:CQIY3fUCQw, got @user:matrix.org:N44C4nQOfa";
        assert!(message_indicates_mismatch(message));
    }

    #[test]
    fn ignores_unrelated_doesnt_match_errors() {
        // Other crypto errors contain "doesn't match" but must never trigger a
        // crypto-store wipe.
        assert!(!message_indicates_mismatch(
            "the public key that was part of the message doesn't match the key we have"
        ));
        assert!(!message_indicates_mismatch(
            "the room id of the room key doesn't match the room id of the decrypted event"
        ));
    }
}
