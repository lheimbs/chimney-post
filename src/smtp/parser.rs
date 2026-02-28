use crate::queue::Message;

pub fn parse_data(from: Option<String>, to: Vec<String>, data: &str) -> Message {
    let (subject, body) = extract_subject_and_body(data);

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
                last_header = Some("subject".to_string());
            } else {
                last_header = None;
            }
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
}
