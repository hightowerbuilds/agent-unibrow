#!/usr/bin/env python3
"""Browser-agent eval suite (roadmap 7.5).

Runs each case in crates/evals/cases.json against a real, isolated
agent-unibrow instance:

- the test pages in crates/evals/fixtures are served on localhost;
- every case gets its own data folder (profile, history, trace, logins), so
  nothing touches the user's own session;
- on Hyprland the instances open on a hidden special workspace and never
  take focus;
- outcomes are checked against the instance's trace file.

Usage:
    cargo build -p shell
    python3 crates/evals/run_evals.py            # all cases
    python3 crates/evals/run_evals.py tabs widgets  # some cases

Case options beyond the basics: `{base}` in a task is replaced with the
fixture server's address, and `{alt}` with the same server under another
host name (127.0.0.1, a different site); `policy` is written to the
instance's policy.json before it starts; `focus` ("site,site") puts sites
in focus; `after_turn` can change a served page once the
agent is done (`mutate`: file/from/to) and keep waiting for a line in the
trace (`wait_trace`), which is how watches are tested; `watch_fast` makes
watches re-check every few seconds; `autosteer` ("secs|text") types a
message while the agent works; `env` sets extra environment variables
(such as `UNIBROW_TEST_PANE` steps that zoom or right-click as the user
would, see browser_pane.rs); `relaunch` kills the instance after the
first turn (a crash) and starts it again on the same data with a new task
(optionally on another `page`, after `mutate`-ing served pages); `inbox`
is a fake mailbox of `{from, subject, body, seconds_ago}` for codes;
`tools_any` lists groups of tools where any one being called is enough
(e.g. browser_type or browser_fill_form), and `reply_any` groups of words
where any one in the reply is enough; `kill_renderer_secs` kills the
instance's renderer processes (only ones carrying its data folder) that
many seconds in; `log_contains` checks the instance's stderr log (and
`log_not_contains` that a line never appeared). `{alt}`
is the fixture server under 127.0.0.1 (another site), `{tls}` the same
fixtures over HTTPS with a self-signed certificate, and `/auth/<page>`
answers 401 (HTTP Basic, user `ada`, password `lovelace`) until signed in.
A `page` of `home` starts on the app's home page, as a real launch does;
`home_layout` seeds its home.json and `home_files` its home/ folder.
`sandbox_files` seeds the file tools' sandbox (`files/` in the instance's
data folder, which is `~` to them), and `expect.files` ({path: text it
must contain, or ""}) and `expect.files_absent` check paths in the data
folder afterwards (`files/…`, or `Trash/files/…` for deleted items).
A case without a `task` runs no agent turn (no quota used): the instance
runs its `UNIBROW_TEST_PANE` steps for `secs` seconds and only the log is
checked (`log_contains`, `log_not_contains`); the media cases work this way.

Results are printed and saved to crates/evals/results/<timestamp>.json.
To run the suite every night, see crates/evals/nightly/ (a systemd user
timer and the script it starts; nothing installs them for you).
Each case runs a real agent turn on the configured backend (Claude Code by
default), which uses your subscription or API quota.
"""

import base64
import functools
import http.server
import json
import os
import shutil
import signal
import socketserver
import ssl
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
EVALS = Path(__file__).resolve().parent
BIN = ROOT / "target" / "debug" / "agent-unibrow"
TEST_CLASS = "agent-unibrow-eval"
CASE_TIMEOUT = 300


AUTH_OK = "Basic " + base64.b64encode(b"ada:lovelace").decode()


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def end_headers(self):
        # Cases change pages mid-run (watches, redesigns); never serve stale copies.
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def do_GET(self):
        # /auth/<page>: the page behind an HTTP Basic sign-in.
        if self.path.startswith("/auth/"):
            if self.headers.get("Authorization") != AUTH_OK:
                body = b"<!doctype html><title>401 Unauthorized</title><h1>401 Unauthorized</h1>"
                self.send_response(401)
                self.send_header("WWW-Authenticate", 'Basic realm="Unibrow staff area"')
                self.send_header("Content-Type", "text/html")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            self.path = self.path[len("/auth"):]
        super().do_GET()


