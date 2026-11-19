//! Reads a usage limit from the error that ends a Gemini CLI turn.
//!
//! Over ACP a failed prompt answers with the API's HTTP status as its
//! JSON-RPC error code, 429 at a quota, and the Gemini CLI's own wording
//! of the quota (`packages/core/src/utils/googleQuotaErrors.ts` in the
//! Gemini CLI repository): "You have exhausted your daily quota on this
//! model.", a per-minute quota with "Suggested retry after 60s.", or the
//! server's "Your quota will reset after 3h20m5s.".

use crate::driver::redact_api_keys;

/// HTTP's "Too Many Requests", the status of every quota error.
const TOO_MANY_REQUESTS: i64 = 429;

/// Words that mark a quota error whatever its status.
const QUOTA_WORDS: [&str; 4] = [
    "resource_exhausted",
    "exhausted your daily quota",
    "exhausted your capacity",
    "quota exceeded",
];

/// Phrases that come right before the time a quota resets in.
const RESET_PHRASES: [&str; 4] = ["reset after", "retry after", "retry in", "resets in"];

/// A usage limit, as [`crate::AgentEvent::UsageLimitReached`] shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Limit {
    pub limit: String,
    pub resets_in_seconds: Option<u64>,
}

/// The usage limit the error with `code` and `text` reports, if it is one.
pub(super) fn usage_limit(code: i64, text: &str) -> Option<Limit> {
    let lower = text.to_lowercase();
    if code != TOO_MANY_REQUESTS && !QUOTA_WORDS.iter().any(|word| lower.contains(word)) {
        return None;
    }
    let limit = if lower.contains("daily quota") || lower.contains("per day") {
        "Daily quota".to_owned()
    } else if lower.contains("capacity") {
        "Model capacity".to_owned()
    } else if lower.contains("spending cap") {
        "Spending cap".to_owned()
    } else if lower.contains("per minute") || lower.contains("rate limit") {
        "Rate limit".to_owned()
    } else {
        // The Gemini CLI's first line names the limit.
        let first = text.lines().next().unwrap_or_default().trim();
        redact_api_keys(&first.chars().take(200).collect::<String>())
    };
    Some(Limit {
        limit,
        resets_in_seconds: resets_in(&lower),
    })
}

/// The seconds until the limit resets, from the first duration after one
/// of [`RESET_PHRASES`], rounded up.
fn resets_in(lower: &str) -> Option<u64> {
    RESET_PHRASES.iter().find_map(|phrase| {
        let start = lower.find(phrase)? + phrase.len();
        duration(lower[start..].trim_start())
    })
}

/// Parses a duration at the start of `text`: `3h20m5s`, `34.5s`, `500ms`.
fn duration(text: &str) -> Option<u64> {
    let mut rest = text;
    let mut milliseconds: u64 = 0;
    let mut parsed = false;
    loop {
        let length = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if length == 0 {
            break;
        }
        let (whole, fraction) = rest[..length]
            .split_once('.')
            .unwrap_or((&rest[..length], ""));
        let after = &rest[length..];
        let (unit, unit_length): (u64, usize) = if after.starts_with("ms") {
            (1, 2)
        } else if after.starts_with('h') {
            (3_600_000, 1)
        } else if after.starts_with('m') {
            (60_000, 1)
        } else if after.starts_with('s') {
            (1_000, 1)
        } else {
            break;
        };
        let whole: u64 = whole.parse().ok()?;
        // Thousandths of the unit; finer digits do not matter.
        let thousandths: u64 = format!("{fraction:0<3}")
            .get(..3)
            .and_then(|digits| digits.parse().ok())?;
        milliseconds = milliseconds
            .checked_add(whole.checked_mul(unit)?)?
            .checked_add(thousandths * unit / 1000)?;
        parsed = true;
        rest = &after[unit_length..];
    }
    parsed.then(|| milliseconds.div_ceil(1000))
}

#[cfg(test)]
mod tests {
    use super::{Limit, usage_limit};

    #[test]
    fn the_daily_quota_resets_at_the_time_the_server_names() {
        assert_eq!(
            usage_limit(
                429,
                "You have exhausted your daily quota on this model. \
                 Your quota will reset after 3h20m5s."
            ),
            Some(Limit {
                limit: "Daily quota".to_owned(),
                resets_in_seconds: Some(12_005),
            })
        );
    }

    #[test]
    fn a_suggested_retry_is_the_reset_rounded_up_to_seconds() {
        assert_eq!(
            usage_limit(
                429,
                "Quota exceeded for quota metric 'Generate Content API requests per minute'.\n\
                 Suggested retry after 34.5s."
            ),
            Some(Limit {
                limit: "Rate limit".to_owned(),
                resets_in_seconds: Some(35),
            })
        );
    }

    #[test]
    fn a_rate_limit_without_a_reset_time_is_a_limit_without_one() {
        assert_eq!(
            usage_limit(429, "Rate limit exceeded. Try again later."),
            Some(Limit {
                limit: "Rate limit".to_owned(),
                resets_in_seconds: None,
            })
        );
    }

    #[test]
    fn exhausted_resources_are_a_limit_whatever_the_status() {
        assert!(
            usage_limit(500, "RESOURCE_EXHAUSTED: Please retry in 500ms")
                .is_some_and(|limit| limit.resets_in_seconds == Some(1))
        );
    }

    #[test]
    fn other_errors_are_no_limit() {
        assert_eq!(usage_limit(400, "API key not valid."), None);
        assert_eq!(usage_limit(500, "Internal error"), None);
    }
}
