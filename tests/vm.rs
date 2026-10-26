//! VM-side tests (hook, spool, serve, herdr socket), ported from the Python
//! tests/test_vm.py.

use herdr_warp::serve::{self, StreamOpts};
use herdr_warp::vm::{self, Env, MAX_BYTES, MAX_ENTRIES, Spool};
use herdr_warp::{hook, wire};
use serde_json::{Map, Value, json};
use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const NOW: f64 = 1_800_000_000.0;

fn snapshot() -> Value {
    json!({
        "focused_pane_id": "w1:p1",
        "workspaces": [{"workspace_id": "w2", "label": "my-project"}],
        "tabs": [{"tab_id": "w2:t1", "label": "1"}],
        "panes": [{"pane_id": "w2:p1", "tab_id": "w2:t1", "workspace_id": "w2",
                   "agent": "claude", "agent_status": "blocked",
                   "terminal_id": "term_65ead8c4a4dc72",
                   "terminal_title_stripped": "Ситуация\x1b с 1400 \u{202e}"}],
        "agents": [],
    })
}

fn herdr_event(status: &str, pane: &str) -> String {
    json!({"event": "pane_agent_status_changed",
           "data": {"type": "pane_agent_status_changed", "pane_id": pane,
                    "workspace_id": "w2", "agent_status": status, "agent": "claude"}})
    .to_string()
}

struct Base {
    home: PathBuf,
    env: Env,
    now: Rc<Cell<f64>>,
    spool: Spool,
}

