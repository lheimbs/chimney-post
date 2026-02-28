use crate::queue::Message;

pub fn format_message(message: &Message) -> String {
    let mut lines = Vec::new();

    if let Some(from) = &message.from {
        lines.push(format!("From: {from}"));
    }

    if !message.to.is_empty() {
        lines.push(format!("To: {}", message.to.join(", ")));
    }

    if let Some(subject) = &message.subject {
        lines.push(format!("Subject: {subject}"));
    } else {
        lines.push("Subject: (none)".to_string());
    }

    lines.push(String::new());

    if message.body.trim().is_empty() {
        lines.push("(empty message body)".to_string());
    } else {
        lines.push(message.body.clone());
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_message_with_missing_fields() {
        let message = Message {
            from: None,
            to: Vec::new(),
            subject: None,
            body: "".to_string(),
        };

        let formatted = format_message(&message);
        assert!(formatted.contains("Subject: (none)"));
        assert!(formatted.contains("(empty message body)"));
    }
}
