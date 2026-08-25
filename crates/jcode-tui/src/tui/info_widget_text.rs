/// Split `s` into lines that each fit within `max_width` display columns. Text
/// is never truncated: word boundaries are preferred, and any single word wider
/// than `max_width` is hard-broken so the full content always survives. Returns
/// at least one (possibly empty) line. This is what lets todo items and group
/// names display completely instead of being ellipsized.
pub(super) fn wrap_text_width(s: &str, max_width: usize) -> Vec<String> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if max_width == 0 {
        // No room: still report the full content on one (invisible) line so the
        // height math never drops it entirely.
        return vec![s.to_string()];
    }

    let words: Vec<&str> = s.split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in words {
        let word_width = word.width();
        let fits = current_width == 0 || current_width + 1 + word_width <= max_width;
        if fits {
            if current_width == 0 {
                current.push_str(word);
                current_width = word_width;
            } else {
                current.push(' ');
                current.push_str(word);
                current_width += 1 + word_width;
            }
            continue;
        }

        // Flush the running line before placing the next word.
        if !current.is_empty() {
            out.push(std::mem::take(&mut current));
            current_width = 0;
        }

        if word_width <= max_width {
            current.push_str(word);
            current_width = word_width;
        } else {
            // Hard-break an over-wide word across multiple lines.
            let mut chunk = String::new();
            let mut cw = 0usize;
            for ch in word.chars() {
                let w = UnicodeWidthChar::width(ch).unwrap_or(0);
                if cw + w > max_width && !chunk.is_empty() {
                    out.push(std::mem::take(&mut chunk));
                    cw = 0;
                }
                chunk.push(ch);
                cw += w;
            }
            if !chunk.is_empty() {
                current = chunk;
                current_width = cw;
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        // Empty / whitespace-only input, or nothing fit: still report one line
        // so callers can index/count it confidently.
        vec![String::new()]
    } else {
        out
    }
}

pub(super) fn truncate_smart(s: &str, max_len: usize) -> String {
    let char_len = s.chars().count();
    if char_len <= max_len {
        return s.to_string();
    }
    if max_len <= 3 {
        return "...".to_string();
    }

    let target = max_len - 3;
    let prefix = truncate_chars(s, target);

    if let Some(pos) = prefix.rfind(' ') {
        let before = &prefix[..pos];
        let pos_chars = before.chars().count();
        if pos_chars > target / 2 {
            return format!("{}...", before);
        }
    }
    format!("{}...", prefix)
}

pub(super) fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

pub(super) fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    let truncated = truncate_chars(s, max_chars.saturating_sub(1));
    format!("{}…", truncated)
}
