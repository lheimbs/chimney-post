//! The Matrix side of answering forwarded emails: watching the rooms for
//! answers, deciding whether each may be sent, and reporting back.
//!
//! An answer is queued only when every one of these holds, checked cheapest
//! first so that nothing is said to people who are not allowed to answer:
//!
//! 1. the sender is in `reply.allowed_senders` (others are ignored silently),
//! 2. the message has the strict format of [`crate::reply::ReplyFormat`],
//! 3. it was end-to-end encrypted, and -- with
//!    `reply.require_cross_signed_device` -- sent from a device its owner has
//!    cross-signed,
//! 4. it replies to an email chimney-post delivered *in this room* no longer
//!    than `reply.max_age_hours` ago,
//! 5. the rolling-hour limit has room, and the event was not accepted before.
//!
//! A sender who passes (1) is told why an answer was refused from (2) on.

use super::client::MatrixClient;
use crate::config::ReplyConfig;
use crate::queue::{unix_now, MessageStore, OutgoingReply, ReplyQueued};
use crate::reply::{answer, FeedbackFuture, Relation, ReplyFeedback, ReplyFormat, Verdict};
use matrix_sdk::deserialized_responses::{EncryptionInfo, VerificationLevel, VerificationState};
use matrix_sdk::event_handler::Ctx;
use matrix_sdk::ruma::events::reaction::ReactionEventContent;
use matrix_sdk::ruma::events::relation::{Annotation, InReplyTo, Reply};
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, Relation as MessageRelation, RoomMessageEventContent,
};
use matrix_sdk::ruma::{EventId, OwnedUserId};
use matrix_sdk::Room;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Reaction put on an answer once its email has been sent.
const SENT_REACTION: &str = "✅";

/// Everything the room-message handler needs, shared via the client's event
/// handler context.
#[derive(Clone)]
pub(super) struct ReplyContext {
    store: MessageStore,
    own_user_id: OwnedUserId,
    allowed_senders: Arc<HashSet<OwnedUserId>>,
    format: ReplyFormat,
    require_cross_signed_device: bool,
    max_age_seconds: i64,
    max_age_hours: u64,
    max_per_hour: u32,
}

impl ReplyContext {
    /// # Errors
    ///
    /// Returns [`crate::ChimneyError::Config`] if an allowed sender is not a
    /// valid user id (already rejected by config validation).
    pub(super) fn new(
        config: &ReplyConfig,
        store: MessageStore,
        own_user_id: OwnedUserId,
    ) -> crate::Result<Self> {
        let allowed_senders = config
            .allowed_senders
            .iter()
            .map(|sender| {
                sender.parse::<OwnedUserId>().map_err(|error| {
                    crate::ChimneyError::Config(format!("invalid reply.allowed_senders: {error}"))
                })
            })
            .collect::<crate::Result<HashSet<_>>>()?;
        Ok(Self {
            store,
            own_user_id,
            allowed_senders: Arc::new(allowed_senders),
            format: ReplyFormat::new(config.prefix.clone(), config.max_body_len),
            require_cross_signed_device: config.require_cross_signed_device,
            max_age_seconds: config.max_age_seconds(),
            max_age_hours: config.max_age_hours,
            max_per_hour: config.max_per_hour,
        })
    }
}

/// Why an event's encryption does not meet the bar for sending mail, if it
/// does not. `state` is `None` for an unencrypted event.
fn trust_problem(
    state: Option<&VerificationState>,
    require_cross_signed: bool,
) -> Option<&'static str> {
    let Some(state) = state else {
        return Some("the message was not end-to-end encrypted");
    };
    // A sender/device mismatch is a forgery signal, not a trust level.
    if *state == VerificationState::Unverified(VerificationLevel::MismatchedSender) {
        return Some("the message's sender does not own the sending device");
    }
    if !require_cross_signed {
        return None;
    }
    match state {
        // Verified by the bot, or cross-signed by its owner: a device the
        // account holder vouched for, which a stolen access token is not.
        VerificationState::Verified
        | VerificationState::Unverified(VerificationLevel::UnverifiedIdentity) => None,
        VerificationState::Unverified(VerificationLevel::UnsignedDevice) => {
            Some("the sending device is not cross-signed; verify it from another session")
        }
        VerificationState::Unverified(VerificationLevel::VerificationViolation) => {
            Some("the sender's identity changed since it was verified")
        }
        VerificationState::Unverified(VerificationLevel::None(_)) => {
            Some("the sending device is unknown")
        }
        VerificationState::Unverified(VerificationLevel::MismatchedSender) => {
            Some("the message's sender does not own the sending device")
        }
    }
}

