//! tab-title end to end: the socket path (herdr's API, no process spawned)
//! and the CLI fallback, with a fake herdr on each.

use herdr_warp::tab_title::{self, Args, Herdr};
use herdr_warp::vm::Env;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn snap() -> Value {
    json!({
        "tabs": [
            {"tab_id": "w1:t1", "label": "1"},
            {"tab_id": "w1:t2", "label": "my own name"},
            {"tab_id": "w1:t3", "label": "3"},
        ],
        "panes": [
            {"pane_id": "w1:p1", "tab_id": "w1:t1", "agent": "claude", "terminal_title_stripped": "Fix the build"},
            {"pane_id": "w1:p2", "tab_id": "w1:t2", "agent": "codex", "terminal_title_stripped": "Topic"},
            {"pane_id": "w1:p3", "tab_id": "w1:t3", "agent": "codex", "terminal_title_stripped": "Ситуация"},
        ],
    })
}

struct T {
    dir: PathBuf,
    env: Env,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Drop for T {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn tmp() -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "hw-tt-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A herdr socket answering snapshot and rename (refusing tabs in `refuse`).
fn with_socket(snapshot: Value, refuse: &'static [&'static str]) -> T {
    let dir = tmp();
    let sock = dir.join("s.sock");
    let srv = UnixListener::bind(&sock).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let reqs = requests.clone();
    std::thread::spawn(move || {
        for conn in srv.incoming() {
            let Ok(mut conn) = conn else { return };
            let mut line = String::new();
            BufReader::new(conn.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            reqs.lock().unwrap().push(line.clone());
            let req: Value = serde_json::from_str(&line).unwrap();
            let reply = match req["method"].as_str() {
                Some("session.snapshot") => {
                    json!({"id": req["id"], "result": {"snapshot": snapshot}})
                }
                Some("tab.rename")
                    if refuse.contains(&req["params"]["tab_id"].as_str().unwrap()) =>
                {
                    json!({"id": req["id"], "error": {"code": "nope", "message": "no"}})
                }
                Some("tab.rename") => json!({"id": req["id"], "result": {}}),
                _ => json!({"id": req["id"], "error": {"code": "unknown", "message": "?"}}),
            };
            let _ = conn.write_all(format!("{reply}\n").as_bytes());
        }
    });
    let mut env = Env::new();
    env.insert("HOME".into(), dir.display().to_string());
    env.insert("HERDR_SOCKET_PATH".into(), sock.display().to_string());
    env.insert(
        "HERDR_PLUGIN_STATE_DIR".into(),
        dir.join("state").display().to_string(),
    );
    T { dir, env, requests }
}

impl T {
    fn run(&self, args: Args, extra: &[(&str, &str)]) -> (Result<(), String>, String) {
        let mut env = self.env.clone();
        for (k, v) in extra {
            env.insert(k.to_string(), v.to_string());
        }
        let mut out = Vec::new();
        let r = tab_title::run(&env, &args, &mut out);
        (r, String::from_utf8(out).unwrap())
    }

    fn state(&self, f: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join("state").join(f)).ok()
    }

    fn debug_on(&self) {
        std::fs::create_dir_all(self.dir.join("state")).unwrap();
        std::fs::write(self.dir.join("state/debug"), "").unwrap();
    }

    fn log(&self) -> Vec<String> {
        self.state("debug.log")
            .unwrap_or_default()
            .lines()
            .map(|l| l.split_once(' ').unwrap().1.to_string())
            .collect()
    }
}

fn all() -> Args {
    Args {
        all: true,
        dry_run: false,
    }
}

#[test]
fn socket_renames_and_records() {
    let t = with_socket(snap(), &[]);
    t.debug_on();
    assert_eq!(t.run(all(), &[]), (Ok(()), String::new()));
    let reqs = t.requests.lock().unwrap().clone();
    assert_eq!(
        reqs,
        [
            "{\"id\": \"herdr-tab-title\", \"method\": \"session.snapshot\", \"params\": {}}\n",
            "{\"id\": \"herdr-tab-title\", \"method\": \"tab.rename\", \"params\": {\"label\": \"Fix the build\", \"tab_id\": \"w1:t1\"}}\n",
            "{\"id\": \"herdr-tab-title\", \"method\": \"tab.rename\", \"params\": {\"label\": \"\\u0421\\u0438\\u0442\\u0443\\u0430\\u0446\\u0438\\u044f\", \"tab_id\": \"w1:t3\"}}\n",
        ]
    );
    assert_eq!(
        t.state("labels.json").unwrap(),
        r#"{"w1:t1": "Fix the build", "w1:t3": "Ситуация"}"#
    );
    assert_eq!(
        t.log(),
        [
            "manual: tab None pane None",
            "manual: w1:t1 -> 'Fix the build'",
            "manual: w1:t3 -> 'Ситуация'",
        ]
    );
    assert!(t.dir.join("state/lock").exists());
}

#[test]
fn refused_rename_is_not_recorded() {
    let t = with_socket(snap(), &["w1:t1"]);
    t.debug_on();
    assert_eq!(t.run(all(), &[]).0, Ok(()));
    assert_eq!(t.state("labels.json").unwrap(), r#"{"w1:t3": "Ситуация"}"#);
    assert_eq!(
        t.log()[1],
        "manual: rename w1:t1 failed: herdr refused tab.rename: {\"code\":\"nope\",\"message\":\"no\"}"
    );
}

#[test]
fn dry_run_touches_nothing() {
    let t = with_socket(snap(), &[]);
    let (r, out) = t.run(
        Args {
            all: true,
            dry_run: true,
        },
        &[],
    );
    assert_eq!(r, Ok(()));
    assert_eq!(
        out,
        "rename w1:t1 \"1\" -> \"Fix the build\"\nrename w1:t3 \"3\" -> \"Ситуация\"\n"
    );
    assert_eq!(t.requests.lock().unwrap().len(), 1);
    assert!(!t.dir.join("state").exists());
}

#[test]
fn event_mode_one_tab() {
    let t = with_socket(snap(), &[]);
    let ev = |j: &'static str| {
        [
            ("HERDR_PLUGIN_EVENT", "pane.focused"),
            ("HERDR_PLUGIN_EVENT_JSON", j),
        ]
    };
    // By pane id: its tab is looked up in the snapshot.
    let (r, _) = t.run(Args::default(), &ev(r#"{"data": {"pane_id": "w1:p3"}}"#));
    assert_eq!(r, Ok(()));
    assert_eq!(t.state("labels.json").unwrap(), r#"{"w1:t3": "Ситуация"}"#);
    // An unknown pane: nothing to do.
    let before = t.requests.lock().unwrap().len();
    assert_eq!(
        t.run(Args::default(), &ev(r#"{"pane_id": "w9:p9"}"#)).0,
        Ok(())
    );
    assert_eq!(t.requests.lock().unwrap().len(), before + 1);
    // No ids at all: not even a snapshot.
    assert_eq!(t.run(Args::default(), &ev("garbage")).0, Ok(()));
    assert_eq!(t.requests.lock().unwrap().len(), before + 1);
    // `startup` means every tab.
    assert_eq!(
        t.run(Args::default(), &[("HERDR_PLUGIN_EVENT", "startup")])
            .0,
        Ok(())
    );
    assert_eq!(
        t.state("labels.json").unwrap(),
        r#"{"w1:t3": "Ситуация", "w1:t1": "Fix the build"}"#
    );
}

#[test]
fn bad_socket_reply_is_an_error() {
    let dir = tmp();
    let sock = dir.join("s.sock");
    let srv = UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        if let Ok((mut c, _)) = srv.accept() {
            let _ = c.write_all(b"garbage\n");
        }
    });
    let mut env = Env::new();
    env.insert("HERDR_SOCKET_PATH".into(), sock.display().to_string());
    env.insert(
        "HERDR_PLUGIN_STATE_DIR".into(),
        dir.join("st").display().to_string(),
    );
    let r = tab_title::run(&env, &all(), &mut Vec::new());
    assert!(r.unwrap_err().starts_with("herdr socket: "));
    std::fs::remove_dir_all(dir).unwrap();
}

fn fake_cli(dir: &std::path::Path, snapshot: &Value, fail: &str) -> String {
    std::fs::write(
        dir.join("snap.json"),
        json!({"result": {"snapshot": snapshot}}).to_string(),
    )
    .unwrap();
    let p = dir.join("herdr");
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\nd=$(dirname \"$0\")\nprintf '%s\\n' \"$*\" >> \"$d/calls\"\n\
             case \"$1\" in api) [ {fail} = api ] && exit 1; cat \"$d/snap.json\" ;; \
             tab) [ \"$3\" = {fail} ] && exit 3; exit 0 ;; esac\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.display().to_string()
}

#[test]
fn cli_fallback_without_a_socket() {
    let dir = tmp();
    let bin = fake_cli(&dir, &snap(), "w1:t3");
    let mut env = Env::new();
    env.insert(
        "HERDR_SOCKET_PATH".into(),
        dir.join("missing.sock").display().to_string(),
    );
    env.insert("HERDR_BIN_PATH".into(), bin.clone());
    env.insert(
        "HERDR_PLUGIN_STATE_DIR".into(),
        dir.join("st").display().to_string(),
    );
    let (_, h) = tab_title::snapshot(&env).unwrap();
    assert_eq!(h, Herdr::Cli(bin.clone()));
    std::fs::create_dir_all(dir.join("st")).unwrap();
    std::fs::write(dir.join("st/debug"), "").unwrap();
    assert_eq!(tab_title::run(&env, &all(), &mut Vec::new()), Ok(()));
    let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
    assert_eq!(
        calls,
        "api snapshot\napi snapshot\ntab rename w1:t1 Fix the build\ntab rename w1:t3 Ситуация\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("st/labels.json")).unwrap(),
        r#"{"w1:t1": "Fix the build"}"#
    );
    let log = std::fs::read_to_string(dir.join("st/debug.log")).unwrap();
    assert!(
        log.contains(&format!(
            "manual: rename w1:t3 failed: Command '['{bin}', 'tab', 'rename', 'w1:t3', 'Ситуация']' returned non-zero exit status 3."
        )),
        "{log}"
    );
    // A failing `herdr api snapshot` is the run's error.
    let bin = fake_cli(&dir, &snap(), "api");
    env.insert("HERDR_BIN_PATH".into(), bin.clone());
    assert_eq!(
        tab_title::run(&env, &all(), &mut Vec::new()),
        Err(format!(
            "Command '['{bin}', 'api', 'snapshot']' returned non-zero exit status 1."
        ))
    );
    // And a missing one.
    env.insert(
        "HERDR_BIN_PATH".into(),
        dir.join("nope").display().to_string(),
    );
    let e = tab_title::run(&env, &all(), &mut Vec::new()).unwrap_err();
    assert!(e.starts_with("[Errno 2] No such file or directory"), "{e}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn state_dir_fallbacks() {
    let t = with_socket(snap(), &[]);
    let mut env = t.env.clone();
    env.remove("HERDR_PLUGIN_STATE_DIR");
    env.insert(
        "XDG_STATE_HOME".into(),
        t.dir.join("xdg").display().to_string(),
    );
    tab_title::run(&env, &all(), &mut Vec::new()).unwrap();
    assert!(
        t.dir
            .join("xdg/herdr/plugins/herdr-tab-title/labels.json")
            .exists()
    );
    env.insert("XDG_STATE_HOME".into(), String::new());
    tab_title::run(&env, &all(), &mut Vec::new()).unwrap();
    // No XDG_STATE_HOME either: under $HOME.
    assert!(
        t.dir
            .join(".local/state/herdr/plugins/herdr-tab-title/labels.json")
            .exists()
    );
}

#[test]
fn default_socket_when_unset() {
    let t = with_socket(snap(), &[]);
    let mut env = t.env.clone();
    env.remove("HERDR_SOCKET_PATH");
    env.insert(
        "XDG_CONFIG_HOME".into(),
        t.dir.join("cfg").display().to_string(),
    );
    std::fs::create_dir_all(t.dir.join("cfg/herdr")).unwrap();
    std::os::unix::fs::symlink(t.dir.join("s.sock"), t.dir.join("cfg/herdr/herdr.sock")).unwrap();
    let (_, h) = tab_title::snapshot(&env).unwrap();
    assert_eq!(h, Herdr::Socket(t.dir.join("cfg/herdr/herdr.sock")));
}

#[test]
fn dry_run_keeps_stale_state() {
    let t = with_socket(snap(), &[]);
    std::fs::create_dir_all(t.dir.join("state")).unwrap();
    let stale = r#"{"w9:t9": "closed tab"}"#;
    std::fs::write(t.dir.join("state/labels.json"), stale).unwrap();
    let (r, out) = t.run(
        Args {
            all: true,
            dry_run: true,
        },
        &[],
    );
    assert_eq!(r, Ok(()));
    assert_eq!(out.lines().count(), 2);
    assert_eq!(t.state("labels.json").unwrap(), stale);
    assert!(!t.dir.join("state/lock").exists());
}
