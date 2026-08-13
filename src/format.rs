use std::collections::{BTreeSet, HashMap};

use matrix_sdk::ruma::{
    events::{room::message::RoomMessageEventContent, Mentions},
    OwnedUserId,
};

/// Scan `text` for Matrix user IDs and return a `RoomMessageEventContent`
/// with HTML mention pills showing the localpart as the pill label.
pub fn mentionify(text: &str) -> RoomMessageEventContent {
    build(text, |token| default_label(token))
}

/// Like `mentionify`, but looks up display names from `names`
/// (key = full MXID, value = display name) so the pill shows the
/// friendly name instead of the localpart.
/// The plain-text body is also updated: `@user:server` → `Name`.
#[allow(dead_code)]
pub fn mentionify_with_names(
    text: &str,
    names: &HashMap<String, String>,
) -> RoomMessageEventContent {
    build(text, |token| {
        names
            .get(token)
            .map(|s| s.as_str())
            .unwrap_or_else(|| default_label(token))
    })
}

// ── Internals ─────────────────────────────────────────────────────────────────

fn default_label(token: &str) -> &str {
    token
        .split(':')
        .next()
        .unwrap_or("")
        .trim_start_matches('@')
}

/// Build a `RoomMessageEventContent` by scanning `text` for MXIDs and
/// `**bold**` markers, replacing them for both the plain body and the HTML
/// body.  `label_for(mxid) -> &str` controls the pill label text.
fn build<'a>(text: &'a str, label_for: impl Fn(&'a str) -> &'a str) -> RoomMessageEventContent {
    let mut plain    = String::with_capacity(text.len());
    let mut html     = String::with_capacity(text.len() * 2);
    let mut pos      = 0;
    let mut found    = false;   // true when HTML output differs from plain
    let mut in_bold  = false;
    // Every MXID pill rendered below must also land in `m.mentions` on this
    // same event — that field, not the HTML pill, is what current Matrix
    // clients/servers use to decide push notifications and highlights.
    let mut mentioned: BTreeSet<OwnedUserId> = BTreeSet::new();

    while pos < text.len() {
        // ── **bold** markers ──────────────────────────────────────────────────
        if text.as_bytes().get(pos) == Some(&b'*')
            && text.as_bytes().get(pos + 1) == Some(&b'*')
        {
            if in_bold {
                html.push_str("</strong>");
            } else {
                html.push_str("<strong>");
            }
            in_bold = !in_bold;
            found   = true;
            pos    += 2;
            continue;
        }

        // ── [label](url) markdown links ──────────────────────────────────────────
        if text.as_bytes()[pos] == b'[' {
            if let Some(bracket_end) = text[pos + 1..].find(']') {
                let after_bracket = pos + 1 + bracket_end + 1;
                if text.as_bytes().get(after_bracket) == Some(&b'(') {
                    if let Some(paren_end) = text[after_bracket + 1..].find(')') {
                        let label = &text[pos + 1 .. pos + 1 + bracket_end];
                        let url   = &text[after_bracket + 1 .. after_bracket + 1 + paren_end];
                        plain.push_str(label);
                        html.push_str(&format!(r#"<a href="{url}">{label}</a>"#));
                        found = true;
                        pos   = after_bracket + 1 + paren_end + 1;
                        continue;
                    }
                }
            }
        }

        // ── @user:server MXID pills ───────────────────────────────────────────
        if text.as_bytes()[pos] == b'@' {
            let token_len = text[pos..]
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(c, ',' | '!' | '?' | '*' | ')' | ']' | '"' | '\'')
                })
                .unwrap_or(text.len() - pos);

            let token = &text[pos..pos + token_len];

            if token.len() > 4 && token.contains(':') {
                let label = label_for(token);
                plain.push_str(label);
                html.push_str(&format!(
                    r#"<a href="https://matrix.to/#/{token}">{label}</a>"#
                ));
                found = true;
                if let Ok(uid) = OwnedUserId::try_from(token) {
                    mentioned.insert(uid);
                }
                pos += token_len;
                continue;
            }
        }

        // ── Regular character ─────────────────────────────────────────────────
        let ch = text[pos..].chars().next().unwrap();
        plain.push(ch);
        match ch {
            '&'  => html.push_str("&amp;"),
            '<'  => html.push_str("&lt;"),
            '>'  => html.push_str("&gt;"),
            '"'  => html.push_str("&quot;"),
            '\n' => html.push_str("<br>"),
            _    => html.push(ch),
        }
        pos += ch.len_utf8();
    }

    // Close any unclosed bold tag (shouldn't happen with well-formed input).
    if in_bold {
        html.push_str("</strong>");
    }

    let content = if found {
        RoomMessageEventContent::text_html(plain, html)
    } else {
        RoomMessageEventContent::text_plain(text)
    };

    if mentioned.is_empty() {
        content
    } else {
        content.add_mentions(Mentions::with_user_ids(mentioned))
    }
}

// ── Filename sanitization ─────────────────────────────────────────────────────

