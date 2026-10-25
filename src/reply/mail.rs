//! Building the email that carries an answer from Matrix, and the transport
//! that submits it.

use crate::config::{ReplySmtpConfig, SmtpSecurity};
use crate::error::{ChimneyError, Result};
use crate::queue::{OutgoingReply, ReplyTarget};
use lettre::message::header::ContentType;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// How long one SMTP submission may take before it counts as failed.
const SMTP_TIMEOUT: Duration = Duration::from_secs(30);

/// The answer to `target` requested by the Matrix event `event_id`: addressed
/// to the email's reply address, threaded under its `Message-ID`, with a
/// `Re:` subject.
pub fn answer(target: &ReplyTarget, event_id: &str, text: &str) -> OutgoingReply {
    OutgoingReply {
        event_id: event_id.to_string(),
        room_id: target.room_id.clone(),
        to: target.reply_to.clone(),
        subject: reply_subject(target.subject.as_deref()),
        in_reply_to: target.message_id.clone(),
        body: text.to_string(),
    }
}

/// `Re: <subject>`, without stacking a second `Re:` and with any control
/// characters (a header line break above all) flattened to spaces.
fn reply_subject(subject: Option<&str>) -> String {
    let subject: String = subject
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let subject = subject.trim();
    if subject.is_empty() {
        "Re: (no subject)".to_string()
    } else if subject
        .get(..3)
        .is_some_and(|start| start.eq_ignore_ascii_case("re:"))
    {
        subject.to_string()
    } else {
        format!("Re: {subject}")
    }
}

/// The RFC 5322 message for `reply`, sent as `from`.
///
/// # Errors
///
/// Returns [`ChimneyError::Reply`] if the recipient is not a valid address or
/// the message cannot be assembled; retrying will not help either.
pub fn build_email(from: &Mailbox, reply: &OutgoingReply) -> Result<lettre::Message> {
    let to: Mailbox = reply
        .to
        .parse()
        .map_err(|error| ChimneyError::Reply(format!("invalid recipient address: {error}")))?;

    let mut builder = lettre::Message::builder()
        .from(from.clone())
        .to(to)
        .subject(reply.subject.clone())
        .header(ContentType::TEXT_PLAIN);
    if let Some(message_id) = &reply.in_reply_to {
        builder = builder
            .in_reply_to(message_id.clone())
            .references(message_id.clone());
    }
    builder
        .body(reply.body.clone())
        .map_err(|error| ChimneyError::Reply(format!("failed to build the email: {error}")))
}

/// Why sending an email failed, and whether trying again could succeed.
#[derive(Debug)]
pub struct SendError {
    pub message: String,
    /// The server refused the message outright (5xx); retrying is pointless.
    pub permanent: bool,
}

/// A boxed, `Send` future returning a send result (see [`crate::queue::DeliveryFuture`]
/// for why it is boxed).
pub type SendFuture<'a> =
    Pin<Box<dyn Future<Output = std::result::Result<(), SendError>> + Send + 'a>>;

/// Something that can send an email. Abstracted so the reply worker can be
/// tested without an SMTP server.
pub trait MailTransport: Send + Sync {
    fn send(&self, email: lettre::Message) -> SendFuture<'_>;
}

/// The SMTP submission transport described by `[reply.smtp]`.
pub struct SmtpTransport {
    inner: AsyncSmtpTransport<Tokio1Executor>,
}

impl SmtpTransport {
    /// Build the transport. TLS is required for `tls` and `starttls`; the
    /// server certificate is checked against the system trust store.
    ///
    /// # Errors
    ///
    /// Returns [`ChimneyError::Reply`] if the TLS configuration cannot be built.
    pub fn from_config(config: &ReplySmtpConfig) -> Result<Self> {
        let tls_error =
            |error| ChimneyError::Reply(format!("failed to set up TLS for reply.smtp: {error}"));
        let builder = match config.security {
            SmtpSecurity::Tls => {
                AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host).map_err(tls_error)?
            }
            SmtpSecurity::Starttls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host)
                    .map_err(tls_error)?
            }
            // Config validation only allows this for a loopback host.
            SmtpSecurity::None => {
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(config.host.clone())
            }
        };
        let mut builder = builder.port(config.port()).timeout(Some(SMTP_TIMEOUT));
        if let (Some(username), Some(password)) = (&config.username, &config.password) {
            builder = builder.credentials(Credentials::new(username.clone(), password.clone()));
        }
        Ok(Self {
            inner: builder.build(),
        })
    }
}