def serve_fixtures(root: Path):
    # Served from a copy, so cases can change pages without touching the repo.
    shutil.copytree(EVALS / "fixtures", root)
    handler = functools.partial(QuietHandler, directory=str(root))
    server = socketserver.ThreadingTCPServer(("127.0.0.1", 0), handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def serve_tls(root: Path, workdir: Path):
    """The fixtures over HTTPS with a throwaway self-signed certificate."""
    cert, key = workdir / "tls-cert.pem", workdir / "tls-key.pem"
    subprocess.run(
        ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2", "-subj", "/CN=localhost",
         "-keyout", str(key), "-out", str(cert)],
        check=True, capture_output=True,
    )
    handler = functools.partial(QuietHandler, directory=str(root))
    server = socketserver.ThreadingTCPServer(("127.0.0.1", 0), handler)
    server.daemon_threads = True
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(str(cert), str(key))
    server.socket = ctx.wrap_socket(server.socket, server_side=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def kill_renderers(data_dir: Path):
    """SIGKILLs the renderer processes of the instance using `data_dir`, and
    only those: each one's environment must carry that data folder. On Linux
    renderers are forked from the zygote and keep its `--type=zygote`
    command line, so they're the zygote's own zygote-typed children."""
    marker = f"UNIBROW_DATA_DIR={data_dir}".encode()
    procs = {}
    for proc in Path("/proc").iterdir():
        if not proc.name.isdigit():
            continue
        try:
            cmd = (proc / "cmdline").read_bytes().split(b"\0")
            env = (proc / "environ").read_bytes().split(b"\0")
            ppid = int((proc / "stat").read_text().rsplit(") ", 1)[1].split()[1])
        except (OSError, ValueError, IndexError):
            continue
        if marker in env:
            procs[int(proc.name)] = (ppid, cmd)

    def kind(pid):
        return next((c for c in procs.get(pid, (0, []))[1] if c.startswith(b"--type=")), None)

    killed = []
    for pid, (ppid, _) in procs.items():
        grandparent = procs.get(ppid, (0, []))[0]
        forked = kind(pid) == b"--type=zygote" and kind(ppid) == b"--type=zygote" and kind(grandparent) == b"--type=zygote"
        if kind(pid) == b"--type=renderer" or forked:
            try:
                os.kill(pid, signal.SIGKILL)
                killed.append(pid)
            except OSError:
                pass
    return killed


def hide_eval_windows():
    if shutil.which("hyprctl"):
        rule = 'hl.window_rule({ match = { class = "%s" }, workspace = "special:unibroweval silent" })' % TEST_CLASS
        subprocess.run(["hyprctl", "eval", rule], capture_output=True)


def read_trace(data_dir: Path, since: float = 0):
    """The newest trace file (only ones written after `since`)."""
    traces = sorted(p for p in (data_dir / "traces").glob("*.jsonl") if p.stat().st_ctime >= since - 1)
    if not traces:
        return []
    out = []
    for line in traces[-1].read_text(errors="replace").splitlines():
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    return out


def wait_turn(data: Path, proc, since: float):
    """Waits for the newest trace to show a finished turn (or the process to exit)."""
    trace = []
    while time.time() - since < CASE_TIMEOUT:
        time.sleep(2)
        trace = read_trace(data, since)
        if any(e.get("summary", "").startswith("Turn finished") for e in trace):
            break
        if proc.poll() is not None:
            break
    return trace


def fill_text(text: str, port, tls_port=None) -> str:
    text = text.replace("{base}", f"http://localhost:{port}").replace("{alt}", f"http://127.0.0.1:{port}")
    return text.replace("{tls}", f"https://localhost:{tls_port}") if tls_port else text


def run_case(case, port, workdir: Path, site: Path, tls_port=None):
    data = workdir / case["name"]
    env = dict(os.environ)

    def fill(text):
        return fill_text(text, port, tls_port)

    env.update(
        UNIBROW_DATA_DIR=str(data),
        UNIBROW_APP_ID=TEST_CLASS,
        UNIBROW_START_URL=fill(case["page"]) if "://" in fill(case["page"]) else f"http://localhost:{port}/{case['page']}",
        UNIBROW_AUTOAPPROVE=case.get("approve", "deny"),
    )
    if case["page"] == "home":
        # The app's own start page, as on a real launch.
        env.pop("UNIBROW_START_URL")
    if case.get("task"):
        env["UNIBROW_AUTOSEND"] = fill(case["task"])
    env.update({k: fill(str(v)) for k, v in case.get("env", {}).items()})
    if "policy" in case:
        data.mkdir(parents=True, exist_ok=True)
        (data / "policy.json").write_text(json.dumps(case["policy"]))
    if "home_layout" in case:
        # The home page's layout (home.json in the instance's config folder).
        data.mkdir(parents=True, exist_ok=True)
        (data / "home.json").write_text(json.dumps(case["home_layout"]))
    if "home_files" in case:
        # The user's own files for the home page (user.css, index.html).
        (data / "home").mkdir(parents=True, exist_ok=True)
        for name, text in case["home_files"].items():
            if text.startswith("base64:"):
                (data / "home" / name).write_bytes(base64.b64decode(text[len("base64:"):]))
            else:
                (data / "home" / name).write_text(text)
    if "sandbox_files" in case:
        # The user's own files, as the file tools see them (the sandbox
        # home, $UNIBROW_DATA_DIR/files).
        for name, text in case["sandbox_files"].items():
            f = data / "files" / name
            f.parent.mkdir(parents=True, exist_ok=True)
            f.write_text(text)
    if "focus" in case:
        env["UNIBROW_TEST_FOCUS"] = case["focus"]
    if "autostop_secs" in case:
        env["UNIBROW_AUTOSTOP_SECS"] = str(case["autostop_secs"])
    if "test_login" in case:
        env["UNIBROW_TEST_LOGIN"] = case["test_login"]
    if "autohandoff" in case:
        env["UNIBROW_AUTOHANDOFF"] = case["autohandoff"]
    if "inbox" in case:
        # A fake inbox standing in for Gmail (verification codes).
        inbox = workdir / f"{case['name']}-inbox.json"
        inbox.write_text(json.dumps(case["inbox"]))
        env["UNIBROW_TEST_INBOX"] = str(inbox)
    if "autosteer" in case:
        env["UNIBROW_AUTOSTEER"] = case["autosteer"]
    if case.get("watch_fast"):
        env["UNIBROW_WATCH_FAST"] = "1"
    after = case.get("after_turn", {})

    log = open(workdir / f"{case['name']}.log", "w")
    start = time.time()
    if "relaunch" in case:
        # First run, then a hard kill: a crash, not a clean exit.
        first = subprocess.Popen([str(BIN)], env=env, stdout=log, stderr=log, start_new_session=True)
        wait_turn(data, first, start)
        first.kill()
        first.wait()
        time.sleep(1)
        env["UNIBROW_AUTOSEND"] = fill(case["relaunch"]["task"])
        if "page" in case["relaunch"]:
            env["UNIBROW_START_URL"] = f"http://localhost:{port}/{case['relaunch']['page']}"
        # Change served pages between the two runs (a site redesign).
        for m in case["relaunch"].get("mutate", []):
            f = site / m["file"]
            f.write_text(f.read_text().replace(m["from"], m["to"]))
    proc = subprocess.Popen([str(BIN)], env=env, stdout=log, stderr=log, start_new_session=True)
    if "kill_renderer_secs" in case:
        def killer():
            time.sleep(case["kill_renderer_secs"])
            if proc.poll() is None:
                pids = kill_renderers(data)
                print(f"     killed renderer(s) {pids}", flush=True)
        threading.Thread(target=killer, daemon=True).start()
    trace = []
    try:
        if not case.get("task"):
            # Pane-only case: let the test steps run, then stop. Steps can
            # still start a turn (the home page's Ask box), so read the trace.
            deadline = time.time() + case.get("secs", 30)
            while time.time() < deadline and proc.poll() is None:
                time.sleep(1)
        else:
            trace = wait_turn(data, proc, time.time())
        if after and proc.poll() is None:
            time.sleep(after.get("delay_secs", 0))
            m = after.get("mutate")
            if m:
                f = site / m["file"]
                f.write_text(f.read_text().replace(m["from"], m["to"]))
            want = after.get("wait_trace")
            deadline = time.time() + after.get("timeout", 60)
            while want and time.time() < deadline:
                time.sleep(2)
                trace = read_trace(data, start)
                if any(want.lower() in json.dumps(e, ensure_ascii=False).lower() for e in trace):
                    break
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
    if not case.get("task"):
        # Steps can still start a turn or act (the home page's Ask box and
        # buttons): read the trace, once the instance has written it out.
        trace = read_trace(data, start)
    if after.get("wait_trace"):
        case.setdefault("expect", {}).setdefault("trace_contains", []).append(after["wait_trace"])
    log.close()
    return check(case, trace, data, time.time() - start, workdir / f"{case['name']}.log")


def check(case, trace, data: Path, seconds, log_path=None):
    exp = case.get("expect", {})
    failures = []
    log_text = log_path.read_text(errors="replace") if log_path and log_path.exists() else ""
    for s in exp.get("log_contains", []):
        if s.lower() not in log_text.lower():
            failures.append(f"log is missing “{s}”")
    for s in exp.get("log_not_contains", []):
        if s.lower() in log_text.lower():
            failures.append(f"log contains “{s}”")
    finished = [e for e in trace if e.get("summary", "").startswith("Turn finished")]
    if not finished and case.get("task"):
        failures.append("the turn never finished (timeout or crash)")
    reply = (finished[-1].get("detail") or "").split("\n\nmodel:")[0] if finished else ""
    summaries = "\n".join(e.get("summary", "") for e in trace)
    calls = [e["summary"].split(" ")[0] for e in trace if e.get("kind") == "tool_call"]
    raw = "\n".join(json.dumps(e, ensure_ascii=False) for e in trace)

    for s in exp.get("reply_contains", []):
        if s.lower() not in reply.lower():
            failures.append(f"reply is missing “{s}”")
    for group in exp.get("reply_any", []):
        if not any(s.lower() in reply.lower() for s in group):
            failures.append(f"reply has none of {group}")
    for s in exp.get("reply_not_contains", []):
        if s.lower() in reply.lower():
            failures.append(f"reply contains “{s}”")
    for s in exp.get("trace_contains", []):
        if s.lower() not in raw.lower():
            failures.append(f"trace is missing “{s}”")
    for t in exp.get("tools", []):
        if t not in calls:
            failures.append(f"{t} was never called")
    for group in exp.get("tools_any", []):
        if not any(t in calls for t in group):
            failures.append(f"none of {', '.join(group)} was called")
    for t in exp.get("tools_not", []):
        if t in calls:
            failures.append(f"{t} was called but shouldn't be")
    if exp.get("no_tool_calls") and calls:
        failures.append(f"the agent was used ({', '.join(calls)}) but Jev should have handled it")
    if exp.get("approval_requested") and not any(e.get("kind") == "approval_requested" for e in trace):
        failures.append("no approval was requested")
    for name, want in exp.get("files", {}).items():
        f = data / name
        if not f.exists():
            failures.append(f"{name} doesn't exist")
        elif want and want not in f.read_text(errors="replace"):
            failures.append(f"{name} is missing “{want}”")
    for name in exp.get("files_absent", []):
        if (data / name).exists():
            failures.append(f"{name} exists but shouldn't")
    for secret in exp.get("never_seen", []):
        if secret in raw:
            failures.append(f"secret “{secret}” appears in the trace")

    return {
        "name": case["name"],
        "passed": not failures,
        "failures": failures,
        "seconds": round(seconds, 1),
        "tool_calls": calls,
        "reply": reply[:2000],
    }


def main():
    if not BIN.exists():
        sys.exit("Build first: cargo build -p shell")
    cases = json.loads((EVALS / "cases.json").read_text())
    wanted = set(sys.argv[1:])
    if wanted:
        cases = [c for c in cases if c["name"] in wanted]
    workdir = Path(tempfile.mkdtemp(prefix="unibrow-evals-"))
    site = workdir / "site"
    server, port = serve_fixtures(site)
    tls_server, tls_port = (None, None)
    if any("{tls}" in c.get("task", "") or "{tls}" in c["page"] for c in cases):
        tls_server, tls_port = serve_tls(site, workdir)
    hide_eval_windows()
    results = []
    try:
        for case in cases:
            print(f"… {case['name']}", flush=True)
            r = run_case(case, port, workdir, site, tls_port)
            results.append(r)
            mark = "PASS" if r["passed"] else "FAIL"
            print(f"{mark} {r['name']} ({r['seconds']}s, {len(r['tool_calls'])} tool calls)")
            for f in r["failures"]:
                print(f"     - {f}")
    finally:
        server.shutdown()
        if tls_server:
            tls_server.shutdown()
    passed = sum(r["passed"] for r in results)
    print(f"\n{passed}/{len(results)} passed")
    out_dir = EVALS / "results"
    out_dir.mkdir(exist_ok=True)
    out = out_dir / time.strftime("%Y-%m-%d_%H-%M-%S.json")
    out.write_text(json.dumps({"passed": passed, "total": len(results), "cases": results}, indent=2))
    print(f"Saved {out.relative_to(ROOT)}  (instance data: {workdir})")
    sys.exit(0 if passed == len(results) else 1)


if __name__ == "__main__":
    main()
