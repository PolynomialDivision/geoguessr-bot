//! Message formatting. Mention pills and markup are shared fleet-wide via
//! `mxbot_common::format`; this module adds geoguessr-specific helpers.

pub use mxbot_common::format::mentionify;

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
                (&input[..dot], &input[dot..]) // ext includes the dot
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
    let clean: String = buf
        .split('_')
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
    use super::*;

    #[test]
    fn sanitize_best_guess() {
        assert_eq!(
            sanitize_filename("🥇 Best guess · 6 km away.png"),
            "best_guess_6_km_away.png"
        );
    }

    #[test]
    fn sanitize_emoji_only() {
        assert_eq!(sanitize_filename("🔥🔥🔥.png").starts_with("file_"), true);
        assert!(sanitize_filename("🔥🔥🔥.png").ends_with(".png"));
    }

    #[test]
    fn sanitize_no_extension() {
        assert_eq!(sanitize_filename("📍 2/5"), "2_5");
    }

    #[test]
    fn sanitize_collapses_underscores() {
        assert_eq!(
            sanitize_filename("📸 My—cool:photo!!!.jpg"),
            "my_cool_photo.jpg"
        );
    }

    #[test]
    fn sanitize_preserves_alphanumeric() {
        assert_eq!(sanitize_filename("photo123.jpg"), "photo123.jpg");
    }
}
