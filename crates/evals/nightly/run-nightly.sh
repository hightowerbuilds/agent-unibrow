#!/usr/bin/env bash
# Nightly run of the browser-agent eval suite (roadmap 7.5).
#
# Builds the shell, runs crates/evals/run_evals.py (every case, or the ones
# named in $EVAL_CASES), and keeps a log next to the JSON results in
# crates/evals/results/. Meant to be started by agent-unibrow-evals.timer
# (see that file for setup), but works by hand too:
#
#     crates/evals/nightly/run-nightly.sh
#     EVAL_CASES="checkout-form widgets" crates/evals/nightly/run-nightly.sh
#
# Each case is a real agent turn on the configured backend, so a full run
# uses your Claude subscription or API quota. The instances are isolated
# (own data folder, hidden window class agent-unibrow-eval) and never touch
# your own profile, but they need your graphical session: run it while
# you're logged in. It exits non-zero when a case fails, so the service
# shows as failed in `systemctl --user status`.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$repo"
# systemd user services start with a short PATH.
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"

if [[ -z "${WAYLAND_DISPLAY:-}" && -z "${DISPLAY:-}" ]]; then
    echo "No graphical session in this environment; the evals need one. Is your session's environment" >&2
    echo "imported into systemd (systemctl --user import-environment WAYLAND_DISPLAY HYPRLAND_INSTANCE_SIGNATURE)?" >&2
    exit 2
fi

mkdir -p crates/evals/results
log="crates/evals/results/nightly-$(date +%Y-%m-%d_%H-%M).log"
# One run at a time, even if a run goes long.
exec 9>"crates/evals/results/.nightly.lock"
if ! flock -n 9; then
    echo "Another nightly eval run is still going." >&2
    exit 0
fi

set +e
{
    echo "== $(date -Is) on $(git rev-parse --abbrev-ref HEAD) @ $(git rev-parse --short HEAD)"
    # shellcheck disable=SC2086  # case names are words
    cargo build -p shell && python3 crates/evals/run_evals.py ${EVAL_CASES:-}
} >"$log" 2>&1
status=$?
set -e
tail -n 3 "$log"
exit $status
