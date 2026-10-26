//! `agent` (legacy name `herdr-notify-agent`): the Mac side of the notifier.
//! Streams agent events from the VM and shows them through Warp.
//!
//! One long-lived ssh session (dedicated key, forced command `serve` on the
//! VM) carries JSON lines. Every line is validated ([`wire::parse_line`]);
//! events become a Warp desktop notification by writing Warp's OSC 777
//! sequence
//!
//! ```text
//! ESC ] 777 ; notify ; <title> ; <body> BEL
//! ```
//!
//! to the tty of the Warp tab whose mosh-client is connected to the VM. Warp
//! then applies its own notification settings (sound, only while Warp is not
//! in front), and clicking the banner brings up that Warp tab.
//!
//! Warp gives no click callback, so click-to-pane is "latest wins": after a
//! notification posted while Warp was in the background, the next time Warp
//! becomes the frontmost app within the jump window (by clicking the banner,
//! or by Cmd-Tab) the agent asks the VM once to focus that herdr pane.
//!
//! Nothing from the VM reaches a shell or selects a program: ssh, ps and
//! lsappinfo run from fixed argv lists; VM strings only end up as cleaned
//! notification text and as the (re-validated) pane/terminal ids in the
//! focus request.
//!
//! The macOS-specific parts (`lsappinfo`, `/dev/ttysNNN`, Warp bundle ids)
//! are runtime lookups, so this compiles and its logic is tested on Linux;
//! there it simply finds no Warp tab.

use crate::proc::{self, RunError};
use crate::py;
use crate::wire::{self, Event, MAX_LINE_BYTES, Msg};
use argh::FromArgs;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const IDLE_TIMEOUT: f64 = 60.0; // 3 missed VM heartbeats
pub const WAKE_JUMP: f64 = 30.0; // wall clock ran ahead of monotonic: we slept
pub const SELECT_SECONDS: f64 = 5.0;
pub const STABLE_SECONDS: f64 = 60.0; // a session this old resets the backoff
pub const BACKOFF_MAX: f64 = 60.0;
pub const FLOOD_BACKOFF: f64 = 300.0;
pub const FLOOD_LINES: u64 = 300; // non-heartbeat lines per minute
pub const FLOOD_BYTES: u64 = 64 * 1024; // per minute
pub const RATE_PER_MIN: f64 = 6.0;
pub const RATE_BURST: f64 = 3.0;
pub const SUMMARY_EVERY: f64 = 60.0;
pub const JUMP_POLL: f64 = 0.3;
pub const FOCUS_TIMEOUT: Duration = Duration::from_secs(10);

pub fn log(msg: &str) {
    let mut e = io::stderr().lock();
    let _ = writeln!(e, "{} {}", proc::local_timestamp(), msg);
    let _ = e.flush();
}

// ---------------------------------------------------------------- ttys / OSC

/// `ps -axo pid=,ppid=,tty=,comm=` -> {pid: (ppid, tty, comm)}.
/// comm is last because it may contain spaces.
pub fn parse_ps(text: &str) -> BTreeMap<i64, (i64, String, String)> {
    let mut procs = BTreeMap::new();
    for line in text.lines() {
        let parts = py::split_ws_max(line, 3);
        if parts.len() != 4 {
            continue;
        }
        let (Ok(pid), Ok(ppid)) = (parts[0].parse(), parts[1].parse()) else {
            continue;
        };
        let comm = parts[3].trim_matches(py::is_space);
        procs.insert(pid, (ppid, parts[2].to_string(), comm.to_string()));
    }
    procs
}

/// `ps -axww -o pid=,args=` -> {pid: [argv tokens]} (split on spaces).
pub fn parse_args(text: &str) -> BTreeMap<i64, Vec<String>> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let parts = py::split_ws_max(line, 1);
        if parts.len() != 2 {
            continue;
        }
        if let Ok(pid) = parts[0].parse() {
            out.insert(pid, py::split_ws(parts[1]).map(str::to_string).collect());
        }
    }
    out
}

fn targets_vm(tokens: &[String], vm_addrs: &HashSet<String>) -> bool {
    tokens
        .iter()
        .skip(1)
        .any(|tok| vm_addrs.contains(tok.rsplit('@').next().unwrap_or(tok)))
}

