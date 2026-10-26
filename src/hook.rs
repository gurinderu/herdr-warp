//! `hook` (legacy name `herdr-notify-hook`): spool done / needs-input agent
//! events for the Mac notifier.
//!
//! Runs from the herdr-notify plugin on `pane.agent_status_changed`, with the
//! event in HERDR_PLUGIN_EVENT_JSON. herdr spawns hooks detached with nowhere
//! to print, so every failure exits 0 quietly; `touch <state dir>/debug`
//! turns on a breadcrumb log (`<state dir>/debug.log`).
//!
//! Only the default herdr session is handled (the one the Mac's login
//! attaches to); every workspace in it is covered.

use crate::py::{self, Raised, get};
use crate::vm::{self, Call, Env, Spool};
use crate::wire::{self, AGENTS, LABEL_MAX, MAX_LINE_BYTES, MAX_SEQ, SCHEMA_VERSION};
use serde_json::{Map, Value, json};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// herdr debounces its own toasts by 1 s; blocked flaps briefly while
/// approval UIs redraw. Wait a little longer, then re-check the pane.
pub const DEBOUNCE_SECONDS: f64 = 1.5;

/// Append a breadcrumb to `<dir>/debug.log` when `<dir>/debug` exists.
pub fn trace(dir: &Path, msg: &str) {
    if !py::exists(dir.join("debug")) {
        return;
    }
    if let Ok(mut f) = OpenOptions::new()
        .append(true)
        .create(true)
        .open(dir.join("debug.log"))
    {
        let _ = writeln!(f, "{} {}", vm::now() as i64, msg);
    }
}

/// herdr delivers `{"event": ..., "data": {...}}`; accept a bare payload too.
pub fn unwrap(raw: Option<&str>) -> Option<Map<String, Value>> {
    let Ok(Value::Object(event)) = serde_json::from_str::<Value>(raw?) else {
        return None;
    };
    if !event.contains_key("agent_status")
        && let Some(Value::Object(data)) = event.get("data")
    {
        return Some(data.clone());
    }
    Some(event)
}

fn same_socket(a: &str, b: &Path) -> bool {
    py::realpath(a) == py::realpath(&b.to_string_lossy())
}

type Found<'a> = (
    &'a Map<String, Value>,
    Option<&'a Map<String, Value>>,
    Option<&'a Map<String, Value>>,
);

/// (pane, tab, workspace) objects from a session snapshot; Ok(None) when the
/// pane is gone, Err where Python would have raised.
fn locate<'a>(snap: &'a Map<String, Value>, pane_id: &str) -> Result<Option<Found<'a>>, Raised> {
    let agents = py::list_for_concat(snap, "agents")?;
    let panes = py::list_for_concat(snap, "panes")?;
    let pane = agents
        .iter()
        .chain(panes)
        .filter_map(Value::as_object)
        .find(|p| get(p, "pane_id").and_then(Value::as_str) == Some(pane_id));
    let Some(pane) = pane else {
        return Ok(None);
    };
    let tab = py::list_for_iter(snap, "tabs")?
        .iter()
        .filter_map(Value::as_object)
        .find(|t| get(t, "tab_id") == get(pane, "tab_id"));
    let ws = py::list_for_iter(snap, "workspaces")?
        .iter()
        .filter_map(Value::as_object)
        .find(|w| get(w, "workspace_id") == get(pane, "workspace_id"));
    Ok(Some((pane, tab, ws)))
}

fn str_id(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
}

