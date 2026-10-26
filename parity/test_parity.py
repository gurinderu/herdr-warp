"""Black-box parity: run the same scenarios against the original Python
scripts and the Rust binary, and require identical observable behaviour
(stdout bytes, exit codes, spool/state files, requests sent to herdr).

The Python originals are not part of this repo. Point at them with:

    HERDR_NOTIFY_PY_SRC  directory holding hook.py, serve.py, hnlib.py, ...
    HERDR_TAB_TITLE_PY   the herdr-tab-title.py script
    HERDR_WARP_BIN       the Rust binary (default: target/debug/herdr-warp)

    python3 -I -m unittest parity/test_parity.py -v

The Rust side is invoked through the legacy names (symlinks), which also
exercises the multicall dispatch.
"""

import json
import os
import re
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
PY_SRC = os.environ.get("HERDR_NOTIFY_PY_SRC", "")
PY_TAB = os.environ.get("HERDR_TAB_TITLE_PY", "")
BIN = os.environ.get("HERDR_WARP_BIN") or os.path.join(HERE, "..", "target", "debug", "herdr-warp")
IMPLS = ("py", "rs")
LEGACY = {"hook": "herdr-notify-hook", "serve": "herdr-notify-serve",
          "agent": "herdr-notify-agent", "tab-title": "herdr-tab-title-hook"}

if not (os.path.isdir(PY_SRC) and os.path.isfile(PY_TAB) and os.path.isfile(BIN)):
    raise unittest.SkipTest("set HERDR_NOTIFY_PY_SRC, HERDR_TAB_TITLE_PY and build the binary")

BIN_DIR = tempfile.mkdtemp(prefix="herdr-warp-bin-")
for _name in LEGACY.values():
    os.symlink(os.path.abspath(BIN), os.path.join(BIN_DIR, _name))


def argv(impl, sub):
    if impl == "rs":
        return [os.path.join(BIN_DIR, LEGACY[sub])]
    if sub == "tab-title":
        return [sys.executable, "-I", PY_TAB]
    return [sys.executable, "-I", os.path.join(PY_SRC, sub + ".py")]


def norm_ts(text):
    return re.sub(r'"ts":\s?\d+', '"ts":0', text)


SNAPSHOT = {
    "focused_pane_id": "w1:p1",
    "workspaces": [{"workspace_id": "w2", "label": "my-project"}],
    "tabs": [{"tab_id": "w2:t1", "label": "1"}],
    "panes": [{"pane_id": "w2:p1", "tab_id": "w2:t1", "workspace_id": "w2",
               "agent": "claude", "agent_status": "blocked",
               "terminal_id": "term_65ead8c4a4dc72",
               "terminal_title_stripped": "Ситуация\x1b с 1400 ‮"}],
    "agents": [],
}


def herdr_event(status="blocked", pane="w2:p1"):
    return json.dumps({"event": "pane_agent_status_changed",
                       "data": {"type": "pane_agent_status_changed", "pane_id": pane,
                                "workspace_id": "w2", "agent_status": status,
                                "agent": "claude"}})


class FakeHerdr:
    """A herdr socket server: records request lines, answers via handler."""

    def __init__(self, path, handler):
        self.requests = []
        self.handler = handler
        self.srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.srv.bind(path)
        self.srv.listen(64)
        self.lock = threading.Lock()
        threading.Thread(target=self.loop, daemon=True).start()

    def loop(self):
        while True:
            try:
                conn, _ = self.srv.accept()
            except OSError:
                return
            threading.Thread(target=self.one, args=(conn,), daemon=True).start()

    def one(self, conn):
        with conn:
            line = conn.makefile("rb").readline()
            with self.lock:
                self.requests.append(line)
            req = json.loads(line)
            reply = self.handler(req["method"], req["params"])
            conn.sendall(json.dumps(reply).encode() + b"\n")

    def close(self):
        self.srv.close()


