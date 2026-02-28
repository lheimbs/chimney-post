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
use tracing::info;

#[derive(Clone)]
pub struct MatrixClient {
    client: Client,
    room_id: OwnedRoomId,
    user_id: OwnedUserId,
}

impl MatrixClient {
    pub async fn connect(config: &Config) -> Result<Self> {
        let homeserver_url: url::Url = config.matrix.homeserver.parse().map_err(|error| {
            ChimneyError::Config(format!("invalid matrix.homeserver URL: {error}"))
        })?;

        let client = Client::builder()
            .homeserver_url(homeserver_url)
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

            client.restore_session(session).await.map_err(|error| {
                ChimneyError::Matrix(format!("Matrix access token login failed: {error}"))
            })?;
        } else if let Some(password) = config.matrix.credentials.password.as_deref() {
            let auth = client.matrix_auth();
            let mut login_builder = auth
                .login_username(&config.matrix.user_id, password)
                .initial_device_display_name(&config.matrix.device_name);

            if let Some(device_id) = config.matrix.credentials.device_id.as_deref() {
                login_builder = login_builder.device_id(device_id);
            }

            login_builder
                .send()
                .await
                .map_err(|error| ChimneyError::Matrix(format!("Matrix login failed: {error}")))?;
        } else {
            return Err(ChimneyError::Config(
                "matrix credentials require password or access_token".to_string(),
            ));
        }

        // Create store directory if it doesn't exist
        std::fs::create_dir_all(&config.matrix.store_path).map_err(|error| {
            ChimneyError::Matrix(format!("failed to create store directory: {error}"))
        })?;

        // Perform initial sync to load encryption keys
        client
            .sync_once(SyncSettings::default())
            .await
            .map_err(|error| {
                ChimneyError::Matrix(format!("Matrix initial sync failed: {error}"))
            })?;

        Ok(Self {
            client,
            room_id,
            user_id,
        })
    }

    pub async fn send_message(&self, message: &Message) -> Result<()> {
        let formatted = format_message(message);
        if formatted.trim().is_empty() {
            return Err(ChimneyError::Matrix(
                "formatted Matrix message is empty".to_string(),
            ));
        }

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
