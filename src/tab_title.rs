//! `tab-title` (legacy name `herdr-tab-title-hook`): name herdr tabs after
//! what their Claude Code / Codex agent is doing.
//!
//! Runs from the herdr-tab-title plugin on agent and focus events (one tab:
//! the event's) and on `startup` (every tab; also `--all` by hand). A tab is
//! renamed to its agent pane's terminal title (spinner already stripped by
//! herdr), trimmed to MAX_LEN, but only while the tab still carries herdr's
//! default numeric label or the label this hook set last time; any other
//! label is a manual rename and is left alone for good. The last labels live
//! in `$HERDR_PLUGIN_STATE_DIR/labels.json`.
//!
//! herdr spawns hooks detached with nowhere to print, so every failure exits
//! 0 quietly; `touch <state dir>/debug` turns on a breadcrumb log
//! (debug.log).
//!
//! `--dry-run` prints the renames it would make (one line each) and touches
//! nothing: no rename, no state write, no lock, no log.

use crate::hook::trace;
use crate::proc::{self, RunError};
use crate::py::{self, Raised, get};
use crate::vm::{self, CallError, Env};
use argh::FromArgs;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::{Map, Value, json};
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_LEN: usize = 32;
pub const TIMEOUT: Duration = Duration::from_secs(5);
const AGENTS: [&str; 2] = ["claude", "codex"];
/// Titles that say nothing beyond "an agent runs here".
const GENERIC: [&str; 6] = [
    "",
    "claude",
    "claude code",
    "codex",
    "openai codex",
    "codex cli",
];

/// Python `re.match(r"^\d+$", s)`: digits, then the end or one final "\n".
pub fn is_default_label(s: &str) -> bool {
    let s = s.strip_suffix('\n').unwrap_or(s);
    !s.is_empty() && s.chars().all(py::is_decimal)
}

/// `^\S+@\S+:|^[~/]`: user@host:path prompts and bare paths. Only used on a
/// title whose whitespace is already collapsed to single spaces.
fn shellish(title: &str) -> bool {
    if title.starts_with('~') || title.starts_with('/') {
        return true;
    }
    let word: Vec<char> = title
        .split(py::is_space)
        .next()
        .unwrap_or("")
        .chars()
        .collect();
    match word.iter().skip(1).position(|&c| c == '@') {
        Some(i) => word.iter().skip(i + 3).any(|&c| c == ':'),
        None => false,
    }
}

