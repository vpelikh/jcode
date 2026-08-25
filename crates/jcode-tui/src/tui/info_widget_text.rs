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
        // A word fits on the current line only if it alone is narrower than
        // `max_width` AND the whole line still fits. The `current_width == 0`
        // case still must reject a word that is itself over-wide, otherwise an
        // over-long single word would never reach the hard-break branch.
        let fits = word_width <= max_width
            && (current_width == 0 || current_width + 1 + word_width <= max_width);
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

#[cfg(test)]
mod wraps {
    use super::wrap_text_width;
    use unicode_width::UnicodeWidthStr;

    fn width_ok(lines: &[String], max: usize) -> bool {
        use unicode_width::UnicodeWidthChar;
        // A line is "bad" (returns true) only when it overflows the budget AND
        // that overflow is avoidable. An indivisible CJK/wide glyph that is
        // itself wider than the slot (e.g. a width-2 char in a 1-wide line) is
        // not a wrap failure: the helper's contract is "never truncate", so it
        // leaves such a glyph on its own line rather than dropping it.
        !lines.iter().any(|l| {
            let w = l.width();
            if w <= max {
                return false;
            }
            let is_single_glyph = l.chars().count() == 1;
            let indivisible_overflow =
                is_single_glyph && l.chars().map(|c| UnicodeWidthChar::width(c).unwrap_or(0)).sum::<usize>() == w;
            !indivisible_overflow
        })
    }

    /// Totality invariant: every character of `src` is preserved across the
    /// wrapped lines, in order. Hard-breaks split a word across lines without a
    /// separator, so we compare whitespace-stripped not content spacing.
    fn content_preserved(lines: &[String], src: &str) -> bool {
        let expect: String = src.chars().filter(|c| !c.is_whitespace()).collect();
        let got: String = lines
            .concat()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        got == expect
    }

    #[test]
    fn fits_on_one_line() {
        let s = "abc def";
        assert_eq!(wrap_text_width(s, 7), vec!["abc def"]);
    }

    #[test]
    fn wraps_at_word_boundary() {
        let s = "aaa bbb ccc";
        // width 7 -> "aaa bbb" (7) then "ccc" (3).
        assert_eq!(wrap_text_width(s, 7), vec!["aaa bbb", "ccc"]);
    }

    #[test]
    fn all_lines_within_width() {
        for width in 1..=12usize {
            for s in [
                "short",
                "one two three four five six",
                "a bb ccc dddd eeeee",
            ] {
                let lines = wrap_text_width(s, width);
                assert!(width_ok(&lines, width), "width {width}: {lines:?}");
                assert!(content_preserved(&lines, s), "width {width}: {lines:?}");
            }
        }
    }

    #[test]
    fn over_wide_word_hard_breaks_and_preserves_all() {
        let s = "abcdefghij";
        for width in 1..=4usize {
            let lines = wrap_text_width(s, width);
            assert!(width_ok(&lines, width), "width {width}: {lines:?}");
            // Full content must survive across hard-break lines.
            let joined: String = lines.join("");
            assert_eq!(joined, s, "width {width}: {lines:?}");
        }
    }

    #[test]
    fn over_wide_word_then_more_words() {
        // A very long single word followed by a short word. The trailing word
        // must land on its own line, not be lost.
        let s = "abcdefghij zz";
        let lines = wrap_text_width(s, 4);
        assert_eq!(lines.last().map(String::as_str), Some("zz"));
        let joined: String = lines.iter().map(|l| l.replace(' ', "")).collect();
        assert_eq!(joined, "abcdefghijzz");
    }

    #[test]
    fn wide_cjk_chars_respect_display_width() {
        // Each CJK char is width 2. max_width 4 -> two chars per line.
        let s = "汉字汉字";
        let lines = wrap_text_width(s, 4);
        assert_eq!(lines, vec!["汉字", "汉字"]);
    }

    #[test]
    fn mixed_ascii_and_cjk() {
        let s = "aa汉字";
        // widths: a=1,a=1,汉=2,字=2 => total 6. Assert every line fits the width
        // budget and the full content is preserved at every width.
        for width in 1..=6usize {
            let lines = wrap_text_width(s, width);
            assert!(width_ok(&lines, width), "width {width}: {lines:?}");
            assert!(content_preserved(&lines, s), "width {width}: {lines:?}");
        }
    }

    #[test]
    fn empties_and_whitespace() {
        assert_eq!(wrap_text_width("", 10), vec![String::new()]);
        assert_eq!(wrap_text_width("   ", 10), vec![String::new()]);
        // max_width 0 still reports the full string on one line.
        assert_eq!(wrap_text_width("abc", 0), vec!["abc".to_string()]);
    }

    #[test]
    fn single_display_fits_exactly() {
        let s = "12345";
        // Exactly max_width columns: must not wrap.
        assert_eq!(wrap_text_width(s, 5), vec!["12345"]);
        // One char too wide: hard-break after 4.
        let lines = wrap_text_width(s, 4);
        assert_eq!(lines.join(""), s);
    }
}
