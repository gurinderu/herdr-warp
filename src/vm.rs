//! VM-side pieces shared by `hook` and `serve`: the spool and herdr's socket.
//!
//! Spool layout, under `$XDG_STATE_HOME/herdr-notify` (0700). Not under /run:
//! with lingering off, /run/user/<uid> disappears without a login session.
//!
//! ```text
//! spool.jsonl  one wire event per line (0600), rewritten atomically
//! meta.json    {"epoch": "<16 hex>", "next_seq": N}
//! lock         flock target; writers exclusive, readers shared
//! focus.last   timestamp of the last focus request (rate limit)
//! ```
//!
//! Every write drops entries older than TTL_SECONDS, keeps one entry per pane
//! (the newest), and caps the file at MAX_ENTRIES / MAX_BYTES.

use crate::py;
use crate::wire::{self, MAX_SEQ, TTL_SECONDS};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_ENTRIES: usize = 64;
pub const MAX_BYTES: usize = 32 * 1024;
pub const SOCKET_TIMEOUT: Duration = Duration::from_secs(2);
pub const MAX_REPLY_BYTES: usize = 16 * 1024 * 1024;

pub type Env = HashMap<String, String>;

/// The process environment as a map (non-UTF-8 entries converted lossily).
pub fn os_env() -> Env {
    std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

fn nonempty<'a>(env: &'a Env, key: &str) -> Option<&'a str> {
    env.get(key).map(String::as_str).filter(|s| !s.is_empty())
}

fn home(env: &Env) -> String {
    py::home_dir(env.get("HOME").map(String::as_str))
}

