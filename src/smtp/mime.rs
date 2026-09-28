//! MIME-aware extraction of a human-readable body from an SMTP `DATA` block.
//!
//! Mail from the tools this service was built for -- cron, apticron, mailx,
//! rkhunter -- is single-part 7-bit `text/plain`, which needs no decoding at
//! all. Real mail clients are another matter: they send `multipart/alternative`
//! with a quoted-printable `text/plain` part beside an HTML one, and forwarding
//! that verbatim puts MIME boundaries, `=3D` escapes and a wall of markup into
//! the Matrix room. This module walks the MIME tree instead and picks the one
//! part a human wants to read.
//!
//! Attachments are deliberately *not* handled -- they are noted by name at the
//! end of the body and otherwise ignored. A Matrix notification that silently
//! omits "there was a PDF here" is worse than one that says so, but uploading
//! attachments to Matrix is a different feature.

use mailparse::{DispositionType, MailHeaderMap, ParsedMail};

/// The subject and body recovered from a MIME message.
pub(super) struct MimeMessage {
    pub(super) subject: Option<String>,
    pub(super) body: String,
}

/// Parse `data` (the raw `DATA` block: headers, blank line, body) as MIME.
///
/// Returns `None` when the block cannot be parsed as a message at all, leaving
/// the caller to fall back to its header/body split.
pub(super) fn parse(data: &str) -> Option<MimeMessage> {
    // `data` reaches us already lossily decoded as UTF-8 by the SMTP reader, so
    // a part declaring a non-UTF-8 charset in a *raw* 8-bit encoding is beyond
    // recovery here. Base64 and quoted-printable parts are unaffected: both are
    // ASCII-armoured, so their bytes survive that decode intact and `get_body`
    // transcodes them with the declared charset.
    let mail = mailparse::parse_mail(data.as_bytes()).ok()?;

    let subject = mail.headers.get_first_value("Subject");

    let text = find_text(&mail, "text/plain")
        .and_then(|part| part.get_body().ok())
        .or_else(|| {
            find_text(&mail, "text/html")
                .and_then(|part| part.get_body().ok())
                .map(|html| html_to_text(&html))
        })
        .map(|body| normalise_newlines(&body));

    let mut attachments = Vec::new();
    collect_attachments(&mail, &mut attachments);

    // A message declaring `multipart/...` whose body holds no part matching the
    // declared boundary parses without error but yields nothing to show. Report
    // that as a parse failure so the caller falls back to the raw header/body
    // split: a mangled alert still beats an empty one.
    if text.is_none() && attachments.is_empty() {
        return None;
    }

    let mut body = text.unwrap_or_default();
    if let Some(note) = describe_attachments(&attachments) {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&note);
    }

    Some(MimeMessage { subject, body })
}

/// Find the first leaf part of type `want`, depth-first.
///
/// One traversal per type, rather than one traversal that decides as it goes,
/// is what makes `multipart/alternative` prefer plain text over HTML: the whole
/// tree is searched for a plain part before HTML is considered at all. The same
/// walk handles `multipart/mixed` (first text part wins) and arbitrary nesting.
fn find_text<'a>(mail: &'a ParsedMail<'a>, want: &str) -> Option<&'a ParsedMail<'a>> {
    if mail.subparts.is_empty() {
        // mailparse defaults `mimetype` to text/plain when a part carries no
        // Content-Type, which is what RFC 2045 asks for and what makes a plain
        // non-MIME message fall out of this function unchanged.
        if mail.ctype.mimetype.eq_ignore_ascii_case(want) && !is_attachment(mail) {
            return Some(mail);
        }
        return None;
    }
    mail.subparts.iter().find_map(|part| find_text(part, want))
}

/// Whether a leaf part should be treated as an attachment rather than content.
///
/// An explicit `Content-Disposition: attachment` is the reliable signal, but
/// plenty of mailers send a bare `Content-Type: application/pdf; name="x.pdf"`
/// with no disposition at all, so any non-text, non-multipart leaf counts too.
fn is_attachment(mail: &ParsedMail<'_>) -> bool {
    if mail.get_content_disposition().disposition == DispositionType::Attachment {
        return true;
    }
    let mimetype = mail.ctype.mimetype.to_ascii_lowercase();
    !mimetype.starts_with("text/") && !mimetype.starts_with("multipart/")
}

