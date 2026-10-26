//! Mac agent tests, ported from the Python tests/test_agent.py, plus the
//! session read loop and the option parser.

use herdr_warp::agent::{self, Agent, Config, Hooks};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::sync::{Arc, Mutex};

const NOW: f64 = 1_800_000_000.0;

// Placeholder hosts: 192.0.2.0/24 is TEST-NET-1, 198.18.0.0/15 a benchmark
// range (what a fake-ip resolver hands out).
const PS: &str = "\
    1     0 ??       /sbin/launchd
  500     1 ??       /Applications/Warp.app/Contents/MacOS/stable
  510   500 ttys003  -zsh
  511   510 ttys003  /opt/homebrew/bin/mosh-client
  520   500 ttys004  -zsh
  521   520 ttys004  /nix/store/abc-mosh/bin/mosh-client
  530   500 ttys005  /usr/bin/ssh
  600     1 ??       /System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal
  610   600 ttys006  /opt/homebrew/bin/mosh-client
  700     1 ??       /Applications/Warp Preview.app/Contents/MacOS/preview
  710   700 ttys007  mosh-client
";
const ARGS: &str = "\
  511 mosh-client -# vm | 192.0.2.10 60001
  521 mosh-client -# other-host | 10.0.0.5 60002
  530 ssh user@192.0.2.10
  610 mosh-client -# user@vm | 192.0.2.10 60003
  710 mosh-client -# user@vm.example.ts.net | 198.18.0.7 60004
";

fn vm() -> HashSet<String> {
    ["192.0.2.10", "vm", "vm.example.ts.net"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[test]
fn candidates() {
    let (procs, args) = (agent::parse_ps(PS), agent::parse_args(ARGS));
    assert_eq!(
        procs[&700].2,
        "/Applications/Warp Preview.app/Contents/MacOS/preview"
    );
    // 521 goes elsewhere, 530 is ssh (not a herdr login), 610 is not in Warp.
    assert_eq!(
        agent::warp_candidates(&procs, &args, &vm()),
        vec![(511, "ttys003".to_string()), (710, "ttys007".to_string())]
    );
}

#[test]
fn warp_app_pattern() {
    assert!(agent::is_warp_app(
        "/Applications/Warp.app/Contents/MacOS/stable"
    ));
    assert!(agent::is_warp_app(
        "/Users/x/Applications/WarpPreview.app/Contents/MacOS/p"
    ));
    assert!(!agent::is_warp_app(
        "/Applications/Warp.app/Contents/stable"
    ));
    assert!(!agent::is_warp_app(
        "/Applications/Warp/x.app/Contents/MacOS/"
    ));
    assert!(!agent::is_warp_app("/Applications/Warpapp/Contents/MacOS/"));
    assert!(agent::is_tty_name("pts/12"));
    assert!(!agent::is_tty_name("ttys12345"));
    assert!(!agent::is_tty_name("??"));
}

#[test]
fn choose_newest_atime_owned_chardev() {
    let table = Mutex::new(vec![
        ("/dev/ttys003", (true, 1000u32, 10.0)),
        ("/dev/ttys007", (true, 1000, 20.0)),
        ("/dev/ttys008", (true, 0, 99.0)),
        ("/dev/ttys009", (false, 1000, 99.0)),
    ]);
    let fake = |p: &str| {
        table
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| *k == p)
            .map(|(_, v)| *v)
    };
    let cands: Vec<(i64, String)> = [
        (511, "ttys003"),
        (710, "ttys007"),
        (800, "ttys008"),
        (900, "ttys009"),
        (1, "ttys010"),
    ]
    .iter()
    .map(|(p, t)| (*p, t.to_string()))
    .collect();
    assert_eq!(
        agent::choose_tty(&cands, &fake, 1000).as_deref(),
        Some("/dev/ttys007")
    );
    table.lock().unwrap()[0].1.2 = 20.0;
    // Tie -> lowest pid.
    assert_eq!(
        agent::choose_tty(&cands, &fake, 1000).as_deref(),
        Some("/dev/ttys003")
    );
    assert_eq!(agent::choose_tty(&[], &fake, 1000), None);
}

#[test]
fn osc_cannot_be_broken_out_of() {
    let data = agent::osc777("t;i\x07tle", "b\x1b]0;evil\x07o\x1b\\dy;x\u{9c}");
    assert!(data.starts_with(b"\x1b]777;notify;"));
    assert!(data.ends_with(b"\x07"));
    let inner = &data[b"\x1b]777;notify;".len()..data.len() - 1];
    assert!(!inner.contains(&0x1b));
    assert!(!inner.contains(&0x07));
    assert!(!inner.windows(2).any(|w| w == "\u{9c}".as_bytes()));
    assert_eq!(inner.iter().filter(|&&b| b == b';').count(), 1);
}

#[test]
fn real_pty() {
    let (mut master, mut slave) = (0, 0);
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if rc != 0 {
        eprintln!("no pty, skipping");
        return;
    }
    let path = unsafe { std::ffi::CStr::from_ptr(libc::ttyname(slave)) }
        .to_string_lossy()
        .into_owned();
    // Raw mode, so the line discipline passes the bytes through unchanged.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        libc::tcgetattr(slave, &mut t);
        libc::cfmakeraw(&mut t);
        libc::tcsetattr(slave, libc::TCSANOW, &t);
    }
    let data = agent::osc777("✅ Claude finished", "my-project › Ситуация");
    assert_eq!(agent::write_tty(&path, &data), Ok(true));
    let mut m = unsafe { File::from_raw_fd(master) };
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while got.len() < data.len() {
        let n = m.read(&mut buf).unwrap();
        got.extend_from_slice(&buf[..n]);
    }
    assert_eq!(got, data);
    unsafe { libc::close(slave) };
}

