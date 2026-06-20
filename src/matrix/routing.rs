//! Maps an incoming email to its destination Matrix room.
//!
//! Routing rules ([`crate::config::RouteConfig`]) are matched against an email's
//! sender (`MAIL FROM`) and recipients (`RCPT TO`), mirroring how mailrise
//! routes on address. The first matching rule wins; if none match, a configured
//! default room is used so an email is never dropped for lack of a route.

use crate::config::{MatrixConfig, RouteConfig};
use crate::error::{ChimneyError, Result};
use crate::queue::Message;
use matrix_sdk::ruma::OwnedRoomId;

/// A routing rule with its target room already parsed and validated.
#[derive(Clone, Debug)]
struct CompiledRoute {
    /// Recipient address to match against any `RCPT TO` (case-insensitive).
    to: Option<String>,
    /// Sender address to match against `MAIL FROM` (case-insensitive).
    from: Option<String>,
    room_id: OwnedRoomId,
}

impl CompiledRoute {
    fn matches(&self, message: &Message) -> bool {
        if let Some(want_from) = &self.from {
            if !message
                .from
                .as_deref()
                .is_some_and(|f| f.eq_ignore_ascii_case(want_from))
            {
                return false;
            }
        }
        if let Some(want_to) = &self.to {
            if !message
                .to
                .iter()
                .any(|addr| addr.eq_ignore_ascii_case(want_to))
            {
                return false;
            }
        }
        true
    }
}

/// Resolves the destination room for a message from an ordered list of routing
/// rules, falling back to a default room when nothing matches.
#[derive(Clone, Debug)]
pub struct Router {
    routes: Vec<CompiledRoute>,
    default_room_id: OwnedRoomId,
}

impl Router {
    /// Build a router from the Matrix configuration, parsing the default room id
    /// and every rule's room id. Returns a [`ChimneyError::Config`] if any room
    /// id is malformed.
    pub fn from_config(matrix: &MatrixConfig) -> Result<Self> {
        Self::build(&matrix.room_id, &matrix.routes)
    }

    fn build(default_room_id: &str, routes: &[RouteConfig]) -> Result<Self> {
        let default_room_id = parse_room_id(default_room_id, "matrix.room_id".to_string())?;
        let routes = routes
            .iter()
            .enumerate()
            .map(|(idx, route)| {
                let to = blank_to_none(route.to.as_deref());
                let from = blank_to_none(route.from.as_deref());
                if to.is_none() && from.is_none() {
                    return Err(ChimneyError::Config(format!(
                        "matrix.routes[{idx}] must set at least one of `to` or `from`"
                    )));
                }
                let room_id =
                    parse_room_id(&route.room_id, format!("matrix.routes[{idx}].room_id"))?;
                Ok(CompiledRoute { to, from, room_id })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            routes,
            default_room_id,
        })
    }

    /// The room a message should be delivered to: the first matching rule's room
    /// (rules are evaluated top to bottom), or the default room when none match.
    pub fn resolve(&self, message: &Message) -> &OwnedRoomId {
        self.routes
            .iter()
            .find(|route| route.matches(message))
            .map(|route| &route.room_id)
            .unwrap_or(&self.default_room_id)
    }
}

