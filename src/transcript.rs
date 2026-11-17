//! Claude Code transcript lines: what we read, and only that.
//!
//! A transcript line is one JSON object. We look at a small set of fields
//! (type, timestamp, message id/model/usage, entrypoint, cwd, custom title,
//! API error markers) and never at `content`. Lines are pre-filtered by
//! substring so most lines are never handed to the JSON parser at all.

use memchr::memmem;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

use crate::timeutil::parse_ts_ms;

/// Token counts for one request or a sum of requests.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "[u64; 5]", into = "[u64; 5]")]
pub struct Tokens {
    /// Uncached input.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_w5m: u64,
    pub cache_w1h: u64,
}

impl From<[u64; 5]> for Tokens {
    fn from(a: [u64; 5]) -> Self {
        Tokens { input: a[0], output: a[1], cache_read: a[2], cache_w5m: a[3], cache_w1h: a[4] }
    }
}

impl From<Tokens> for [u64; 5] {
    fn from(t: Tokens) -> Self {
        [t.input, t.output, t.cache_read, t.cache_w5m, t.cache_w1h]
    }
}

impl Tokens {
    pub fn add(&mut self, o: &Tokens) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_w5m += o.cache_w5m;
        self.cache_w1h += o.cache_w1h;
    }
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_w5m + self.cache_w1h
    }
    pub fn is_zero(&self) -> bool {
        self.total() == 0
    }
}

/// Model families. Index into per-family arrays.
pub const FAMILIES: [&str; 4] = ["opus", "sonnet", "haiku", "other"];

pub fn family(model: &str) -> usize {
    if model.contains("opus") {
        0
    } else if model.contains("sonnet") {
        1
    } else if model.contains("haiku") {
        2
    } else {
        3
    }
}

/// List price in USD per million tokens: (input, output, cache read).
/// Source: the Claude model table (Opus 5.5, Sonnet 5.5, Haiku 5.5), which
/// matches T3's own rate file. Used only as relative weights for the 5-hour
/// limit estimate; no dollar figure is ever shown.
fn price(model: &str) -> (f64, f64, f64) {
    let m = model;
    if m.contains("fable") || m.contains("mythos") {
        if m.contains("5-2") {
            (8.0, 40.0, 0.20)
        } else {
            (10.0, 50.0, 0.50)
        }
    } else if m.contains("opus") {
        if m.contains("5-5") {
            (4.0, 20.0, 0.20)
        } else {
            (5.0, 25.0, 0.50)
        }
    } else if m.contains("sonnet") {
        if m.contains("5-5") {
            (2.0, 10.0, 0.10)
        } else if m.contains("sonnet-5") {
            (2.0, 10.0, 0.20)
        } else {
            (3.0, 15.0, 0.30)
        }
    } else if m.contains("haiku") {
        if m.contains("5-5") {
            (0.10, 0.50, 0.01)
        } else {
            (1.0, 5.0, 0.10)
        }
    } else {
        (4.0, 20.0, 0.20)
    }
}

/// Weighted usage of a request in "list-price micro-dollars". Only ratios of
/// this number are ever shown. Cache writes cost 1.25x input (5m) and 2x (1h);
/// fast mode costs 2x.
pub fn weight(model: &str, t: &Tokens, fast: bool) -> f64 {
    let (i, o, r) = price(model);
    let w = t.input as f64 * i
        + t.output as f64 * o
        + t.cache_read as f64 * r
        + t.cache_w5m as f64 * i * 1.25
        + t.cache_w1h as f64 * i * 2.0;
    if fast {
        w * 2.0
    } else {
        w
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A billed API response (one line per content block; dedupe by msg id).
    Usage,
    /// A rate-limit (429) error recorded by Claude Code.
    RateLimit,
    /// A session title set by the user or the app.
    Title,
    /// Any other line we parsed for session metadata (cwd, entrypoint).
    Meta,
}

#[derive(Debug, Clone)]
pub struct Line {
    pub kind: LineKind,
    pub ts_ms: i64,
    pub msg_id: Option<String>,
    pub model: Option<String>,
    pub tokens: Tokens,
    pub fast: bool,
    pub entrypoint: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
}

#[derive(Deserialize)]
struct RawLine<'a> {
    #[serde(rename = "type", borrow, default)]
    t: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    timestamp: Option<Cow<'a, str>>,
    // Never wrapped in `Lenient`: an untagged fallback would buffer the whole
    // message, content included. Unknown fields (content) are skipped unread.
    #[serde(borrow, default)]
    message: Option<RawMsg<'a>>,
    #[serde(rename = "isApiErrorMessage", default)]
    api_error: Option<Lenient<bool>>,
    #[serde(default)]
    error: Option<Lenient<Cow<'a, str>>>,
    #[serde(default)]
    entrypoint: Option<Lenient<Cow<'a, str>>>,
    #[serde(default)]
    cwd: Option<Lenient<Cow<'a, str>>>,
    #[serde(rename = "customTitle", default)]
    custom_title: Option<Lenient<Cow<'a, str>>>,
}