/// Collect the filenames of every attachment leaf, depth-first.
fn collect_attachments(mail: &ParsedMail<'_>, out: &mut Vec<String>) {
    if !mail.subparts.is_empty() {
        for part in &mail.subparts {
            collect_attachments(part, out);
        }
        return;
    }
    if !is_attachment(mail) {
        return;
    }
    let disposition = mail.get_content_disposition();
    let name = disposition
        .params
        .get("filename")
        .or_else(|| mail.ctype.params.get("name"))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unnamed".to_string());
    out.push(name);
}

/// Render the attachment list as the trailing line appended to the body.
fn describe_attachments(names: &[String]) -> Option<String> {
    match names.len() {
        0 => None,
        1 => Some(format!("[1 attachment: {}]", names[0])),
        n => Some(format!("[{} attachments: {}]", n, names.join(", "))),
    }
}

/// Normalise to `\n` line endings and drop the trailing newline.
///
/// Matches what the non-MIME path produces, so a plain-text message reads
/// identically whether or not it went through MIME parsing.
fn normalise_newlines(body: &str) -> String {
    body.lines().collect::<Vec<_>>().join("\n")
}

/// Reduce an HTML part to something readable in a chat message.
///
/// This is a fallback for senders that offer no plain-text alternative, not an
/// HTML renderer: block-level tags become line breaks, everything else is
/// dropped, and the handful of entities that actually show up in mail are
/// decoded. Marked-up text arriving slightly ragged beats markup arriving raw.
fn html_to_text(html: &str) -> String {
    let without_scripts = strip_elements(html, &["script", "style", "head"]);

    let mut text = String::with_capacity(without_scripts.len());
    let mut chars = without_scripts.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '<' {
            text.push(ch);
            continue;
        }
        let mut tag = String::new();
        for tag_char in chars.by_ref() {
            if tag_char == '>' {
                break;
            }
            tag.push(tag_char);
        }
        if breaks_line(&tag) {
            text.push('\n');
        }
    }

    tidy(&decode_entities(&text))
}

/// Remove the named elements along with their content.
fn strip_elements(html: &str, names: &[&str]) -> String {
    let mut out = html.to_string();
    for name in names {
        let lower = out.to_ascii_lowercase();
        let open = format!("<{name}");
        let close = format!("</{name}");
        let mut result = String::with_capacity(out.len());
        let mut cursor = 0;
        while let Some(start) = lower[cursor..].find(&open) {
            let start = cursor + start;
            result.push_str(&out[cursor..start]);
            match lower[start..].find(&close) {
                Some(end) => {
                    let end = start + end;
                    // Skip past the closing tag's own '>' as well.
                    cursor = match lower[end..].find('>') {
                        Some(gt) => end + gt + 1,
                        None => out.len(),
                    };
                }
                // Unclosed element: drop the rest rather than emit its markup.
                None => cursor = out.len(),
            }
        }
        result.push_str(&out[cursor..]);
        out = result;
    }
    out
}

/// Whether a tag should become a line break in the text rendering.
fn breaks_line(tag: &str) -> bool {
    let name = tag
        .trim_start_matches('/')
        .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "br" | "p"
            | "div"
            | "tr"
            | "li"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "table"
            | "blockquote"
            | "hr"
    )
}

/// Decode the named and numeric entities that turn up in real mail.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        // An entity is short; anything longer is a stray ampersand.
        let end = tail[1..].find(';').map(|i| i + 1).filter(|i| *i <= 10);
        let Some(end) = end else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..end];
        match named_entity(entity) {
            Some(decoded) => out.push_str(decoded),
            None => match numeric_entity(entity) {
                Some(decoded) => out.push(decoded),
                None => out.push_str(&tail[..=end]),
            },
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

fn named_entity(entity: &str) -> Option<&'static str> {
    match entity.to_ascii_lowercase().as_str() {
        "amp" => Some("&"),
        "lt" => Some("<"),
        "gt" => Some(">"),
        "quot" => Some("\""),
        "apos" | "#39" => Some("'"),
        // Rendered as a normal space: a chat message has no use for a
        // non-breaking one, and `tidy` below can then collapse it.
        "nbsp" => Some(" "),
        _ => None,
    }
}

fn numeric_entity(entity: &str) -> Option<char> {
    let digits = entity.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse().ok()?,
    };
    char::from_u32(code)
}

