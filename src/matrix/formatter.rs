use crate::error::{ChimneyError, Result};
use crate::queue::Message;
use minijinja::{context, Environment};

pub fn format_message(message: &Message, template_str: &str) -> Result<String> {
    let mut env = Environment::new();
    env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);
    env.add_template("message", template_str)
        .map_err(|error| ChimneyError::Template(format!("failed to compile template: {error}")))?;

    let tmpl = env
        .get_template("message")
        .map_err(|error| ChimneyError::Template(format!("template not found: {error}")))?;

    let to_joined = message.to.join(", ");

    let ctx = context! {
        from => message.from.as_deref().unwrap_or(""),
        to => to_joined,
        subject => message.subject.as_deref().unwrap_or(""),
        body => &message.body,
    };

    tmpl.render(ctx)
        .map_err(|error| ChimneyError::Template(format!("template render failed: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_MESSAGE_TEMPLATE;

    #[test]
    fn default_template_formats_full_message() {
        let message = Message {
            from: Some("alice@example.com".to_string()),
            to: vec!["bob@example.com".to_string()],
            subject: Some("Hello".to_string()),
            body: "Hi Bob!".to_string(),
        };

        let result = format_message(&message, DEFAULT_MESSAGE_TEMPLATE).unwrap();
        assert!(result.contains("From: alice@example.com"));
        assert!(result.contains("To: bob@example.com"));
        assert!(result.contains("Subject: Hello"));
        assert!(result.contains("Hi Bob!"));
    }

    #[test]
    fn default_template_formats_missing_fields() {
        let message = Message {
            from: None,
            to: Vec::new(),
            subject: None,
            body: "".to_string(),
        };

        let result = format_message(&message, DEFAULT_MESSAGE_TEMPLATE).unwrap();
        assert!(result.contains("Subject: (none)"));
        assert!(result.contains("(empty message body)"));
        assert!(!result.contains("From:"));
    }

    #[test]
    fn custom_template_works() {
        let template = "[{{ subject }}] {{ body }}";
        let message = Message {
            from: Some("sender@test.com".to_string()),
            to: vec!["rcpt@test.com".to_string()],
            subject: Some("Alert".to_string()),
            body: "Server is down".to_string(),
        };

        let result = format_message(&message, template).unwrap();
        assert_eq!(result, "[Alert] Server is down");
    }

    #[test]
    fn invalid_template_returns_error() {
        let template = "{{ unclosed";
        let message = Message {
            from: None,
            to: vec![],
            subject: None,
            body: "test".to_string(),
        };

        let result = format_message(&message, template);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Template error"));
    }

    #[test]
    fn template_with_multiple_recipients() {
        let template = "To: {{ to }}";
        let message = Message {
            from: None,
            to: vec!["a@example.com".to_string(), "b@example.com".to_string()],
            subject: None,
            body: "test".to_string(),
        };

        let result = format_message(&message, template).unwrap();
        assert_eq!(result, "To: a@example.com, b@example.com");
    }
}