class Env:
    def __init__(self):
        self.home = tempfile.mkdtemp(prefix="herdr-warp-parity-")
        os.makedirs(os.path.join(self.home, "cfg", "herdr"))
        self.sock = os.path.join(self.home, "cfg", "herdr", "herdr.sock")
        self.state = os.path.join(self.home, "state", "herdr-notify")
        self.env = {"HOME": self.home, "PATH": os.environ.get("PATH", ""),
                    "XDG_STATE_HOME": os.path.join(self.home, "state"),
                    "XDG_CONFIG_HOME": os.path.join(self.home, "cfg"),
                    "HERDR_SOCKET_PATH": self.sock}

    def read(self, name):
        try:
            with open(os.path.join(self.state, name), encoding="utf-8") as fh:
                return fh.read()
        except FileNotFoundError:
            return None

    def cleanup(self):
        shutil.rmtree(self.home, ignore_errors=True)


def slurp(path):
    """File content, or None if it does not exist."""
    try:
        with open(path, encoding="utf-8") as fh:
            return fh.read()
    except FileNotFoundError:
        return None


def run(cmd, env, **kw):
    return subprocess.run(cmd, env=env, capture_output=True, timeout=60, **kw)


class Parity(unittest.TestCase):
    def both(self, scenario):
        """Run scenario(impl) for both; assert equal results; return them."""
        out = {}
        for impl in IMPLS:
            with self.subTest(impl=impl):
                out[impl] = scenario(impl)
        self.assertEqual(out["py"], out["rs"])
        return out["rs"]

    # ------------------------------------------------------------ hook

    def hook_scenario(self, impl):
        e = Env()
        snap = {"snapshot": SNAPSHOT}
        srv = FakeHerdr(e.sock, lambda m, p: {"result": snap})
        steps = []

        def hook(event, extra=None):
            env = dict(e.env, HERDR_PLUGIN_EVENT_JSON=event, **(extra or {}))
            before = len(srv.requests)
            r = run(argv(impl, "hook"), env)
            steps.append((r.returncode, r.stdout, r.stderr, len(srv.requests) - before,
                          norm_ts(e.read("spool.jsonl") or "")))
        try:
            os.makedirs(e.state)
            open(os.path.join(e.state, "debug"), "w").close()
            hook(herdr_event())                         # spooled
            hook(herdr_event())                         # coalesced, seq 2
            hook(herdr_event("working"))                # cleared, no snapshot call
            hook(herdr_event(pane="w1:p1;id"))          # bad id
            hook("not json")
            hook("")
            hook(herdr_event(), {"HERDR_SOCKET_PATH": os.path.join(e.home, "other.sock")})
            hook(json.dumps({"pane_id": "w2:p1", "agent_status": "blocked"}))  # bare payload
            snap["snapshot"] = dict(SNAPSHOT, panes=[dict(SNAPSHOT["panes"][0], agent_status="working")])
            hook(herdr_event())                         # changed during debounce
            snap["snapshot"] = dict(SNAPSHOT, panes=[dict(SNAPSHOT["panes"][0], agent="gemini")])
            hook(herdr_event())                         # other agent
            snap["snapshot"] = "nope"
            hook(herdr_event())                         # not a dict
            meta = json.loads(e.read("meta.json"))
            log = [l.split(" ", 1)[1].replace(e.home, "HOME") for l in e.read("debug.log").splitlines()]
            # (The state dir itself was made by this test, for the debug flag.)
            modes = (oct(os.stat(os.path.join(e.state, "spool.jsonl")).st_mode & 0o777),
                     oct(os.stat(os.path.join(e.state, "meta.json")).st_mode & 0o777))
            return steps, srv.requests, meta["next_seq"], sorted(meta), log, modes
        finally:
            srv.close()
            e.cleanup()

    def test_hook(self):
        steps, reqs, next_seq, keys, log, modes = self.both(self.hook_scenario)
        self.assertEqual(steps[0][4].count("\n"), 1)
        self.assertIn('"title":"Ситуация с 1400"', steps[0][4])
        self.assertEqual(reqs[0], b'{"id": "herdr-notify", "method": "session.snapshot", "params": {}}\n')
        self.assertEqual(next_seq, 4)
        self.assertEqual(modes, ("0o600", "0o600"))

    # ------------------------------------------------------------ serve

    def test_bad_commands(self):
        cmds = ["", "id", "stream", "stream x", "stream 01", "stream 1 2", "stream 1; id",
                "stream 1\nid", "focus w1:p1", "focus w1:p1 term_1",
                "focus $(id) term_65ead8c4a4dc72", "focus w1:p1 term_65ead8c4a4dc72 x",
                "stream 99999999999999999", "stream 1 ", "stream 1 0123456789ABCDEF",
                "focus w1:p1 term_65ead8c4a4dc72\n", "Stream 1"]

        def scenario(impl):
            e = Env()
            try:
                res = []
                for c in cmds:
                    r = run(argv(impl, "serve"), dict(e.env, SSH_ORIGINAL_COMMAND=c))
                    res.append((r.returncode, r.stdout))
                r = run(argv(impl, "serve"), {k: v for k, v in e.env.items()})
                res.append((r.returncode, r.stdout))  # no SSH_ORIGINAL_COMMAND
                return res
            finally:
                e.cleanup()
        res = self.both(scenario)
        self.assertEqual(set(res), {(2, b'{"code":"bad_command","type":"error","v":1}\n')})

    def stream_lines(self, impl, env, cmd, want, timeout=30):
        """Run `serve` with cmd; collect lines until want(lines) or timeout;
        then close the pipe and return (lines, exit code)."""
        p = subprocess.Popen(argv(impl, "serve"), env=dict(env, SSH_ORIGINAL_COMMAND=cmd),
                             stdout=subprocess.PIPE, stdin=subprocess.DEVNULL)
        got, buf, deadline = [], b"", time.time() + timeout
        try:
            while time.time() < deadline and not want(got):
                r, _, _ = select.select([p.stdout], [], [], 0.2)
                if r:
                    chunk = os.read(p.stdout.fileno(), 65536)
                    if not chunk:
                        break
                    buf += chunk
                    *done, buf = buf.split(b"\n")
                    got += [norm_ts(x.decode()) for x in done]
        finally:
            p.stdout.close()
        # Reader gone: the next write (heartbeat within 20 s) hits EPIPE.
        return got, p

    def spool_with(self, e, writer, panes, ts=None):
        """Spool events through `writer`'s implementation of Spool.append."""
        ts = int(time.time()) if ts is None else ts
        if writer == "py":
            # The same records the hook builds from SNAPSHOT, so a spool
            # written either way has the same content.
            code = ("import sys; sys.path.insert(0, %r); import hnvm\n"
                    "s = hnvm.Spool(%r)\n"
                    "for p in %r: s.append({'v': 1, 'type': 'event', 'ts': %d, 'pane': p,\n"
                    "  'event': 'needs_input', 'agent': 'claude', 'ws': 'w2', 'tab': 'w2:t1',\n"
                    "  'term': 'term_65ead8c4a4dc72', 'ws_label': 'my-project', 'tab_label': '',\n"
                    "  'title': 'Ситуация с 1400', 'focused': p == 'w1:p1'})\n"
                    % (PY_SRC, e.state, panes, ts))
            subprocess.run([sys.executable, "-I", "-c", code], check=True)
        else:
            snap = {"snapshot": dict(SNAPSHOT, panes=[
                dict(SNAPSHOT["panes"][0], pane_id=p) for p in panes])}
            srv = FakeHerdr(e.sock, lambda m, prm: {"result": snap})
            try:
                for p in panes:
                    run(argv("rs", "hook"), dict(e.env, HERDR_PLUGIN_EVENT_JSON=herdr_event(pane=p)))
            finally:
                srv.close()
                os.unlink(e.sock)

    def test_stream_replay_cross_written(self):
        """Each reader replays a spool written by the *other* implementation."""
        def scenario(impl):
            e = Env()
            try:
                self.spool_with(e, "rs" if impl == "py" else "py", ["w1:p0", "w1:p1", "w1:p2"])
                epoch = json.loads(e.read("meta.json"))["epoch"]
                out = {}
                for name, cmd, n in [("from1", "stream 1", 3), ("same_epoch", "stream 2 " + epoch, 2),
                                     ("other_epoch", "stream 5 0000000000000000", 4),
                                     ("future", "stream 9", 1)]:
                    lines, p = self.stream_lines(impl, e.env, cmd, lambda g, n=n: len(g) >= n, timeout=3)
                    p.kill()
                    p.wait()
                    out[name] = [re.sub(epoch, "EPOCH", l) for l in lines]
                return out
            finally:
                e.cleanup()
        out = self.both(scenario)
        self.assertEqual(json.loads(out["from1"][0])["head"], 3)
        self.assertEqual([json.loads(l)["seq"] for l in out["from1"][1:]], [2, 3])
        self.assertEqual([json.loads(l)["seq"] for l in out["other_epoch"][1:]], [1, 2, 3])

    def test_stream_expired_not_sent(self):
        def scenario(impl):
            e = Env()
            try:
                self.spool_with(e, "py", ["w1:p1"], ts=int(time.time()) - 4000)
                lines, p = self.stream_lines(impl, e.env, "stream 0", lambda g: len(g) >= 2, timeout=3)
                p.kill()
                p.wait()
                return [json.loads(l)["type"] for l in lines]
            finally:
                e.cleanup()
        self.assertEqual(self.both(scenario), ["hello"])

    def test_live_stream_heartbeat_and_epipe(self):
        """New events arrive live, a heartbeat comes within 20 s, and the
        server exits 0 once the reader is gone. Both run concurrently."""
        results = {}

        def scenario(impl):
            e = Env()
            try:
                def later():
                    time.sleep(1.5)
                    self.spool_with(e, "py", ["w1:p1"])
                threading.Thread(target=later).start()
                lines, p = self.stream_lines(
                    impl, e.env, "stream 0",
                    lambda g: any('"hb"' in l for l in g) and any('"event"' in l for l in g),
                    timeout=30)
                rc = p.wait(timeout=30)
                results[impl] = ([json.loads(l)["type"] for l in lines], rc)
            finally:
                e.cleanup()
        ts = [threading.Thread(target=scenario, args=(i,)) for i in IMPLS]
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        self.assertEqual(results["py"], results["rs"])
        self.assertEqual(results["rs"], (["hello", "event", "hb"], 0))

    def test_focus(self):
        def scenario(impl):
            e = Env()
            term = {"id": "term_aaaaaaaa"}

            def handler(method, params):
                if method == "pane.get":
                    if params["pane_id"] == "w9:p9":
                        return {"error": {"code": "not_found"}}
                    return {"result": {"pane": {"terminal_id": term["id"]}}}
                if method == "pane.focus":
                    return {"result": {}} if params["pane_id"] != "w8:p8" else {"error": {}}
                return {}
            srv = FakeHerdr(e.sock, handler)
            res = []

            def focus(pane, t):
                last = os.path.join(e.state, "focus.last")
                if os.path.exists(last):
                    os.utime(last, (0, 0))
                r = run(argv(impl, "serve"), dict(e.env, SSH_ORIGINAL_COMMAND="focus %s %s" % (pane, t)))
                res.append((r.returncode, r.stdout))
            try:
                focus("w1:p1", "term_bbbbbbbb")      # terminal_changed
                focus("w1:p1", "term_aaaaaaaa")      # focused
                r = run(argv(impl, "serve"), dict(e.env, SSH_ORIGINAL_COMMAND="focus w1:p1 term_aaaaaaaa"))
                res.append((r.returncode, r.stdout))  # rate_limited (no utime)
                focus("w9:p9", "term_aaaaaaaa")      # pane_not_found
                focus("w8:p8", "term_aaaaaaaa")      # focus_failed
                srv.close()
                os.unlink(e.sock)
                focus("w1:p1", "term_aaaaaaaa")      # no herdr: pane_not_found
                return res, srv.requests
            finally:
                srv.close()
                e.cleanup()
        res, reqs = self.both(scenario)
        codes = [json.loads(o)["code"] for _, o in res]
        self.assertEqual(codes, ["terminal_changed", "focused", "rate_limited",
                                 "pane_not_found", "focus_failed", "pane_not_found"])
        self.assertEqual([rc for rc, _ in res], [1, 0, 1, 1, 1, 1])
        self.assertEqual(reqs[2], b'{"id": "herdr-notify", "method": "pane.focus", "params": {"pane_id": "w1:p1"}}\n')

    def test_concurrent_mixed_writers(self):
        """Python Spool.append processes and Rust hook processes share one
        spool and lock: 100 appends, unique seqs, capped at 64 entries."""
        e = Env()
        panes = ["w%d:p%d" % (w, i) for w in (2, 3) for i in range(25)]
        snap = {"snapshot": dict(SNAPSHOT, panes=[
            dict(SNAPSHOT["panes"][0], pane_id=p) for p in panes])}
        srv = FakeHerdr(e.sock, lambda m, p: {"result": snap})
        try:
            code = ("import sys, time; sys.path.insert(0, %r); import hnvm\n"
                    "s = hnvm.Spool(%r)\n"
                    "for i in range(25): s.append({'ts': int(time.time()), 'pane': 'w%%s:p%%d' %% (sys.argv[1], i)})\n"
                    % (PY_SRC, e.state))
            procs = [subprocess.Popen([sys.executable, "-I", "-c", code, str(n)]) for n in (0, 1)]
            procs += [subprocess.Popen(argv("rs", "hook"),
                                       env=dict(e.env, HERDR_PLUGIN_EVENT_JSON=herdr_event(pane=p)))
                      for p in panes]
            for p in procs:
                self.assertEqual(p.wait(timeout=60), 0)
            sys.path.insert(0, PY_SRC)
            import hnvm
            _, head, es = hnvm.Spool(e.state).snapshot()
            self.assertEqual(head, 100)
            self.assertEqual(len({x["seq"] for x in es}), len(es))
            self.assertEqual(len(es), 64)
        finally:
            srv.close()
            e.cleanup()

    # ------------------------------------------------------------ agent

    def agent_scenario(self, impl, lines, with_key=True, run_for=1.0):
        """Run the Mac agent against a fake ssh that records its argv and
        prints `lines`. On Linux there is no Warp tty, so deliveries are
        dropped and logged, which is what gets compared."""
        e = Env()
        try:
            key = os.path.join(e.home, "key")
            if with_key:
                open(key, "w").close()
            out = os.path.join(e.home, "ssh.out")
            with open(out, "wb") as fh:
                fh.write(b"".join(l + b"\n" for l in lines))
            ssh = os.path.join(e.home, "ssh")
            with open(ssh, "w") as fh:
                fh.write('#!/bin/sh\nprintf "%%s\\n" "$*" >> %s/ssh.argv\ncat %s\n' % (e.home, out))
            os.chmod(ssh, 0o755)
            state = os.path.join(e.home, "agent-state")
            args = ["--host", "192.0.2.10", "--user", "user", "--key", key,
                    "--known-hosts", "/dev/null", "--host-key-alias", "vm",
                    "--vm-addr", "192.0.2.10", "--vm-addr", "vm", "--state-dir", state,
                    "--ssh", ssh]
            p = subprocess.Popen(argv(impl, "agent") + args, env=e.env,
                                 stderr=subprocess.PIPE, stdin=subprocess.DEVNULL)
            time.sleep(run_for)
            p.kill()
            _, err = p.communicate()
            log = [re.sub(r"\(\d+s old\)", "(Ns old)", l.split(" ", 1)[1].replace(e.home, "HOME"))
                   for l in err.decode().splitlines()]
            calls = (slurp(os.path.join(e.home, "ssh.argv")) or "").replace(e.home, "HOME")
            return log, calls, slurp(os.path.join(state, "state.json"))
        finally:
            e.cleanup()

    def test_agent_session(self):
        now = int(time.time())
        ev = {"v": 1, "type": "event", "seq": 1, "ts": now, "event": "done", "agent": "codex",
              "ws": "w2", "tab": "w2:t1", "pane": "w2:p1", "term": "term_65ead8c4a4dc72",
              "ws_label": "my-project", "tab_label": "", "title": "fix it", "focused": False}
        lines = [
            b'{"v":1,"type":"hello","epoch":"0123456789abcdef","head":3,"ts":1}',
            json.dumps(ev).encode(),
            json.dumps(dict(ev, seq=2, ts=now - 1000)).encode(),   # stale
            json.dumps(dict(ev, seq=2)).encode(),                  # already seen
            json.dumps(dict(ev, seq=3, pane="$(id)")).encode(),    # invalid
            b'{"v":1,"type":"hb","head":3,"ts":1}',
            b'{"v":1,"type":"focus","ok":true,"code":"focused"}',
            b'{"v":1,"type":"error","code":"bad_command"}',
            b"garbage",
            b"x" * 5000,
            json.dumps(dict(ev, seq=7, title="it's \"quoted\"")).encode(),
        ]
        log, calls, state = self.both(lambda impl: self.agent_scenario(impl, lines))
        self.assertEqual(log, [
            "drop: no Warp tab with a mosh-client to the VM",
            "stale seq=2 (Ns old), not shown",
            "drop line: bad pane",
            "vm says: {'type': 'focus', 'ok': True, 'code': 'focused'}",
            "vm says: {'type': 'error', 'code': 'bad_command'}",
            "drop line: not json",
            "drop line: too long",
            "drop: no Warp tab with a mosh-client to the VM",
            "session ended (eof after 0s), retry in 2s",
        ])
        self.assertTrue(calls.startswith("-F /dev/null -T -n -i HOME/key -p 22 -o IdentitiesOnly=yes"))
        self.assertTrue(calls.endswith("user@192.0.2.10 stream 0\n"))
        self.assertEqual(state, '{"epoch": "0123456789abcdef", "last_seq": 7}')

    def test_agent_no_key(self):
        log, calls, state = self.both(
            lambda impl: self.agent_scenario(impl, [], with_key=False, run_for=0.5))
        self.assertEqual(log, ["no key at HOME/key; generate it (see README) -- sleeping"])
        self.assertEqual((calls, state), ("", None))

    def test_agent_bad_args(self):
        """Exit codes for a bad or missing option and for --help. The texts
        differ by design: argparse in Python, argh in Rust."""
        def scenario(impl):
            codes = []
            for args in (["--host", "x"], ["--host", "x", "--port", "y"], ["--help"]):
                r = run(argv(impl, "agent") + args, {"PATH": "/usr/bin:/bin"})
                codes.append(r.returncode)
            return codes
        self.assertEqual(self.both(scenario), [2, 2, 0])

    # ------------------------------------------------------------ tab-title

    TAB_SNAP = {
        "tabs": [
            {"tab_id": "w1:t1", "label": "1"},
            {"tab_id": "w1:t2", "label": "my own name"},
            {"tab_id": "w1:t3", "label": "Old topic"},
            {"tab_id": "w1:t4", "label": "4"},
            {"tab_id": "w1:t5", "label": "5"},
            {"tab_id": "w1:t6", "label": "6"},
            {"tab_id": "w1:t7", "label": "7"},
        ],
        "layouts": [{"tab_id": "w1:t1", "focused_pane_id": "w1:p2"}],
        "panes": [
            {"pane_id": "w1:p1", "tab_id": "w1:t1", "agent": "claude", "terminal_title_stripped": "First"},
            {"pane_id": "w1:p2", "tab_id": "w1:t1"},
            {"pane_id": "w1:p3", "tab_id": "w1:t2", "agent": "claude", "terminal_title_stripped": "Topic"},
            {"pane_id": "w1:p4", "tab_id": "w1:t3", "agent": "codex", "terminal_title_stripped": "New  topic\t2"},
            {"pane_id": "w1:p5", "tab_id": "w1:t4", "terminal_title_stripped": "zsh"},
            {"pane_id": "w1:p6", "tab_id": "w1:t5", "agent": "claude",
             "terminal_title_stripped": "Refactor the spool writer to use atomic renames everywhere"},
            {"pane_id": "w1:p7", "tab_id": "w1:t6", "agent": "claude", "terminal_title_stripped": "user@vm:~/src"},
            {"pane_id": "w1:p8", "tab_id": "w1:t7", "agent": "codex", "terminal_title_stripped": "Задача про ёжиков"},
        ],
        "agents": [{"pane_id": "w1:p2", "agent": "codex", "terminal_title_stripped": "Focused one"}],
    }

    def fake_herdr_bin(self, d, snap, fail_rename=None):
        path = os.path.join(d, "herdr")
        with open(os.path.join(d, "snap.json"), "w", encoding="utf-8") as fh:
            json.dump({"result": {"snapshot": snap}}, fh, ensure_ascii=False)
        with open(path, "w") as fh:
            fh.write("#!/bin/sh\n"
                     "d=$(dirname \"$0\")\n"
                     "printf '%%s\\n' \"$*\" >> \"$d/calls.log\"\n"
                     "case \"$1\" in api) cat \"$d/snap.json\" ;; "
                     "tab) [ \"$3\" = %s ] && exit 1; exit 0 ;; esac\n" % (fail_rename or "none"))
        os.chmod(path, 0o755)
        return path

    def tab_scenario(self, impl, args, event=None, event_json=None, labels=None, fail=None):
        e = Env()
        try:
            state = os.path.join(e.home, "plugin-state")
            os.makedirs(state)
            open(os.path.join(state, "debug"), "w").close()
            if labels is not None:
                with open(os.path.join(state, "labels.json"), "w", encoding="utf-8") as fh:
                    fh.write(labels)
            env = dict(e.env, HERDR_BIN_PATH=self.fake_herdr_bin(e.home, self.TAB_SNAP, fail),
                       HERDR_PLUGIN_STATE_DIR=state)
            if event:
                env["HERDR_PLUGIN_EVENT"] = event
            if event_json is not None:
                env["HERDR_PLUGIN_EVENT_JSON"] = event_json
            r = run(argv(impl, "tab-title") + args, env)
            calls = slurp(os.path.join(e.home, "calls.log")) or ""
            lab = slurp(os.path.join(state, "labels.json"))
            log = [l.split(" ", 1)[1]
                   for l in (slurp(os.path.join(state, "debug.log")) or "").splitlines()]
            log = [l.replace(e.home, "HOME") for l in log]
            return r.returncode, r.stdout, calls, lab, log
        finally:
            e.cleanup()

    def test_tab_title_all(self):
        labels = '{"w1:t3": "Old topic", "w1:t2": "was mine", "w9:t1": "closed"}'
        rc, out, calls, lab, log = self.both(
            lambda impl: self.tab_scenario(impl, ["--all"], labels=labels))
        self.assertEqual(rc, 0)
        self.assertEqual(calls.splitlines(), [
            "api snapshot",
            "tab rename w1:t1 Focused one",
            "tab rename w1:t3 New topic 2",
            "tab rename w1:t5 Refactor the spool writer to us…",
            "tab rename w1:t7 Задача про ёжиков",
        ])
        self.assertEqual(json.loads(lab)["w1:t3"], "New topic 2")
        self.assertNotIn("w9:t1", json.loads(lab))

    def test_tab_title_events(self):
        cases = [
            ("pane.focused", json.dumps({"event": "pane_focused", "data": {"pane_id": "w1:p8"}})),
            ("tab.focused", json.dumps({"event": "tab_focused", "data": {"tab": {"tab_id": "w1:t1"}}})),
            ("pane.agent_status_changed", json.dumps({"pane_id": "w1:p99"})),
            ("pane.focused", "garbage"),
            ("startup", None),
            ("pane.focused", json.dumps({"data": {"tab_id": "w1:t3"}})),
        ]
        for event, ej in cases:
            with self.subTest(event=event, json=ej):
                self.both(lambda impl: self.tab_scenario(
                    impl, [], event=event, event_json=ej, labels='{"w1:t3": "Old topic"}'))

    def test_tab_title_rename_failure_and_bad_state(self):
        for labels in ('["not", "a", "dict"]', "{broken", '{"w1:t3": "Old topic", "w1:t9": 5}'):
            with self.subTest(labels=labels):
                self.both(lambda impl: self.tab_scenario(impl, ["--all"], labels=labels, fail="w1:t1"))

    def test_tab_title_no_herdr(self):
        def scenario(impl):
            e = Env()
            try:
                state = os.path.join(e.home, "st")
                env = dict(e.env, HERDR_BIN_PATH=os.path.join(e.home, "missing"),
                           HERDR_PLUGIN_STATE_DIR=state)
                r = run(argv(impl, "tab-title") + ["--all"], env)
                return r.returncode, r.stdout, sorted(os.listdir(state))
            finally:
                e.cleanup()
        self.assertEqual(self.both(scenario), (0, b"", ["lock"]))


if __name__ == "__main__":
    unittest.main()
