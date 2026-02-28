use chimney_post::config::Config;

#[test]
fn config_requires_loopback_bind() {
    let config = Config {
        smtp: chimney_post::config::SmtpConfig {
            bind: "0.0.0.0:2525".to_string(),
            max_message_size: 1024,
            timeout: 30,
        },
        matrix: chimney_post::config::MatrixConfig {
            homeserver: "https://example.org".to_string(),
            user_id: "@bot:example.org".to_string(),
            device_name: "chimney-post".to_string(),
            room_id: "!room:example.org".to_string(),
            store_path: "/tmp/matrix".to_string(),
            credentials: chimney_post::config::MatrixCredentials {
                password: None,
                access_token: None,
                device_id: None,
            },
        },
        logging: chimney_post::config::LoggingConfig {
            level: "info".to_string(),
            format: "json".to_string(),
        },
        queue: chimney_post::config::QueueConfig {
            max_retries: 5,
            retry_backoff: 60,
            capacity: 10,
        },
    };

    assert!(config.validate().is_err());
}