#[test]
fn write_tty_errors_read_like_python() {
    let err = agent::write_tty("/nonexistent/ttys001", b"x").unwrap_err();
    assert_eq!(
        err,
        "[Errno 2] No such file or directory: '/nonexistent/ttys001'"
    );
}

// ---------------------------------------------------------------- logic

/// One write_tty call: (tty path, bytes).
type TtyWrite = (String, Vec<u8>);

struct Fake {
    now: Arc<Mutex<f64>>,
    mono: Arc<Mutex<f64>>,
    front: Arc<Mutex<Option<String>>>,
    tty: Arc<Mutex<Option<String>>>,
    writes: Arc<Mutex<Vec<TtyWrite>>>,
    focus: Arc<Mutex<Vec<(String, String)>>>,
    dir: std::path::PathBuf,
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "herdr-warp-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cfg(state_dir: &str) -> Config {
    Config {
        host: "127.0.0.1".into(),
        port: 22,
        user: "user".into(),
        key: "/nonexistent".into(),
        known_hosts: "/dev/null".into(),
        host_key_alias: "vm".into(),
        vm_addr: vm().into_iter().collect(),
        state_dir: state_dir.into(),
        ssh: "/usr/bin/ssh".into(),
        max_age: 900,
        jump_window: 600,
        warp_bundle_prefix: "dev.warp.".into(),
        focus_on_click: true,
    }
}