impl MailTransport for SmtpTransport {
    fn send(&self, email: lettre::Message) -> SendFuture<'_> {
        Box::pin(async move {
            self.inner
                .send(email)
                .await
                .map(|_| ())
                .map_err(|error| SendError {
                    permanent: error.is_permanent(),
                    message: error.to_string(),
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(subject: Option<&str>, message_id: Option<&str>) -> ReplyTarget {
        ReplyTarget {
            event_id: "$email".to_string(),
            room_id: "!room:example.org".to_string(),
            reply_to: "app@example.com".to_string(),
            message_id: message_id.map(str::to_string),
            subject: subject.map(str::to_string),
        }
    }

    fn formatted(email: &lettre::Message) -> String {
        String::from_utf8(email.formatted()).unwrap()
    }

    #[test]
    fn answer_addresses_and_threads_the_original() {
        let reply = answer(
            &target(Some("ping"), Some("<1@example.com>")),
            "$answer",
            "pong",
        );
        assert_eq!(reply.event_id, "$answer");
        assert_eq!(reply.room_id, "!room:example.org");
        assert_eq!(reply.to, "app@example.com");
        assert_eq!(reply.subject, "Re: ping");
        assert_eq!(reply.in_reply_to.as_deref(), Some("<1@example.com>"));
        assert_eq!(reply.body, "pong");
    }

    #[test]
    fn reply_subject_handles_prefixes_and_control_characters() {
        assert_eq!(reply_subject(Some("ping")), "Re: ping");
        assert_eq!(reply_subject(Some("RE: ping")), "RE: ping");
        assert_eq!(reply_subject(Some("re:ping")), "re:ping");
        assert_eq!(reply_subject(None), "Re: (no subject)");
        assert_eq!(reply_subject(Some("  ")), "Re: (no subject)");
        assert_eq!(
            reply_subject(Some("a\r\nBcc: victim@example.com")),
            "Re: a  Bcc: victim@example.com"
        );
        // Multi-byte start must not panic on the byte slice.
        assert_eq!(reply_subject(Some("ÄÖ")), "Re: ÄÖ");
    }

    #[test]
    fn email_carries_threading_headers() {
        let from: Mailbox = "chimney-post@example.org".parse().unwrap();
        let reply = answer(
            &target(Some("ping"), Some("<1@example.com>")),
            "$answer",
            "pong",
        );
        let text = formatted(&build_email(&from, &reply).unwrap());

        assert!(
            text.contains("From: chimney-post@example.org\r\n"),
            "{text}"
        );
        assert!(text.contains("To: app@example.com\r\n"), "{text}");
        assert!(text.contains("Subject: Re: ping\r\n"), "{text}");
        assert!(text.contains("In-Reply-To: <1@example.com>\r\n"), "{text}");
        assert!(text.contains("References: <1@example.com>\r\n"), "{text}");
        assert!(text.ends_with("pong"), "{text}");
    }

    #[test]
    fn email_without_message_id_has_no_threading_headers() {
        let from: Mailbox = "chimney-post@example.org".parse().unwrap();
        let reply = answer(&target(None, None), "$answer", "pong");
        let text = formatted(&build_email(&from, &reply).unwrap());
        assert!(!text.contains("In-Reply-To"), "{text}");
        assert!(!text.contains("References"), "{text}");
    }

    #[test]
    fn email_to_an_invalid_address_is_refused() {
        let from: Mailbox = "chimney-post@example.org".parse().unwrap();
        let mut reply = answer(&target(None, None), "$answer", "pong");
        reply.to = "not an address".to_string();
        assert!(build_email(&from, &reply).is_err());
    }

    #[tokio::test]
    async fn transport_builds_for_every_security_mode() {
        for security in [
            SmtpSecurity::Tls,
            SmtpSecurity::Starttls,
            SmtpSecurity::None,
        ] {
            let config = ReplySmtpConfig {
                host: "localhost".to_string(),
                security,
                username: Some("user".to_string()),
                password: Some("secret".to_string()),
                ..Default::default()
            };
            assert!(SmtpTransport::from_config(&config).is_ok(), "{security:?}");
        }
    }
}
