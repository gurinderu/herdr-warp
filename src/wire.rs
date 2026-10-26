//! Shared rules for the notifier: the wire schema, label cleaning and limits.
//!
//! Used by the VM side (hook, serve) and by the Mac agent, so both ends apply
//! the same rules.
//!
//! Wire format: one JSON object per line, UTF-8, at most [`MAX_LINE_BYTES`].
//!
//! ```text
//! {"v":1,"type":"hello","epoch":"<16 hex>","head":<seq>,"ts":<unix s>}
//! {"v":1,"type":"hb","head":<seq>,"ts":<unix s>}
//! {"v":1,"type":"event","seq":124,"ts":1793000000,"event":"needs_input",
//!  "agent":"claude","ws":"w2","tab":"w2:t1","pane":"w2:p1",
//!  "term":"term_65ead8c4a4dc72","ws_label":"my-project","tab_label":"",
//!  "title":"...","focused":false}
//! {"v":1,"type":"focus","ok":true,"code":"focused"}
//! {"v":1,"type":"error","code":"bad_command"}
//! ```

use crate::py;
use serde_json::{Map, Value};
use unicode_normalization::UnicodeNormalization;

pub const SCHEMA_VERSION: i64 = 1;
pub const MAX_LINE_BYTES: usize = 1024;
pub const LABEL_MAX: usize = 40;
/// Raw label strings longer than this are rejected before cleaning: no
/// legitimate label gets close, and it bounds the cleaning work.
pub const RAW_LABEL_MAX: usize = 256;
pub const TTL_SECONDS: i64 = 3600;
pub const FUTURE_SKEW: i64 = 300;
pub const MAX_SEQ: i64 = (1 << 53) - 1;

pub const EVENTS: [&str; 2] = ["done", "needs_input"];
pub const AGENTS: [&str; 2] = ["claude", "codex"];

/// herdr agent_status -> wire event.
pub fn status_to_event(status: &str) -> Option<&'static str> {
    match status {
        "done" => Some("done"),
        "blocked" => Some("needs_input"),
        _ => None,
    }
}

fn all_bytes(s: &str, min: usize, max: usize, ok: impl Fn(u8) -> bool) -> bool {
    (min..=max).contains(&s.len()) && s.bytes().all(ok)
}

/// `[A-Za-z0-9:_-]{1,32}`: a herdr pane / tab / workspace id.
pub fn is_id(s: &str) -> bool {
    all_bytes(s, 1, 32, |b| {
        b.is_ascii_alphanumeric() || b == b':' || b == b'_' || b == b'-'
    })
}

fn is_lower_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
}

/// `term_[0-9a-f]{6,32}`: a herdr terminal id.
pub fn is_term(s: &str) -> bool {
    s.strip_prefix("term_")
        .is_some_and(|h| all_bytes(h, 6, 32, is_lower_hex))
}

/// `[0-9a-f]{16}`: a spool epoch.
pub fn is_epoch(s: &str) -> bool {
    all_bytes(s, 16, 16, is_lower_hex)
}

/// `[a-z_]{1,32}`: a focus / error code.
pub fn is_code(s: &str) -> bool {
    all_bytes(s, 1, 32, |b| b.is_ascii_lowercase() || b == b'_')
}

/// Turned into a plain space first, so "a\nb" stays two words.
fn is_space_first(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\r'
            | '\x0b'
            | '\x0c'
            | '\x1c'
            | '\x1d'
            | '\x1e'
            | '\x1f'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// A short, single-line, printable label.
///
/// Letters of every script are kept (pane titles are often Cyrillic).
/// Dropped outright: controls (incl. ESC, BEL, C1), format chars (bidi
/// overrides, zero-width joiners/spaces), private use, surrogates, unassigned
/// and line/paragraph separators.
pub fn clean_str(text: &str, limit: usize) -> String {
    let text: String = py::take_chars(text, RAW_LABEL_MAX * 2).nfc().collect();
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if is_space_first(ch) {
            out.push(' ');
        } else if !py::is_drop(ch) {
            out.push(ch);
        }
    }
    let text = py::split_ws(&out).collect::<Vec<_>>().join(" ");
    if text.chars().count() > limit {
        let mut t = py::rstrip(py::take_chars(&text, limit.saturating_sub(1))).to_string();
        t.push('…');
        t
    } else {
        text
    }
}