/// The wire record (without seq) for a herdr event; Ok(None) to skip.
pub fn build_record(
    event: &Map<String, Value>,
    snap: &Map<String, Value>,
    now: f64,
) -> Result<Option<Map<String, Value>>, Raised> {
    let pane_id = get(event, "pane_id").and_then(Value::as_str).unwrap_or("");
    let status = get(event, "agent_status");
    let Some((pane, tab, ws)) = locate(snap, pane_id)? else {
        return Ok(None);
    };
    // Gone, or no longer in that state after the debounce.
    if get(pane, "agent_status") != status {
        return Ok(None);
    }
    let Some(event_kind) = status
        .and_then(Value::as_str)
        .and_then(wire::status_to_event)
    else {
        return Err(Raised);
    };
    let agent = py::or(&[get(pane, "agent"), get(event, "agent")]);
    let Some(agent) = agent.and_then(Value::as_str).filter(|a| AGENTS.contains(a)) else {
        return Ok(None);
    };
    let ws_id = py::or(&[get(pane, "workspace_id"), get(event, "workspace_id")]);
    let ws_id = if py::truthy(ws_id) { ws_id } else { None };
    let Some(ws_id) = str_id(ws_id).filter(|s| wire::is_id(s)) else {
        return Ok(None);
    };
    let tab_id = match get(pane, "tab_id") {
        v if !py::truthy(v) => "",
        v => str_id(v).filter(|s| wire::is_id(s)).unwrap_or(""),
    };
    let term = str_id(get(pane, "terminal_id"))
        .filter(|s| wire::is_term(s))
        .unwrap_or("");
    let mut tab_label = wire::clean_label(tab.and_then(|t| get(t, "label")), LABEL_MAX);
    if py::isdigit(&tab_label) {
        tab_label.clear();
    }
    let title = wire::clean_label(
        py::or(&[
            get(event, "title"),
            get(pane, "terminal_title_stripped"),
            get(pane, "label"),
        ]),
        LABEL_MAX,
    );
    let focused = get(snap, "focused_pane_id").and_then(Value::as_str) == Some(pane_id);
    let mut record = json!({
        "v": SCHEMA_VERSION,
        "type": "event",
        "ts": now as i64,
        "event": event_kind,
        "agent": agent,
        "ws": ws_id,
        "tab": tab_id,
        "pane": pane_id,
        "term": term,
        "ws_label": wire::clean_label(ws.and_then(|w| get(w, "label")), LABEL_MAX),
        "tab_label": tab_label,
        "title": title,
        "focused": focused,
    });
    // Leave room for the seq; 40-char labels keep this far below the cap.
    let mut probe = record.clone();
    probe["seq"] = Value::from(MAX_SEQ);
    if wire::dumps(&probe).len() > MAX_LINE_BYTES {
        record["title"] = Value::from("");
    }
    match record {
        Value::Object(m) => Ok(Some(m)),
        _ => Err(Raised),
    }
}

/// The hook proper; always 0.
pub fn run(env: &Env, sleep: &dyn Fn(f64), clock: &dyn Fn() -> f64, call: Call) -> i32 {
    let state = vm::state_dir(env);
    let Some(event) = unwrap(env.get("HERDR_PLUGIN_EVENT_JSON").map(String::as_str)) else {
        return 0;
    };
    let Some(pane_id) = get(&event, "pane_id")
        .and_then(Value::as_str)
        .filter(|p| wire::is_id(p))
    else {
        return 0;
    };
    let status = get(&event, "agent_status");
    let sock = env.get("HERDR_SOCKET_PATH").cloned().unwrap_or_default();
    if !same_socket(&sock, &vm::default_socket(env)) {
        trace(&state, &format!("skip non-default session {sock}"));
        return 0;
    }
    let spool = Spool::new(&state);
    match status {
        // `status in dict` raises for lists and dicts.
        Some(Value::Array(_)) | Some(Value::Object(_)) => return 0,
        _ => {}
    }
    if status
        .and_then(Value::as_str)
        .and_then(wire::status_to_event)
        .is_none()
    {
        if let Ok(true) = spool.drop_pane(pane_id) {
            trace(
                &state,
                &format!("cleared {pane_id} ({})", py::str_value(status)),
            );
        }
        return 0;
    }
    sleep(DEBOUNCE_SECONDS);
    let reply = call(Path::new(&sock), "session.snapshot", json!({}));
    let snap = reply
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|r| r.get("result"))
        .and_then(Value::as_object)
        .and_then(|r| r.get("snapshot"));
    let Some(snap) = snap else {
        trace(&state, "no snapshot");
        return 0;
    };
    let Some(snap) = snap.as_object() else {
        return 0;
    };
    let record = match build_record(&event, snap, clock()) {
        Ok(Some(r)) => r,
        Ok(None) => {
            trace(&state, &format!("skip {pane_id} {}", py::str_value(status)));
            return 0;
        }
        Err(Raised) => return 0,
    };
    let kind = record["event"].as_str().unwrap_or("").to_string();
    if let Ok(seq) = spool.append(record) {
        trace(&state, &format!("spooled seq={seq} {kind} {pane_id}"));
    }
    0
}

pub fn main(_args: &[String]) -> i32 {
    let env = vm::os_env();
    let sleep = |s: f64| std::thread::sleep(std::time::Duration::from_secs_f64(s));
    run(&env, &sleep, &vm::now, &vm::herdr_call)
}