/// The tab label a pane's title gives; Ok(None) when it is not useful, Err
/// where Python would have raised.
pub fn label_for(pane: &Map<String, Value>) -> Result<Option<String>, Raised> {
    let raw = py::or(&[get(pane, "terminal_title_stripped")]);
    let raw = match raw {
        v if !py::truthy(v) => "",
        Some(Value::String(s)) => s.as_str(),
        _ => return Err(Raised),
    };
    let title = py::split_ws(raw).collect::<Vec<_>>().join(" ");
    let lower = title.to_lowercase();
    let agent = match get(pane, "agent") {
        v if !py::truthy(v) => Some(""),
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    if GENERIC.contains(&lower.as_str())
        || agent == Some(lower.as_str())
        || shellish(&title)
        || is_default_label(&title)
    {
        return Ok(None);
    }
    if title.chars().count() > MAX_LEN {
        let mut t = py::rstrip(py::take_chars(&title, MAX_LEN - 1)).to_string();
        t.push('…');
        return Ok(Some(t));
    }
    Ok(Some(title))
}

/// (tab_id, pane_id) of an event (`{"event": ..., "data": {...}}` or a bare
/// payload, ids at top level or under "pane" / "tab"); either may be None.
pub fn event_ids(raw: Option<&str>) -> (Option<String>, Option<String>) {
    let ev = raw.and_then(|r| serde_json::from_str::<Value>(r).ok());
    let mut objs: Vec<Option<&Value>> = Vec::new();
    if let Some(Value::Object(o)) = &ev {
        objs.push(ev.as_ref());
        objs.push(get(o, "data"));
    }
    let mut more = Vec::new();
    for o in objs.iter().flatten() {
        if let Value::Object(m) = o {
            for k in ["pane", "tab"] {
                more.push(get(m, k));
            }
        }
    }
    objs.extend(more);
    let (mut tab, mut pane) = (None, None);
    for o in objs.into_iter().flatten() {
        if let Value::Object(m) = o {
            if tab.is_none()
                && let Some(Value::String(s)) = m.get("tab_id")
            {
                tab = Some(s.clone());
            }
            if pane.is_none()
                && let Some(Value::String(s)) = m.get("pane_id")
            {
                pane = Some(s.clone());
            }
        }
    }
    (tab, pane)
}

/// labels.json: tab id -> last label set, in file order (like a Python dict).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Labels(pub Vec<(String, Value)>);

impl Labels {
    pub fn get(&self, k: &str) -> Option<&Value> {
        self.0.iter().find(|(key, _)| key == k).map(|(_, v)| v)
    }

    pub fn set(&mut self, k: &str, v: Value) {
        match self.0.iter_mut().find(|(key, _)| key == k) {
            Some(slot) => slot.1 = v,
            None => self.0.push((k.to_string(), v)),
        }
    }

    pub fn remove(&mut self, k: &str) {
        self.0.retain(|(key, _)| key != k);
    }

    /// Dict equality: same keys and values, any order.
    pub fn same(&self, other: &Labels) -> bool {
        self.0.len() == other.0.len() && self.0.iter().all(|(k, v)| other.get(k) == Some(v))
    }

    /// Python's `json.loads`; anything but an object gives empty labels.
    pub fn parse(text: &str) -> Labels {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Labels;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Self::Value, A::Error> {
                let mut l = Labels::default();
                while let Some((k, v)) = m.next_entry::<String, Value>()? {
                    l.set(&k, v);
                }
                Ok(l)
            }
        }
        let mut de = serde_json::Deserializer::from_str(text);
        // Anything but an object fails to deserialize (the visitor takes
        // maps only), and so does trailing garbage.
        match de.deserialize_map(V) {
            Ok(l) if de.end().is_ok() => l,
            _ => Labels::default(),
        }
    }

    /// Python's `json.dump(labels, fh, ensure_ascii=False)`.
    pub fn dump(&self) -> String {
        let items: Vec<String> = self
            .0
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}: {}",
                    serde_json::to_string(k).unwrap_or_default(),
                    py_value(v)
                )
            })
            .collect();
        format!("{{{}}}", items.join(", "))
    }
}