/// Normalise an optional, possibly blank selector to `Some` only when it holds
/// a non-whitespace value.
fn blank_to_none(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn parse_room_id(value: &str, field: String) -> Result<OwnedRoomId> {
    value
        .parse()
        .map_err(|error| ChimneyError::Config(format!("invalid {field} ({value}): {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(to: Option<&str>, from: Option<&str>, room_id: &str) -> RouteConfig {
        RouteConfig {
            to: to.map(str::to_string),
            from: from.map(str::to_string),
            room_id: room_id.to_string(),
        }
    }

    fn msg(from: Option<&str>, to: &[&str]) -> Message {
        Message {
            from: from.map(str::to_string),
            to: to.iter().map(|s| s.to_string()).collect(),
            subject: None,
            body: String::new(),
        }
    }

    const DEFAULT: &str = "!default:example.org";

    fn router(routes: Vec<RouteConfig>) -> Router {
        Router::build(DEFAULT, &routes).expect("router config should be valid")
    }

    fn resolved(router: &Router, message: &Message) -> String {
        router.resolve(message).as_str().to_string()
    }

    #[test]
    fn empty_routes_always_resolve_to_default() {
        let r = router(vec![]);
        assert_eq!(
            resolved(&r, &msg(Some("anyone@x"), &["anything@y"])),
            DEFAULT
        );
    }

    #[test]
    fn to_rule_matches_recipient() {
        let r = router(vec![route(
            Some("alerts@chimney"),
            None,
            "!alerts:example.org",
        )]);
        assert_eq!(
            resolved(&r, &msg(Some("app@server"), &["alerts@chimney"])),
            "!alerts:example.org"
        );
    }

    #[test]
    fn to_rule_matches_any_of_multiple_recipients() {
        let r = router(vec![route(Some("ops@chimney"), None, "!ops:example.org")]);
        let m = msg(Some("app@server"), &["other@chimney", "ops@chimney"]);
        assert_eq!(resolved(&r, &m), "!ops:example.org");
    }

    #[test]
    fn to_rule_falls_through_when_no_recipient_matches() {
        let r = router(vec![route(Some("ops@chimney"), None, "!ops:example.org")]);
        let m = msg(Some("app@server"), &["someone@chimney", "else@chimney"]);
        assert_eq!(resolved(&r, &m), DEFAULT);
    }

    #[test]
    fn from_rule_matches_sender() {
        let r = router(vec![route(
            None,
            Some("nextcloud@server"),
            "!nextcloud:example.org",
        )]);
        let m = msg(Some("nextcloud@server"), &["whatever@chimney"]);
        assert_eq!(resolved(&r, &m), "!nextcloud:example.org");
    }

    #[test]
    fn from_rule_does_not_match_a_null_sender() {
        // Bounce messages arrive with no MAIL FROM; a `from` rule must not match.
        let r = router(vec![route(None, Some("root@server"), "!ops:example.org")]);
        let m = msg(None, &["alerts@chimney"]);
        assert_eq!(resolved(&r, &m), DEFAULT);
    }

    #[test]
    fn matching_is_case_insensitive_for_both_fields() {
        let r = router(vec![route(
            Some("Alerts@Chimney"),
            Some("Root@Server"),
            "!ops:example.org",
        )]);
        let m = msg(Some("root@SERVER"), &["ALERTS@chimney"]);
        assert_eq!(resolved(&r, &m), "!ops:example.org");
    }

    #[test]
    fn combined_rule_requires_both_to_and_from() {
        let r = router(vec![route(
            Some("ops@chimney"),
            Some("root@server"),
            "!ops:example.org",
        )]);
        // Both match.
        assert_eq!(
            resolved(&r, &msg(Some("root@server"), &["ops@chimney"])),
            "!ops:example.org"
        );
        // Only the recipient matches -> no match.
        assert_eq!(
            resolved(&r, &msg(Some("other@server"), &["ops@chimney"])),
            DEFAULT
        );
        // Only the sender matches -> no match.
        assert_eq!(
            resolved(&r, &msg(Some("root@server"), &["other@chimney"])),
            DEFAULT
        );
    }

    #[test]
    fn first_matching_rule_wins() {
        let r = router(vec![
            route(Some("ops@chimney"), None, "!first:example.org"),
            route(Some("ops@chimney"), None, "!second:example.org"),
        ]);
        assert_eq!(
            resolved(&r, &msg(Some("app@server"), &["ops@chimney"])),
            "!first:example.org"
        );
    }

    #[test]
    fn earlier_specific_rule_beats_later_broad_rule() {
        // A precise from+to rule placed first takes priority over a broad
        // to-only rule that would also match.
        let r = router(vec![
            route(
                Some("ops@chimney"),
                Some("root@server"),
                "!urgent:example.org",
            ),
            route(Some("ops@chimney"), None, "!ops:example.org"),
        ]);
        assert_eq!(
            resolved(&r, &msg(Some("root@server"), &["ops@chimney"])),
            "!urgent:example.org"
        );
        // A different sender skips the first rule and lands on the second.
        assert_eq!(
            resolved(&r, &msg(Some("cron@server"), &["ops@chimney"])),
            "!ops:example.org"
        );
    }

    #[test]
    fn build_rejects_route_without_selectors() {
        let err = Router::build(DEFAULT, &[route(None, None, "!x:example.org")]).unwrap_err();
        assert!(err.to_string().contains("at least one of"));
    }

    #[test]
    fn build_treats_blank_selectors_as_unset() {
        let err =
            Router::build(DEFAULT, &[route(Some("   "), Some(""), "!x:example.org")]).unwrap_err();
        assert!(err.to_string().contains("at least one of"));
    }

    #[test]
    fn build_rejects_malformed_route_room_id() {
        let err =
            Router::build(DEFAULT, &[route(Some("ops@chimney"), None, "not-a-room")]).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("matrix.routes[0].room_id"));
        // The offending value is surfaced; sender/recipient selectors are not.
        assert!(message.contains("not-a-room"));
    }

    #[test]
    fn build_rejects_malformed_default_room_id() {
        let err = Router::build("not-a-room", &[]).unwrap_err();
        assert!(err.to_string().contains("matrix.room_id"));
    }

    #[test]
    fn blank_selectors_never_match_but_keep_a_set_selector() {
        // A rule with a blank `to` but a real `from` still routes on `from`,
        // and the blanked `to` does not accidentally match empty recipients.
        let r = router(vec![route(
            Some("  "),
            Some("root@server"),
            "!ops:example.org",
        )]);
        assert_eq!(
            resolved(&r, &msg(Some("root@server"), &[])),
            "!ops:example.org"
        );
    }
}