/// Collapse the whitespace that stripping tags leaves behind.
fn tidy(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut blank_run = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            blank_run += 1;
            // One blank line survives; the pile of them between table rows
            // does not.
            if blank_run > 1 || lines.is_empty() {
                continue;
            }
            lines.push(String::new());
        } else {
            blank_run = 0;
            lines.push(collapse_spaces(line));
        }
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn collapse_spaces(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::{html_to_text, parse};

    /// A cut-down but structurally faithful copy of the mail Nextcloud sends
    /// from Settings -> Basic settings -> "Send email": multipart/alternative,
    /// both parts quoted-printable, soft line breaks mid-word.
    const NEXTCLOUD_TEST_EMAIL: &str = concat!(
        "Subject: Email setting test\r\n",
        "From: nextcloud@mail.home.heimbs.me\r\n",
        "To: lenny@example.org\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/alternative; boundary=ZkhUrnZN\r\n",
        "\r\n",
        "--ZkhUrnZN\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "Content-Transfer-Encoding: quoted-printable\r\n",
        "\r\n",
        "Well done, lenny!\r\n",
        "\r\n",
        "If you received this email, the email configuration =\r\n",
        "seems to be correct.\r\n",
        "\r\n",
        "--=20\r\n",
        "Nextcloud - a safe home for all your dat=\r\n",
        "a\r\n",
        "--ZkhUrnZN\r\n",
        "Content-Type: text/html; charset=utf-8\r\n",
        "Content-Transfer-Encoding: quoted-printable\r\n",
        "\r\n",
        "<html><body><h1 class=3D\"text-center\">Well done, lenny!</h1>\r\n",
        "=09<p>If you received this email, the email configuration seems to be =\r\n",
        "correct.</p></body></html>\r\n",
        "--ZkhUrnZN--\r\n",
    );

    #[test]
    fn multipart_alternative_takes_the_plain_part_and_decodes_it() {
        let parsed = parse(NEXTCLOUD_TEST_EMAIL).expect("parses");

        assert_eq!(parsed.subject.as_deref(), Some("Email setting test"));
        assert_eq!(
            parsed.body,
            "Well done, lenny!\n\
             \n\
             If you received this email, the email configuration seems to be correct.\n\
             \n\
             -- \n\
             Nextcloud - a safe home for all your data"
        );
    }

    #[test]
    fn multipart_alternative_leaves_no_trace_of_the_html_part() {
        let parsed = parse(NEXTCLOUD_TEST_EMAIL).expect("parses");

        // The three symptoms of forwarding the raw DATA block: boundary
        // delimiters, the markup itself, and undecoded quoted-printable.
        assert!(!parsed.body.contains("ZkhUrnZN"));
        assert!(!parsed.body.contains("<h1"));
        assert!(!parsed.body.contains("=3D"));
        assert!(!parsed.body.contains("=09"));
    }

    #[test]
    fn quoted_printable_soft_breaks_rejoin_the_word_they_split() {
        let parsed = parse(NEXTCLOUD_TEST_EMAIL).expect("parses");

        // "dat=\r\na" is one word, not two lines.
        assert!(parsed.body.contains("all your data"));
    }

    #[test]
    fn base64_parts_are_decoded() {
        let data = concat!(
            "Subject: encoded\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "SGVsbG8sIHdvcmxkIQ==\r\n",
        );

        assert_eq!(parse(data).expect("parses").body, "Hello, world!");
    }

    #[test]
    fn encoded_word_subjects_are_decoded() {
        let data = concat!(
            "Subject: =?utf-8?B?QsO8Y2hlcmVpIHZlcmbDvGdiYXI=?=\r\n",
            "\r\n",
            "body\r\n",
        );

        assert_eq!(
            parse(data).expect("parses").subject.as_deref(),
            Some("Bücherei verfügbar")
        );
    }

    #[test]
    fn a_plain_non_mime_message_is_unchanged() {
        // The shape every cron / apticron / mailx message has. This path must
        // keep behaving exactly as it did before MIME parsing existed.
        let data =
            "Subject: Cron <root@omv> /usr/bin/backup\r\n\r\n/usr/bin/backup: line 4: warning\r\n";
        let parsed = parse(data).expect("parses");

        assert_eq!(
            parsed.subject.as_deref(),
            Some("Cron <root@omv> /usr/bin/backup")
        );
        assert_eq!(parsed.body, "/usr/bin/backup: line 4: warning");
    }

    #[test]
    fn html_only_mail_falls_back_to_stripped_text() {
        let data = concat!(
            "Subject: html only\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<html><body><p>Disk <b>sda</b> is failing.</p>",
            "<p>Replace it.</p></body></html>\r\n",
        );

        // Paragraphs keep a blank line between them, as they read on screen.
        assert_eq!(
            parse(data).expect("parses").body,
            "Disk sda is failing.\n\nReplace it."
        );
    }

    #[test]
    fn nested_multipart_still_finds_the_plain_part() {
        let data = concat!(
            "Subject: nested\r\n",
            "Content-Type: multipart/mixed; boundary=outer\r\n",
            "\r\n",
            "--outer\r\n",
            "Content-Type: multipart/alternative; boundary=inner\r\n",
            "\r\n",
            "--inner\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "the readable part\r\n",
            "--inner\r\n",
            "Content-Type: text/html\r\n",
            "\r\n",
            "<p>the markup</p>\r\n",
            "--inner--\r\n",
            "--outer--\r\n",
        );

        assert_eq!(parse(data).expect("parses").body, "the readable part");
    }

    #[test]
    fn attachments_are_named_but_not_included() {
        let data = concat!(
            "Subject: backup report\r\n",
            "Content-Type: multipart/mixed; boundary=sep\r\n",
            "\r\n",
            "--sep\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "Backup finished.\r\n",
            "--sep\r\n",
            "Content-Type: application/pdf\r\n",
            "Content-Disposition: attachment; filename=\"report.pdf\"\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "SGVsbG8sIHdvcmxkIQ==\r\n",
            "--sep--\r\n",
        );
        let parsed = parse(data).expect("parses");

        assert_eq!(
            parsed.body,
            "Backup finished.\n\n[1 attachment: report.pdf]"
        );
        // The attachment is noted, never decoded into the message.
        assert!(!parsed.body.contains("Hello, world!"));
    }

    #[test]
    fn several_attachments_are_listed_together() {
        let data = concat!(
            "Content-Type: multipart/mixed; boundary=sep\r\n",
            "\r\n",
            "--sep\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "see attached\r\n",
            "--sep\r\n",
            "Content-Type: application/pdf\r\n",
            "Content-Disposition: attachment; filename=\"a.pdf\"\r\n",
            "\r\n",
            "x\r\n",
            "--sep\r\n",
            "Content-Type: image/png\r\n",
            "Content-Disposition: attachment; filename=\"b.png\"\r\n",
            "\r\n",
            "y\r\n",
            "--sep--\r\n",
        );

        assert_eq!(
            parse(data).expect("parses").body,
            "see attached\n\n[2 attachments: a.pdf, b.png]"
        );
    }

    #[test]
    fn an_attachment_without_a_disposition_is_still_named() {
        // Plenty of mailers send only `Content-Type: ...; name="x"`.
        let data = concat!(
            "Content-Type: multipart/mixed; boundary=sep\r\n",
            "\r\n",
            "--sep\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "body\r\n",
            "--sep\r\n",
            "Content-Type: application/octet-stream; name=\"core.dump\"\r\n",
            "\r\n",
            "z\r\n",
            "--sep--\r\n",
        );

        assert_eq!(
            parse(data).expect("parses").body,
            "body\n\n[1 attachment: core.dump]"
        );
    }

    #[test]
    fn an_attachment_only_message_still_says_what_arrived() {
        let data = concat!(
            "Subject: scan\r\n",
            "Content-Type: application/pdf; name=\"scan.pdf\"\r\n",
            "Content-Disposition: attachment; filename=\"scan.pdf\"\r\n",
            "\r\n",
            "data\r\n",
        );

        assert_eq!(
            parse(data).expect("parses").body,
            "[1 attachment: scan.pdf]"
        );
    }

    #[test]
    fn html_entities_are_decoded() {
        let html = "<p>a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#39; &#x2713;</p>";

        assert_eq!(html_to_text(html), "a & b <c> \"d\" 'e' ✓");
    }

    #[test]
    fn html_scripts_and_styles_are_dropped_entirely() {
        let html = concat!(
            "<html><head><style>body{color:#fff}</style></head>",
            "<body><script>alert('x')</script><p>real text</p></body></html>",
        );

        assert_eq!(html_to_text(html), "real text");
    }
}
