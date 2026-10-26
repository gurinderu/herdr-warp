//! `pin`: attach a herdr UI client and keep *this* client on one workspace,
//! so a Warp tab can be dedicated to a herdr workspace.
//!
//! Not implemented yet. herdr has no flag, env var or API to pick the
//! workspace of one attached client (`workspace.focus` over the socket moves
//! every client), so the plan is a pty wrapper: run `herdr` in a pty, type
//! the navigate-mode keys (prefix, workspace picker, digit or Down*n, Enter)
//! once it has drawn, and exit when the pinned workspace is closed
//! (`events.subscribe` for `workspace.closed`).

/// Keep one herdr client on one workspace (not implemented yet).
#[derive(argh::FromArgs, Debug, Default, PartialEq)]
#[argh(subcommand, name = "pin")]
pub struct Args {}

pub fn main(_args: Args) -> i32 {
    eprintln!("herdr-warp pin: not implemented yet");
    2
}