/// `/Warp[^/]*\.app/Contents/MacOS/` somewhere in `comm`:
/// /Applications/Warp.app/Contents/MacOS/stable, Warp Preview.app, a copy in
/// ~/Applications, ...
pub fn is_warp_app(comm: &str) -> bool {
    comm.match_indices("/Warp").any(|(i, _)| {
        let seg_start = i + 1;
        let seg_end = comm[seg_start..]
            .find('/')
            .map(|j| seg_start + j)
            .unwrap_or(comm.len());
        let seg = &comm[seg_start..seg_end];
        seg.len() >= "Warp.app".len()
            && seg.ends_with(".app")
            && comm[seg_end..].starts_with("/Contents/MacOS/")
    })
}

pub fn under_warp(mut pid: i64, procs: &BTreeMap<i64, (i64, String, String)>) -> bool {
    let mut seen = HashSet::new();
    for _ in 0..64 {
        if !seen.insert(pid) {
            return false;
        }
        let Some((ppid, _tty, comm)) = procs.get(&pid) else {
            return false;
        };
        if is_warp_app(comm) {
            return true;
        }
        if *ppid <= 1 {
            return false;
        }
        pid = *ppid;
    }
    false
}

/// `(ttys[0-9]{1,4}|pts/[0-9]{1,4})`, whole string.
pub fn is_tty_name(tty: &str) -> bool {
    let digits = |d: &str| (1..=4).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit());
    tty.strip_prefix("ttys").is_some_and(digits) || tty.strip_prefix("pts/").is_some_and(digits)
}

/// [(pid, tty)] of VM clients (mosh-client: on the VM only mosh logins attach
/// to herdr) running in Warp.
pub fn warp_candidates(
    procs: &BTreeMap<i64, (i64, String, String)>,
    args: &BTreeMap<i64, Vec<String>>,
    vm_addrs: &HashSet<String>,
) -> Vec<(i64, String)> {
    let mut found = Vec::new();
    for (&pid, (_ppid, tty, comm)) in procs {
        let base = comm.rsplit('/').next().unwrap_or(comm);
        if base != "mosh-client" || !is_tty_name(tty) {
            continue;
        }
        if !targets_vm(args.get(&pid).map(Vec::as_slice).unwrap_or(&[]), vm_addrs) {
            continue;
        }
        if under_warp(pid, procs) {
            found.push((pid, tty.clone()));
        }
    }
    found
}

/// What choose_tty needs from stat(): (is a char device, owner uid, atime).
pub type TtyStat = (bool, u32, f64);

pub fn stat_tty(path: &str) -> Option<TtyStat> {
    let m = fs::metadata(path).ok()?;
    Some((
        m.file_type().is_char_device(),
        m.uid(),
        m.atime() as f64 + m.atime_nsec() as f64 / 1e9,
    ))
}

/// The candidate tty most recently typed into (newest atime, like `w`'s idle
/// time); ties go to the lowest pid. Only character devices we own.
pub fn choose_tty(
    candidates: &[(i64, String)],
    stat_fn: &dyn Fn(&str) -> Option<TtyStat>,
    uid: u32,
) -> Option<String> {
    let mut best: Option<((f64, i64), String)> = None;
    for (pid, tty) in candidates {
        let path = format!("/dev/{tty}");
        let Some((chr, owner, atime)) = stat_fn(&path) else {
            continue;
        };
        if !chr || owner != uid {
            continue;
        }
        let key = (-atime, *pid);
        if best.as_ref().is_none_or(|(k, _)| key < *k) {
            best = Some((key, path));
        }
    }
    best.map(|(_, p)| p)
}

/// Warp splits the payload on ';' and ends it at BEL/ST: no controls, no
/// semicolons, one line.
pub fn osc_safe(text: &str) -> String {
    wire::clean_str(text, 200).replace(';', ",")
}

pub fn osc777(title: &str, body: &str) -> Vec<u8> {
    format!("\x1b]777;notify;{};{}\x07", osc_safe(title), osc_safe(body)).into_bytes()
}