fn setup_with(tweak: impl FnOnce(&mut Config)) -> (Agent, Fake) {
    let dir = tmpdir("agent");
    let f = Fake {
        now: Arc::new(Mutex::new(NOW)),
        mono: Arc::new(Mutex::new(1000.0)),
        front: Arc::new(Mutex::new(Some("com.apple.Safari".into()))),
        tty: Arc::new(Mutex::new(Some("/dev/ttys003".into()))),
        writes: Arc::default(),
        focus: Arc::default(),
        dir: dir.clone(),
    };
    let (now, mono, front, tty, writes, focus) = (
        f.now.clone(),
        f.mono.clone(),
        f.front.clone(),
        f.tty.clone(),
        f.writes.clone(),
        f.focus.clone(),
    );
    let hooks = Hooks {
        find_tty: Arc::new(move || tty.lock().unwrap().clone()),
        front_bundle: Arc::new(move || front.lock().unwrap().clone()),
        write_tty: Arc::new(move |p, d| {
            writes.lock().unwrap().push((p.to_string(), d.to_vec()));
            Ok(true)
        }),
        clock: Arc::new(move || *now.lock().unwrap()),
        monotonic: Arc::new(move || *mono.lock().unwrap()),
        sleep: Arc::new(|_| {}),
        send_focus: Arc::new(move |p, t| focus.lock().unwrap().push((p.into(), t.into()))),
    };
    let mut c = cfg(dir.to_str().unwrap());
    tweak(&mut c);
    (Agent::new(c, hooks), f)
}

fn setup() -> (Agent, Fake) {
    setup_with(|_| {})
}

fn line(over: Value) -> Vec<u8> {
    let mut base = json!({"v": 1, "type": "event", "seq": 5, "ts": NOW as i64,
        "event": "needs_input", "agent": "claude", "ws": "w2", "tab": "w2:t1",
        "pane": "w2:p1", "term": "term_65ead8c4a4dc72", "ws_label": "my-project",
        "tab_label": "", "title": "fix it", "focused": false});
    for (k, v) in over.as_object().unwrap() {
        base[k] = v.clone();
    }
    serde_json::to_vec(&base).unwrap()
}

#[test]
fn notify_and_dedup() {
    let (mut a, f) = setup();
    a.handle(br#"{"v":1,"type":"hello","epoch":"0123456789abcdef","head":5,"ts":1}"#);
    a.handle(&line(json!({})));
    a.handle(&line(json!({}))); // same seq: dropped
    a.handle(&line(json!({"seq": 4}))); // older: dropped
    let w = f.writes.lock().unwrap();
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].0, "/dev/ttys003");
    let text = String::from_utf8_lossy(&w[0].1);
    assert!(text.contains("Claude needs you"));
    assert_eq!(
        text,
        "\x1b]777;notify;\u{1f514} Claude needs you;my-project › fix it\x07"
    );
    let st = std::fs::read_to_string(f.dir.join("state.json")).unwrap();
    // Byte-identical to Python's json.dump.
    assert_eq!(st, r#"{"epoch": "0123456789abcdef", "last_seq": 5}"#);
}

