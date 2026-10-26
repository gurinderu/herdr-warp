//! `serve` (legacy name `herdr-notify-serve`): the forced command of the Mac
//! notifier's ssh key.
//!
//! sshd runs this for every connection with that key and passes what the
//! client asked for in SSH_ORIGINAL_COMMAND. That string is only ever matched
//! against the two whole-string forms below; it is never run or interpolated.
//!
//! ```text
//! stream <seq> [<epoch>]   hello, spooled events after <seq>, then new ones
//!                          as they arrive, heartbeats every HB_SECONDS
//! focus <pane> <term>      focus that herdr pane in the default session, if
//!                          it still hosts terminal <term>
//! ```
//!
//! Anything else gets `{"code":"bad_command","type":"error","v":1}`, exit 2.

use crate::vm::{self, Call, Env, Spool};
use crate::wire::{self, MAX_LINE_BYTES, SCHEMA_VERSION, TTL_SECONDS};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

pub const POLL_SECONDS: f64 = 1.0;
pub const HB_SECONDS: f64 = 20.0;
pub const MAX_STREAM_SECONDS: f64 = 12.0 * 3600.0;
pub const FOCUS_MIN_INTERVAL: f64 = 1.0;

#[derive(Debug, PartialEq)]
pub enum Command {
    Stream(i64, Option<String>),
    Focus(String, String),
    Bad,
}

/// `stream (0|[1-9][0-9]{0,15})(?: ([0-9a-f]{16}))?` or
/// `focus ([A-Za-z0-9:_-]{1,32}) (term_[0-9a-f]{6,32})`, whole string.
pub fn parse_command(cmd: &str) -> Command {
    if let Some(rest) = cmd.strip_prefix("stream ") {
        let (num, epoch) = match rest.split_once(' ') {
            Some((n, e)) => (n, Some(e)),
            None => (rest, None),
        };
        let num_ok = num == "0"
            || (!num.is_empty()
                && num.len() <= 16
                && num.bytes().all(|b| b.is_ascii_digit())
                && !num.starts_with('0'));
        if num_ok
            && epoch.is_none_or(wire::is_epoch)
            && let Ok(n) = num.parse()
        {
            return Command::Stream(n, epoch.map(str::to_string));
        }
        return Command::Bad;
    }
    if let Some(rest) = cmd.strip_prefix("focus ")
        && let Some((pane, term)) = rest.split_once(' ')
        && wire::is_id(pane)
        && wire::is_term(term)
    {
        return Command::Focus(pane.into(), term.into());
    }
    Command::Bad
}

