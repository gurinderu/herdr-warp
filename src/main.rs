//! Multicall entry point: `herdr-warp <subcommand> [args]`, or one of the
//! legacy command names via a symlink (the name picks the subcommand).

use herdr_warp::{agent, hook, pin, serve, tab_title};
use std::panic;

const USAGE: &str = "usage: herdr-warp <command> [args]

commands:
  hook       herdr plugin hook: spool done/needs-input agent events (VM)
  serve      ssh forced command: stream the spool, focus a pane (VM)
  agent      launchd agent: ssh stream -> Warp OSC 777 notifications (Mac)
  tab-title  herdr plugin hook: name agent tabs after their terminal title (VM)
  pin        keep one herdr client on one workspace (not implemented yet)

legacy names (symlinks): herdr-notify-hook, herdr-notify-serve,
herdr-notify-agent, herdr-tab-title-hook";

/// Subcommand for a legacy program name.
fn legacy(name: &str) -> Option<&'static str> {
    match name {
        "herdr-notify-hook" => Some("hook"),
        "herdr-notify-serve" => Some("serve"),
        "herdr-notify-agent" => Some("agent"),
        "herdr-tab-title-hook" => Some("tab-title"),
        _ => None,
    }
}

/// Run a hook that herdr spawns detached: nowhere to report, so a panic is
/// silent and still exits 0.
fn quiet(f: fn(&[String]) -> i32, args: &[String]) -> i32 {
    panic::set_hook(Box::new(|_| {}));
    panic::catch_unwind(|| f(args)).unwrap_or(0)
}

fn dispatch(cmd: &str, args: &[String]) -> i32 {
    match cmd {
        "hook" => quiet(hook::main, args),
        "tab-title" => quiet(tab_title::main, args),
        "serve" => panic::catch_unwind(|| serve::main(args)).unwrap_or(1),
        "agent" => agent::main(args),
        "pin" => pin::main(args),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            0
        }
        other => {
            eprintln!("herdr-warp: unknown command {other:?}\n{USAGE}");
            2
        }
    }
}

fn main() {
    let argv: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let name = argv
        .first()
        .map(|a| a.rsplit('/').next().unwrap_or(a).to_string())
        .unwrap_or_default();
    let code = match legacy(&name) {
        Some(cmd) => dispatch(cmd, &argv[1..]),
        None => match argv.get(1) {
            Some(cmd) => dispatch(cmd, &argv[2..]),
            None => {
                eprintln!("{USAGE}");
                2
            }
        },
    };
    std::process::exit(code);
}
