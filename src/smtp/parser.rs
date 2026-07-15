use crate::queue::Message;

/// Builds the delivered [`Message`] from the SMTP envelope and body.
///
/// `MAIL FROM:<>` (the RFC 5321 null sender, used for bounces) is captured by
/// the SMTP session as `Some("")` -- that representation is needed to track
/// "MAIL was issued" independently of "no sender was given". Here, at the
/// boundary where the envelope becomes a `Message`, it is normalised to
/// `None` so the rest of the system (routing, templating) has a single
/// canonical way to ask "is there a sender".
pub fn parse_data(from: Option<String>, to: Vec<String>, data: &str) -> Message {
    let (subject, body) = extract_subject_and_body(data);
    let from = from.filter(|addr| !addr.is_empty());

    Message {
        from,
        to,
        subject,
        body,
    }
}

fn extract_subject_and_body(data: &str) -> (Option<String>, String) {
    let mut subject = None;
    let mut in_headers = true;
    let mut body_lines = Vec::new();
    let mut last_header = None;

    for line in data.lines() {
        if in_headers {
            if line.trim().is_empty() {
                in_headers = false;
                continue;
            }

            if line.starts_with(' ') || line.starts_with('\t') {
                if let Some("subject") = last_header.as_deref() {
                    if let Some(existing) = subject.take() {
                        let combined = format!("{} {}", existing, line.trim());
                        subject = Some(combined);
                    }
                }
                continue;
            }

            let mut parts = line.splitn(2, ':');
            let name = parts.next().unwrap_or("").trim().to_ascii_lowercase();
            let value = parts.next().unwrap_or("").trim();

            if name == "subject" {
                subject = Some(value.to_string());
            }
            last_header = Some(name);
        } else {
            body_lines.push(line);
        }
    }

    (subject, body_lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_subject_case_insensitive() {
        let data = "subject: hello\r\n\r\nBody";
        let (subject, body) = extract_subject_and_body(data);
        assert_eq!(subject.as_deref(), Some("hello"));
        assert_eq!(body, "Body");
    }

    #[test]
    fn parses_folded_subject() {
        let data = "Subject: Hello\r\n\tWorld\r\n\r\nBody";
        let (subject, body) = extract_subject_and_body(data);
        assert_eq!(subject.as_deref(), Some("Hello World"));
        assert_eq!(body, "Body");
    }

    #[test]
    fn parse_data_normalises_null_sender_to_none() {
        // MAIL FROM:<> is tracked as Some("") by the SMTP session; the Message
        // it produces must report no sender, matching how bounces are described
        // everywhere else (e.g. routing rules that key off `from.is_none()`).
        let message = parse_data(Some(String::new()), vec!["to@example.com".to_string()], "");
        assert_eq!(message.from, None);
    }

    #[test]
    fn parse_data_preserves_a_real_sender() {
        let message = parse_data(
            Some("sender@example.com".to_string()),
            vec!["to@example.com".to_string()],
            "",
        );
        assert_eq!(message.from.as_deref(), Some("sender@example.com"));
    }
}
