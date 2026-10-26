//! Running helper programs from fixed argv lists, with a timeout, the way
//! Python's `subprocess.run(argv, stdin=DEVNULL, capture_output=True,
//! timeout=...)` does.

use std::io::{self, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum RunError {
    /// The program could not be started.
    Spawn(io::Error),
    /// It did not finish in time and was killed.
    Timeout,
}

pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
}

fn drain(mut r: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        buf
    })
}

/// Run `argv` with stdin from /dev/null, stdout and stderr captured; kill it
/// after `timeout`.
pub fn run(argv: &[String], timeout: Duration) -> Result<Output, RunError> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(RunError::Spawn)?;
    let out = child.stdout.take().map(drain);
    let err = child.stderr.take().map(drain);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Timeout);
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(RunError::Spawn(e)),
        }
    };
    let stdout = out.and_then(|h| h.join().ok()).unwrap_or_default();
    if let Some(h) = err {
        let _ = h.join();
    }
    Ok(Output { status, stdout })
}

/// Local time as `%Y-%m-%dT%H:%M:%S` (Python's time.strftime default zone).
pub fn local_timestamp() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        let mut buf = [0u8; 64];
        let fmt = c"%Y-%m-%dT%H:%M:%S";
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), fmt.as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_and_times_out() {
        let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let out = run(
            &argv(&["sh", "-c", "echo hi; echo err >&2; exit 3"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out.stdout, b"hi\n");
        assert_eq!(out.status.code(), Some(3));
        assert!(matches!(
            run(&argv(&["sleep", "5"]), Duration::from_millis(100)),
            Err(RunError::Timeout)
        ));
        assert!(matches!(
            run(&argv(&["/nonexistent/x"]), Duration::from_secs(1)),
            Err(RunError::Spawn(_))
        ));
        assert_eq!(local_timestamp().len(), 19);
        // Waits for a slower child instead of timing it out at once.
        let out = run(
            &argv(&["sh", "-c", "sleep 0.3; echo late"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out.stdout, b"late\n");
    }
}