impl Drop for Base {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn base() -> Base {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let home = std::env::temp_dir().join(format!(
        "hw-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(home.join("cfg").join("herdr")).unwrap();
    let mut env = Env::new();
    env.insert("HOME".into(), home.display().to_string());
    env.insert(
        "XDG_STATE_HOME".into(),
        home.join("state").display().to_string(),
    );
    env.insert(
        "XDG_CONFIG_HOME".into(),
        home.join("cfg").display().to_string(),
    );
    let sock = vm::default_socket(&env);
    env.insert("HERDR_SOCKET_PATH".into(), sock.display().to_string());
    let now = Rc::new(Cell::new(NOW));
    let n2 = now.clone();
    let spool = Spool::with_clock(vm::state_dir(&env), Box::new(move || n2.get()));
    Base {
        home,
        env,
        now,
        spool,
    }
}

type Calls = Vec<(PathBuf, String, Value)>;

impl Base {
    fn run_hook(&self, event_json: &str, snap: Value) -> Calls {
        let mut env = self.env.clone();
        env.insert("HERDR_PLUGIN_EVENT_JSON".into(), event_json.into());
        let calls = RefCell::new(Vec::new());
        let call = |p: &Path, m: &str, params: Value| {
            calls
                .borrow_mut()
                .push((p.to_path_buf(), m.to_string(), params));
            Some(json!({"result": {"snapshot": snap.clone()}}))
        };
        let now = self.now.get();
        assert_eq!(hook::run(&env, &|_| {}, &|| now, &call), 0);
        calls.into_inner()
    }

    fn entries(&self) -> Vec<Map<String, Value>> {
        self.spool.snapshot().unwrap().2
    }

    fn rec(&self, pane: &str) -> Map<String, Value> {
        json!({"v": 1, "type": "event", "ts": self.now.get() as i64, "pane": pane})
            .as_object()
            .unwrap()
            .clone()
    }
}

// ---------------------------------------------------------------- hook

#[test]
fn blocked_is_spooled_clean() {
    let b = base();
    let calls = b.run_hook(&herdr_event("blocked", "w2:p1"), snapshot());
    assert_eq!(calls[0].1, "session.snapshot");
    let es = b.entries();
    assert_eq!(es.len(), 1);
    let e = &es[0];
    assert_eq!(e["event"], "needs_input");
    assert_eq!(e["title"], "Ситуация с 1400");
    assert_eq!(e["tab_label"], ""); // numeric tab dropped
    assert_eq!(e["term"], "term_65ead8c4a4dc72");
    assert_eq!(e["focused"], false);
    assert_eq!(e["seq"], 1);
    let line = wire::dumps(&Value::Object(e.clone()));
    wire::parse_line(line.as_bytes(), NOW).unwrap(); // wire-valid
    // Byte-identical to what the Python hook spooled for the same input.
    assert_eq!(
        std::fs::read_to_string(&b.spool.path).unwrap(),
        "{\"agent\":\"claude\",\"event\":\"needs_input\",\"focused\":false,\"pane\":\"w2:p1\",\
         \"seq\":1,\"tab\":\"w2:t1\",\"tab_label\":\"\",\"term\":\"term_65ead8c4a4dc72\",\
         \"title\":\"Ситуация с 1400\",\"ts\":1800000000,\"type\":\"event\",\"v\":1,\
         \"ws\":\"w2\",\"ws_label\":\"my-project\"}\n"
    );
    let meta = std::fs::read_to_string(&b.spool.meta_path).unwrap();
    assert!(meta.starts_with("{\"epoch\": \"") && meta.ends_with("\", \"next_seq\": 2}"));
}

#[test]
fn state_changed_during_debounce() {
    let b = base();
    let mut snap = snapshot();
    snap["panes"][0]["agent_status"] = json!("working");
    b.run_hook(&herdr_event("blocked", "w2:p1"), snap);
    assert!(b.entries().is_empty());
}

#[test]
fn other_agent_dropped() {
    let b = base();
    let mut snap = snapshot();
    snap["panes"][0]["agent"] = json!("gemini");
    b.run_hook(&herdr_event("blocked", "w2:p1"), snap);
    assert!(b.entries().is_empty());
}

#[test]
fn named_session_ignored() {
    let mut b = base();
    let other = b.home.join("other.sock").display().to_string();
    b.env.insert("HERDR_SOCKET_PATH".into(), other);
    assert!(
        b.run_hook(&herdr_event("blocked", "w2:p1"), snapshot())
            .is_empty()
    );
}

#[test]
fn socket_compared_after_symlinks() {
    let mut b = base();
    let link = b.home.join("cfg-link");
    std::os::unix::fs::symlink(b.home.join("cfg"), &link).unwrap();
    let via = link.join("herdr").join("herdr.sock").display().to_string();
    b.env.insert("HERDR_SOCKET_PATH".into(), via);
    assert_eq!(
        b.run_hook(&herdr_event("blocked", "w2:p1"), snapshot())
            .len(),
        1
    );
}

#[test]
fn bad_pane_id_ignored() {
    let b = base();
    assert!(
        b.run_hook(&herdr_event("blocked", "w1:p1;id"), snapshot())
            .is_empty()
    );
    assert!(b.run_hook("not json", snapshot()).is_empty());
    assert!(b.run_hook("", snapshot()).is_empty());
}

#[test]
fn coalesce_and_clear() {
    let b = base();
    b.run_hook(&herdr_event("blocked", "w2:p1"), snapshot());
    b.run_hook(&herdr_event("blocked", "w2:p1"), snapshot());
    let seqs: Vec<Value> = b.entries().iter().map(|e| e["seq"].clone()).collect();
    assert_eq!(seqs, [json!(2)]);
    // No snapshot call for a state that is not notified.
    assert!(
        b.run_hook(&herdr_event("working", "w2:p1"), snapshot())
            .is_empty()
    );
    assert!(b.entries().is_empty());
}

#[test]
fn ttl_and_caps() {
    let b = base();
    for i in 0..100 {
        let mut r = b.rec(&format!("w1:p{i}"));
        r.insert("x".into(), json!("y".repeat(300)));
        b.spool.append(r).unwrap();
    }
    let es = b.entries();
    assert!(es.len() <= MAX_ENTRIES);
    assert!(std::fs::metadata(&b.spool.path).unwrap().len() as usize <= MAX_BYTES);
    assert_eq!(es.last().unwrap()["seq"], 100);
    b.now.set(b.now.get() + 3601.0);
    b.spool.append(b.rec("w9:p9")).unwrap();
    let panes: Vec<Value> = b.entries().iter().map(|e| e["pane"].clone()).collect();
    assert_eq!(panes, [json!("w9:p9")]);
}

#[test]
fn concurrent_writers() {
    let b = base();
    let dir = b.spool.dir.clone();
    // Separate processes, like the Python test: re-run this test binary in
    // writer mode.
    let exe = std::env::current_exe().unwrap();
    let kids: Vec<_> = (0..4)
        .map(|n| {
            std::process::Command::new(&exe)
                .args(["--exact", "spool_writer_child", "--nocapture"])
                .env("HW_SPOOL_DIR", &dir)
                .env("HW_WRITER", n.to_string())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut k in kids {
        assert!(k.wait().unwrap().success());
    }
    let (_, head, es) = Spool::new(&dir).snapshot().unwrap();
    assert_eq!(head, 100);
    let seqs: std::collections::HashSet<i64> =
        es.iter().map(|e| e["seq"].as_i64().unwrap()).collect();
    assert_eq!(seqs.len(), es.len());
    assert_eq!(es.len(), 64);
}

/// Helper for concurrent_writers; a no-op unless run as its child.
#[test]
fn spool_writer_child() {
    let (Ok(dir), Ok(n)) = (std::env::var("HW_SPOOL_DIR"), std::env::var("HW_WRITER")) else {
        return;
    };
    let s = Spool::new(dir);
    for i in 0..25 {
        let r = json!({"ts": vm::now() as i64, "pane": format!("w{n}:p{i}")});
        s.append(r.as_object().unwrap().clone()).unwrap();
    }
}

#[test]
fn files_private() {
    let b = base();
    b.run_hook(&herdr_event("blocked", "w2:p1"), snapshot());
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&b.spool.dir), 0o700);
    assert_eq!(mode(&b.spool.path), 0o600);
    assert_eq!(mode(&b.spool.meta_path), 0o600);
}

#[test]
fn debug_trace() {
    let b = base();
    std::fs::create_dir_all(&b.spool.dir).unwrap();
    File::create(b.spool.dir.join("debug")).unwrap();
    b.run_hook(&herdr_event("blocked", "w2:p1"), snapshot());
    b.run_hook(&herdr_event("idle", "w2:p1"), snapshot());
    let log = std::fs::read_to_string(b.spool.dir.join("debug.log")).unwrap();
    let msgs: Vec<&str> = log.lines().map(|l| l.split_once(' ').unwrap().1).collect();
    assert_eq!(
        msgs,
        ["spooled seq=1 needs_input w2:p1", "cleared w2:p1 (idle)"]
    );
}

#[test]
fn long_title_dropped_to_fit_line() {
    let b = base();
    let mut snap = snapshot();
    // The widest labels the cleaner lets through (40 four-byte chars each).
    // The hook drops the title if the line would exceed the cap; with these
    // limits it still fits, which is what this pins down.
    snap["workspaces"][0]["label"] = json!("\u{1f600}".repeat(40));
    snap["tabs"][0]["label"] = json!("\u{1f601}".repeat(40));
    snap["panes"][0]["terminal_title_stripped"] = json!("\u{1f602}".repeat(40));
    b.run_hook(&herdr_event("blocked", "w2:p1"), snap);
    let e = &b.entries()[0];
    let line = wire::dumps(&Value::Object(e.clone()));
    assert!(line.len() <= wire::MAX_LINE_BYTES);
}

// ---------------------------------------------------------------- serve

fn lines(buf: &[u8]) -> Vec<Value> {
    buf.split(|&c| c == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_slice(l).unwrap())
        .collect()
}

fn no_call(_: &Path, _: &str, _: Value) -> Option<Value> {
    None
}

#[test]
fn bad_commands() {
    let b = base();
    for cmd in [
        "",
        "id",
        "stream",
        "stream x",
        "stream 01",
        "stream 1 2",
        "stream 1; id",
        "stream 1\nid",
        "focus w1:p1",
        "focus w1:p1 term_1",
        "focus $(id) term_65ead8c4a4dc72",
        "focus w1:p1 term_65ead8c4a4dc72 x",
        "stream 99999999999999999",
    ] {
        let mut env = b.env.clone();
        env.insert("SSH_ORIGINAL_COMMAND".into(), cmd.into());
        let mut out = Vec::new();
        let rc = serve::run(&env, &mut out, &no_call, &opts(0.0, &|| NOW));
        assert_eq!(rc, 2, "{cmd:?}");
        assert_eq!(
            lines(&out),
            [json!({"v": 1, "type": "error", "code": "bad_command"})]
        );
        assert_eq!(
            out,
            b"{\"code\":\"bad_command\",\"type\":\"error\",\"v\":1}\n"
        );
    }
}

fn opts<'a>(max_seconds: f64, clock: &'a dyn Fn() -> f64) -> StreamOpts<'a> {
    StreamOpts {
        clock,
        monotonic: &|| 0.0,
        sleep: &|_| {},
        max_seconds,
        hb_seconds: serve::HB_SECONDS,
    }
}

impl Base {
    fn stream(&self, since: i64, epoch: Option<&str>) -> Vec<Value> {
        let mut out = Vec::new();
        let now = self.now.get();
        let clock = move || now;
        serve::run_stream(since, epoch, &self.spool, &mut out, &opts(0.0, &clock)).unwrap();
        lines(&out)
    }
}

fn event_seqs(out: &[Value]) -> Vec<i64> {
    out.iter()
        .filter(|o| o["type"] == "event")
        .map(|o| o["seq"].as_i64().unwrap())
        .collect()
}

#[test]
fn replay_from_seq() {
    let b = base();
    for i in 0..3 {
        b.spool.append(b.rec(&format!("w1:p{i}"))).unwrap();
    }
    let out = b.stream(1, None);
    assert_eq!(out[0]["type"], "hello");
    assert_eq!(out[0]["head"], 3);
    assert_eq!(event_seqs(&out), [2, 3]);
}

#[test]
fn epoch_mismatch_replays_all() {
    let b = base();
    b.spool.append(b.rec("w1:p1")).unwrap();
    let out = b.stream(5, Some("0000000000000000"));
    assert_eq!(event_seqs(&out), [1]);
}

#[test]
fn expired_not_sent() {
    let b = base();
    b.spool.append(b.rec("w1:p1")).unwrap();
    b.now.set(b.now.get() + 4000.0);
    assert!(event_seqs(&b.stream(0, None)).is_empty());
}

#[test]
fn spool_recreated_sends_new_hello() {
    let b = base();
    b.spool.append(b.rec("w1:p1")).unwrap();
    let dir = b.spool.dir.clone();
    let mut out = Vec::new();
    let ticks = Cell::new(0);
    let sleep = |_| {
        if ticks.get() == 0 {
            std::fs::remove_file(dir.join("meta.json")).unwrap();
            std::fs::remove_file(dir.join("spool.jsonl")).unwrap();
            Spool::new(&dir).append(b.rec("w1:p7")).unwrap();
        }
        ticks.set(ticks.get() + 1);
    };
    let mono = || ticks.get() as f64;
    let o = StreamOpts {
        clock: &vm::now,
        monotonic: &mono,
        sleep: &sleep,
        max_seconds: 2.0,
        hb_seconds: 100.0,
    };
    serve::run_stream(0, None, &b.spool, &mut out, &o).unwrap();
    let out = lines(&out);
    let kinds: Vec<&str> = out.iter().map(|o| o["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["hello", "event", "hello", "event"]);
    assert_ne!(out[0]["epoch"], out[2]["epoch"]);
    assert_eq!(event_seqs(&out), [1, 1]);
}

fn pipe() -> (File, File) {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

#[test]
fn live_stream_and_heartbeat() {
    let b = base();
    let (r, w) = pipe();
    let mut env = b.env.clone();
    env.insert("SSH_ORIGINAL_COMMAND".into(), "stream 0".into());
    let dir = b.spool.dir.clone();
    let server = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let mono = move || start.elapsed().as_secs_f64();
        let sleep = |s: f64| std::thread::sleep(std::time::Duration::from_secs_f64(s / 10.0));
        let o = StreamOpts {
            clock: &vm::now,
            monotonic: &mono,
            sleep: &sleep,
            max_seconds: serve::MAX_STREAM_SECONDS,
            hb_seconds: 0.5,
        };
        let mut w = std::io::BufWriter::new(w);
        serve::run(&env, &mut w, &no_call, &o)
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    let rec = json!({"v": 1, "type": "event", "ts": vm::now() as i64, "pane": "w1:p1"});
    Spool::new(&dir)
        .append(rec.as_object().unwrap().clone())
        .unwrap();
    let mut got: Vec<String> = Vec::new();
    let mut rd = BufReader::new(r);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline
        && !(got.iter().any(|k| k == "event") && got.iter().any(|k| k == "hb"))
    {
        let mut l = String::new();
        if rd.read_line(&mut l).unwrap() == 0 {
            break;
        }
        let v: Value = serde_json::from_str(&l).unwrap();
        got.push(v["type"].as_str().unwrap().to_string());
    }
    assert_eq!(got[0], "hello");
    assert!(got.iter().any(|k| k == "event"));
    assert!(got.iter().any(|k| k == "hb"));
    // Reader gone: the next write hits EPIPE and the server exits 0.
    drop(rd);
    assert_eq!(server.join().unwrap(), 0);
}

#[test]
fn focus_checks_terminal() {
    let b = base();
    let calls = RefCell::new(Vec::new());
    let call = |_: &Path, m: &str, _: Value| {
        calls.borrow_mut().push(m.to_string());
        if m == "pane.get" {
            Some(json!({"result": {"pane": {"terminal_id": "term_aaaaaaaa"}}}))
        } else {
            Some(json!({"result": {}}))
        }
    };
    let clock = vm::now;
    let mut s = Vec::new();
    let rc = serve::run_focus(
        "w1:p1",
        "term_bbbbbbbb",
        &b.spool,
        &mut s,
        &call,
        &clock,
        &b.env,
    )
    .unwrap();
    assert_eq!(rc, 1);
    assert_eq!(lines(&s)[0]["code"], "terminal_changed");
    assert_eq!(*calls.borrow(), ["pane.get"]);
    // Backdate the rate-limit stamp.
    let last = b.spool.dir.join("focus.last");
    let t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let c = std::ffi::CString::new(last.to_str().unwrap()).unwrap();
    assert_eq!(
        unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [t, t].as_ptr(), 0) },
        0
    );
    let mut s = Vec::new();
    calls.borrow_mut().clear();
    let rc = serve::run_focus(
        "w1:p1",
        "term_aaaaaaaa",
        &b.spool,
        &mut s,
        &call,
        &clock,
        &b.env,
    )
    .unwrap();
    assert_eq!(rc, 0);
    assert_eq!(
        lines(&s)[0],
        json!({"v": 1, "type": "focus", "ok": true, "code": "focused"})
    );
    assert_eq!(*calls.borrow(), ["pane.get", "pane.focus"]);
    let mut s = Vec::new();
    serve::run_focus(
        "w1:p1",
        "term_aaaaaaaa",
        &b.spool,
        &mut s,
        &call,
        &clock,
        &b.env,
    )
    .unwrap();
    assert_eq!(lines(&s)[0]["code"], "rate_limited");
}

#[test]
fn focus_pane_not_found_and_failed() {
    let b = base();
    let mut env = b.env.clone();
    env.insert(
        "SSH_ORIGINAL_COMMAND".into(),
        "focus w1:p1 term_aaaaaaaa".into(),
    );
    let mut s = Vec::new();
    assert_eq!(serve::run(&env, &mut s, &no_call, &opts(0.0, &vm::now)), 1);
    assert_eq!(lines(&s)[0]["code"], "pane_not_found");
    std::fs::remove_file(b.spool.dir.join("focus.last")).unwrap();
    let call = |_: &Path, m: &str, _: Value| {
        if m == "pane.get" {
            Some(json!({"result": {"pane": {"terminal_id": "term_aaaaaaaa"}}}))
        } else {
            Some(json!({"error": {"code": "x"}}))
        }
    };
    let mut s = Vec::new();
    assert_eq!(serve::run(&env, &mut s, &call, &opts(0.0, &vm::now)), 1);
    assert_eq!(
        lines(&s)[0],
        json!({"v": 1, "type": "focus", "ok": false, "code": "focus_failed"})
    );
}

// ---------------------------------------------------------------- socket

#[test]
fn herdr_call_roundtrip() {
    let b = base();
    let sock = vm::default_socket(&b.env);
    let srv = UnixListener::bind(&sock).unwrap();
    let t = std::thread::spawn(move || {
        let (mut conn, _) = srv.accept().unwrap();
        let mut l = String::new();
        BufReader::new(conn.try_clone().unwrap())
            .read_line(&mut l)
            .unwrap();
        conn.write_all(b"{\"id\":\"x\",\"result\":{\"ok\":1}}\n")
            .unwrap();
        l
    });
    let reply = vm::herdr_call(&sock, "pane.focus", json!({"pane_id": "w1:p1"})).unwrap();
    let req: Value = serde_json::from_str(&t.join().unwrap()).unwrap();
    assert_eq!(reply["result"], json!({"ok": 1}));
    assert_eq!(req["method"], "pane.focus");
    assert_eq!(req["params"]["pane_id"], "w1:p1");
    assert!(vm::herdr_call(&b.home.join("nope"), "x", json!({})).is_none());
}

#[test]
fn herdr_call_times_out() {
    let b = base();
    let sock = vm::default_socket(&b.env);
    let srv = UnixListener::bind(&sock).unwrap();
    let t = std::thread::spawn(move || {
        let (mut conn, _) = srv.accept().unwrap();
        let mut buf = [0u8; 64];
        let _ = conn.read(&mut buf);
        std::thread::sleep(std::time::Duration::from_secs(3));
    });
    let start = std::time::Instant::now();
    assert!(vm::herdr_call(&sock, "x", json!({})).is_none());
    assert!(start.elapsed() < std::time::Duration::from_millis(2900));
    t.join().unwrap();
}
