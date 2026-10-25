//! The strict format a Matrix message must have to be sent back as an email.
//!
//! Sending mail is the one thing chimney-post does that leaves the machine, so
//! the bar for "this message is an email answer" is deliberately high and
//! unambiguous. A message qualifies only when it is:
//!
//! 1. a plain text message (`m.text`; no notices, emotes, edits or media),
//! 2. whose text starts with the configured prefix word (`!reply` by default)
//!    followed by whitespace -- so ordinary chat never sends mail --
//! 3. sent as a Matrix *reply* (`m.in_reply_to`, not a thread message) to the
//!    forwarded email it answers,
//! 4. with a non-empty answer of at most `max_body_len` characters that holds
//!    no control characters besides newlines and tabs.
//!
//! Messages failing (1) or (2) are not addressed to chimney-post at all and are
//! ignored silently. Ones that carry the prefix but fail (3) or (4) are
//! rejected with a reason, so the sender learns why nothing was sent.
//!
//! Whether the replied-to event is a forwarded email, and whether the sender
//! may answer it, is checked by the caller -- this module only judges shape.

use std::fmt;

/// How a Matrix message relates to an earlier event, reduced to what the
/// format check needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation<'a> {
    /// A standalone message.
    None,
    /// A reply (`m.in_reply_to`) to the event with this id.
    Reply { event_id: &'a str },
    /// An edit, a thread message or any other relation.
    Other,
}

/// The outcome of checking a message against the reply format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Not addressed to chimney-post; do nothing and say nothing.
    Ignore,
    /// Addressed to chimney-post but malformed; tell the sender why.
    Reject(Rejection),
    /// A well-formed answer to the event `in_reply_to`.
    Accept { in_reply_to: String, text: String },
}

/// Why a message carrying the reply prefix was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    NotAReply,
    Empty,
    TooLong { max: usize },
    ControlCharacters,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rejection::NotAReply => write!(
                f,
                "send it as a reply to the forwarded email (not in a thread)"
            ),
            Rejection::Empty => write!(f, "the answer is empty"),
            Rejection::TooLong { max } => {
                write!(f, "the answer is longer than {max} characters")
            }
            Rejection::ControlCharacters => {
                write!(f, "the answer contains control characters")
            }
        }
    }
}

/// The reply format, parameterised by the configured prefix and length cap.
#[derive(Clone, Debug)]
pub struct ReplyFormat {
    prefix: String,
    max_body_len: usize,
}

impl ReplyFormat {
    pub fn new(prefix: impl Into<String>, max_body_len: usize) -> Self {
        Self {
            prefix: prefix.into(),
            max_body_len,
        }
    }

    /// Judge a message: `is_text` is whether its msgtype is `m.text`, `body`
    /// its plain-text body and `relation` how it relates to earlier events.
    pub fn check(&self, is_text: bool, body: &str, relation: Relation<'_>) -> Verdict {
        if !is_text {
            return Verdict::Ignore;
        }

        let body = match relation {
            Relation::Reply { .. } => strip_reply_fallback(body),
            Relation::None | Relation::Other => body,
        };
        let Some(answer) = self.strip_prefix(body) else {
            return Verdict::Ignore;
        };

        let Relation::Reply { event_id } = relation else {
            return Verdict::Reject(Rejection::NotAReply);
        };

        let answer = answer.replace("\r\n", "\n");
        let answer = answer.trim();
        if answer.is_empty() {
            return Verdict::Reject(Rejection::Empty);
        }
        if answer.chars().count() > self.max_body_len {
            return Verdict::Reject(Rejection::TooLong {
                max: self.max_body_len,
            });
        }
        if answer
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return Verdict::Reject(Rejection::ControlCharacters);
        }

        Verdict::Accept {
            in_reply_to: event_id.to_string(),
            text: answer.to_string(),
        }
    }

    /// The text after the prefix word, if `body` starts with exactly the
    /// prefix followed by whitespace or the end of the message.
    fn strip_prefix<'b>(&self, body: &'b str) -> Option<&'b str> {
        let rest = body.strip_prefix(self.prefix.as_str())?;
        match rest.chars().next() {
            None => Some(rest),
            Some(c) if c.is_whitespace() => Some(rest),
            Some(_) => None,
        }
    }
}