/// [`clean_str`] for a decoded JSON value: "" for anything not a string.
pub fn clean_label(v: Option<&Value>, limit: usize) -> String {
    match v {
        Some(Value::String(s)) => clean_str(s, limit),
        _ => String::new(),
    }
}

/// Compact one-line JSON with sorted keys, UTF-8 kept as is (Python's
/// `json.dumps(obj, ensure_ascii=False, separators=(",", ":"), sort_keys=True)`).
pub fn dumps(v: &Value) -> String {
    // serde_json's Map is a BTreeMap here (no preserve_order), so keys come
    // out in code point order, like sort_keys.
    serde_json::to_string(v).unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub seq: i64,
    pub ts: i64,
    pub event: String,
    pub agent: String,
    pub ws: String,
    pub pane: String,
    pub tab: String,
    pub term: String,
    pub ws_label: String,
    pub tab_label: String,
    pub title: String,
    pub focused: bool,
}

/// A validated wire line, holding only known, validated, cleaned fields.
// Event is the common case; boxing it would only add an allocation.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Hello { head: i64, epoch: String },
    Hb { head: i64 },
    Focus { ok: bool, code: String },
    Error { code: String },
    Event(Event),
}

fn pybool(b: bool) -> &'static str {
    if b { "True" } else { "False" }
}

impl Msg {
    pub fn kind(&self) -> &'static str {
        match self {
            Msg::Hello { .. } => "hello",
            Msg::Hb { .. } => "hb",
            Msg::Focus { .. } => "focus",
            Msg::Error { .. } => "error",
            Msg::Event(_) => "event",
        }
    }

    /// What Python printed for the parsed dict (`"%s" % msg`), keys in the
    /// order parse_line inserted them.
    pub fn py_repr(&self) -> String {
        let r = py::repr_str;
        match self {
            Msg::Hello { head, epoch } => {
                format!("{{'type': 'hello', 'head': {head}, 'epoch': {}}}", r(epoch))
            }
            Msg::Hb { head } => format!("{{'type': 'hb', 'head': {head}}}"),
            Msg::Focus { ok, code } => {
                format!(
                    "{{'type': 'focus', 'ok': {}, 'code': {}}}",
                    pybool(*ok),
                    r(code)
                )
            }
            Msg::Error { code } => format!("{{'type': 'error', 'code': {}}}", r(code)),
            Msg::Event(e) => format!(
                "{{'type': 'event', 'seq': {}, 'ts': {}, 'event': {}, 'agent': {}, \
                 'ws': {}, 'pane': {}, 'tab': {}, 'term': {}, 'ws_label': {}, \
                 'tab_label': {}, 'title': {}, 'focused': {}}}",
                e.seq,
                e.ts,
                r(&e.event),
                r(&e.agent),
                r(&e.ws),
                r(&e.pane),
                r(&e.tab),
                r(&e.term),
                r(&e.ws_label),
                r(&e.tab_label),
                r(&e.title),
                pybool(e.focused)
            ),
        }
    }
}

/// Why a line was dropped (a short reason for the log).
#[derive(Debug, Clone, PartialEq)]
pub struct Invalid(pub String);

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn req(cond: bool, reason: &str) -> Result<(), Invalid> {
    if cond {
        Ok(())
    } else {
        Err(Invalid(reason.to_string()))
    }
}

/// `obj.get(key, default)` where only a missing key takes the default.
fn str_or<'a>(obj: &'a Map<String, Value>, key: &str, default: &'a str) -> Option<&'a str> {
    match obj.get(key) {
        None => Some(default),
        Some(Value::String(s)) => Some(s),
        Some(_) => None,
    }
}

fn label(obj: &Map<String, Value>, key: &str) -> Result<String, Invalid> {
    let value = str_or(obj, key, "");
    req(value.is_some(), &format!("{key} not a string"))?;
    let value = value.unwrap_or_default();
    req(
        value.chars().count() <= RAW_LABEL_MAX,
        &format!("{key} too long"),
    )?;
    Ok(clean_str(value, LABEL_MAX))
}

fn int_in(v: Option<&Value>, lo: i64, hi: i64) -> Option<i64> {
    py::as_int(v)
        .filter(|n| (i128::from(lo)..=i128::from(hi)).contains(n))
        .map(|n| n as i64)
}

