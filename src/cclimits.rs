//! Claude Code's own rate-limit numbers, handed to its `statusLine` command.
//!
//! `scripts/statusline.sh` writes the small `cc-limits.json` file in the state
//! directory. The tail worker reads it on its one-second tick, only when the
//! file's mtime changes, and publishes it in the live payload and `/api/brief`.
//! Nothing from the status line JSON other than `.rate_limits` is ever stored.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// One rate-limit window, in the shape the page and the brief both use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct CcWindow {
    /// Percent used, 0 to 100.
    pub pct: f64,
    /// When the window resets, epoch milliseconds.
    pub resets_ms: i64,
}

/// The two windows Claude Code reports, plus when the report was written.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CcLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<CcWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<CcWindow>,
    /// Epoch milliseconds; the page uses it to age the report.
    pub at: i64,
}

/// The on-disk shape. Unknown keys are ignored by serde; window objects carry
/// `used_percentage` and `resets_at` (epoch seconds) exactly as Claude sends
/// them. Keeping the raw names here means the status line script needs no
/// arithmetic beyond the timestamp.
#[derive(Deserialize)]
struct RawLimits {
    #[serde(default)]
    five_hour: Option<RawWindow>,
    #[serde(default)]
    seven_day: Option<RawWindow>,
    #[serde(default)]
    at: Option<i64>,
}

#[derive(Deserialize)]
struct RawWindow {
    #[serde(default)]
    used_percentage: Option<f64>,
    #[serde(default)]
    resets_at: Option<i64>,
}

fn window(raw: RawWindow, now: i64) -> Option<CcWindow> {
    let pct = raw.used_percentage?;
    if !pct.is_finite() || !(0.0..=100.0).contains(&pct) {
        return None;
    }
    let resets_ms = raw.resets_at?.checked_mul(1000)?;
    // A window whose reset already passed is not shown at all.
    if resets_ms <= now {
        return None;
    }
    Some(CcWindow { pct, resets_ms })
}

/// Strict parse: a malformed or expired window is dropped, never guessed.
pub fn parse(bytes: &[u8], now: i64) -> Option<CcLimits> {
    let raw: RawLimits = serde_json::from_slice(bytes).ok()?;
    let at = raw.at.unwrap_or(0);
    let five_hour = raw.five_hour.and_then(|w| window(w, now));
    let seven_day = raw.seven_day.and_then(|w| window(w, now));
    if at <= 0 && five_hour.is_none() && seven_day.is_none() {
        return None;
    }
    Some(CcLimits { at, five_hour, seven_day })
}

pub fn load(path: &Path, now: i64) -> Option<CcLimits> {
    parse(&std::fs::read(path).ok()?, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_738_425_600_000; // epoch ms

    fn json(five: &str, seven: &str, at: i64) -> Vec<u8> {
        format!("{{\"five_hour\":{five},\"seven_day\":{seven},\"at\":{at},\"session_id\":\"secret\",\"cwd\":\"/private\"}}").into_bytes()
    }

    #[test]
    fn parses_both_windows_and_ignores_unknown_keys() {
        let c = parse(
            &json(
                r#"{"used_percentage":23.5,"resets_at":1738429200,"extra":1}"#,
                r#"{"used_percentage":41.2,"resets_at":1738857600}"#,
                NOW,
            ),
            NOW,
        )
        .unwrap();
        assert_eq!(c.at, NOW);
        let f = c.five_hour.unwrap();
        assert_eq!(f.pct, 23.5);
        assert_eq!(f.resets_ms, 1_738_429_200_000);
        let s = c.seven_day.unwrap();
        assert_eq!(s.pct, 41.2);
        assert_eq!(s.resets_ms, 1_738_857_600_000);
    }

    #[test]
    fn missing_window_is_omitted() {
        let c = parse(&json(r#"{"used_percentage":23.5,"resets_at":1738429200}"#, "null", NOW), NOW).unwrap();
        assert!(c.five_hour.is_some() && c.seven_day.is_none());
        let c = parse(&json("null", "null", NOW), NOW).unwrap();
        assert!(c.five_hour.is_none() && c.seven_day.is_none() && c.at == NOW);
    }

    #[test]
    fn out_of_range_percentage_is_rejected() {
        for bad in ["-1", "100.1", "1000", "null"] {
            let w = format!(r#"{{"used_percentage":{bad},"resets_at":1738429200}}"#);
            let c = parse(&json(&w, r#"{"used_percentage":41.2,"resets_at":1738857600}"#, NOW), NOW).unwrap();
            assert!(c.five_hour.is_none(), "{bad}");
            assert!(c.seven_day.is_some(), "{bad}");
        }
        // 0 and 100 are both valid endpoints.
        for good in ["0", "100"] {
            let w = format!(r#"{{"used_percentage":{good},"resets_at":1738429200}}"#);
            let c = parse(&json(&w, "null", NOW), NOW).unwrap();
            assert_eq!(c.five_hour.unwrap().pct, good.parse::<f64>().unwrap());
        }
    }

    #[test]
    fn past_reset_is_dropped() {
        let c = parse(&json(r#"{"used_percentage":23.5,"resets_at":1738425600}"#, r#"{"used_percentage":41.2,"resets_at":1738425600}"#, NOW), NOW).unwrap();
        assert!(c.five_hour.is_none() && c.seven_day.is_none());
    }

    #[test]
    fn garbage_is_not_a_limit_report() {
        assert!(parse(b"not json", NOW).is_none());
        assert!(parse(&json("null", "null", 0), NOW).is_none());
    }
}
