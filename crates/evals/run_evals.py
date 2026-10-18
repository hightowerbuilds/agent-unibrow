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

Results are printed and saved to crates/evals/results/<timestamp>.json.
Each case runs a real agent turn on the configured backend (Claude Code by
default), which uses your subscription or API quota.
"""

import functools
import http.server
import json
import os
import shutil
import socketserver
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


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


def serve_fixtures():
    handler = functools.partial(QuietHandler, directory=str(EVALS / "fixtures"))
    server = socketserver.TCPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def hide_eval_windows():
    if shutil.which("hyprctl"):
        rule = 'hl.window_rule({ match = { class = "%s" }, workspace = "special:unibroweval silent" })' % TEST_CLASS
        subprocess.run(["hyprctl", "eval", rule], capture_output=True)


def read_trace(data_dir: Path):
    traces = sorted((data_dir / "traces").glob("*.jsonl"))
    if not traces:
        return []
    out = []
    for line in traces[-1].read_text(errors="replace").splitlines():
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    return out


def run_case(case, port, workdir: Path):
    data = workdir / case["name"]
    env = dict(os.environ)
    env.update(
        UNIBROW_DATA_DIR=str(data),
        UNIBROW_APP_ID=TEST_CLASS,
        UNIBROW_START_URL=f"http://localhost:{port}/{case['page']}",
        UNIBROW_AUTOSEND=case["task"],
        UNIBROW_AUTOAPPROVE=case.get("approve", "deny"),
    )
    if "autostop_secs" in case:
        env["UNIBROW_AUTOSTOP_SECS"] = str(case["autostop_secs"])
    if "test_login" in case:
        env["UNIBROW_TEST_LOGIN"] = case["test_login"]
    if "autohandoff" in case:
        env["UNIBROW_AUTOHANDOFF"] = case["autohandoff"]

    log = open(workdir / f"{case['name']}.log", "w")
    proc = subprocess.Popen([str(BIN)], env=env, stdout=log, stderr=log, start_new_session=True)
    start = time.time()
    trace = []
    try:
        while time.time() - start < CASE_TIMEOUT:
            time.sleep(2)
            trace = read_trace(data)
            if any(e.get("summary", "").startswith("Turn finished") for e in trace):
                break
            if proc.poll() is not None:
                break
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
    return check(case, trace, data, time.time() - start)


def check(case, trace, data: Path, seconds):
    exp = case.get("expect", {})
    failures = []
    finished = [e for e in trace if e.get("summary", "").startswith("Turn finished")]
    if not finished:
        failures.append("the turn never finished (timeout or crash)")
    reply = (finished[-1].get("detail") or "").split("\n\nmodel:")[0] if finished else ""
    summaries = "\n".join(e.get("summary", "") for e in trace)
    calls = [e["summary"].split(" ")[0] for e in trace if e.get("kind") == "tool_call"]
    raw = "\n".join(json.dumps(e) for e in trace)

    for s in exp.get("reply_contains", []):
        if s.lower() not in reply.lower():
            failures.append(f"reply is missing “{s}”")
    for s in exp.get("reply_not_contains", []):
        if s.lower() in reply.lower():
            failures.append(f"reply contains “{s}”")
    for s in exp.get("trace_contains", []):
        if s.lower() not in raw.lower():
            failures.append(f"trace is missing “{s}”")
    for t in exp.get("tools", []):
        if t not in calls:
            failures.append(f"{t} was never called")
    for t in exp.get("tools_not", []):
        if t in calls:
            failures.append(f"{t} was called but shouldn't be")
    if exp.get("approval_requested") and not any(e.get("kind") == "approval_requested" for e in trace):
        failures.append("no approval was requested")
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
    server, port = serve_fixtures()
    hide_eval_windows()
    workdir = Path(tempfile.mkdtemp(prefix="unibrow-evals-"))
    results = []
    try:
        for case in cases:
            print(f"… {case['name']}", flush=True)
            r = run_case(case, port, workdir)
            results.append(r)
            mark = "PASS" if r["passed"] else "FAIL"
            print(f"{mark} {r['name']} ({r['seconds']}s, {len(r['tool_calls'])} tool calls)")
            for f in r["failures"]:
                print(f"     - {f}")
    finally:
        server.shutdown()
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