/// Validate one wire line (bytes, without the newline).
pub fn parse_line(raw: &[u8], now: f64) -> Result<Msg, Invalid> {
    req(raw.len() <= MAX_LINE_BYTES, "line too long")?;
    let obj: Value = std::str::from_utf8(raw)
        .ok()
        .and_then(|s| serde_json::from_str(s).ok())
        .ok_or_else(|| Invalid("not json".into()))?;
    let Value::Object(obj) = obj else {
        return Err(Invalid("not an object".into()));
    };
    req(py::as_int(obj.get("v")) == Some(1), "bad version")?;
    let kind = obj.get("type").and_then(Value::as_str);
    match kind {
        Some("hello") | Some("hb") => {
            let head = int_in(obj.get("head"), 0, MAX_SEQ);
            req(head.is_some(), "bad head")?;
            let head = head.unwrap_or_default();
            if kind == Some("hb") {
                return Ok(Msg::Hb { head });
            }
            let epoch = obj.get("epoch").and_then(Value::as_str);
            req(epoch.is_some_and(is_epoch), "bad epoch")?;
            Ok(Msg::Hello {
                head,
                epoch: epoch.unwrap_or_default().to_string(),
            })
        }
        Some("focus") => {
            let ok = obj.get("ok").and_then(Value::as_bool);
            req(ok.is_some(), "bad ok")?;
            let code = str_or(&obj, "code", "");
            req(code.is_some_and(|c| c.is_empty() || is_code(c)), "bad code")?;
            Ok(Msg::Focus {
                ok: ok.unwrap_or_default(),
                code: code.unwrap_or_default().to_string(),
            })
        }
        Some("error") => {
            let code = str_or(&obj, "code", "");
            req(code.is_some_and(is_code), "bad code")?;
            Ok(Msg::Error {
                code: code.unwrap_or_default().to_string(),
            })
        }
        _ => {
            req(kind == Some("event"), "unknown type")?;
            let seq = int_in(obj.get("seq"), 1, MAX_SEQ);
            req(seq.is_some(), "bad seq")?;
            let ts = py::as_int(obj.get("ts")).filter(|&t| {
                let t = t as f64;
                now - TTL_SECONDS as f64 <= t && t <= now + FUTURE_SKEW as f64
            });
            req(ts.is_some(), "bad ts")?;
            let event = obj.get("event").and_then(Value::as_str);
            req(event.is_some_and(|e| EVENTS.contains(&e)), "bad event")?;
            let agent = obj.get("agent").and_then(Value::as_str);
            req(agent.is_some_and(|a| AGENTS.contains(&a)), "bad agent")?;
            let id = |key: &str| -> Result<String, Invalid> {
                let v = obj.get(key).and_then(Value::as_str);
                req(v.is_some_and(is_id), &format!("bad {key}"))?;
                Ok(v.unwrap_or_default().to_string())
            };
            let ws = id("ws")?;
            let pane = id("pane")?;
            let tab = str_or(&obj, "tab", "");
            req(tab.is_some_and(|t| t.is_empty() || is_id(t)), "bad tab")?;
            let term = str_or(&obj, "term", "");
            req(term.is_some_and(|t| t.is_empty() || is_term(t)), "bad term")?;
            let ws_label = label(&obj, "ws_label")?;
            let tab_label = label(&obj, "tab_label")?;
            let title = label(&obj, "title")?;
            let focused = match obj.get("focused") {
                None => Some(false),
                Some(v) => v.as_bool(),
            };
            req(focused.is_some(), "bad focused")?;
            Ok(Msg::Event(Event {
                seq: seq.unwrap_or_default(),
                ts: ts.unwrap_or_default() as i64,
                event: event.unwrap_or_default().to_string(),
                agent: agent.unwrap_or_default().to_string(),
                ws,
                pane,
                tab: tab.unwrap_or_default().to_string(),
                term: term.unwrap_or_default().to_string(),
                ws_label,
                tab_label,
                title,
                focused: focused.unwrap_or_default(),
            }))
        }
    }
}

