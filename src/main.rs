//! Multicall entry point: `herdr-warp <subcommand> [args]`, or one of the
//! legacy command names via a symlink (the name picks the subcommand).

use argh::FromArgs;
use herdr_warp::{agent, hook, pin, serve, tab_title};

/// Helpers that connect herdr-managed agents on a VM to Warp on a Mac.
/// Also callable as herdr-notify-hook, herdr-notify-serve,
/// herdr-notify-agent and herdr-tab-title-hook (symlinks).
#[derive(FromArgs, Debug, PartialEq)]
struct Top {
    #[argh(subcommand)]
    cmd: Cmd,
}

// Built once per process; the agent's options being the bulk is fine.
#[allow(clippy::large_enum_variant)]
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
enum Cmd {
    Hook(hook::Args),
    Serve(serve::Args),
    Agent(agent::Args),
    TabTitle(tab_title::Args),
    Pin(pin::Args),
}

/// What a legacy program name runs.
#[derive(Debug, PartialEq)]
enum Legacy {
    Hook,
    Serve,
    Agent,
    TabTitle,
}

fn legacy(name: &str) -> Option<Legacy> {
    match name {
        "herdr-notify-hook" => Some(Legacy::Hook),
        "herdr-notify-serve" => Some(Legacy::Serve),
        "herdr-notify-agent" => Some(Legacy::Agent),
        "herdr-tab-title-hook" => Some(Legacy::TabTitle),
        _ => None,
    }
}

/// With panic=abort a panic cannot be caught; this hook ends the process
/// with `code` instead (quietly: hooks have nowhere to report).
fn exit_on_panic(code: i32) {
    std::panic::set_hook(Box::new(move |_| std::process::exit(code)));
}

fn run(cmd: Cmd) -> i32 {
    match cmd {
        Cmd::Hook(_) => {
            exit_on_panic(0);
            hook::main()
        }
        Cmd::Serve(_) => {
            exit_on_panic(1);
            serve::main()
        }
        Cmd::Agent(a) => agent::main(a),
        Cmd::TabTitle(a) => {
            exit_on_panic(0);
            tab_title::main(a)
        }
        Cmd::Pin(a) => pin::main(a),
    }
}

/// The exit code for `argv` (argv[0] included).
fn dispatch(argv: &[String]) -> i32 {
    let name = argv
        .first()
        .map(|a| a.rsplit('/').next().unwrap_or(a))
        .unwrap_or("herdr-warp");
    let rest: Vec<&str> = argv.iter().skip(1).map(String::as_str).collect();
    match legacy(name) {
        // The Python hook and forced command ignored their arguments.
        Some(Legacy::Hook) => run(Cmd::Hook(hook::Args {})),
        Some(Legacy::Serve) => run(Cmd::Serve(serve::Args {})),
        Some(Legacy::Agent) => match agent::Args::from_args(&[name], &rest) {
            Ok(a) => run(Cmd::Agent(a)),
            Err(e) => agent::arg_error(agent::ArgError(
                e.output,
                if e.status.is_ok() { 0 } else { 2 },
            )),
        },
        // herdr runs it with no arguments; a bad command line does nothing
        // and still exits 0.
        Some(Legacy::TabTitle) => match tab_title::Args::from_args(&[name], &rest) {
            Ok(a) => run(Cmd::TabTitle(a)),
            Err(e) => {
                if e.status.is_ok() {
                    println!("{}", e.output);
                } else {
                    eprintln!("{}", e.output);
                }
                0
            }
        },
        None => match Top::from_args(&["herdr-warp"], &rest) {
            Ok(top) => run(top.cmd),
            Err(e) => agent::arg_error(agent::ArgError(
                e.output,
                if e.status.is_ok() { 0 } else { 2 },
            )),
        },
    }
}

fn main() {
    let argv: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    std::process::exit(dispatch(&argv));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_names() {
        assert_eq!(legacy("herdr-notify-hook"), Some(Legacy::Hook));
        assert_eq!(legacy("herdr-notify-serve"), Some(Legacy::Serve));
        assert_eq!(legacy("herdr-notify-agent"), Some(Legacy::Agent));
        assert_eq!(legacy("herdr-tab-title-hook"), Some(Legacy::TabTitle));
        assert_eq!(legacy("herdr-warp"), None);
        assert_eq!(legacy(""), None);
    }

    #[test]
    fn subcommands_parse() {
        let p = |v: &[&str]| Top::from_args(&["herdr-warp"], v).map(|t| t.cmd);
        assert_eq!(p(&["hook"]).unwrap(), Cmd::Hook(hook::Args {}));
        assert_eq!(p(&["serve"]).unwrap(), Cmd::Serve(serve::Args {}));
        assert_eq!(p(&["pin"]).unwrap(), Cmd::Pin(pin::Args {}));
        assert_eq!(
            p(&["tab-title", "--all", "--dry-run"]).unwrap(),
            Cmd::TabTitle(tab_title::Args {
                all: true,
                dry_run: true
            })
        );
        assert!(p(&["nope"]).is_err());
        assert!(p(&[]).is_err());
        assert!(p(&["hook", "x"]).is_err());
    }

    #[test]
    fn dispatch_codes() {
        let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(dispatch(&v(&["/x/herdr-warp", "pin"])), 2);
        assert_eq!(dispatch(&v(&["herdr-warp", "bogus"])), 2);
        assert_eq!(dispatch(&v(&["herdr-warp", "--help"])), 0);
        assert_eq!(dispatch(&v(&["herdr-notify-agent", "--host"])), 2);
        assert_eq!(dispatch(&v(&["herdr-notify-agent", "--help"])), 0);
        // A legacy hook never fails, whatever it is given.
        assert_eq!(dispatch(&v(&["herdr-tab-title-hook", "--bogus"])), 0);
        assert_eq!(dispatch(&v(&["herdr-tab-title-hook", "--help"])), 0);
    }

    /// Child half of `panic_exits_with_the_hook_code`.
    #[test]
    fn panic_child() {
        if std::env::var("HW_PANIC_CHILD").is_ok() {
            exit_on_panic(7);
            panic!("boom");
        }
    }

    #[test]
    fn panic_exits_with_the_hook_code() {
        let st = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::panic_child", "--nocapture"])
            .env("HW_PANIC_CHILD", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(st.code(), Some(7));
    }
}
