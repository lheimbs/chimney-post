use crate::config::Config;
use crate::error::{ChimneyError, Result};
use crate::matrix::format_message;
use crate::queue::Message;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::authentication::SessionTokens;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId};
use matrix_sdk::Client;
use matrix_sdk::SessionMeta;
use tracing::{info, warn};

#[derive(Clone)]
pub struct MatrixClient {
    client: Client,
    room_id: OwnedRoomId,
    user_id: OwnedUserId,
    require_e2ee: bool,
    message_template: String,
}

impl MatrixClient {
    pub async fn connect(config: &Config) -> Result<Self> {
        let homeserver_url: url::Url = config.matrix.homeserver.parse().map_err(|error| {
            ChimneyError::Config(format!("invalid matrix.homeserver URL: {error}"))
        })?;

        // Create store directory before the builder needs it
        std::fs::create_dir_all(&config.matrix.store_path).map_err(|error| {
            ChimneyError::Matrix(format!("failed to create store directory: {error}"))
        })?;

        let mut client = Client::builder()
            .homeserver_url(homeserver_url.clone())
            .sqlite_store(&config.matrix.store_path, None)
            .build()
            .await
            .map_err(|error| {
                ChimneyError::Matrix(format!("Matrix client build failed: {error}"))
            })?;

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
                let msg = error.to_string();
                if msg.contains("doesn't match") {
                    warn!(
                        "crypto store has keys for a different device; \
                         clearing store and retrying"
                    );
                    drop(client);
                    std::fs::remove_dir_all(&config.matrix.store_path).map_err(|error| {
                        ChimneyError::Matrix(format!("failed to clear store: {error}"))
                    })?;
                    std::fs::create_dir_all(&config.matrix.store_path).map_err(|error| {
                        ChimneyError::Matrix(format!("failed to recreate store directory: {error}"))
                    })?;
                    client = Client::builder()
                        .homeserver_url(homeserver_url.clone())
                        .sqlite_store(&config.matrix.store_path, None)
                        .build()
                        .await
                        .map_err(|error| {
                            ChimneyError::Matrix(format!("Matrix client rebuild failed: {error}"))
                        })?;
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
            let auth = client.matrix_auth();
            let mut login_builder = auth
                .login_username(&config.matrix.user_id, password)
                .initial_device_display_name(&config.matrix.device_name);

            if let Some(device_id) = config.matrix.credentials.device_id.as_deref() {
                login_builder = login_builder.device_id(device_id);
            }

            let response = login_builder
                .send()
                .await
                .map_err(|error| ChimneyError::Matrix(format!("Matrix login failed: {error}")))?;

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

    pub async fn send_message(&self, message: &Message) -> Result<()> {
        let formatted = format_message(message, &self.message_template)?;

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