/// One write() of the whole sequence, so it lands between mosh-client's own
/// frame writes rather than inside one (each is a single write too). Ok(true)
/// when every byte went out; Err carries Python's OSError text.
pub fn write_tty(path: &str, data: &[u8]) -> Result<bool, String> {
    let f = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| py::os_error(&e, Some(path)))?;
    let n = unsafe { libc::write(f.as_raw_fd(), data.as_ptr().cast(), data.len()) };
    if n < 0 {
        return Err(py::os_error(&io::Error::last_os_error(), None));
    }
    Ok(n as usize == data.len())
}

/// stdout of `argv` (whatever its exit status), None if it could not run.
pub fn run_text(argv: &[&str], timeout: Duration) -> Option<String> {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    proc::run(&argv, timeout)
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Bundle id of the frontmost app ("" if unknown), None without lsappinfo.
pub fn front_bundle() -> Option<String> {
    front_bundle_with("/usr/bin/lsappinfo")
}

/// [`front_bundle`] with the lsappinfo binary given.
pub fn front_bundle_with(lsappinfo: &str) -> Option<String> {
    let t = Duration::from_secs(3);
    let asn = run_text(&[lsappinfo, "front"], t)?;
    let info = run_text(
        &[
            lsappinfo,
            "info",
            "-only",
            "bundleid",
            asn.trim_matches(py::is_space),
        ],
        t,
    );
    let Some(info) = info.filter(|i| i.contains('=')) else {
        return Some(String::new());
    };
    // "CFBundleIdentifier"="dev.warp.Warp-Stable"
    let last = info.rsplit('=').next().unwrap_or("");
    Some(
        last.trim_matches(py::is_space)
            .trim_matches('"')
            .to_string(),
    )
}

// ---------------------------------------------------------------- config

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: i64,
    pub user: String,
    pub key: String,
    pub known_hosts: String,
    pub host_key_alias: String,
    pub vm_addr: Vec<String>,
    pub state_dir: String,
    pub ssh: String,
    pub max_age: i64,
    pub jump_window: i64,
    pub warp_bundle_prefix: String,
    pub focus_on_click: bool,
}

/// Mac side of herdr-notify: stream agent events from the VM, show them via
/// Warp.
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "agent")]
pub struct Args {
    /// the VM's address
    #[argh(option)]
    pub host: String,
    /// ssh port
    #[argh(option, default = "22")]
    pub port: i64,
    /// user on the VM
    #[argh(option)]
    pub user: String,
    /// private key of the dedicated ssh key
    #[argh(option)]
    pub key: String,
    /// known_hosts file pinning the VM's host key
    #[argh(option)]
    pub known_hosts: String,
    /// host name the pinned key is listed under
    #[argh(option)]
    pub host_key_alias: String,
    /// host spelling a mosh-client to the VM may carry (repeatable, at
    /// least one)
    #[argh(option)]
    pub vm_addr: Vec<String>,
    /// where state.json lives
    #[argh(option)]
    pub state_dir: String,
    /// the ssh binary
    #[argh(option, default = "String::from(\"/usr/bin/ssh\")")]
    pub ssh: String,
    /// events older than this many seconds are not shown
    #[argh(option, default = "900")]
    pub max_age: i64,
    /// seconds after a notification during which bringing Warp to the front
    /// focuses the agent's pane
    #[argh(option, default = "600")]
    pub jump_window: i64,
    /// bundle id prefix that identifies Warp
    #[argh(option, default = "String::from(\"dev.warp.\")")]
    pub warp_bundle_prefix: String,
    /// do not focus the pane when Warp comes to the front
    #[argh(switch)]
    pub no_focus_on_click: bool,
}

/// A parse failure: (message, exit code). Exit 0 is `--help`.
#[derive(Debug)]
pub struct ArgError(pub String, pub i32);

impl TryFrom<Args> for Config {
    type Error = ArgError;