/// Remove the quoted fallback older Matrix clients put in front of a reply's
/// body (`> <@alice:example.org> original text` lines, then a blank line), so
/// the prefix check sees what the user actually typed.
fn strip_reply_fallback(body: &str) -> &str {
    if !body.starts_with('>') {
        return body;
    }
    let mut rest = body;
    while rest.starts_with('>') {
        rest = match rest.find('\n') {
            Some(newline) => &rest[newline + 1..],
            None => "",
        };
    }
    rest.strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
        .unwrap_or(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMAIL: &str = "$email:example.org";

    fn format() -> ReplyFormat {
        ReplyFormat::new("!reply", 20)
    }

    fn reply() -> Relation<'static> {
        Relation::Reply { event_id: EMAIL }
    }

    fn accept(text: &str) -> Verdict {
        Verdict::Accept {
            in_reply_to: EMAIL.to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn accepts_a_prefixed_reply() {
        assert_eq!(
            format().check(true, "!reply thanks", reply()),
            accept("thanks")
        );
    }

    #[test]
    fn keeps_newlines_and_trims_the_answer() {
        assert_eq!(
            format().check(true, "!reply\r\n  line one\r\nline two  \n", reply()),
            accept("line one\nline two")
        );
    }

    #[test]
    fn strips_a_legacy_reply_fallback() {
        let body = "> <@chimney:example.org> Subject: disk full\n> more quoted\n\n!reply on it";
        assert_eq!(format().check(true, body, reply()), accept("on it"));
    }

    #[test]
    fn ignores_messages_without_the_prefix() {
        for body in [
            "thanks!",
            "!replyfoo bar",
            "!Reply caps differ",
            " !reply leading space",
            "say !reply later",
        ] {
            assert_eq!(
                format().check(true, body, reply()),
                Verdict::Ignore,
                "{body}"
            );
        }
    }

    #[test]
    fn ignores_non_text_messages() {
        assert_eq!(format().check(false, "!reply hi", reply()), Verdict::Ignore);
    }

    #[test]
    fn rejects_a_prefixed_message_that_is_not_a_reply() {
        for relation in [Relation::None, Relation::Other] {
            assert_eq!(
                format().check(true, "!reply hi", relation),
                Verdict::Reject(Rejection::NotAReply)
            );
        }
    }

    #[test]
    fn does_not_strip_quotes_from_non_replies() {
        // Without a reply relation a leading quote is just text, so the
        // message does not start with the prefix.
        let body = "> quoted\n\n!reply hi";
        assert_eq!(format().check(true, body, Relation::None), Verdict::Ignore);
    }

    #[test]
    fn rejects_an_empty_answer() {
        for body in ["!reply", "!reply   ", "!reply\n\n"] {
            assert_eq!(
                format().check(true, body, reply()),
                Verdict::Reject(Rejection::Empty),
                "{body:?}"
            );
        }
    }

    #[test]
    fn enforces_the_length_cap_in_characters() {
        // 20 multi-byte characters fit; 21 do not.
        let fits = "ü".repeat(20);
        assert_eq!(
            format().check(true, &format!("!reply {fits}"), reply()),
            accept(&fits)
        );
        let long = "ü".repeat(21);
        assert_eq!(
            format().check(true, &format!("!reply {long}"), reply()),
            Verdict::Reject(Rejection::TooLong { max: 20 })
        );
    }

    #[test]
    fn rejects_control_characters() {
        for body in ["!reply a\u{0}b", "!reply bell\u{7}", "!reply a\rb"] {
            assert_eq!(
                format().check(true, body, reply()),
                Verdict::Reject(Rejection::ControlCharacters),
                "{body:?}"
            );
        }
    }

    #[test]
    fn honours_a_custom_prefix() {
        let format = ReplyFormat::new("@mail", 100);
        assert_eq!(format.check(true, "@mail ok", reply()), accept("ok"));
        assert_eq!(format.check(true, "!reply ok", reply()), Verdict::Ignore);
    }
}