/// (title, body) for a validated event. The title comes from fixed strings
/// only; the body is "workspace › tab › title" with empty and redundant parts
/// left out.
pub fn compose(e: &Event) -> (String, String) {
    let name = if e.agent == "codex" {
        "Codex"
    } else {
        "Claude"
    };
    let title = if e.event == "needs_input" {
        format!("\u{1f514} {name} needs you")
    } else {
        format!("✅ {name} finished")
    };
    // herdr names tabs "1", "2", ... until renamed: noise as a destination.
    let tab = if py::isdigit(&e.tab_label) {
        ""
    } else {
        e.tab_label.as_str()
    };
    let mut where_: Vec<&str> = Vec::new();
    for part in [tab, e.title.as_str()] {
        if !part.is_empty() && !where_.contains(&part) {
            where_.push(part);
        }
    }
    // A tab label that only prefixes the pane title collapses into it.
    if where_.len() == 2 && where_[1].starts_with(where_[0]) {
        where_.remove(0);
    }
    let ws = e.ws_label.as_str();
    if !ws.is_empty() && !where_.contains(&ws) {
        where_.insert(0, ws);
    }
    let body = if where_.is_empty() {
        "Open herdr to pick it up".to_string()
    } else {
        where_.join(" › ")
    };
    (title, body)
}

#[cfg(test)]
mod tests {
    //! Ported from tests/test_hnlib.py.
    use super::*;
    use serde_json::json;

    const NOW: f64 = 1_800_000_000.0;

    fn ev(over: Value) -> Vec<u8> {
        let mut base = json!({"v": 1, "type": "event", "seq": 5, "ts": NOW as i64,
            "event": "done", "agent": "claude", "ws": "w2", "tab": "w2:t1",
            "pane": "w2:p1", "term": "term_65ead8c4a4dc72", "ws_label": "my-project",
            "tab_label": "", "title": "fix it", "focused": false});
        for (k, v) in over.as_object().unwrap() {
            base[k] = v.clone();
        }
        serde_json::to_vec(&base).unwrap()
    }

    fn cl(s: &str) -> String {
        clean_str(s, LABEL_MAX)
    }

    #[test]
    fn keeps_other_scripts() {
        assert_eq!(cl("Ситуация с 1400"), "Ситуация с 1400");
    }

    #[test]
    fn strips_controls_bidi_zero_width() {
        let s = "a\x1b]777;x\x07b\u{202e} c\u{200b}d\u{2066}e\u{9b}";
        assert_eq!(cl(s), "a]777;xb cde");
    }

    #[test]
    fn newlines_become_spaces() {
        assert_eq!(cl("a\nb\r\n\tc d"), "a b c d");
    }

