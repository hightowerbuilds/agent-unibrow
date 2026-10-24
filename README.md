# agent-unibrow

A GPUI desktop shell where Claude handles email, orders, purchases and browsing
as one conversation. The web engine is one tool ("rail") among several; most
tasks route to APIs and never touch a web page.

## Layout

```
crates/
  shell/            GPUI app: conversation, content area, trace pane, approvals
  agent-core/       Claude session loop, tools, streaming
  router/           Intent → rail selection (Rail trait)
  policy/           Vault, allowlist, spend caps, approval gates
  trace/            Append-only action log
  page-model/       A11y tree + DOM + screenshot → structured input
  evals/            Replayable task fixtures
  rails/
    rail-api/        Gmail, Graph, Calendar, Plaid
    rail-commerce/   ACP / UCP checkout
    rail-aggregator/ Zinc, Crossmint (Amazon)
    rail-browser/    Embedded engine (CEF), last resort
```

Dependencies point one way: shell → agent-core → router ← rails. The `Tool`
and `Rail` traits live in `router`; the shell constructs each rail and
registers it, so no rail knows about another.

## Milestones

- **M0** Shell skeleton: three-pane GPUI window on macOS and Linux.
- **M1** Agent core + Gmail rail: draft replies, every tool call in the trace.
- **M2** Embedded engine + page model.
- **M3** Policy layer + commerce and aggregator rails; first one-step purchase.
- **M4** Long tail via browser, auth handoff, evals, Plaid.

The full list of browser-agent capabilities, phased, is in
[docs/BROWSER_AGENT_ROADMAP.md](docs/BROWSER_AGENT_ROADMAP.md).

## Using it

The sidebar's **Explained** tab (Alt+5) is the in-app guide: what the agent can do, how
to ask well, approvals and Stop, what it won't do, and shortcuts. Its example requests
drop straight into the chat box.

Everything the app saves goes to one folder, `~/Desktop/unibrow-workspace`:
`Screenshots` (the toolbar camera), `Downloads`, `Pages` (pages saved as PDF), `Chats`
(exported chats) and `Files` (new files the agent writes, the one place it may create
files without asking). The sidebar's **Workspace** tab (Alt+6) browses it, with
thumbnails, open, rename, delete (to the Trash) and show in the file manager. The agent
can save screenshots there too ("save a screenshot of this page"), with passwords and
card numbers masked.

**Pop out** (⤢ at the top of the sidebar, or Ctrl+Shift+B) turns the sidebar into a tab in
the tab strip, next to your web tabs, with chat, history and the rest at full size; the
Sidebar tab's × puts it back beside the page.

**Change log**: every file the agent changes is kept with the message that asked for it.
A strip after each reply lists the changes with Undo, and History → Changes shows all of
them and which chat made each (undo only touches files still as the agent left them).
Playbooks keep their earlier versions too: saving one again, or removing it, keeps the old
steps, and the site's view in Focus can restore any version.

**Split view** shows 2 to 4 tabs at once: Ctrl+click a tab, or use the toolbar's split
button. The agent can set one up and look at several open tabs at once
(`browser_view_tabs`) without switching away from the one it's working in.

## Run

```
cargo run -p shell
```

### Agents

Pick one in the bar at the top of the conversation pane (saved in
`~/.config/agent-unibrow/settings.json`):

| Agent | Runs | Signed in with |
| --- | --- | --- |
| Claude Code (default) | `claude -p`, one long-lived process | your Claude subscription |
| Codex | `codex exec --json`, resumed per message | your ChatGPT sign-in |
| OpenCode | `opencode run --format json` | a provider from `opencode auth login` (set `opencode_model`) |
| API key | Messages API directly | an Anthropic API key |

The CLI agents get the app's tools from a local MCP server
(`127.0.0.1`, random port, per-launch bearer token). Their own built-in
tools (shell, file editing, browser) are switched off, so they can only act
through the app. Every tool call, from any agent, goes through the same
policy and approval cards, and shows in the trace.

The app only runs the `claude` command; it never reads or reuses Claude's
sign-in. OpenCode can't use a Claude subscription (Anthropic doesn't allow
subscription sign-ins in third-party tools), and its free models refuse
requests that don't come from OpenCode's own app.

### Setup

- **Gmail**: create a Google OAuth client of type *Desktop app* with the Gmail
  API enabled and yourself as a test user, then save its JSON as
  `~/.config/agent-unibrow/google_client.json`. The first Gmail tool call opens
  the consent screen; the refresh token is stored in the keyring.
- **API key agent only**: set `ANTHROPIC_API_KEY`, or paste a key into the app
  when it asks (saved to the GNOME keyring).

Run the browser-agent eval suite (real agent turns on hidden, isolated
instances against the local test pages in `crates/evals/fixtures`):

```
cargo build -p shell
python3 crates/evals/run_evals.py          # or name cases: ... tabs widgets
```

Smoke-test an agent without the UI (clock tool plus a fake send that waits for
approval and is denied):

```
cargo run -p agent-core --example headless -- claude_code "What time is it?"
```

Traces are written to `~/.local/share/agent-unibrow/traces/` (one JSONL file
per session). Send approvals marked "always allow" are kept in
`~/.config/agent-unibrow/policy.json`.

### API-key agent settings

`claude-opus-5-5` at `high` effort with adaptive thinking, streaming, prompt
caching, server-side refusal fallbacks (`fallbacks: "default"`), and progress
notes between tool calls (`thinking.display: "updates"`). See
`crates/agent-core/src/client.rs`.

GPUI is pinned to an exact version (`=0.2.2`); upgrade deliberately.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