/// `$XDG_STATE_HOME/herdr-notify`, falling back to ~/.local/state.
pub fn state_dir(env: &Env) -> PathBuf {
    let base = nonempty(env, "XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&home(env)).join(".local").join("state"));
    base.join("herdr-notify")
}

/// The default herdr session's socket. Under ~/.config, not /run, so the
/// forced command reaches it without a login session.
pub fn default_socket(env: &Env) -> PathBuf {
    let base = nonempty(env, "XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&home(env)).join(".config"));
    base.join("herdr").join("herdr.sock")
}

/// A function that makes one herdr socket request (tests swap it out).
pub type Call<'a> = &'a dyn Fn(&Path, &str, Value) -> Option<Value>;

/// Python's `json.dumps` with default separators and ensure_ascii.
fn py_json(v: &Value) -> String {
    fn s(out: &mut String, text: &str) {
        out.push('"');
        for c in text.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\x08' => out.push_str("\\b"),
                '\x0c' => out.push_str("\\f"),
                c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                    let mut buf = [0u16; 2];
                    for u in c.encode_utf16(&mut buf) {
                        out.push_str(&format!("\\u{:04x}", u));
                    }
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }
    fn go(out: &mut String, v: &Value) {
        match v {
            Value::String(t) => s(out, t),
            Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    go(out, x);
                }
                out.push(']');
            }
            Value::Object(o) => {
                out.push('{');
                for (i, (k, x)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    s(out, k);
                    out.push_str(": ");
                    go(out, x);
                }
                out.push('}');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    go(&mut out, v);
    out
}

/// The request line herdr_call sends.
pub fn request_line(method: &str, params: &Value) -> String {
    format!(
        "{{\"id\": \"herdr-notify\", \"method\": {}, \"params\": {}}}\n",
        py_json(&Value::String(method.into())),
        py_json(params)
    )
}

/// One request over herdr's socket API; the decoded reply, or None.
pub fn herdr_call(path: &Path, method: &str, params: Value) -> Option<Value> {
    let mut sock = UnixStream::connect(path).ok()?;
    sock.set_read_timeout(Some(SOCKET_TIMEOUT)).ok()?;
    sock.set_write_timeout(Some(SOCKET_TIMEOUT)).ok()?;
    sock.write_all(request_line(method, &params).as_bytes())
        .ok()?;
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 65536];
    while !buf.contains(&b'\n') && buf.len() < MAX_REPLY_BYTES {
        let n = sock.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    if buf.is_empty() {
        return None;
    }
    let line = buf.split(|&b| b == b'\n').next().unwrap_or_default();
    serde_json::from_slice(line).ok()
}

/// 8 random bytes as 16 hex digits.
pub fn random_epoch() -> io::Result<String> {
    let mut b = [0u8; 8];
    File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Python's bytes.splitlines(): \n, \r\n and \r.
fn splitlines(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            b'\n' => {
                out.push(&data[start..i]);
                start = i + 1;
            }
            b'\r' => {
                out.push(&data[start..i]);
                if data.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < data.len() {
        out.push(&data[start..]);
    }
    out
}

pub type Entry = Map<String, Value>;

/// Held while the spool lock is taken; dropping it unlocks.
pub struct Lock(#[allow(dead_code)] File);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp(u64, i128, u64, u64, i128);

pub struct Spool {
    pub dir: PathBuf,
    pub path: PathBuf,
    pub meta_path: PathBuf,
    pub lock_path: PathBuf,
    clock: Box<dyn Fn() -> f64>,
}

pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn seq_of(e: &Entry) -> i64 {
    e.get("seq").and_then(Value::as_i64).unwrap_or(0)
}

fn ts_of(e: &Entry) -> f64 {
    e.get("ts").and_then(Value::as_f64).unwrap_or(0.0)
}

impl Spool {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self::with_clock(dir, Box::new(now))
    }

    pub fn with_clock(dir: impl Into<PathBuf>, clock: Box<dyn Fn() -> f64>) -> Self {
        let dir = dir.into();
        Spool {
            path: dir.join("spool.jsonl"),
            meta_path: dir.join("meta.json"),
            lock_path: dir.join("lock"),
            dir,
            clock,
        }
    }

    fn ensure_dir(&self) -> io::Result<()> {
        if self.dir.is_dir() {
            return Ok(());
        }
        if let Some(parent) = self.dir.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::DirBuilder::new().mode(0o700).create(&self.dir) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && self.dir.is_dir() => Ok(()),
            r => r,
        }
    }

    pub fn locked(&self, exclusive: bool) -> io::Result<Lock> {
        self.ensure_dir()?;
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&self.lock_path)?;
        let op = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        };
        loop {
            if unsafe { libc::flock(f.as_raw_fd(), op) } == 0 {
                return Ok(Lock(f));
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }

    fn write_atomic(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let tmp = PathBuf::from(format!("{}.tmp.{}", path.display(), std::process::id()));
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        drop(f);
        fs::rename(&tmp, path)
    }

    fn read_meta(&self) -> Option<(String, i64)> {
        let mut buf = Vec::new();
        File::open(&self.meta_path)
            .ok()?
            .take(4096)
            .read_to_end(&mut buf)
            .ok()?;
        let meta: Value = serde_json::from_slice(&buf).ok()?;
        let epoch = meta.get("epoch")?.as_str()?;
        let next = py::as_int(meta.get("next_seq"))?;
        if wire::is_epoch(epoch) && (1..=i128::from(MAX_SEQ)).contains(&next) {
            Some((epoch.to_string(), next as i64))
        } else {
            None
        }
    }

    fn write_meta(&self, epoch: &str, next_seq: i64) -> io::Result<()> {
        // Same bytes as Python's json.dumps({"epoch": ..., "next_seq": ...}).
        let data = format!("{{\"epoch\": \"{epoch}\", \"next_seq\": {next_seq}}}");
        self.write_atomic(&self.meta_path, data.as_bytes())
    }

    /// Current meta, creating a fresh epoch if missing. Caller holds the
    /// exclusive lock.
    fn meta(&self) -> io::Result<(String, i64)> {
        if let Some(m) = self.read_meta() {
            return Ok(m);
        }
        let epoch = random_epoch()?;
        self.write_meta(&epoch, 1)?;
        Ok((epoch, 1))
    }

    pub fn entries(&self) -> Vec<Entry> {
        let mut data = Vec::new();
        let Ok(f) = File::open(&self.path) else {
            return Vec::new();
        };
        if f.take((MAX_BYTES * 4) as u64)
            .read_to_end(&mut data)
            .is_err()
        {
            return Vec::new();
        }
        let mut out: Vec<Entry> = splitlines(&data)
            .into_iter()
            .filter_map(|line| match serde_json::from_slice::<Value>(line) {
                Ok(Value::Object(o))
                    if py::as_int(o.get("seq")).is_some()
                        && py::as_int(o.get("ts")).is_some()
                        && o.get("pane").is_some_and(Value::is_string) =>
                {
                    Some(o)
                }
                _ => None,
            })
            .collect();
        out.sort_by_key(seq_of);
        out
    }

    fn store(&self, entries: Vec<Entry>) -> io::Result<()> {
        let now = (self.clock)();
        let entries: Vec<Entry> = entries
            .into_iter()
            .filter(|e| ts_of(e) >= now - TTL_SECONDS as f64)
            .collect();
        let skip = entries.len().saturating_sub(MAX_ENTRIES);
        let mut lines: Vec<Vec<u8>> = entries[skip..]
            .iter()
            .map(|e| {
                let mut l = wire::dumps(&Value::Object(e.clone())).into_bytes();
                l.push(b'\n');
                l
            })
            .collect();
        let mut total: usize = lines.iter().map(Vec::len).sum();
        let mut first = 0;
        while first < lines.len() && total > MAX_BYTES {
            total -= lines[first].len();
            first += 1;
        }
        let data: Vec<u8> = lines.drain(first..).flatten().collect();
        self.write_atomic(&self.path, &data)
    }

    /// Give `record` the next seq and spool it, replacing the pane's
    /// previous entry. Returns the seq.
    pub fn append(&self, mut record: Entry) -> io::Result<i64> {
        let _lock = self.locked(true)?;
        let (epoch, seq) = self.meta()?;
        // Meta first: a crash between the two writes skips a seq rather
        // than reusing one.
        self.write_meta(&epoch, seq + 1)?;
        record.insert("seq".into(), Value::from(seq));
        let pane = record.get("pane").cloned();
        let mut entries: Vec<Entry> = self
            .entries()
            .into_iter()
            .filter(|e| e.get("pane") != pane.as_ref())
            .collect();
        entries.push(record);
        self.store(entries)?;
        Ok(seq)
    }

    /// Forget the pane's pending entry (it went back to working/idle).
    pub fn drop_pane(&self, pane: &str) -> io::Result<bool> {
        if !py::exists(&self.path) {
            return Ok(false);
        }
        let _lock = self.locked(true)?;
        let entries = self.entries();
        let n = entries.len();
        let kept: Vec<Entry> = entries
            .into_iter()
            .filter(|e| e.get("pane").and_then(Value::as_str) != Some(pane))
            .collect();
        if kept.len() == n {
            return Ok(false);
        }
        self.store(kept)?;
        Ok(true)
    }

    /// (epoch, head seq, entries) for a reader; creates the epoch if needed.
    pub fn snapshot(&self) -> io::Result<(String, i64, Vec<Entry>)> {
        let _lock = self.locked(true)?;
        let (epoch, next) = self.meta()?;
        Ok((epoch, next - 1, self.entries()))
    }

    /// Cheap change detector for the reader's poll loop.
    pub fn stamp(&self) -> Option<Stamp> {
        let st = fs::metadata(&self.path).ok()?;
        let mst = fs::metadata(&self.meta_path).ok()?;
        let ns =
            |m: &fs::Metadata| i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec());
        Some(Stamp(st.ino(), ns(&st), st.size(), mst.ino(), ns(&mst)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn splitlines_like_python() {
        assert_eq!(
            splitlines(b"a\nb\r\nc\rd"),
            vec![&b"a"[..], b"b", b"c", b"d"]
        );
        assert_eq!(splitlines(b"a\n"), vec![&b"a"[..]]);
        assert!(splitlines(b"").is_empty());
    }

    #[test]
    fn request_line_matches_python() {
        assert_eq!(
            request_line("pane.get", &json!({"pane_id": "w1:p1"})),
            "{\"id\": \"herdr-notify\", \"method\": \"pane.get\", \"params\": {\"pane_id\": \"w1:p1\"}}\n"
        );
        assert_eq!(
            request_line("session.snapshot", &json!({})),
            "{\"id\": \"herdr-notify\", \"method\": \"session.snapshot\", \"params\": {}}\n"
        );
    }

    #[test]
    fn paths() {
        let mut env = Env::new();
        env.insert("HOME".into(), "/h".into());
        assert_eq!(
            state_dir(&env),
            PathBuf::from("/h/.local/state/herdr-notify")
        );
        assert_eq!(
            default_socket(&env),
            PathBuf::from("/h/.config/herdr/herdr.sock")
        );
        env.insert("XDG_STATE_HOME".into(), "/s".into());
        env.insert("XDG_CONFIG_HOME".into(), "".into());
        assert_eq!(state_dir(&env), PathBuf::from("/s/herdr-notify"));
        assert_eq!(
            default_socket(&env),
            PathBuf::from("/h/.config/herdr/herdr.sock")
        );
    }
}