fn out(obj: Value, stream: &mut dyn Write) -> io::Result<()> {
    let mut obj = obj;
    obj["v"] = Value::from(SCHEMA_VERSION);
    stream.write_all(wire::dumps(&obj).as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

pub struct StreamOpts<'a> {
    pub clock: &'a dyn Fn() -> f64,
    pub monotonic: &'a dyn Fn() -> f64,
    pub sleep: &'a dyn Fn(f64),
    pub max_seconds: f64,
    pub hb_seconds: f64,
}

pub fn run_stream(
    since: i64,
    client_epoch: Option<&str>,
    spool: &Spool,
    stream: &mut dyn Write,
    o: &StreamOpts,
) -> io::Result<i32> {
    let (mut epoch, mut head, _) = spool.snapshot()?;
    let mut sent = if client_epoch.is_some_and(|e| e != epoch) {
        0
    } else {
        since
    };
    out(
        json!({"type": "hello", "epoch": epoch, "head": head, "ts": (o.clock)() as i64}),
        stream,
    )?;
    let mut stamp = None;
    let start = (o.monotonic)();
    let mut last_hb = start;
    loop {
        let now = (o.clock)();
        if stamp.is_none() || spool.stamp() != stamp {
            stamp = spool.stamp();
            let (new_epoch, h, entries) = spool.snapshot()?;
            head = h;
            if new_epoch != epoch {
                // Spool recreated: start over so the client resets too.
                epoch = new_epoch;
                sent = 0;
                out(
                    json!({"type": "hello", "epoch": epoch, "head": head, "ts": now as i64}),
                    stream,
                )?;
            }
            for entry in entries {
                let seq = entry.get("seq").and_then(Value::as_i64).unwrap_or(0);
                let ts = entry.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
                if seq > sent && ts >= now - TTL_SECONDS as f64 {
                    let line = wire::dumps(&Value::Object(entry));
                    if line.len() <= MAX_LINE_BYTES {
                        stream.write_all(line.as_bytes())?;
                        stream.write_all(b"\n")?;
                    }
                    sent = sent.max(seq);
                }
            }
            stream.flush()?;
        }
        if (o.monotonic)() - last_hb >= o.hb_seconds {
            out(
                json!({"type": "hb", "head": head, "ts": now as i64}),
                stream,
            )?;
            last_hb = (o.monotonic)();
        }
        if (o.monotonic)() - start >= o.max_seconds {
            return Ok(0);
        }
        (o.sleep)(POLL_SECONDS);
    }
}

fn mtime(path: &Path) -> f64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn run_focus(
    pane: &str,
    term: &str,
    spool: &Spool,
    stream: &mut dyn Write,
    call: Call,
    clock: &dyn Fn() -> f64,
    env: &Env,
) -> io::Result<i32> {
    let sock = vm::default_socket(env);
    {
        let _lock = spool.locked(true)?;
        let last_path = spool.dir.join("focus.last");
        if clock() - mtime(&last_path) < FOCUS_MIN_INTERVAL {
            out(
                json!({"type": "focus", "ok": false, "code": "rate_limited"}),
                stream,
            )?;
            return Ok(1);
        }
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&last_path)?;
    }
    let reply = call(&sock, "pane.get", json!({"pane_id": pane}));
    let found = reply
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|r| r.get("result"))
        .and_then(Value::as_object)
        .and_then(|r| r.get("pane"))
        .and_then(Value::as_object)
        .and_then(|p| p.get("terminal_id"));
    let Some(found) = found else {
        out(
            json!({"type": "focus", "ok": false, "code": "pane_not_found"}),
            stream,
        )?;
        return Ok(1);
    };
    if found.as_str() != Some(term) {
        out(
            json!({"type": "focus", "ok": false, "code": "terminal_changed"}),
            stream,
        )?;
        return Ok(1);
    }
    let reply = call(&sock, "pane.focus", json!({"pane_id": pane}));
    let ok = reply
        .as_ref()
        .and_then(Value::as_object)
        .is_some_and(|r| r.contains_key("result"));
    let code = if ok { "focused" } else { "focus_failed" };
    out(json!({"type": "focus", "ok": ok, "code": code}), stream)?;
    Ok(if ok { 0 } else { 1 })
}

fn is_gone(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
    )
}

/// The forced command proper, with the clocks and socket call injectable.
pub fn run(env: &Env, stream: &mut dyn Write, call: Call, o: &StreamOpts) -> i32 {
    let cmd = env
        .get("SSH_ORIGINAL_COMMAND")
        .map(String::as_str)
        .unwrap_or("");
    let spool = Spool::new(vm::state_dir(env));
    let res = match parse_command(cmd) {
        Command::Stream(since, epoch) => run_stream(since, epoch.as_deref(), &spool, stream, o),
        Command::Focus(pane, term) => run_focus(&pane, &term, &spool, stream, call, o.clock, env),
        Command::Bad => out(json!({"type": "error", "code": "bad_command"}), stream).map(|_| 2),
    };
    match res {
        Ok(rc) => rc,
        // The client went away.
        Err(e) if is_gone(&e) => 0,
        Err(e) => {
            eprintln!("herdr-notify-serve: {e}");
            1
        }
    }
}

pub fn main(_args: &[String]) -> i32 {
    let env = vm::os_env();
    let base = Instant::now();
    let monotonic = move || base.elapsed().as_secs_f64();
    let sleep = |s: f64| std::thread::sleep(std::time::Duration::from_secs_f64(s));
    let opts = StreamOpts {
        clock: &vm::now,
        monotonic: &monotonic,
        sleep: &sleep,
        max_seconds: MAX_STREAM_SECONDS,
        hb_seconds: HB_SECONDS,
    };
    let stdout = io::stdout();
    let mut lock = io::BufWriter::new(stdout.lock());
    run(&env, &mut lock, &vm::herdr_call, &opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_commands() {
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
            "stream 1 ",
            "stream  1",
            "stream 1 0123456789abcdeF",
            "stream 1 0123456789abcdef ",
            "focus w1:p1 term_65ead8c4a4dc72\n",
        ] {
            assert_eq!(parse_command(cmd), Command::Bad, "{cmd:?}");
        }
    }

    #[test]
    fn good_commands() {
        assert_eq!(parse_command("stream 0"), Command::Stream(0, None));
        assert_eq!(
            parse_command("stream 9999999999999999 0123456789abcdef"),
            Command::Stream(9999999999999999, Some("0123456789abcdef".into()))
        );
        assert_eq!(
            parse_command("focus w1:p1 term_65ead8c4a4dc72"),
            Command::Focus("w1:p1".into(), "term_65ead8c4a4dc72".into())
        );
    }
}