/// A JSON value with Python's default separators, non-ASCII kept.
fn py_value(v: &Value) -> String {
    match v {
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(py_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, v)| format!(
                    "{}: {}",
                    serde_json::to_string(k).unwrap_or_default(),
                    py_value(v)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => other.to_string(),
    }
}

/// A rename to apply.
#[derive(Debug, Clone, PartialEq)]
pub struct Rename {
    pub tab_id: Value,
    pub current: String,
    pub want: String,
}

/// Hashable key for a JSON value used as a Python dict key.
fn key(v: &Value) -> Result<String, Raised> {
    match v {
        Value::Array(_) | Value::Object(_) => Err(Raised),
        v => Ok(v.to_string()),
    }
}

/// The renames to apply; updates `labels` (the state) in place.
pub fn plan(
    snap: &Map<String, Value>,
    labels: &mut Labels,
    only_tab: Option<&Value>,
) -> Result<Vec<Rename>, Raised> {
    // panes + agents, merged per pane id, first-seen order.
    let mut panes: Vec<(String, Map<String, Value>)> = Vec::new();
    let all = py::list_for_concat(snap, "panes")?
        .iter()
        .chain(py::list_for_concat(snap, "agents")?);
    for p in all {
        let Value::Object(p) = p else { continue };
        let Some(id) = get(p, "pane_id").filter(|v| py::truthy(Some(v))) else {
            continue;
        };
        let k = key(id)?;
        match panes.iter_mut().find(|(pk, _)| *pk == k) {
            Some((_, m)) => m.extend(p.clone()),
            None => panes.push((k, p.clone())),
        }
    }
    let mut focused: Vec<(Option<Value>, Option<Value>)> = Vec::new();
    for l in py::list_for_iter(snap, "layouts")? {
        let Value::Object(l) = l else { continue };
        let k = get(l, "tab_id").cloned();
        if let Some(k) = &k {
            key(k)?;
        }
        let v = get(l, "focused_pane_id").cloned();
        match focused.iter_mut().find(|(fk, _)| *fk == k) {
            Some(slot) => slot.1 = v,
            None => focused.push((k, v)),
        }
    }
    let mut tabs: Vec<(Value, &Map<String, Value>)> = Vec::new();
    for t in py::list_for_iter(snap, "tabs")? {
        let Value::Object(t) = t else { continue };
        let Some(id) = get(t, "tab_id").filter(|v| py::truthy(Some(v))) else {
            continue;
        };
        key(id)?;
        match tabs.iter_mut().find(|(tk, _)| tk == id) {
            Some(slot) => slot.1 = t,
            None => tabs.push((id.clone(), t)),
        }
    }
    // Forget closed tabs.
    labels
        .0
        .retain(|(k, _)| tabs.iter().any(|(id, _)| id.as_str() == Some(k)));
    let mut todo = Vec::new();
    for (tab_id, tab) in &tabs {
        if only_tab.is_some_and(|o| o != tab_id) {
            continue;
        }
        let current = match get(tab, "label") {
            v if !py::truthy(v) => String::new(),
            Some(Value::String(s)) => s.clone(),
            _ => return Err(Raised),
        };
        let last = tab_id.as_str().and_then(|k| labels.get(k));
        if !is_default_label(&current) && last != Some(&Value::String(current.clone())) {
            // Manual rename: hands off.
            if let Some(k) = tab_id.as_str() {
                labels.remove(k);
            }
            continue;
        }
        let want_focus = focused
            .iter()
            .find(|(k, _)| k.as_ref() == Some(tab_id))
            .and_then(|(_, v)| v.as_ref());
        let mut agents: Vec<&Map<String, Value>> = panes
            .iter()
            .map(|(_, p)| p)
            .filter(|p| {
                get(p, "tab_id") == Some(tab_id)
                    && get(p, "agent")
                        .and_then(Value::as_str)
                        .is_some_and(|a| AGENTS.contains(&a))
            })
            .collect();
        agents.sort_by_key(|p| get(p, "pane_id") != want_focus);
        let mut want = None;
        for p in agents {
            if let Some(l) = label_for(p)?
                && !l.is_empty()
            {
                want = Some(l);
                break;
            }
        }
        let Some(want) = want else { continue };
        if want == current {
            continue;
        }
        todo.push(Rename {
            tab_id: tab_id.clone(),
            current,
            want,
        });
    }
    Ok(todo)
}

fn state_dir(env: &Env) -> PathBuf {
    if let Some(d) = env.get("HERDR_PLUGIN_STATE_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(d);
    }
    let base = env
        .get("XDG_STATE_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(&py::home_dir(env.get("HOME").map(String::as_str))).join(".local/state")
        });
    base.join("herdr").join("plugins").join("herdr-tab-title")
}

fn argv_repr(argv: &[String]) -> String {
    py::repr_value(&Value::from(argv.to_vec()))
}

/// `herdr <args>` with check=True; Err carries Python's exception text.
fn herdr_cli(bin: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut argv = vec![bin.to_string()];
    argv.extend(args.iter().map(|s| s.to_string()));
    match proc::run(&argv, TIMEOUT) {
        Ok(o) if o.status.success() => Ok(o.stdout),
        Ok(o) => Err(format!(
            "Command '{}' returned non-zero exit status {}.",
            argv_repr(&argv),
            o.status.code().unwrap_or(-1)
        )),
        Err(RunError::Timeout) => Err(format!(
            "Command '{}' timed out after {} seconds",
            argv_repr(&argv),
            TIMEOUT.as_secs()
        )),
        Err(RunError::Spawn(e)) => Err(py::os_error(&e, Some(bin))),
    }
}

/// How this run talks to herdr: its socket API directly (no process
/// spawned), or, when no socket answers, the `herdr` CLI as the Python did.
#[derive(Debug, Clone, PartialEq)]
pub enum Herdr {
    Socket(PathBuf),
    Cli(String),
}

const REQUEST_ID: &str = "herdr-tab-title";

fn snapshot_of(reply: &Value) -> Result<Map<String, Value>, String> {
    match reply.get("result").and_then(|r| r.get("snapshot")) {
        Some(Value::Object(m)) => Ok(m.clone()),
        _ => Err("no snapshot in reply".into()),
    }
}