/// Accepts the expected type or silently ignores anything else, so one
/// unexpected field shape never drops the whole line.
#[derive(Debug)]
enum Lenient<T> {
    Ok(T),
    Other,
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Lenient<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Untagged fallback without buffering big values: try T, else skip.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either<T> {
            A(T),
            B(IgnoredAny),
        }
        Ok(match Either::<T>::deserialize(d)? {
            Either::A(v) => Lenient::Ok(v),
            Either::B(_) => Lenient::Other,
        })
    }
}

impl<T> Lenient<T> {
    fn get(self) -> Option<T> {
        match self {
            Lenient::Ok(v) => Some(v),
            Lenient::Other => None,
        }
    }
}

#[derive(Deserialize)]
struct RawMsg<'a> {
    #[serde(borrow, default)]
    id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    model: Option<Cow<'a, str>>,
    #[serde(default)]
    usage: Option<RawUsage<'a>>,
}

#[derive(Deserialize, Default)]
struct RawUsage<'a> {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation: Option<RawCacheCreation>,
    #[serde(borrow, default)]
    speed: Option<Cow<'a, str>>,
}

#[derive(Deserialize, Default)]
struct RawCacheCreation {
    #[serde(default)]
    ephemeral_5m_input_tokens: Option<u64>,
    #[serde(default)]
    ephemeral_1h_input_tokens: Option<u64>,
}

fn has(line: &[u8], needle: &[u8]) -> bool {
    memmem::find(line, needle).is_some()
}