    fn try_from(a: Args) -> Result<Config, ArgError> {
        if a.vm_addr.is_empty() {
            return Err(ArgError(
                "Required options not provided:\n    --vm-addr".into(),
                2,
            ));
        }
        Ok(Config {
            host: a.host,
            port: a.port,
            user: a.user,
            key: a.key,
            known_hosts: a.known_hosts,
            host_key_alias: a.host_key_alias,
            vm_addr: a.vm_addr,
            state_dir: a.state_dir,
            ssh: a.ssh,
            max_age: a.max_age,
            jump_window: a.jump_window,
            warp_bundle_prefix: a.warp_bundle_prefix,
            focus_on_click: !a.no_focus_on_click,
        })
    }
}

/// Parse the agent's options as `name` would get them (argh; exit code 2 on
/// a bad command line, like argparse).
pub fn parse_config_as(name: &str, argv: &[String]) -> Result<Config, ArgError> {
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    match Args::from_args(&[name], &argv) {
        Ok(a) => Config::try_from(a),
        Err(e) => Err(ArgError(e.output, if e.status.is_ok() { 0 } else { 2 })),
    }
}

pub fn parse_config(argv: &[String]) -> Result<Config, ArgError> {
    parse_config_as("herdr-notify-agent", argv)
}

/// The fixed ssh command line; only `remote_cmd` varies.
pub fn ssh_argv(c: &Config, remote_cmd: &str) -> Vec<String> {
    let mut v: Vec<String> = [
        c.ssh.as_str(),
        "-F",
        "/dev/null",
        "-T",
        "-n",
        "-i",
        &c.key,
        "-p",
        &c.port.to_string(),
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "IdentityAgent=none",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    v.push("-o".into());
    v.push(format!("UserKnownHostsFile={}", c.known_hosts));
    v.push("-o".into());
    v.push(format!("HostKeyAlias={}", c.host_key_alias));
    for o in [
        "CheckHostIP=no",
        "UpdateHostKeys=no",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=3",
        "ConnectTimeout=10",
        "ClearAllForwardings=yes",
        "ForwardAgent=no",
        "PermitLocalCommand=no",
        "ControlMaster=no",
        "ControlPath=none",
        "LogLevel=ERROR",
    ] {
        v.push("-o".into());
        v.push(o.into());
    }
    v.push(format!("{}@{}", c.user, c.host));
    v.push(remote_cmd.into());
    v
}

// ---------------------------------------------------------------- agent

type Shared<T> = Arc<dyn Fn() -> T + Send + Sync>;
type WriteTty = Arc<dyn Fn(&str, &[u8]) -> Result<bool, String> + Send + Sync>;
type SendFocus = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Everything the agent does to the outside world (tests replace these).
#[derive(Clone)]
pub struct Hooks {
    pub find_tty: Shared<Option<String>>,
    pub front_bundle: Shared<Option<String>>,
    pub write_tty: WriteTty,
    pub clock: Shared<f64>,
    pub monotonic: Shared<f64>,
    pub sleep: Arc<dyn Fn(f64) + Send + Sync>,
    pub send_focus: SendFocus,
}

/// The real tty lookup: ps, then the Warp ancestry and atime rules.
pub fn find_tty(cfg: &Config) -> Option<String> {
    let t = Duration::from_secs(3);
    let procs =
        parse_ps(&run_text(&["/bin/ps", "-axo", "pid=,ppid=,tty=,comm="], t).unwrap_or_default());
    let args =
        parse_args(&run_text(&["/bin/ps", "-axww", "-o", "pid=,args="], t).unwrap_or_default());
    let vm: HashSet<String> = cfg.vm_addr.iter().cloned().collect();
    let uid = unsafe { libc::getuid() };
    choose_tty(&warp_candidates(&procs, &args, &vm), &stat_tty, uid)
}

/// Python's `returncode`: the exit status, or minus the signal.
pub fn py_exit_code(st: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    st.code().or_else(|| st.signal().map(|s| -s)).unwrap_or(-1)
}

/// Ask the VM to focus `pane` (if it still hosts `term`) over the same key.
pub fn send_focus(cfg: &Config, pane: &str, term: &str) {
    if let Some(line) = focus_request(cfg, pane, term) {
        log(&line);
    }
}

/// The focus request proper: the log line it produces, or None when the ids
/// do not validate (nothing is run then).
pub fn focus_request(cfg: &Config, pane: &str, term: &str) -> Option<String> {
    // Re-validate: these came off the wire, even if parse_line passed them.
    if !(wire::is_id(pane) && wire::is_term(term)) {
        return None;
    }
    let argv = ssh_argv(cfg, &format!("focus {pane} {term}"));
    let res = match proc::run(&argv, FOCUS_TIMEOUT) {
        Ok(r) => r,
        Err(RunError::Spawn(e)) => {
            return Some(format!(
                "focus {pane}: {}",
                py::os_error(&e, Some(&cfg.ssh))
            ));
        }
        Err(RunError::Timeout) => {
            let list = serde_json::Value::from(argv.clone());
            return Some(format!(
                "focus {pane}: Command '{}' timed out after {:.1} seconds",
                py::repr_value(&list),
                FOCUS_TIMEOUT.as_secs_f64()
            ));
        }
    };
    let line = res.stdout.split(|&b| b == b'\n').next().unwrap_or_default();
    Some(match wire::parse_line(line, crate::vm::now()) {
        Ok(m) => format!("focus {pane}: {}", m.py_repr()),
        Err(e) => format!("focus {pane}: exit {}, {e}", py_exit_code(res.status)),
    })
}

impl Hooks {
    pub fn real(cfg: &Arc<Config>) -> Hooks {
        let base = Instant::now();
        let c1 = cfg.clone();
        let c2 = cfg.clone();
        Hooks {
            find_tty: Arc::new(move || find_tty(&c1)),
            front_bundle: Arc::new(front_bundle),
            write_tty: Arc::new(write_tty),
            clock: Arc::new(crate::vm::now),
            monotonic: Arc::new(move || base.elapsed().as_secs_f64()),
            sleep: Arc::new(|s| std::thread::sleep(Duration::from_secs_f64(s.max(0.0)))),
            send_focus: Arc::new(move |p, t| send_focus(&c2, p, t)),
        }
    }
}

pub struct RateLimiter {
    rate: f64,
    burst: f64,
    tokens: f64,
    clock: Shared<f64>,
    last: f64,
}

impl RateLimiter {
    pub fn new(per_min: f64, burst: f64, clock: Shared<f64>) -> Self {
        let last = clock();
        RateLimiter {
            rate: per_min / 60.0,
            burst,
            tokens: burst,
            clock,
            last,
        }
    }

    pub fn allow(&mut self) -> bool {
        let now = (self.clock)();
        self.tokens = self.burst.min(self.tokens + (now - self.last) * self.rate);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Latest-wins jump target: (pane, term, deadline).
pub type Target = (String, String, f64);

#[derive(Default)]
pub struct Jump {
    pub target: Mutex<Option<Target>>,
    cv: Condvar,
}

impl Jump {
    fn get(&self) -> Option<Target> {
        self.target.lock().map(|g| g.clone()).unwrap_or(None)
    }
}

/// One poll of the armed target; true if a focus request was sent.
pub fn jump_once(cfg: &Config, hooks: &Hooks, jump: &Jump) -> bool {
    let Some(target) = jump.get() else {
        return false;
    };
    let (pane, term, deadline) = target.clone();
    if (hooks.monotonic)() > deadline {
        if let Ok(mut g) = jump.target.lock()
            && g.as_ref() == Some(&target)
        {
            *g = None;
        }
        return false;
    }
    let front = (hooks.front_bundle)();
    if !front.is_some_and(|f| !f.is_empty() && f.starts_with(&cfg.warp_bundle_prefix)) {
        return false;
    }
    {
        let Ok(mut g) = jump.target.lock() else {
            return false;
        };
        if g.as_ref() != Some(&target) {
            return false;
        }
        *g = None;
    }
    (hooks.send_focus)(&pane, &term);
    true
}

fn jump_loop(cfg: Arc<Config>, hooks: Hooks, jump: Arc<Jump>) {
    loop {
        {
            let Ok(mut g) = jump.target.lock() else {
                return;
            };
            while g.is_none() {
                g = match jump.cv.wait(g) {
                    Ok(g) => g,
                    Err(_) => return,
                };
            }
        }
        jump_once(&cfg, &hooks, &jump);
        (hooks.sleep)(JUMP_POLL);
    }
}

/// Jitter for the reconnect delay (xorshift64; no crate for this).
pub struct Rng(u64);

impl Default for Rng {
    fn default() -> Rng {
        Rng::new()
    }
}

impl Rng {
    pub fn new() -> Rng {
        let mut b = [0u8; 8];
        let seed = fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut b))
            .map(|_| u64::from_le_bytes(b))
            .unwrap_or_else(|_| crate::vm::now().to_bits() ^ u64::from(std::process::id()));
        Rng::seeded(seed)
    }

    pub fn seeded(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    /// random.uniform(a, b)
    pub fn uniform(&mut self, a: f64, b: f64) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        a + (b - a) * ((self.0 >> 11) as f64 / (1u64 << 53) as f64)
    }
}

pub struct Agent {
    pub cfg: Arc<Config>,
    pub hooks: Hooks,
    pub state_path: PathBuf,
    pub epoch: Option<String>,
    pub last_seq: i64,
    pub limiter: RateLimiter,
    pub suppressed: u64,
    pub last_summary: f64,
    pub jump: Arc<Jump>,
    /// How long one poll of the ssh pipe waits (tests shorten it).
    pub select_seconds: f64,
}

impl Agent {
    pub fn new(cfg: Config, hooks: Hooks) -> Agent {
        let cfg = Arc::new(cfg);
        let limiter = RateLimiter::new(RATE_PER_MIN, RATE_BURST, hooks.monotonic.clone());
        let mut a = Agent {
            state_path: Path::new(&cfg.state_dir).join("state.json"),
            cfg,
            hooks,
            epoch: None,
            last_seq: 0,
            limiter,
            suppressed: 0,
            last_summary: 0.0,
            jump: Arc::new(Jump::default()),
            select_seconds: SELECT_SECONDS,
        };
        a.load_state();
        a
    }

    // -- state
    pub fn load_state(&mut self) {
        let mut buf = Vec::new();
        let Ok(f) = fs::File::open(&self.state_path) else {
            return;
        };
        if f.take(4096).read_to_end(&mut buf).is_err() {
            return;
        }
        let Ok(serde_json::Value::Object(st)) = serde_json::from_slice(&buf) else {
            return;
        };
        if let Some(e) = st.get("epoch").and_then(|v| v.as_str())
            && wire::is_epoch(e)
        {
            self.epoch = Some(e.to_string());
        }
        if let Some(n) = py::as_int(st.get("last_seq"))
            && (0..=i128::from(wire::MAX_SEQ)).contains(&n)
        {
            self.last_seq = n as i64;
        }
    }

    pub fn save_state(&self) {
        let dir = Path::new(&self.cfg.state_dir);
        if !dir.is_dir() {
            if let Some(p) = dir.parent() {
                let _ = fs::create_dir_all(p);
            }
            let _ = fs::DirBuilder::new().mode(0o700).create(dir);
        }
        let epoch = match &self.epoch {
            Some(e) => format!("\"{e}\""),
            None => "null".into(),
        };
        let data = format!("{{\"epoch\": {epoch}, \"last_seq\": {}}}", self.last_seq);
        let tmp = PathBuf::from(format!("{}.tmp", self.state_path.display()));
        let ok = fs::write(&tmp, data).and_then(|_| fs::rename(&tmp, &self.state_path));
        if let Err(e) = ok {
            log(&format!("cannot save state: {e}"));
        }
    }

    // -- delivery
    pub fn deliver(&self, title: &str, body: &str) -> bool {
        let Some(path) = (self.hooks.find_tty)() else {
            // No Warp tab is connected to the VM: there is nowhere a click
            // could land, and we would only be guessing which terminal to
            // use. Drop.
            log("drop: no Warp tab with a mosh-client to the VM");
            return false;
        };
        match (self.hooks.write_tty)(&path, &osc777(title, body)) {
            Err(e) => {
                log(&format!("drop: write {path} failed: {e}"));
                false
            }
            Ok(false) => {
                log(&format!("partial write to {path}"));
                false
            }
            Ok(true) => true,
        }
    }

    pub fn notify(&self, ev: &Event) -> bool {
        let (title, body) = wire::compose(ev);
        let front = if self.cfg.focus_on_click {
            (self.hooks.front_bundle)()
        } else {
            None
        };
        if !self.deliver(&title, &body) {
            return false;
        }
        log(&format!("notified seq={} {} {}", ev.seq, ev.event, ev.pane));
        // Warp only shows the banner while it is not in front; nothing to
        // click, nothing to arm otherwise.
        if self.cfg.focus_on_click
            && !ev.term.is_empty()
            && front.is_some_and(|f| !f.starts_with(&self.cfg.warp_bundle_prefix))
        {
            self.arm(&ev.pane, &ev.term);
        }
        true
    }

    pub fn maybe_summary(&mut self) {
        if self.suppressed > 0 && (self.hooks.monotonic)() - self.last_summary >= SUMMARY_EVERY {
            let n = self.suppressed;
            self.suppressed = 0;
            self.last_summary = (self.hooks.monotonic)();
            self.deliver("herdr", &format!("{n} more agent notifications held back"));
        }
    }

    // -- lines
    pub fn handle(&mut self, raw: &[u8]) -> Option<Msg> {
        let msg = match wire::parse_line(raw, (self.hooks.clock)()) {
            Ok(m) => m,
            Err(e) => {
                log(&format!("drop line: {e}"));
                return None;
            }
        };
        match &msg {
            Msg::Hello { epoch, .. } => {
                if self.epoch.as_deref() != Some(epoch) {
                    if self.epoch.is_some() {
                        log("spool epoch changed, resetting seq");
                    }
                    self.epoch = Some(epoch.clone());
                    self.last_seq = 0;
                    self.save_state();
                }
            }
            Msg::Event(ev) => {
                if ev.seq <= self.last_seq {
                    return None;
                }
                self.last_seq = ev.seq;
                self.save_state();
                let age = (self.hooks.clock)() - ev.ts as f64;
                if age > self.cfg.max_age as f64 {
                    log(&format!(
                        "stale seq={} ({}s old), not shown",
                        ev.seq, age as i64
                    ));
                    return Some(msg);
                }
                if !self.limiter.allow() {
                    self.suppressed += 1;
                    return Some(msg);
                }
                self.notify(ev);
            }
            Msg::Error { .. } | Msg::Focus { .. } => {
                log(&format!("vm says: {}", msg.py_repr()));
            }
            Msg::Hb { .. } => {}
        }
        Some(msg)
    }

    // -- latest-wins jump
    pub fn arm(&self, pane: &str, term: &str) {
        if let Ok(mut g) = self.jump.target.lock() {
            *g = Some((
                pane.to_string(),
                term.to_string(),
                (self.hooks.monotonic)() + self.cfg.jump_window as f64,
            ));
            self.jump.cv.notify_one();
        }
    }

    pub fn jump_target(&self) -> Option<Target> {
        self.jump.get()
    }

    pub fn jump_once(&self) -> bool {
        jump_once(&self.cfg, &self.hooks, &self.jump)
    }

    pub fn ssh_argv(&self, remote_cmd: &str) -> Vec<String> {
        ssh_argv(&self.cfg, remote_cmd)
    }

    // -- session
    /// Run one ssh session to the end. Returns why it ended: "eof", "idle",
    /// "wake" or "flood". Err when ssh could not be started or read.
    pub fn session(&mut self) -> Result<&'static str, String> {
        let mut remote = format!("stream {}", self.last_seq);
        if let Some(e) = &self.epoch
            && !e.is_empty()
        {
            remote.push(' ');
            remote.push_str(e);
        }
        let argv = self.ssh_argv(&remote);
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| py::os_error(&e, Some(&argv[0])))?;
        let mut stdout = child.stdout.take().ok_or("no stdout")?;
        let res = self.read_session(&mut stdout);
        if let Ok(None) = child.try_wait() {
            let _ = child.kill();
        }
        let _ = child.wait();
        res
    }

    /// The read loop of [`Agent::session`], on any readable fd (a pipe in
    /// tests).
    pub fn read_session<R: Read + AsRawFd>(
        &mut self,
        stdout: &mut R,
    ) -> Result<&'static str, String> {
        let fd = stdout.as_raw_fd();
        let mut buf: Vec<u8> = Vec::new();
        let mut skipping = false;
        let mut last_data = (self.hooks.monotonic)();
        let (mut wall, mut mono) = ((self.hooks.clock)(), (self.hooks.monotonic)());
        let (mut window_start, mut lines, mut nbytes) = (mono, 0u64, 0u64);
        let mut chunk = [0u8; 4096];
        loop {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, (self.select_seconds * 1000.0) as i32) };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(py::os_error(&e, None));
                }
            }
            let ready = rc > 0;
            let (now_wall, now_mono) = ((self.hooks.clock)(), (self.hooks.monotonic)());
            if (now_wall - wall) - (now_mono - mono) > WAKE_JUMP {
                return Ok("wake");
            }
            (wall, mono) = (now_wall, now_mono);
            if now_mono - window_start >= 60.0 {
                (window_start, lines, nbytes) = (now_mono, 0, 0);
            }
            self.maybe_summary();
            if !ready {
                if now_mono - last_data > IDLE_TIMEOUT {
                    return Ok("idle");
                }
                continue;
            }
            let n = stdout
                .read(&mut chunk)
                .map_err(|e| py::os_error(&e, None))?;
            if n == 0 {
                return Ok("eof");
            }
            last_data = now_mono;
            buf.extend_from_slice(&chunk[..n]);
            while let Some(i) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=i).take(i).collect();
                if skipping {
                    skipping = false;
                    continue;
                }
                let msg = self.handle(&line);
                if !matches!(msg, Some(Msg::Hb { .. })) {
                    lines += 1;
                    nbytes += line.len() as u64;
                }
            }
            if buf.len() > MAX_LINE_BYTES {
                log("drop line: too long");
                buf.clear();
                skipping = true;
            }
            if lines > FLOOD_LINES || nbytes > FLOOD_BYTES {
                return Ok("flood");
            }
        }
    }

    pub fn run(&mut self) -> i32 {
        if !py::exists(&self.cfg.key) {
            log(&format!(
                "no key at {}; generate it (see README) -- sleeping",
                self.cfg.key
            ));
            (self.hooks.sleep)(300.0);
            return 0;
        }
        if self.cfg.focus_on_click {
            let (c, h, j) = (self.cfg.clone(), self.hooks.clone(), self.jump.clone());
            std::thread::spawn(move || jump_loop(c, h, j));
        }
        let mut rng = Rng::new();
        let mut backoff: f64 = 1.0;
        loop {
            let started = (self.hooks.monotonic)();
            let reason = match self.session() {
                Ok(r) => r,
                Err(e) => {
                    log(&format!("ssh failed to start: {e}"));
                    "eof"
                }
            };
            let lived = (self.hooks.monotonic)() - started;
            let delay = match reason {
                "flood" => {
                    log(&format!(
                        "VM sent too much, backing off {}s",
                        FLOOD_BACKOFF as i64
                    ));
                    FLOOD_BACKOFF
                }
                "wake" => {
                    log("woke from sleep, reconnecting");
                    backoff = 1.0;
                    1.0
                }
                _ => {
                    backoff = if lived >= STABLE_SECONDS {
                        1.0
                    } else {
                        (backoff * 2.0).min(BACKOFF_MAX)
                    };
                    log(&format!(
                        "session ended ({reason} after {}s), retry in {}s",
                        lived as i64, backoff as i64
                    ));
                    backoff
                }
            };
            (self.hooks.sleep)(delay * rng.uniform(0.8, 1.2));
        }
    }
}

/// Report a bad command line (stdout for `--help`) and give its exit code.
pub fn arg_error(e: ArgError) -> i32 {
    if e.1 == 0 {
        println!("{}", e.0);
    } else {
        eprintln!("{}", e.0);
    }
    e.1
}

/// Run the agent; returns only when it gives up (no key yet).
pub fn main(args: Args) -> i32 {
    let cfg = match Config::try_from(args) {
        Ok(c) => c,
        Err(e) => return arg_error(e),
    };
    let cfg_arc = Arc::new(cfg.clone());
    let hooks = Hooks::real(&cfg_arc);
    Agent::new(cfg, hooks).run()
}