/// The session snapshot, and the way it was reached (used for the renames
/// too). The socket is `$HERDR_SOCKET_PATH`, which herdr sets for plugin
/// hooks, else the default session's.
pub fn snapshot(env: &Env) -> Result<(Map<String, Value>, Herdr), String> {
    let sock = env
        .get("HERDR_SOCKET_PATH")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| vm::default_socket(env));
    match vm::herdr_request(&sock, REQUEST_ID, "session.snapshot", &json!({}), TIMEOUT) {
        Ok(reply) => Ok((snapshot_of(&reply)?, Herdr::Socket(sock))),
        Err(CallError::Exchange(e)) => Err(format!("herdr socket: {e}")),
        Err(CallError::Connect(_)) => {
            let bin = env
                .get("HERDR_BIN_PATH")
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| "herdr".into());
            let out = herdr_cli(&bin, &["api", "snapshot"])?;
            let v: Value =
                serde_json::from_slice(&out).map_err(|e| format!("bad snapshot json: {e}"))?;
            Ok((snapshot_of(&v)?, Herdr::Cli(bin)))
        }
    }
}

/// Rename one tab the same way the snapshot was read.
pub fn rename(h: &Herdr, tab_id: &str, label: &str) -> Result<(), String> {
    match h {
        Herdr::Cli(bin) => herdr_cli(bin, &["tab", "rename", tab_id, label]).map(|_| ()),
        Herdr::Socket(sock) => {
            let params = json!({"tab_id": tab_id, "label": label});
            match vm::herdr_request(sock, REQUEST_ID, "tab.rename", &params, TIMEOUT) {
                Ok(r) if r.get("result").is_some() => Ok(()),
                Ok(r) => Err(format!(
                    "herdr refused tab.rename: {}",
                    r.get("error").map(Value::to_string).unwrap_or_default()
                )),
                Err(CallError::Connect(e)) => Err(py::os_error(&e, None)),
                Err(CallError::Exchange(e)) => Err(format!("herdr socket: {e}")),
            }
        }
    }
}

/// Name herdr tabs after what their agent is doing (a herdr plugin hook).
#[derive(FromArgs, Debug, Default, PartialEq)]
#[argh(subcommand, name = "tab-title")]
pub struct Args {
    /// handle every tab, not only the event's (as on `startup`)
    #[argh(switch)]
    pub all: bool,
    /// print the renames it would make and change nothing
    #[argh(switch)]
    pub dry_run: bool,
}

/// The hook proper. Errors are traced (unless dry-run) and swallowed.
pub fn run(env: &Env, opts: &Args, out: &mut dyn std::io::Write) -> Result<(), String> {
    let state = state_dir(env);
    let tr = |m: &str| {
        if !opts.dry_run {
            trace(&state, m);
        }
    };
    let event = env
        .get("HERDR_PLUGIN_EVENT")
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| "manual".into());
    let everything = event == "startup" || opts.all;
    let (mut tab_id, mut pane_id) = (None, None);
    if !everything {
        (tab_id, pane_id) = event_ids(env.get("HERDR_PLUGIN_EVENT_JSON").map(String::as_str));
        if tab_id.is_none() && pane_id.is_none() {
            tr(&format!("{event}: no tab/pane id"));
            return Ok(());
        }
    }
    let show = |o: &Option<String>| o.clone().unwrap_or_else(|| "None".into());
    tr(&format!(
        "{event}: tab {} pane {}",
        show(&tab_id),
        show(&pane_id)
    ));
    let _lock = if opts.dry_run {
        None
    } else {
        fs::create_dir_all(&state).map_err(|e| py::os_error(&e, None))?;
        let f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(state.join("lock"))
            .map_err(|e| py::os_error(&e, None))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(py::os_error(&std::io::Error::last_os_error(), None));
        }
        Some(f)
    };
    let (snap, herdr) = snapshot(env)?;
    let mut only: Option<Value> = tab_id.map(Value::String);
    if !everything && only.is_none() {
        let mut found = None;
        for p in py::list_for_iter(&snap, "panes").map_err(|_| "bad panes".to_string())? {
            let Value::Object(p) = p else {
                return Err("pane is not an object".into());
            };
            if get(p, "pane_id").and_then(Value::as_str) == pane_id.as_deref() {
                found = Some(get(p, "tab_id").cloned());
                break;
            }
        }
        match found.flatten().filter(|v| py::truthy(Some(v))) {
            Some(t) => only = Some(t),
            None => return Ok(()),
        }
    }
    let path = state.join("labels.json");
    let mut labels = fs::read_to_string(&path)
        .map(|t| Labels::parse(&t))
        .unwrap_or_default();
    let before = labels.clone();
    let todo = plan(
        &snap,
        &mut labels,
        if everything { None } else { only.as_ref() },
    )
    .map_err(|_| "unexpected snapshot shape".to_string())?;
    for r in todo {
        let Some(tid) = r.tab_id.as_str() else {
            return Err("tab id is not a string".into());
        };
        if opts.dry_run {
            let _ = writeln!(
                out,
                "rename {tid} {} -> {}",
                serde_json::to_string(&r.current).unwrap_or_default(),
                serde_json::to_string(&r.want).unwrap_or_default()
            );
            continue;
        }
        match rename(&herdr, tid, &r.want) {
            Ok(()) => {
                labels.set(tid, Value::String(r.want.clone()));
                tr(&format!("{event}: {tid} -> {}", py::repr_str(&r.want)));
            }
            Err(e) => tr(&format!("{event}: rename {tid} failed: {e}")),
        }
    }
    if !opts.dry_run && !labels.same(&before) {
        let tmp = state.join("labels.json.tmp");
        fs::write(&tmp, labels.dump()).map_err(|e| py::os_error(&e, None))?;
        fs::rename(&tmp, &path).map_err(|e| py::os_error(&e, None))?;
    }
    Ok(())
}

