# herdr-warp

Helpers that connect herdr-managed
coding agents (Claude Code, Codex) running on a remote VM to
[Warp](https://www.warp.dev) on a Mac:

- desktop notifications when an agent finishes or needs input, with
  click-to-focus of the herdr pane;
- herdr tab names that follow what the agent in the tab is working on.

One crate, one binary (`herdr-warp`), several subcommands. The hooks run on
every herdr event, so the binary is built to start fast: no async runtime,
a small set of crates (serde_json, libc, unicode-normalization, argh for the
command line), a fat-LTO, `panic = "abort"` release build, and herdr is
reached over its unix socket rather than by spawning the `herdr` CLI. The
programs it does run (`ssh` and, on the Mac, `ps` and `lsappinfo`) are run
from fixed argument lists.

## Subcommands

| subcommand  | legacy name            | runs on | what it does |
|-------------|------------------------|---------|--------------|
| `hook`      | `herdr-notify-hook`    | VM      | herdr plugin hook for `pane.agent_status_changed`. After a 1.5 s debounce it re-reads the pane from herdr's socket and, if the agent is still `done` / `blocked`, appends a small fixed-schema record to the spool. Other states clear the pane's pending record. Default herdr session only. Always exits 0. |
| `serve`     | `herdr-notify-serve`   | VM      | ssh forced command for the Mac's dedicated key. Reads `SSH_ORIGINAL_COMMAND` and accepts exactly `stream <seq> [<epoch>]` (hello, spooled events after `<seq>`, then live events and a heartbeat every 20 s) or `focus <pane> <terminal_id>` (focus that pane if it still hosts that terminal, at most once a second). Anything else gets `{"code":"bad_command","type":"error","v":1}` and exit 2. |
| `agent`     | `herdr-notify-agent`   | Mac     | launchd agent. Holds one `ssh ... stream` session, validates every line, and shows events as Warp notifications by writing OSC 777 (`ESC ] 777 ; notify ; title ; body BEL`) to the tty of the Warp tab whose `mosh-client` is connected to the VM. Click-to-pane is "latest wins": the next time Warp comes to the front within the jump window, it asks the VM to focus the last notified pane. Reconnects with jittered exponential backoff. |
| `tab-title` | `herdr-tab-title-hook` | VM      | herdr plugin hook (agent and focus events, and `startup`). Renames a tab to its agent pane's terminal title, trimmed to 32 characters, but only while the tab has herdr's default numeric label or the label this hook set last; manual renames are left alone. Skips generic titles ("Claude Code", shell prompts, paths). State in `$HERDR_PLUGIN_STATE_DIR/labels.json`. Talks to `$HERDR_SOCKET_PATH` (`session.snapshot`, `tab.rename`), falling back to the `herdr` CLI (`$HERDR_BIN_PATH`) when no socket answers. Always exits 0. `--all` handles every tab; `--dry-run` prints the renames it would make and changes nothing (no rename, no state, no lock). |
| `pin`       |                        | Mac/VM  | Reserved: keep one herdr client on one workspace (for a Warp tab per workspace). Not implemented yet. |

The legacy names are symlinks in the package's `bin/`; the binary picks the
subcommand from `argv[0]`, so `herdr-notify-serve` behaves exactly like
`herdr-warp serve`. They keep the command line, environment, stdin/stdout
and exit-code contract of the Python scripts these replace.

### Files

VM, under `$XDG_STATE_HOME/herdr-notify` (default `~/.local/state`, mode
0700): `spool.jsonl` (one JSON event per line, 0600, rewritten atomically,
at most 64 entries / 32 KiB, one per pane, one hour TTL), `meta.json`
(`{"epoch": ..., "next_seq": ...}`), `lock` (flock), `focus.last` (focus rate
limit). `touch debug` there for a breadcrumb log in `debug.log`.

herdr's socket: `$XDG_CONFIG_HOME/herdr/herdr.sock` (default `~/.config`).

Mac: `--state-dir` holds `state.json` (`{"epoch": ..., "last_seq": ...}`);
logs go to stderr.

### `agent` options

```
herdr-notify-agent --host <vm address> --user <user> --key <private key>
    --known-hosts <file> --host-key-alias <alias> --state-dir <dir>
    --vm-addr <spelling> [--vm-addr <spelling> ...]
    [--port 22] [--ssh /usr/bin/ssh] [--max-age 900] [--jump-window 600]
    [--warp-bundle-prefix dev.warp.] [--no-focus-on-click]
```

Options take their value as the next argument (`--port 22`, not
`--port=22`). A bad command line exits 2; `--help` lists everything.

`--vm-addr` lists every host spelling a `mosh-client` to the VM may carry in
its argv (the name you typed and the address it resolved to). All
host-specific values (addresses, user, key paths, the pinned host key) come
from the caller; nothing is built in.

## Using it from a Nix configuration

```nix
# flake.nix
inputs.herdr-warp = {
  url = "github:gurinderu/herdr-warp";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

Then, in the modules that used the Python package, replace the package
reference and keep everything else (plugin manifests, the forced command,
the launchd `ProgramArguments`), since the command names are unchanged:

```nix
herdr-notify = inputs.herdr-warp.packages.${pkgs.stdenv.hostPlatform.system}.default;
```

The tab-title module can drop its `writeTextFile` wrapper and put the same
package in `home.packages`; it provides `bin/herdr-tab-title-hook`.

Packages: `x86_64-linux` and `aarch64-darwin`.

## Development

```sh
nix develop            # pinned toolchain (rust-toolchain.toml), actionlint, cargo-mutants, python3
cargo test
cargo clippy --all-targets -- -D warnings
nix flake check        # cargo test, clippy, fmt
```

`parity/test_parity.py` runs the same black-box scenarios (hook, serve,
agent, tab-title) against the original Python scripts and this binary and
requires identical output, files and exit codes. The Python sources are not
in this repository; point at them:

```sh
cargo build
HERDR_NOTIFY_PY_SRC=/path/to/herdr-notify/src \
HERDR_TAB_TITLE_PY=/path/to/herdr-tab-title.py \
  python3 -I parity/test_parity.py -v
```

Mutation testing uses cargo-mutants (`nix develop`, then `cargo mutants`);
its settings are in `.cargo/mutants.toml`. CI mutates the lines a pull
request changes (advisory) and the whole crate weekly.

`src/unicode_tables.rs` is generated from CPython's `unicodedata` (the
Python version's label cleaning rules) by `tools/gen-unicode-tables.py`.

## License

MIT