/// Parses one transcript line. `want_meta` asks for cwd/entrypoint from a
/// line that is otherwise uninteresting (used until a session's metadata is
/// known). Returns `None` for lines we do not need.
pub fn parse_line(line: &[u8], want_meta: bool) -> Option<Line> {
    let usage_like = has(line, b"\"type\":\"assistant\"") && has(line, b"\"usage\"");
    let limit_like = has(line, b"\"rate_limit\"");
    let title_like = has(line, b"\"customTitle\"");
    let meta_like = want_meta && has(line, b"\"cwd\"");
    if !(usage_like || limit_like || title_like || meta_like) {
        return None;
    }
    let raw: RawLine = serde_json::from_slice(line).ok()?;
    let ts_ms = raw.timestamp.as_deref().and_then(parse_ts_ms).unwrap_or(0);
    let entrypoint = raw.entrypoint.and_then(Lenient::get).map(Cow::into_owned);
    let cwd = raw.cwd.and_then(Lenient::get).map(Cow::into_owned);
    let mut out = Line {
        kind: LineKind::Meta,
        ts_ms,
        msg_id: None,
        model: None,
        tokens: Tokens::default(),
        fast: false,
        entrypoint,
        cwd,
        title: None,
    };
    let t = raw.t.as_deref().unwrap_or("");

    if t == "custom-title" {
        out.kind = LineKind::Title;
        out.title = raw.custom_title.and_then(Lenient::get).map(|s| s.trim().to_string());
        return out.title.as_ref().filter(|s| !s.is_empty()).map(|_| out.clone());
    }

    let api_error = raw.api_error.and_then(Lenient::get).unwrap_or(false);
    let error = raw.error.and_then(Lenient::get);
    if api_error && error.as_deref() == Some("rate_limit") {
        out.kind = LineKind::RateLimit;
        return if ts_ms > 0 { Some(out) } else { None };
    }

    if t == "assistant" {
        if let Some(msg) = raw.message {
            let model = msg.model.as_deref().unwrap_or("");
            if let Some(u) = msg.usage {
                if !model.is_empty() && model != "<synthetic>" && ts_ms > 0 {
                    let cc_total = u.cache_creation_input_tokens.unwrap_or(0);
                    let (w5, w1) = match u.cache_creation {
                        Some(cc) => {
                            let a = cc.ephemeral_5m_input_tokens.unwrap_or(0);
                            let b = cc.ephemeral_1h_input_tokens.unwrap_or(0);
                            if a + b == 0 {
                                (cc_total, 0)
                            } else {
                                (a, b)
                            }
                        }
                        None => (cc_total, 0),
                    };
                    let tokens = Tokens {
                        input: u.input_tokens.unwrap_or(0),
                        output: u.output_tokens.unwrap_or(0),
                        cache_read: u.cache_read_input_tokens.unwrap_or(0),
                        cache_w5m: w5,
                        cache_w1h: w1,
                    };
                    if !tokens.is_zero() {
                        out.kind = LineKind::Usage;
                        out.msg_id = msg.id.map(Cow::into_owned);
                        out.model = Some(model.to_string());
                        out.fast = u.speed.as_deref() == Some("fast");
                        out.tokens = tokens;
                        return Some(out);
                    }
                }
            }
        }
    }

    if want_meta && (out.cwd.is_some() || out.entrypoint.is_some()) {
        return Some(out);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const USAGE: &str = r#"{"parentUuid":"a","isSidechain":false,"message":{"id":"msg_1","model":"claude-opus-5-5","role":"assistant","content":[{"type":"text","text":"he said \"usage\":{ hi"}],"usage":{"input_tokens":2,"cache_creation_input_tokens":1885,"cache_read_input_tokens":93313,"output_tokens":836,"cache_creation":{"ephemeral_1h_input_tokens":1885,"ephemeral_5m_input_tokens":0},"speed":"standard"}},"type":"assistant","timestamp":"2026-11-17T15:10:00.000Z","entrypoint":"cli","cwd":"/Users/x/projects/demo","sessionId":"s1"}"#;

    #[test]
    fn usage_line_has_all_token_fields() {
        let l = parse_line(USAGE.as_bytes(), false).unwrap();
        assert_eq!(l.kind, LineKind::Usage);
        assert_eq!(l.msg_id.as_deref(), Some("msg_1"));
        assert_eq!(l.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(l.ts_ms, 1_794_928_200_000);
        assert_eq!(l.tokens, Tokens { input: 2, output: 836, cache_read: 93_313, cache_w5m: 0, cache_w1h: 1885 });
        assert_eq!(l.entrypoint.as_deref(), Some("cli"));
        assert_eq!(l.cwd.as_deref(), Some("/Users/x/projects/demo"));
        assert!(!l.fast);
    }

    #[test]
    fn synthetic_zero_usage_is_ignored() {
        let s = r#"{"type":"assistant","timestamp":"2026-11-17T15:10:00Z","message":{"id":"m","model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0}}}"#;
        assert!(parse_line(s.as_bytes(), false).is_none());
    }

    #[test]
    fn rate_limit_line_is_detected() {
        let s = r#"{"type":"assistant","timestamp":"2026-11-17T12:56:00Z","isApiErrorMessage":true,"error":"rate_limit","apiErrorStatus":429,"message":{"id":"m","model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0},"content":[{"type":"text","text":"limit"}]}}"#;
        let l = parse_line(s.as_bytes(), false).unwrap();
        assert_eq!(l.kind, LineKind::RateLimit);
        assert_eq!(l.ts_ms, parse_ts_ms("2026-11-17T12:56:00Z").unwrap());
    }

    #[test]
    fn custom_title_line() {
        let s = r#"{"type":"custom-title","customTitle":"  My project  ","sessionId":"s1"}"#;
        let l = parse_line(s.as_bytes(), false).unwrap();
        assert_eq!(l.kind, LineKind::Title);
        assert_eq!(l.title.as_deref(), Some("My project"));
    }

    #[test]
    fn odd_field_shapes_do_not_drop_the_line() {
        let s = r#"{"type":"assistant","error":{"kind":"x"},"cwd":5,"timestamp":"2026-11-17T15:10:00Z","message":{"id":"m2","model":"claude-haiku-5-5","usage":{"input_tokens":10,"output_tokens":5,"speed":"fast"}}}"#;
        let l = parse_line(s.as_bytes(), false).unwrap();
        assert_eq!(l.kind, LineKind::Usage);
        assert!(l.fast);
        assert!(l.cwd.is_none());
    }

    #[test]
    fn meta_only_when_wanted() {
        let s = r#"{"type":"user","cwd":"/a/b","entrypoint":"claude-desktop","timestamp":"2026-11-17T15:10:00Z","message":{"role":"user","content":"secret text"}}"#;
        assert!(parse_line(s.as_bytes(), false).is_none());
        let l = parse_line(s.as_bytes(), true).unwrap();
        assert_eq!(l.kind, LineKind::Meta);
        assert_eq!(l.entrypoint.as_deref(), Some("claude-desktop"));
    }

    #[test]
    fn weights_follow_price_ratios() {
        let t = Tokens { output: 1_000_000, ..Default::default() };
        let opus = weight("claude-opus-5-5", &t, false);
        let haiku = weight("claude-haiku-5-5", &t, false);
        assert!((opus / haiku - 40.0).abs() < 1e-9);
        assert!((weight("claude-opus-5-5", &t, true) / opus - 2.0).abs() < 1e-9);
        let c = Tokens { cache_read: 1_000_000, ..Default::default() };
        assert!((weight("claude-opus-5-5", &c, false) - 200_000.0).abs() < 1e-6);
    }
}