    #[test]
    fn truncates() {
        let out = cl(&"x".repeat(100));
        assert_eq!(out.chars().count(), 40);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn non_string() {
        for v in [json!(null), json!(5), json!(["a"]), json!({"a": 1})] {
            assert_eq!(clean_label(Some(&v), LABEL_MAX), "");
        }
        assert_eq!(clean_label(None, LABEL_MAX), "");
    }

    #[test]
    fn nfc_and_truncate_rstrip() {
        assert_eq!(cl("e\u{301}"), "é");
        let s = format!("{} {}", "a".repeat(38), "bbbb");
        assert_eq!(cl(&s), format!("{}…", "a".repeat(38)));
    }

    fn event(m: Msg) -> Event {
        match m {
            Msg::Event(e) => e,
            other => panic!("not an event: {other:?}"),
        }
    }

    #[test]
    fn good_event() {
        let out = event(parse_line(&ev(json!({"title": "t\x1b[31mx"})), NOW).unwrap());
        assert_eq!(out.pane, "w2:p1");
        assert_eq!(out.title, "t[31mx");
    }

    #[test]
    fn unknown_keys_dropped() {
        // The parsed form has no place for extra keys; check the repr too.
        let out = parse_line(&ev(json!({"extra": "$(rm -rf /)", "url": "http://x"})), NOW).unwrap();
        let r = out.py_repr();
        assert!(!r.contains("extra") && !r.contains("url"));
    }

    #[test]
    fn rejects() {
        let mut bad: Vec<Vec<u8>> = vec![
            b"not json".to_vec(),
            b"[1,2]".to_vec(),
            b"\"str\"".to_vec(),
            b"{}".to_vec(),
        ];
        for over in [
            json!({"v": 2}),
            json!({"v": true}),
            json!({"v": 1.0}),
            json!({"type": "other"}),
            json!({"seq": 0}),
            json!({"seq": -1}),
            json!({"seq": 1.5}),
            json!({"seq": true}),
            json!({"seq": "5"}),
            json!({"seq": 1u64 << 60}),
            json!({"ts": NOW as i64 - 3601}),
            json!({"ts": NOW as i64 + 301}),
            json!({"ts": NOW}),
            json!({"event": "idle"}),
            json!({"event": "blocked"}),
            json!({"agent": "gemini"}),
            json!({"agent": null}),
            json!({"pane": "w1:p1; id"}),
            json!({"pane": "w1 p1"}),
            json!({"pane": "$(id)"}),
            json!({"pane": "a".repeat(33)}),
            json!({"pane": "w1:p1\n"}),
            json!({"pane": ""}),
            json!({"ws": null}),
            json!({"tab": "x y"}),
            json!({"term": "term_XYZ"}),
            json!({"term": "term_12345678 ; ls"}),
            json!({"title": 5}),
            json!({"title": "x".repeat(257)}),
            json!({"focused": "yes"}),
        ] {
            bad.push(ev(over));
        }
        let mut long = b"{".to_vec();
        long.extend(std::iter::repeat_n(b' ', 2000));
        long.push(b'}');
        bad.push(long);
        bad.push(b"\xff\xfe".to_vec());
        for raw in bad {
            assert!(
                parse_line(&raw, NOW).is_err(),
                "accepted {:?}",
                String::from_utf8_lossy(&raw[..raw.len().min(60)])
            );
        }
    }

    #[test]
    fn hello_hb_focus_error() {
        let h = parse_line(
            br#"{"v":1,"type":"hello","epoch":"0123456789abcdef","head":3,"ts":1}"#,
            NOW,
        )
        .unwrap();
        assert_eq!(
            h,
            Msg::Hello {
                head: 3,
                epoch: "0123456789abcdef".into()
            }
        );
        assert_eq!(
            h.py_repr(),
            "{'type': 'hello', 'head': 3, 'epoch': '0123456789abcdef'}"
        );
        assert_eq!(
            parse_line(br#"{"v":1,"type":"hb","head":0}"#, NOW)
                .unwrap()
                .kind(),
            "hb"
        );
        assert!(parse_line(br#"{"v":1,"type":"hello","epoch":"zz","head":3}"#, NOW).is_err());
        assert_eq!(
            parse_line(br#"{"v":1,"type":"focus","ok":true,"code":"focused"}"#, NOW).unwrap(),
            Msg::Focus {
                ok: true,
                code: "focused".into()
            }
        );
        assert!(parse_line(br#"{"v":1,"type":"error","code":"Bad Code!"}"#, NOW).is_err());
    }

    #[test]
    fn reasons_match_python() {
        let r = |raw: &[u8]| parse_line(raw, NOW).unwrap_err().0;
        assert_eq!(r(b"nope"), "not json");
        assert_eq!(r(b"[]"), "not an object");
        assert_eq!(r(&ev(json!({"title": null}))), "title not a string");
        assert_eq!(r(&ev(json!({"title": "x".repeat(257)}))), "title too long");
        assert_eq!(r(&ev(json!({"pane": "a b"}))), "bad pane");
    }

    fn e(over: Value) -> Event {
        event(parse_line(&ev(over), NOW).unwrap())
    }

    #[test]
    fn titles_fixed() {
        assert_eq!(
            compose(&e(json!({"event": "needs_input"}))).0,
            "\u{1f514} Claude needs you"
        );
        assert_eq!(
            compose(&e(json!({"agent": "codex"}))).0,
            "✅ Codex finished"
        );
    }

    #[test]
    fn body_rules() {
        assert_eq!(
            compose(&e(json!({"tab_label": "3"}))).1,
            "my-project › fix it"
        );
        assert_eq!(
            compose(&e(json!({"tab_label": "fix"}))).1,
            "my-project › fix it"
        );
        assert_eq!(
            compose(&e(json!({"tab_label": "web", "title": "task"}))).1,
            "my-project › web › task"
        );
        assert_eq!(
            compose(&e(json!({"ws_label": "", "title": ""}))).1,
            "Open herdr to pick it up"
        );
    }

    #[test]
    fn dumps_sorted_compact() {
        assert_eq!(
            dumps(&json!({"v": 1, "type": "error", "code": "bad_command"})),
            r#"{"code":"bad_command","type":"error","v":1}"#
        );
        assert_eq!(dumps(&json!({"t": "Ситуация"})), r#"{"t":"Ситуация"}"#);
    }

    #[test]
    fn limits_are_the_documented_ones() {
        assert_eq!(MAX_SEQ, 9_007_199_254_740_991);
        assert_eq!((MAX_LINE_BYTES, LABEL_MAX, RAW_LABEL_MAX), (1024, 40, 256));
        assert_eq!((TTL_SECONDS, FUTURE_SKEW), (3600, 300));
        assert_eq!(status_to_event("done"), Some("done"));
        assert_eq!(status_to_event("blocked"), Some("needs_input"));
        assert_eq!(status_to_event("working"), None);
    }

    #[test]
    fn ids() {
        for ok in ["a", "w1:p1", "a_b", "a-b", "Z9", &"x".repeat(32)] {
            assert!(is_id(ok), "{ok}");
        }
        for bad in ["", "a b", "a.b", "é", &"x".repeat(33)] {
            assert!(!is_id(bad), "{bad}");
        }
        assert!(is_term("term_abcdef") && is_term(&format!("term_{}", "0".repeat(32))));
        assert!(!is_term("term_abcde") && !is_term("term_ABCDEF") && !is_term("abcdef"));
        assert!(is_code("a_b") && !is_code("") && !is_code("A"));
        assert!(is_epoch("0123456789abcdef") && !is_epoch("0123456789abcde"));
    }

    #[test]
    fn clean_limits() {
        // Exactly the limit stays as is.
        let s40 = "y".repeat(40);
        assert_eq!(cl(&s40), s40);
        // Only the first 512 characters are looked at, as in Python.
        let s = format!("{}a{}", "\u{0}".repeat(511), "b".repeat(10));
        assert_eq!(cl(&s), "a");
        assert_eq!(clean_str("abcdef", 3), "ab…");
        assert_eq!(clean_str("ab cdef", 4), "ab…");
    }

    #[test]
    fn error_and_focus_lines() {
        assert_eq!(
            parse_line(br#"{"v":1,"type":"error","code":"bad_command"}"#, NOW).unwrap(),
            Msg::Error {
                code: "bad_command".into()
            }
        );
        assert_eq!(
            parse_line(br#"{"v":1,"type":"focus","ok":false}"#, NOW).unwrap(),
            Msg::Focus {
                ok: false,
                code: String::new()
            }
        );
        let r = |raw: &[u8]| parse_line(raw, NOW).unwrap().py_repr();
        assert_eq!(
            r(br#"{"v":1,"type":"focus","ok":false,"code":"rate_limited"}"#),
            "{'type': 'focus', 'ok': False, 'code': 'rate_limited'}"
        );
        assert_eq!(
            r(br#"{"v":1,"type":"focus","ok":true,"code":"focused"}"#),
            "{'type': 'focus', 'ok': True, 'code': 'focused'}"
        );
        assert_eq!(
            r(br#"{"v":1,"type":"hb","head":4}"#),
            "{'type': 'hb', 'head': 4}"
        );
        assert_eq!(
            r(&ev(json!({"title": "it's"}))),
            "{'type': 'event', 'seq': 5, 'ts': 1800000000, 'event': 'done', 'agent': 'claude', \
             'ws': 'w2', 'pane': 'w2:p1', 'tab': 'w2:t1', 'term': 'term_65ead8c4a4dc72', \
             'ws_label': 'my-project', 'tab_label': '', 'title': \"it's\", 'focused': False}"
        );
        assert_eq!(format!("{}", Invalid("why".into())), "why");
        let e = parse_line(b"[]", NOW).unwrap_err();
        assert_eq!(e.to_string(), "not an object");
    }

    #[test]
    fn ts_window_edges() {
        assert!(parse_line(&ev(json!({"ts": NOW as i64 - 3600})), NOW).is_ok());
        assert!(parse_line(&ev(json!({"ts": NOW as i64 + 300})), NOW).is_ok());
        assert!(parse_line(&ev(json!({"seq": MAX_SEQ})), NOW).is_ok());
        assert!(parse_line(&ev(json!({"seq": MAX_SEQ + 1})), NOW).is_err());
        assert!(parse_line(br#"{"v":1,"type":"hb","head":-1}"#, NOW).is_err());
        let mut line = ev(json!({}));
        line.truncate(line.len() - 1);
        line.extend(std::iter::repeat_n(b' ', 1024 - line.len() - 1));
        line.push(b'}');
        assert_eq!(line.len(), 1024);
        assert!(parse_line(&line, NOW).is_ok());
    }
}