#[test]
fn state_survives_restart() {
    let (mut a, f) = setup();
    a.handle(br#"{"v":1,"type":"hello","epoch":"0123456789abcdef","head":5}"#);
    a.handle(&line(json!({"seq": 7})));
    let b = Agent::new(cfg(f.dir.to_str().unwrap()), a.hooks.clone());
    assert_eq!(b.epoch.as_deref(), Some("0123456789abcdef"));
    assert_eq!(b.last_seq, 7);
}

#[test]
fn epoch_change_resets() {
    let (mut a, f) = setup();
    a.handle(br#"{"v":1,"type":"hello","epoch":"0123456789abcdef","head":5}"#);
    a.handle(&line(json!({"seq": 9})));
    a.handle(br#"{"v":1,"type":"hello","epoch":"fedcba9876543210","head":1}"#);
    a.handle(&line(json!({"seq": 1})));
    assert_eq!(f.writes.lock().unwrap().len(), 2);
}

#[test]
fn stale_not_shown() {
    let (mut a, f) = setup();
    a.handle(&line(json!({"ts": NOW as i64 - 901})));
    assert!(f.writes.lock().unwrap().is_empty());
    assert_eq!(a.last_seq, 5);
}

#[test]
fn invalid_not_shown() {
    let (mut a, f) = setup();
    a.handle(&line(json!({"pane": "$(id)"})));
    a.handle(b"garbage");
    assert!(f.writes.lock().unwrap().is_empty());
}

#[test]
fn rate_limit_and_summary() {
    let (mut a, f) = setup();
    for seq in 1..=10 {
        a.handle(&line(json!({"seq": seq, "pane": format!("w1:p{seq}")})));
    }
    assert_eq!(f.writes.lock().unwrap().len(), 3);
    assert_eq!(a.suppressed, 7);
    *f.mono.lock().unwrap() += 61.0;
    a.maybe_summary();
    let w = f.writes.lock().unwrap();
    let last = String::from_utf8_lossy(&w.last().unwrap().1).into_owned();
    assert!(last.contains("7 more agent notifications"), "{last}");
}

#[test]
fn rate_limit_refills() {
    let (mut a, f) = setup();
    for seq in 1..=4 {
        a.handle(&line(json!({"seq": seq, "pane": format!("w1:p{seq}")})));
    }
    assert_eq!(f.writes.lock().unwrap().len(), 3);
    *f.mono.lock().unwrap() += 10.0; // 6/min: one token per 10 s
    a.handle(&line(json!({"seq": 5, "pane": "w1:p5"})));
    assert_eq!(f.writes.lock().unwrap().len(), 4);
}

#[test]
fn no_tty_drops() {
    let (mut a, f) = setup();
    *f.tty.lock().unwrap() = None;
    a.handle(&line(json!({})));
    assert!(a.jump_target().is_none());
    assert!(f.writes.lock().unwrap().is_empty());
}

#[test]
fn jump_latest_wins() {
    let (mut a, f) = setup();
    a.handle(&line(json!({"seq": 1, "pane": "w1:p1"})));
    a.handle(&line(
        json!({"seq": 2, "pane": "w3:p2", "term": "term_aaaaaaaa"}),
    ));
    assert!(!a.jump_once()); // Safari still in front
    *f.front.lock().unwrap() = Some("dev.warp.Warp-Stable".into());
    assert!(a.jump_once());
    assert_eq!(
        *f.focus.lock().unwrap(),
        vec![("w3:p2".to_string(), "term_aaaaaaaa".to_string())]
    );
    assert!(!a.jump_once()); // fires once
    assert_eq!(f.focus.lock().unwrap().len(), 1);
}

#[test]
fn jump_not_armed_when_warp_in_front() {
    let (mut a, f) = setup();
    *f.front.lock().unwrap() = Some("dev.warp.Warp-Stable".into());
    a.handle(&line(json!({})));
    assert!(a.jump_target().is_none());
}

#[test]
fn jump_armed_when_front_unknown() {
    // lsappinfo answered but gave no bundle id: Warp is not known to be in
    // front, so the jump is armed (Python: "" does not start with the prefix).
    let (mut a, f) = setup();
    *f.front.lock().unwrap() = Some(String::new());
    a.handle(&line(json!({})));
    assert!(a.jump_target().is_some());
    // ...and no lsappinfo at all: not armed.
    let (mut a, f) = setup();
    *f.front.lock().unwrap() = None;
    a.handle(&line(json!({})));
    assert!(a.jump_target().is_none());
}

#[test]
fn jump_expires() {
    let (mut a, f) = setup();
    a.handle(&line(json!({})));
    *f.mono.lock().unwrap() += 601.0;
    *f.front.lock().unwrap() = Some("dev.warp.Warp-Stable".into());
    assert!(!a.jump_once());
    assert!(f.focus.lock().unwrap().is_empty());
    assert!(a.jump_target().is_none());
}

#[test]
fn jump_flag_off() {
    let (mut a, _f) = setup_with(|c| c.focus_on_click = false);
    a.handle(&line(json!({})));
    assert!(a.jump_target().is_none());
}

#[test]
fn no_term_no_jump() {
    let (mut a, _f) = setup();
    a.handle(&line(json!({"term": ""})));
    assert!(a.jump_target().is_none());
}

#[test]
fn ssh_argv_fixed() {
    let (a, _f) = setup();
    let argv = a.ssh_argv("stream 5 0123456789abcdef");
    assert_eq!(argv[..3], ["/usr/bin/ssh", "-F", "/dev/null"]);
    assert_eq!(argv.last().unwrap(), "stream 5 0123456789abcdef");
    assert!(argv.iter().any(|a| a == "IdentitiesOnly=yes"));
    assert!(argv.iter().any(|a| a == "StrictHostKeyChecking=yes"));
    assert_eq!(argv[argv.len() - 2], "user@127.0.0.1");
    assert!(argv.iter().any(|a| a == "HostKeyAlias=vm"));
}

// ---------------------------------------------------------------- session

fn pipe() -> (File, File) {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

#[test]
fn session_reads_lines_and_ends_on_eof() {
    let (mut a, f) = setup();
    let (mut r, mut w) = pipe();
    let mut data = Vec::new();
    data.extend_from_slice(
        b"{\"v\":1,\"type\":\"hello\",\"epoch\":\"0123456789abcdef\",\"head\":1}\n",
    );
    data.extend_from_slice(&line(json!({"seq": 1})));
    // A line split across writes, then an over-long line that is skipped.
    data.extend_from_slice(b"\n{\"v\":1,\"type\":\"hb\",");
    w.write_all(&data).unwrap();
    w.write_all(b"\"head\":1}\n").unwrap();
    w.write_all(&vec![b'x'; 5000]).unwrap();
    w.write_all(b"\n").unwrap();
    w.write_all(&line(json!({"seq": 2, "pane": "w1:p9"})))
        .unwrap();
    w.write_all(b"\n").unwrap();
    drop(w);
    assert_eq!(a.read_session(&mut r), Ok("eof"));
    assert_eq!(a.last_seq, 2);
    assert_eq!(f.writes.lock().unwrap().len(), 2);
}

#[test]
fn session_flood() {
    let (mut a, _f) = setup();
    let (mut r, mut w) = pipe();
    let t = std::thread::spawn(move || {
        for _ in 0..301 {
            if w.write_all(b"garbage\n").is_err() {
                break;
            }
        }
    });
    assert_eq!(a.read_session(&mut r), Ok("flood"));
    drop(r);
    t.join().unwrap();
}

// ---------------------------------------------------------------- options

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn options() {
    let base = [
        "--host",
        "192.0.2.10",
        "--user",
        "user",
        "--key",
        "/k",
        "--known-hosts",
        "/kh",
        "--host-key-alias",
        "vm",
        "--state-dir",
        "/st",
        "--vm-addr",
        "192.0.2.10",
        "--vm-addr=vm",
    ];
    let c = agent::parse_config(&s(&base)).unwrap();
    assert_eq!(c.vm_addr, ["192.0.2.10", "vm"]);
    assert_eq!((c.port, c.max_age, c.jump_window), (22, 900, 600));
    assert!(c.focus_on_click);
    assert_eq!(c.ssh, "/usr/bin/ssh");
    let mut more = s(&base);
    more.extend(s(&[
        "--no-focus-on-click",
        "--port",
        "2222",
        "--max-age=60",
        "--jump",
        "5",
    ]));
    let c = agent::parse_config(&more).unwrap();
    assert!(!c.focus_on_click);
    assert_eq!((c.port, c.max_age, c.jump_window), (2222, 60, 5));

    let e = agent::parse_config(&s(&["--host", "x"])).unwrap_err();
    assert_eq!(e.1, 2);
    assert!(e.0.contains(
        "the following arguments are required: --user, --key, --known-hosts, \
         --host-key-alias, --vm-addr, --state-dir"
    ));
    let mut bad = s(&base);
    bad.extend(s(&["--port", "x"]));
    let e = agent::parse_config(&bad).unwrap_err();
    assert!(
        e.0.contains("argument --port: invalid int value: 'x'"),
        "{}",
        e.0
    );
    let mut bad = s(&base);
    bad.push("extra".into());
    assert!(
        agent::parse_config(&bad)
            .unwrap_err()
            .0
            .contains("unrecognized arguments: extra")
    );
    assert_eq!(agent::parse_config(&s(&["-h"])).unwrap_err().1, 0);
}
