//! Detection and wait-time extraction for HTTP 422 token-limit errors from
//! OpenAI-compatible providers.
//!
//! Some OpenAI-compatible endpoints enforce a strict completion-token cap and
//! return a `422 Unprocessable Entity` when the request would exceed it. The
//! error body frequently carries a human-readable wait time (e.g. "Retry in 18
//! min.") that is more useful than the default exponential backoff. This module
//! recognizes only genuine token-limit 422s so unrelated 422s (a malformed
//! request, tool-schema rejections, etc.) are left alone.
//!
//! Relying on a `regex` dependency would pull a heavy crate into the provider
//! runtime, so wait-time parsing is done with case-insensitive substring
//! scanning over a small set of well-known patterns.

use std::time::Duration;

/// The lowercased markers that identify a 422 as a token-limit exhaustion
/// rather than some other 422. A body matching any of these is retryable.
const TOKEN_LIMIT_MARKERS: &[&str] = &[
    // English.
    "token limit",
    "token_limit",
    "completion token",
    "completion_token",
    "exceeded",
    // Russian ("превышен" = exceeded, "лимит" = limit, "токен" = token).
    "превышен",
    "лимит",
    "токен",
];

/// Identify whether an HTTP 422 error body points to token-limit exhaustion.
///
/// `body` may be the raw response body; markers are matched case-insensitively.
/// Returns `false` for anything that does not look like a token-limit error so
/// unrelated 422s are not retried.
pub fn is_token_limit_error(body: &str) -> bool {
    let lower = body.to_lowercase();
    TOKEN_LIMIT_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Extract a server-suggested wait from an error body, if one is present.
///
/// Supported forms (case-insensitive), resolved in pattern-specific priority
/// and otherwise left-to-right by first match:
///
/// - Minutes: `retry in (\d+) minutes`, `retry after (\d+) minutes`,
///   `повторите попытку через (\d+) мин`, `через (\d+) мин`, `(\d+) минут`
/// - Seconds: `retry after (\d+) seconds`, `через (\d+) сек`, `(\d+) секунд`
///
/// Returns `None` when no wait is found so callers fall back to exponential
/// backoff.
pub fn extract_wait_time(body: &str) -> Option<Duration> {
    let lower = body.to_lowercase();

    if let Some(secs) = match_number_then(&lower, "мин", |n| n * 60)
        .or_else(|| match_number_then(&lower, "minute", |n| n * 60))
        .or_else(|| match_number_then(&lower, "сек", |n| n))
        .or_else(|| match_number_then(&lower, "sec", |n| n))
        .or_else(|| match_number_then(&lower, "second", |n| n))
    {
        return Some(Duration::from_secs(secs));
    }

    None
}

/// Match a `<number> <unit>` pair anywhere in `lower` (with optional whitespace
/// between them) and map the number through `map`. Returns `None` if `unit` is
/// absent or no digit precedes it.
fn match_number_then(lower: &str, unit: &str, map: impl Fn(u64) -> u64) -> Option<u64> {
    let mut from = 0;
    while let Some(idx) = lower[from..].find(unit) {
        let idx = from + idx;
        let prefix = &lower[..idx];
        if let Some(num) = trailing_number(prefix) {
            return Some(map(num));
        }
        // Skip past this occurrence to allow later matches for other units.
        from = idx + unit.len();
    }
    None
}

/// Parse the integer immediately before the end of `s` (skipping trailing
/// whitespace). Returns `None` if there is no digit.
fn trailing_number(s: &str) -> Option<u64> {
    let trimmed = s.trim_end();
    let bytes = trimmed.as_bytes();
    let start = bytes
        .iter()
        .rposition(|byte| byte.is_ascii_digit())?;
    let mut begin = start;
    while begin > 0 && bytes[begin - 1].is_ascii_digit() {
        begin -= 1;
    }
    let num = trimmed[begin..=start].parse::<u64>().ok()?;
    if num > u64::MAX / 2 {
        None
    } else {
        Some(num)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_english_token_limit_422() {
        assert!(is_token_limit_error(
            "Token limit exceeded: used 5000, limit 4096"
        ));
        assert!(is_token_limit_error("completion token limit reached"));
        assert!(is_token_limit_error("maximum context length exceeded"));
    }

    #[test]
    fn detects_russian_token_limit_422() {
        assert!(is_token_limit_error(
            "Превышен лимит completion-токенов: использовано 60323, лимит 60000. Повторите попытку через 18 мин."
        ));
        assert!(is_token_limit_error("Лимит токенов превышен"));
    }

    #[test]
    fn ignores_unrelated_422s() {
        assert!(!is_token_limit_error("invalid request payload"));
        assert!(!is_token_limit_error("model is not supported"));
        assert!(!is_token_limit_error(""));
        // A clear-cut unrelated error that shares none of the marker tokens.
        assert!(!is_token_limit_error("internal constraint violation: no quota"));
    }

    #[test]
    fn exact_reported_deepseek_body_is_detected_and_wait_extracted() {
        // Verbatim OpenAI-compatible body reported by a real user hitting a
        // completion-token cap on DeepSeek-V4-Flash. Guards the exact strings
        // that prompted the retry feature.
        let body = r#"{"error":"Превышен лимит completion-токенов: использовано 60520, лимит 60000. Повторите попытку через 8 мин."}"#;
        assert!(is_token_limit_error(body));
        assert_eq!(
            extract_wait_time(body),
            Some(Duration::from_secs(8 * 60))
        );
    }

    #[test]
    fn waits_are_case_insensitive() {
        assert!(is_token_limit_error("TOKEN LIMIT EXCEEDED: RETRY IN 5 MINUTES"));
        assert!(is_token_limit_error("ЛИМИТ ТОКЕНОВ ПРЕВЫШЕН"));
    }

    #[test]
    fn extracts_english_minutes() {
        assert_eq!(
            extract_wait_time("Token limit exceeded: retry in 18 minutes"),
            Some(Duration::from_secs(18 * 60))
        );
        assert_eq!(
            extract_wait_time("retry after 2 minutes"),
            Some(Duration::from_secs(2 * 60))
        );
    }

    #[test]
    fn extracts_seconds() {
        assert_eq!(
            extract_wait_time("Token limit exceeded: retry after 45 seconds"),
            Some(Duration::from_secs(45))
        );
        assert_eq!(
            extract_wait_time("please wait 30 sec"),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn extracts_russian() {
        assert_eq!(
            extract_wait_time(
                "Превышен лимит completion-токенов: использовано 60323, лимит 60000. Повторите попытку через 18 мин."
            ),
            Some(Duration::from_secs(18 * 60))
        );
        assert_eq!(
            extract_wait_time("Превышен лимит токенов, повторите через 3 минут"),
            Some(Duration::from_secs(3 * 60))
        );
        assert_eq!(
            extract_wait_time("Лимит превышен, через 45 сек"),
            Some(Duration::from_secs(45))
        );
    }

    #[test]
    fn no_wait_suggests_none() {
        assert_eq!(
            extract_wait_time("Token limit exceeded: model is full, try again later"),
            None
        );
        assert_eq!(extract_wait_time("Превышен лимит токенов"), None);
    }

    #[test]
    fn overflows_and_garbage_do_not_panic() {
        assert_eq!(
            extract_wait_time("повторите попытку через 99999999999999999999999999 мин"),
            None
        );
        assert_eq!(extract_wait_time(""), None);
        assert_eq!(
            extract_wait_time("Повторите попытку через 18 мин."),
            Some(Duration::from_secs(18 * 60))
        );
    }
}