/// Run with parsed arguments; always 0 (a hook has nowhere to report).
pub fn main(opts: Args) -> i32 {
    let env = vm::os_env();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if let Err(e) = run(&env, &opts, &mut out) {
        if opts.dry_run {
            eprintln!("herdr-tab-title: {e}");
        } else {
            trace(&state_dir(&env), &format!("error: {e}"));
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn pane(title: &str, agent: &str) -> Map<String, Value> {
        obj(json!({"pane_id": "w1:p1", "agent": agent, "terminal_title_stripped": title}))
    }

    #[test]
    fn junk_titles_skipped() {
        for t in [
            "",
            "  ",
            "Claude Code",
            "claude",
            "OpenAI Codex",
            "codex cli",
            "user@host:~/src",
            "~/src",
            "/tmp",
            "12",
            "١٢",
        ] {
            assert_eq!(label_for(&pane(t, "claude")).unwrap(), None, "{t:?}");
        }
        assert_eq!(label_for(&pane("Gemini", "gemini")).unwrap(), None);
        assert_eq!(label_for(&pane("a@b:c", "claude")).unwrap(), None);
        assert_eq!(
            label_for(&pane("@b:c", "claude")).unwrap(),
            Some("@b:c".into())
        );
        assert_eq!(
            label_for(&pane("a@:c", "claude")).unwrap(),
            Some("a@:c".into())
        );
        assert_eq!(
            label_for(&pane("mail a@b :c", "claude")).unwrap(),
            Some("mail a@b :c".into())
        );
    }

    #[test]
    fn titles_trimmed_and_collapsed() {
        assert_eq!(
            label_for(&pane(" Fix\tthe\n build ", "claude")).unwrap(),
            Some("Fix the build".into())
        );
        let long = "Refactor the spool writer to use atomic renames everywhere";
        let got = label_for(&pane(long, "codex")).unwrap().unwrap();
        assert_eq!(got.chars().count(), 32);
        assert_eq!(got, "Refactor the spool writer to us…");
        let spaced = format!("{} {}", "a".repeat(30), "bbbb");
        assert_eq!(
            label_for(&pane(&spaced, "claude")).unwrap().unwrap(),
            format!("{}…", "a".repeat(30))
        );
        assert_eq!(
            label_for(&obj(json!({"terminal_title_stripped": 5}))),
            Err(Raised)
        );
    }

    #[test]
    fn default_label_regex() {
        assert!(is_default_label("1"));
        assert!(is_default_label("12\n"));
        assert!(!is_default_label("12\n\n"));
        assert!(!is_default_label(""));
        assert!(!is_default_label("1a"));
    }

    #[test]
    fn event_ids_shapes() {
        assert_eq!(
            event_ids(Some(r#"{"event":"x","data":{"pane_id":"w1:p2"}}"#)),
            (None, Some("w1:p2".into()))
        );
        assert_eq!(
            event_ids(Some(
                r#"{"data":{"tab":{"tab_id":"w1:t1"},"pane":{"pane_id":"w1:p1","tab_id":"w9:t9"}}}"#
            )),
            (Some("w9:t9".into()), Some("w1:p1".into()))
        );
        assert_eq!(
            event_ids(Some(r#"{"tab_id":"a","data":{"tab_id":"b"}}"#)),
            (Some("a".into()), None)
        );
        assert_eq!(event_ids(Some("nope")), (None, None));
        assert_eq!(event_ids(None), (None, None));
        assert_eq!(event_ids(Some("[1]")), (None, None));
    }

    fn snap() -> Map<String, Value> {
        obj(json!({
            "tabs": [
                {"tab_id": "w1:t1", "label": "1"},
                {"tab_id": "w1:t2", "label": "my own name"},
                {"tab_id": "w1:t3", "label": "Old topic"},
                {"tab_id": "w1:t4", "label": "4"},
                {"tab_id": "w1:t5", "label": "Same"},
            ],
            "layouts": [{"tab_id": "w1:t1", "focused_pane_id": "w1:p2"}],
            "panes": [
                {"pane_id": "w1:p1", "tab_id": "w1:t1", "agent": "claude", "terminal_title_stripped": "First"},
                {"pane_id": "w1:p2", "tab_id": "w1:t1"},
                {"pane_id": "w1:p3", "tab_id": "w1:t2", "agent": "claude", "terminal_title_stripped": "Topic"},
                {"pane_id": "w1:p4", "tab_id": "w1:t3", "agent": "codex", "terminal_title_stripped": "New topic"},
                {"pane_id": "w1:p5", "tab_id": "w1:t4", "terminal_title_stripped": "zsh"},
                {"pane_id": "w1:p6", "tab_id": "w1:t5", "agent": "claude", "terminal_title_stripped": "Same"},
            ],
            "agents": [
                {"pane_id": "w1:p2", "agent": "codex", "terminal_title_stripped": "Focused one"},
            ],
        }))
    }

    #[test]
    fn plan_rules() {
        let mut labels = Labels::parse(
            r#"{"w1:t3": "Old topic", "w1:t2": "was mine", "w9:t1": "closed", "w1:t5": "Same"}"#,
        );
        let todo = plan(&snap(), &mut labels, None).unwrap();
        let got: Vec<(&str, &str)> = todo
            .iter()
            .map(|r| (r.tab_id.as_str().unwrap(), r.want.as_str()))
            .collect();
        // t1: focused pane's agent (merged from agents) wins; t2 manual; t3
        // ours, follows the title; t4 has no agent; t5 already right.
        assert_eq!(got, [("w1:t1", "Focused one"), ("w1:t3", "New topic")]);
        assert_eq!(todo[0].current, "1");
        assert_eq!(labels.dump(), r#"{"w1:t3": "Old topic", "w1:t5": "Same"}"#);
        let mut labels = Labels::default();
        let todo = plan(&snap(), &mut labels, Some(&json!("w1:t3"))).unwrap();
        assert!(todo.is_empty(), "t3 not ours without state: {todo:?}");
    }

    #[test]
    fn labels_roundtrip() {
        let l = Labels::parse(r#"{"b": "Ситуация", "a": "x", "b": "y"}"#);
        assert_eq!(l.dump(), r#"{"b": "y", "a": "x"}"#);
        assert_eq!(Labels::parse("[1]"), Labels::default());
        assert_eq!(Labels::parse("{} x"), Labels::default());
        assert_eq!(
            Labels::parse(r#"{"k": "Ситуация"}"#).dump(),
            r#"{"k": "Ситуация"}"#
        );
    }

    #[test]
    fn label_edges() {
        let exact = "x".repeat(32);
        assert_eq!(
            label_for(&pane(&exact, "claude")).unwrap(),
            Some(exact.clone())
        );
        // Falsy values that are not strings count as "no title" / "no agent".
        assert_eq!(
            label_for(&obj(json!({"terminal_title_stripped": false}))),
            Ok(None)
        );
        assert_eq!(
            label_for(&obj(json!({"terminal_title_stripped": "Work", "agent": 0}))),
            Ok(Some("Work".into()))
        );
        assert_eq!(
            label_for(&obj(json!({"terminal_title_stripped": "Work", "agent": 5}))),
            Ok(Some("Work".into()))
        );
    }

    #[test]
    fn labels_same_and_parse() {
        let a = Labels::parse(r#"{"x": "1", "y": "2"}"#);
        assert!(a.same(&Labels::parse(r#"{"y": "2", "x": "1"}"#)));
        assert!(!a.same(&Labels::parse(r#"{"x": "1", "y": "3"}"#)));
        assert!(!a.same(&Labels::parse(r#"{"x": "1"}"#)));
        assert!(!a.same(&Labels::parse(r#"{"x": "1", "z": "2"}"#)));
        for t in [
            "5",
            "-5",
            "1.5",
            "true",
            "\"s\"",
            "null",
            "[1]",
            "",
            r#"{"a": "b"} x"#,
        ] {
            assert_eq!(Labels::parse(t), Labels::default(), "{t}");
        }
        assert_eq!(Labels::parse(" {\"a\": \"b\"}\n").dump(), r#"{"a": "b"}"#);
        let mut l = Labels::default();
        l.set("a", json!("1"));
        l.set("b", json!(2));
        l.set("a", json!("3"));
        assert_eq!(l.dump(), r#"{"a": "3", "b": 2}"#);
        l.remove("a");
        assert_eq!(l.dump(), r#"{"b": 2}"#);
        l.set("c", json!({"k": [1, "v"]}));
        assert_eq!(l.dump(), r#"{"b": 2, "c": {"k": [1, "v"]}}"#);
    }

    #[test]
    fn plan_duplicates_and_odd_labels() {
        let s = obj(json!({
            "tabs": [
                {"tab_id": "w1:t1", "label": "first"},
                {"tab_id": "w1:t2", "label": false},
                {"tab_id": "w1:t1", "label": "1"},
            ],
            "panes": [
                {"pane_id": "w1:p1", "tab_id": "w1:t1", "agent": "claude", "terminal_title_stripped": "Doing"},
                {"pane_id": "w1:p2", "tab_id": "w1:t2", "agent": "claude", "terminal_title_stripped": "Other"},
            ],
        }));
        // The last tab entry wins (its label "1" is a default); a falsy
        // label is "" (not a default, not ours: hands off).
        let mut labels = Labels::parse(r#"{"w1:t2": "mine"}"#);
        let todo = plan(&s, &mut labels, None).unwrap();
        assert_eq!(todo.len(), 1);
        assert_eq!(
            (
                todo[0].tab_id.as_str(),
                todo[0].current.as_str(),
                todo[0].want.as_str()
            ),
            (Some("w1:t1"), "1", "Doing")
        );
        assert_eq!(labels, Labels::default());
        let bad = obj(json!({"tabs": [{"tab_id": "w1:t1", "label": 5}]}));
        assert_eq!(plan(&bad, &mut Labels::default(), None), Err(Raised));
        let bad = obj(json!({"tabs": [{"tab_id": ["x"], "label": "1"}]}));
        assert_eq!(plan(&bad, &mut Labels::default(), None), Err(Raised));
        let bad = obj(json!({"panes": 5}));
        assert_eq!(plan(&bad, &mut Labels::default(), None), Err(Raised));
    }

    #[test]
    fn focused_pane_per_tab_from_layouts() {
        let s = obj(json!({
            "tabs": [{"tab_id": "w1:t1", "label": "1"}, {"tab_id": "w1:t2", "label": "2"}],
            "layouts": [
                {"tab_id": "w1:t1", "focused_pane_id": "w1:p2"},
                {"tab_id": "w1:t2", "focused_pane_id": "w1:p9"},
            ],
            "panes": [
                {"pane_id": "w1:p1", "tab_id": "w1:t1", "agent": "claude", "terminal_title_stripped": "First"},
                {"pane_id": "w1:p2", "tab_id": "w1:t1", "agent": "claude", "terminal_title_stripped": "Second"},
            ],
        }));
        let todo = plan(&s, &mut Labels::default(), None).unwrap();
        assert_eq!(todo.len(), 1);
        assert_eq!(todo[0].want, "Second");
    }
}