/// Room-message event handler: queue the event as an email answer if it
/// qualifies, tell the sender if it is refused.
pub(super) async fn on_room_message(
    event: OriginalSyncRoomMessageEvent,
    room: Room,
    encryption_info: Option<EncryptionInfo>,
    Ctx(ctx): Ctx<ReplyContext>,
) {
    if event.sender == ctx.own_user_id || !ctx.allowed_senders.contains(&event.sender) {
        return;
    }
    // Older than any answerable email: nothing to do, and staying quiet keeps
    // a replayed backlog after downtime from producing a flood of refusals.
    let sent_at = i64::try_from(u64::from(event.origin_server_ts.as_secs())).unwrap_or(i64::MAX);
    if sent_at < unix_now().saturating_sub(ctx.max_age_seconds) {
        return;
    }

    let (is_text, body) = match &event.content.msgtype {
        MessageType::Text(text) => (true, text.body.as_str()),
        _ => (false, ""),
    };
    let relation = match &event.content.relates_to {
        None => Relation::None,
        Some(MessageRelation::Reply(reply)) => Relation::Reply {
            event_id: reply.in_reply_to.event_id.as_str(),
        },
        Some(_) => Relation::Other,
    };

    let (in_reply_to, text) = match ctx.format.check(is_text, body, relation) {
        Verdict::Ignore => return,
        Verdict::Reject(rejection) => {
            return refuse(&room, &event.event_id, &rejection.to_string()).await;
        }
        Verdict::Accept { in_reply_to, text } => (in_reply_to, text),
    };

    let state = encryption_info
        .as_ref()
        .map(|info| &info.verification_state);
    if let Some(problem) = trust_problem(state, ctx.require_cross_signed_device) {
        warn!(room_id = %room.room_id(), sender = %event.sender, reason = problem, "refused an email answer");
        return refuse(&room, &event.event_id, problem).await;
    }

    let target = match ctx
        .store
        .reply_target(&in_reply_to, ctx.max_age_seconds)
        .await
    {
        Ok(Some(target)) if target.room_id == room.room_id().as_str() => target,
        Ok(_) => {
            let reason = format!(
                "that message is not a forwarded email from the last {} hours",
                ctx.max_age_hours
            );
            return refuse(&room, &event.event_id, &reason).await;
        }
        Err(error) => {
            error!(%error, "failed to look up the email being answered");
            return refuse(
                &room,
                &event.event_id,
                "internal error; see the service log",
            )
            .await;
        }
    };

    let reply = answer(&target, event.event_id.as_str(), &text);
    match ctx.store.enqueue_reply(&reply, ctx.max_per_hour).await {
        Ok(ReplyQueued::Queued(id)) => {
            info!(id, room_id = %room.room_id(), sender = %event.sender, "email answer queued")
        }
        Ok(ReplyQueued::Duplicate) => {
            debug!(event_id = %event.event_id, "email answer already accepted; ignoring replay")
        }
        Ok(ReplyQueued::RateLimited) => {
            let reason = format!(
                "the limit of {} answers per hour is reached",
                ctx.max_per_hour
            );
            refuse(&room, &event.event_id, &reason).await;
        }
        Err(error) => {
            error!(%error, "failed to queue an email answer");
            refuse(
                &room,
                &event.event_id,
                "internal error; see the service log",
            )
            .await;
        }
    }
}

/// Tell the sender, in a notice replying to their message, why nothing was
/// sent. Best effort: a failure is only logged.
async fn refuse(room: &Room, event_id: &EventId, reason: &str) {
    notice(room, event_id, &format!("Not sent: {reason}.")).await;
}

async fn notice(room: &Room, event_id: &EventId, text: &str) {
    let mut content = RoomMessageEventContent::notice_plain(text);
    content.relates_to = Some(MessageRelation::Reply(Reply::new(InReplyTo::new(
        event_id.to_owned(),
    ))));
    if let Err(error) = room.send(content).await {
        warn!(%error, room_id = %room.room_id(), "failed to post a notice about an email answer");
    }
}

impl ReplyFeedback for MatrixClient {
    fn sent<'a>(&'a self, reply: &'a OutgoingReply) -> FeedbackFuture<'a> {
        Box::pin(async move {
            let Some((room, event_id)) = self.reply_room(reply) else {
                return;
            };
            let reaction =
                ReactionEventContent::new(Annotation::new(event_id, SENT_REACTION.to_string()));
            if let Err(error) = room.send(reaction).await {
                warn!(%error, room_id = %room.room_id(), "failed to confirm a sent email answer");
            }
        })
    }

    fn failed<'a>(&'a self, reply: &'a OutgoingReply, reason: &'a str) -> FeedbackFuture<'a> {
        Box::pin(async move {
            let Some((room, event_id)) = self.reply_room(reply) else {
                return;
            };
            let text = format!("Could not send the answer to {}: {reason}", reply.to);
            notice(&room, &event_id, &text).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::deserialized_responses::DeviceLinkProblem;

    #[test]
    fn unencrypted_messages_are_never_trusted() {
        assert!(trust_problem(None, true).is_some());
        assert!(trust_problem(None, false).is_some());
    }

    #[test]
    fn cross_signed_and_verified_devices_are_trusted() {
        for state in [
            VerificationState::Verified,
            VerificationState::Unverified(VerificationLevel::UnverifiedIdentity),
        ] {
            assert_eq!(trust_problem(Some(&state), true), None, "{state:?}");
        }
    }

    #[test]
    fn other_devices_are_refused_when_cross_signing_is_required() {
        for state in [
            VerificationState::Unverified(VerificationLevel::UnsignedDevice),
            VerificationState::Unverified(VerificationLevel::VerificationViolation),
            VerificationState::Unverified(VerificationLevel::None(
                DeviceLinkProblem::MissingDevice,
            )),
        ] {
            assert!(trust_problem(Some(&state), true).is_some(), "{state:?}");
            assert_eq!(trust_problem(Some(&state), false), None, "{state:?}");
        }
    }

    #[test]
    fn a_sender_mismatch_is_always_refused() {
        let state = VerificationState::Unverified(VerificationLevel::MismatchedSender);
        assert!(trust_problem(Some(&state), false).is_some());
    }
}