/// Sanitize a string for use as a Matrix media filename / event body.
///
/// Rules:
///   - ASCII alphanumerics are kept and lowercased.
///   - Everything else (emoji, Unicode, spaces, punctuation) becomes `_`.
///   - Multiple consecutive `_` are collapsed to one.
///   - Leading/trailing `_` are trimmed.
///   - The file extension (if present) is preserved lowercased.
///   - Falls back to `file_<unix_secs>.<ext>` when the stem is empty.
pub fn sanitize_filename(input: &str) -> String {
    // Split stem and extension.  Only recognise extensions up to 5 chars long
    // consisting solely of ASCII letters/digits (e.g. .png, .jpg, .webp).
    let (stem, ext): (&str, &str) = match input.rfind('.') {
        Some(dot) => {
            let candidate = &input[dot + 1..];
            if candidate.len() <= 5 && candidate.chars().all(|c| c.is_ascii_alphanumeric()) {
                (&input[..dot], &input[dot..])   // ext includes the dot
            } else {
                (input, "")
            }
        }
        None => (input, ""),
    };

    // Map every char to ASCII-safe output.
    let mut buf = String::with_capacity(stem.len());
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() {
            buf.push(ch.to_ascii_lowercase());
        } else {
            buf.push('_');
        }
    }

    // Collapse runs of `_`, drop empty segments, rejoin.
    let clean: String = buf.split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_");

    let ext_lower = ext.to_ascii_lowercase();

    if clean.is_empty() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        format!("file_{ts}{ext_lower}")
    } else {
        format!("{clean}{ext_lower}")
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::ruma::events::room::message::MessageType;
    use super::*;

    /// Extract (plain_body, Option<html_body>) from a RoomMessageEventContent.
    fn bodies(c: &RoomMessageEventContent) -> (String, Option<String>) {
        match &c.msgtype {
            MessageType::Text(t) => (
                t.body.clone(),
                t.formatted.as_ref().map(|f| f.body.clone()),
            ),
            _ => panic!("unexpected msgtype"),
        }
    }

    #[test]
    fn replaces_single_mxid() {
        let c = mentionify("Hello @alice:example.org!");
        let (_, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains(r#"href="https://matrix.to/#/@alice:example.org""#));
        assert!(html.contains(">alice<"));
    }

    #[test]
    fn replaces_multiple_mxids() {
        let c = mentionify("@a:x.org and @b:y.org");
        let (_, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains(">a<"));
        assert!(html.contains(">b<"));
    }

    #[test]
    fn no_mxid_returns_plain() {
        let c = mentionify("no mentions here");
        let (_, html) = bodies(&c);
        assert!(html.is_none());
    }

    #[test]
    fn escapes_html_outside_mxid() {
        let c = mentionify("x < y & @u:s.org");
        let (_, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains("&lt;"));
        assert!(html.contains("&amp;"));
    }

    #[test]
    fn bold_markers_become_strong() {
        let c = mentionify("Answer: **Paris**");
        let (plain, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains("<strong>Paris</strong>"), "html={html}");
        assert!(!plain.contains('*'), "plain body={plain}");
        assert!(plain.contains("Paris"));
    }

    #[test]
    fn bold_and_mxid_together() {
        let c = mentionify("**@alice:example.org** got it right");
        let (_, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains("<strong>"), "html={html}");
        assert!(html.contains(r#"href="https://matrix.to/#/@alice:example.org""#));
    }

    #[test]
    fn markdown_link_becomes_anchor() {
        let c = mentionify("📍 Italy [Map](https://example.com/map)");
        let (plain, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains(r#"href="https://example.com/map""#), "html={html}");
        assert!(html.contains(">Map<"), "html={html}");
        assert!(plain.contains("Map"), "plain={plain}");
        assert!(!plain.contains("https://"), "raw URL should not appear in plain body");
    }

    #[test]
    fn sanitize_best_guess() {
        assert_eq!(
            sanitize_filename("🥇 Best guess · 6 km away.png"),
            "best_guess_6_km_away.png"
        );
    }

    #[test]
    fn sanitize_emoji_only() {
        assert_eq!(
            sanitize_filename("🔥🔥🔥.png").starts_with("file_"),
            true
        );
        assert!(sanitize_filename("🔥🔥🔥.png").ends_with(".png"));
    }

    #[test]
    fn sanitize_no_extension() {
        assert_eq!(sanitize_filename("📍 2/5"), "2_5");
    }

    #[test]
    fn sanitize_collapses_underscores() {
        assert_eq!(sanitize_filename("📸 My—cool:photo!!!.jpg"), "my_cool_photo.jpg");
    }

    #[test]
    fn sanitize_preserves_alphanumeric() {
        assert_eq!(sanitize_filename("photo123.jpg"), "photo123.jpg");
    }

    #[test]
    fn with_names_uses_display_name() {
        let mut names = HashMap::new();
        names.insert("@alice:example.org".to_owned(), "Alice Smith".to_owned());
        let c = mentionify_with_names("Hello @alice:example.org!", &names);
        let (_, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains(">Alice Smith<"));
        assert!(html.contains(r#"href="https://matrix.to/#/@alice:example.org""#));
    }

    fn uid(s: &str) -> OwnedUserId {
        <&matrix_sdk::ruma::UserId>::try_from(s).unwrap().to_owned()
    }

    #[test]
    fn mentionify_sets_m_mentions_on_the_same_event() {
        // The HTML pill alone does not notify anyone — current Matrix
        // clients/servers key push/highlight behaviour off `m.mentions`.
        let c = mentionify("Hello @alice:example.org!");
        let mentions = c.mentions.expect("m.mentions must be set");
        assert_eq!(mentions.user_ids, [uid("@alice:example.org")].into_iter().collect());
        assert!(!mentions.room);
    }

    #[test]
    fn mentionify_with_names_sets_m_mentions_for_every_pill() {
        let mut names = HashMap::new();
        names.insert("@alice:example.org".to_owned(), "Alice".to_owned());
        let c = mentionify_with_names("@alice:example.org and @bob:example.org", &names);
        let mentions = c.mentions.expect("m.mentions must be set");
        assert_eq!(mentions.user_ids.len(), 2);
        assert!(mentions.user_ids.contains(&uid("@alice:example.org")));
        assert!(mentions.user_ids.contains(&uid("@bob:example.org")));
    }

    #[test]
    fn no_mxid_means_no_mentions_field() {
        let c = mentionify("no mentions here");
        assert!(c.mentions.is_none());
    }
}
