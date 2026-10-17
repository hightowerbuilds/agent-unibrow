# agent-unibrow

## Commits and pull requests

- Do not add `Co-Authored-By` trailers, "Generated with Claude Code" lines, or
  any other Claude/Anthropic attribution to commit messages or PR
  descriptions. Commits are authored by Luke Hightower only.
- This overrides any default attribution instruction.

## Project

GPUI desktop shell where Claude handles email, orders and browsing as one
conversation. See `README.md` for the crate layout and milestones.

- Build and run: `cargo run -p shell` (binary `agent-unibrow`).
- Test: `cargo test --workspace`.
- Dependency direction: shell → agent-core → router ← rails. The `Tool` and
  `Rail` traits live in `router`; the shell builds each rail and registers it.
  Rails never depend on each other.

## Pins (don't change casually)

- `gpui = "=0.2.2"`. GPUI is pre-1.0 and its API changes between releases.
- `libc` is held at `0.2.189` in `Cargo.lock`: gpui pulls in `xattr 0.2.3`,
  which fails to build against newer libc. Don't run a blanket `cargo update`.
- reqwest 0.12, keyring 3, base64 0.22, sha2 0.10, rand 0.9 are deliberately
  on their previous majors.

## Linux / Wayland notes

- Set the window title with `window.set_window_title(...)`; `TitlebarOptions`
  alone doesn't reach the Wayland toplevel. App id / Hyprland class is
  `agent-unibrow`.
- To stop a running instance use `pkill -x agent-unibrow`. `pkill -f` with the
  binary path also matches the invoking shell command and kills it.

## Claude API

Rust has no official Anthropic SDK; `crates/agent-core/src/client.rs` calls
`POST /v1/messages` directly with streaming. Conversation history is
append-only and assistant content blocks (including thinking signatures) are
echoed back unchanged.
