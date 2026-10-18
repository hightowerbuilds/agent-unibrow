# Homepage roadmap

The page people land on when they open the app or a new tab: served by the
app itself, not from the web, and the front door to everything you can
adjust: the agent and model, focus sites and Jev, watches, playbooks,
safety settings, the guide, and the page itself.

Status: ✅ **COMPLETED** (built and tested) · ◐ partial (started; what is left is noted) · todo.

## Where things stand

| Area | Status | Notes |
| --- | --- | --- |
| First page on launch | todo | `HOME = "https://duckduckgo.com/"` in `crates/shell/src/browser_pane.rs`; `UNIBROW_START_URL` overrides it (evals use this) |
| New tab (Ctrl+T, + button) | todo | `BrowserPane::new_tab` opens `HOME` and focuses an empty address bar |
| Last tab closed | todo | `close_tab` reopens `HOME` |
| Agent's new tab with no URL | todo | `TabOp::New` in `browser_pane.rs` falls back to `HOME` too |
| App-served pages | todo | Only reader mode, as a `data:` URL (`rail-browser/src/reader.rs`) |
| Places to tinker | ✅ in the sidebar | History (History/Bookmarks/Trace), Focus (sites, Learn with Jev, per-site View, watches, logins, safety), Explained (Alt+5), Model menu |

### Progress by phase

| Phase | Completed | Partial | Todo |
| --- | --- | --- | --- |
| H1. A local home page | | | H1.1–H1.5 |
| H2. Live data | | | H2.1–H2.4 |
| H3. Acting from the page | | | H3.1–H3.5 |
| H4. The tinkering hub | | | H4.1–H4.4 |
| H5. Making it yours | | | H5.1–H5.5 |

## How the page is served

### Options

| Option | For | Against |
| --- | --- | --- |
| **Custom scheme** `unibrow://home/` (CEF scheme handler) | Stable URL that can be typed and reloaded; real origin, so the page can `fetch` its own data; isolation flags keep web pages out; no port, no socket; assets ship inside the binary | Must be registered in every process at startup |
| Localhost HTTP server (like the MCP server in `agent-core/src/mcp.rs`) | Easy to write, easy to open in another browser | Any local program and any web page can reach `127.0.0.1` (form POSTs, DNS rebinding); random port changes the URL every launch, so bookmarks and Back break; needs token and Host checks on every request |
| Files from disk (`file://`) | Users can edit them directly | Opaque origin; giving `file://` a bridge would give it to every local HTML file; data has to be pushed in from outside |
| Generated `data:` URL (what reader mode does) | No setup at all | Opaque origin, can't fetch, huge URL in the address bar, can't be typed or reloaded |

### Recommendation: a `unibrow://` scheme

- Register `unibrow` in `App::on_register_custom_schemes` (the `wrap_app!`
  block in `crates/rails/rail-browser/src/handlers.rs`). The same `App`
  is passed to `cef::execute_process` (`lib.rs`), so renderers get it too.
- Flags: `STANDARD | SECURE | DISPLAY_ISOLATED | FETCH_ENABLED`. Not
  `CORS_ENABLED`, not `CSP_BYPASSING`, not `LOCAL`. `DISPLAY_ISOLATED`
  means pages from other schemes can't link to it, frame it or open it;
  without `CORS_ENABLED`, web pages can't `fetch` it.
- After `cef::initialize`, call `cef::register_scheme_handler_factory("unibrow", "", factory)`.
  The factory returns a `ResourceHandler` that serves embedded assets and
  answers the page's API. Code: a new `crates/rails/rail-browser/src/app_pages.rs`.
- **The bridge is plain same-origin `fetch`**: `GET /api/...` for data,
  `POST /api/cmd` for actions, answered in the browser process. No
  JavaScript is injected into renderers. Rejected alternatives:
  - CDP `Runtime.addBinding`: the binding stays when the tab navigates to a
    website, so every call would need an origin check.
  - cefQuery (`cef::wrapper::message_router`): needs a render-process
    handler in every renderer. Keep it in reserve in case streaming is
    ever needed.
- Security rules, all enforced on the app side:
  - The factory gets the request's `Frame`. API requests are served only
    when that frame is a main frame whose URL is on `unibrow://home`.
  - `POST /api/cmd` also needs a per-launch token, written into the served
    HTML (the same idea as the MCP bearer token).
  - Every response carries
    `Content-Security-Policy: default-src 'self'; img-src 'self' data:; frame-ancestors 'none'`.
    Nothing is loaded from the web, so no outside script ever runs on the
    home origin.
  - In `handlers.rs`, `on_before_browse` and the popup handler refuse
    navigations and new-tab requests to `unibrow://` that a web page
    starts (defense in depth beyond `DISPLAY_ISOLATED`). Only the address
    bar, the shell, and `unibrow://` pages themselves can open it.
  - Agent tools refuse to act on `unibrow://` pages (see H1.4). The agent
    drives pages through CDP input, which a page can't tell apart from the
    user, so the check belongs in the tool layer.
  - The API never returns secrets (key values, passwords, tokens). It
    never changes safety settings either: focus-only mode, always-allow
    rules, spend limits and saved logins stay in the native UI.
- Ownership: `rail-browser` handles the scheme plumbing and defines an
  `AppPages` trait (`fn get(path, query) -> Response`,
  `fn command(json) -> Result<json>`). The shell implements it in a new
  `crates/shell/src/home.rs`, with the page in `crates/shell/home/`
  (`index.html`, `home.css`, `home.js`, `include_str!`'d). This matches
  the existing dependency direction: shell → rail-browser.
- Threads: CEF calls scheme handlers on its IO thread. `AppPages` is
  `Send + Sync`:
  - It reads `Arc<Browsing>` (already behind a mutex) directly.
  - Anything only the `Shell` knows (settings, whether the agent is busy,
    key status) comes from an `Arc<RwLock<HomeSnapshot>>` that the shell
    refreshes.
  - Commands go to the `Shell` over an `async_channel` and run on the UI
    thread, the same way `jev_jobs` and the watch receiver work.

---

## Phase H1: A local home page (opens on launch and in new tabs)

### H1.1 The `unibrow://` scheme — todo
- Register the scheme and its handler factory as described above. Static
  assets are served with the right MIME types, plus a 404 for anything
  else. Paths with `..` or `%2e` are refused.
- Home tabs are created with a background color from `theme::BG`, so they
  don't flash white (`settings.background_color` is `0xFFFFFFFF` today).
- Unit tests in `app_pages.rs`: path routing, traversal refused, the CSP
  header on every response.

### H1.2 Home on launch, new tab and last-tab-closed — todo
- `HOME` in `browser_pane.rs` becomes `unibrow://home/`.
  `UNIBROW_START_URL` still wins, so every existing eval keeps its start
  page.
- `TabOp::New` with no URL (the agent's background tab) opens
  `about:blank` instead. The agent has nothing to do on the home page.
- `normalize_address` (`rail-browser/src/lib.rs`) already passes
  `unibrow://…` through unchanged. Typing `home` alone stays a search.
- Eval: `home-launch` (no task, `page: "unibrow://home/"`, steps
  `wait;log;js:document.title` → `log_contains` `url=unibrow://home/` and
  `js => Home`).
- Eval: `home-newtab` needs a new `newtab` pane step in
  `run_test_step` (the Ctrl+T path) → the new tab's URL is home.
- Harness: let a case leave `UNIBROW_START_URL` unset (e.g.
  `"page": "home"`), so the real launch path gets tested.

### H1.3 Browser chrome on internal pages — todo
- On home, the address bar is empty and focused (`new_tab` already does
  this), the tab is titled "Home" with the app icon (`/favicon.png`), and
  the ◎ Focus and bookmark toolbar buttons are hidden.
- Not recorded in history: `browsing::site_of` already returns `None` for
  anything other than http(s). Zoom is keyed by scheme (`zoom_key`).
- Eval: `home-not-history` (open home, then a fixture page; the History
  count is 1 and `log_not_contains` `unibrow://`).

### H1.4 Lockdown — todo
- `on_before_browse` and the popup handler in `handlers.rs` refuse
  web-initiated navigations and new tabs to `unibrow://`.
- Browser tools (`rail-browser/src/tools.rs`, next to
  `scope::current_site`) refuse click, type, press, drag, fill, upload and
  select on a `unibrow://` page: "This is the app's home page; open a site
  first." `browser_read` and `browser_screenshot` return that same short
  note instead of the page, so the agent uses `history_search` and the
  focus tools rather than scraping the home page.
- Eval: `home-isolation`. A fixture `home-probe.html` on `{base}` tries
  `fetch('unibrow://home/api/state')`, an iframe, `window.open`, a link
  click, and a form POST to `/api/cmd`. A `js:` step logs each outcome;
  every one must be blocked, and no tab ends up on home.
- Eval: `home-agent-refuses` (task on `page: "unibrow://home/"`: "type
  hello into the search box"; `trace_contains` the refusal and
  `reply_any` mentions opening a site).

### H1.5 First content — todo
- A static page in the app's palette:
  - A search box: a plain `<form>` to DuckDuckGo, so no bridge is needed
    yet.
  - Four starter requests as text.
  - The main shortcuts (Ctrl+L, Ctrl+T, Alt+1–5, Esc to stop).
  - A line pointing to Explained (Alt+5).
- Eval: `home-search` (no task; steps `clicksel:#q;type:bass nut` plus
  Enter; `log` shows the tab on `duckduckgo.com/?q=bass+nut`).

---

## Phase H2: Live data (read-only bridge)

### H2.1 `/api/state` — todo
- JSON built in `crates/shell/src/home.rs` from the store:
  - recent visits: `Browsing::history` with a limit of 12, folded by site
  - bookmarks: `bookmarks()`
  - focus sites: `focus_sites()`, with Jev status from `jev_sites()`
  - watches: `watches()`, with the last value, last check and error
  - playbook counts per site: `playbook_counts()`
  - recent chats: `chats.all()`, titles only
- The Jev status line ("Jev ready: find · open · price…", "Jev off",
  "couldn't learn") moves from `render_focus` in `main.rs` into a shared
  helper, so the Focus tab and the home page say the same thing.
- The page renders sections in this order: Recent · Bookmarks · Focus
  sites · Watches · Playbooks · Chats. Empty sections show a one-line
  hint, e.g. "No focus sites yet. Use ◎ Focus in the toolbar."
- Eval: `home-data`. Start on `hist-a.html` with `focus: "localhost"`,
  then the `newtab` pane step from H1.2. The `js:` step reads `#recent` and `#focus`
  text, and the check is that both name the fixture site.

### H2.2 Site icons — todo
- `GET /icon/<site>.png` from `Browsing::favicon` (BGRA → PNG with the
  `image` crate, already a shell dependency), with a letter tile as the
  fallback.
- Eval: covered by `home-data` (the `js:` step checks
  `img.naturalWidth > 0` for a site with a favicon, using
  `polish-icon.png`).

### H2.3 Live refresh without polling — todo
- When the shell sees `browsing_changed` (in `BrowserPane::tick`), a
  watch check, Jev learning progress, or a settings change, it calls
  `WebView::evaluate("window.home?.refresh()")` on tabs showing
  `unibrow://home`. The page then fetches `/api/state` again.
- Eval: `home-refresh` (home open in tab 0; a `tab:` step visits a page in
  another tab; back on tab 0, the recent list includes it without a
  reload).

### H2.4 Agent, model and keys status — todo
- Shows the current agent and model (`agent_core::Settings`), and whether
  the agent is idle or working.
- Shows whether each key is set, never its value:
  - Anthropic API key: keyring `agent-unibrow`/`anthropic-api-key`, or
    `ANTHROPIC_API_KEY`
  - Jev: `agent-unibrow-models`/`jev`, or `JEV_API_KEY`
  - SubQ
  - Gmail: `google_client.json` present, and a refresh token in the
    keyring
- Key status is computed by the shell at startup and whenever a key
  changes, then put in `HomeSnapshot`. It's never read per request, so
  the page can't cause keyring prompts.
- A unit test serializes a snapshot built with known fake keys and asserts
  that none of them appear in the output.
- Eval: `home-keys` (`env: {"JEV_API_KEY": "sk-test-home-123"}`; `js:` →
  page text has "Jev · set"; another `js:` checks
  `document.documentElement.outerHTML.includes('sk-test')` → `js => false`).

---

## Phase H3: Acting from the page (narrow, allowlisted)

### H3.1 Commands — todo
- `POST /api/cmd` takes a `HomeCmd` enum in `home.rs`. Anything not on the
  list is refused with a 400. Each command:
  - `open {text}`: through `normalize_address`, in this tab
  - `new_tab {url}`
  - `show {tab, sub}`: the sidebar, on History/Bookmarks/Trace, Focus, a
    site's View, or Explained page N
  - `fill_chat {text}`: like Explained's `use_example`
  - `ask {text}`: `Shell::submit`
  - `set_agent {backend, model}`
  - `learn_jev {site}`, `toggle_jev {site}`
  - `check_watch {id}`: `check_watch_soon`
  - `remove_watch {id}`, `remove_bookmark {url}`
- Not on the list on purpose: anything that loosens safety (focus-only
  mode, always-allow rules, spend limits), saved logins, entering keys,
  and clearing history. The page links to the native control instead
  (`show {tab: "focus"}`).
- Every command is written to the trace as `Info` "Home: …", so evals and
  the user can see it.
- Eval: `home-cmd-allowlist` (a `js:` step posts
  `{cmd: "set_focus_only", on: false}` from home → refused; policy.json
  unchanged; `log_contains` the refusal).

### H3.2 Ask the agent from home — todo
- An "Ask the agent" box. Enter sends it as a normal chat message, so Jev
  routing on Jev-ready sites, approvals and Stop all work as usual. The
  sidebar opens on Chat.
- Under the box: three example chips taken from Explained's examples (needs
  `explained.rs` to expose them; coordinate with the guide's owner).
- Eval: `home-ask` (page home; pane steps type "What time is it?" into
  `#ask` and press Enter; `reply_any` has a time; `trace_contains`
  "Home: ask").

### H3.3 Agent and model picker — todo
- Shows the same choices as `render_model_menu` (`agent_core::models`; it
  lists OpenCode models with `opencode models` the first time). Uses the
  same code path to save the settings and restart the agent.
- Eval: `home-model` (no task; pick Codex from home; settings.json in the
  case's config dir says `codex`, `log_contains` "Home: set_agent").

### H3.4 Focus sites and Jev — todo
- Each focus site is a card: open it, see its Jev status, and use Learn
  with Jev / Jev on-off (`jev_worker::Job`, progress pushed via H2.3).
  "View" opens the sidebar on that site's View (8.6).
- Eval: `home-jev-learn` (with `UNIBROW_TEST_JEV_LEARN`, like the existing
  Jev cases; clicking Learn on the fixture shop's card shows "Jev ready"
  after refresh).

### H3.5 Watches and playbooks — todo
- Watches: label, last value, last check, Check now, Remove.
- Playbooks: name, site, runs and last failure (from `Playbook`). "Run…"
  fills the chat with "Run my playbook <name> for …", because parameters
  need the user's words and replays are an agent tool.
- Eval: `home-watch` (seed a watch on `watch.html` with an agent turn
  first; on home, Check now → `wait_trace` for the check).

---

## Phase H4: The tinkering hub

### H4.1 The guide on home — todo
- Explained's sections as cards, so the page doubles as the "how to use
  this" screen. Needs `explained.rs` to expose its `GUIDE` data (e.g. a
  `guide_json()`), so the text stays in one place. "More" opens the
  sidebar's Explained tab on that page (`show`).
- Eval: `home-guide` (the `js:` step counts cards = `explained::PAGES`;
  clicking one logs "Home: show explained").

### H4.2 `unibrow://settings` — todo
- A second page from the same handler. It shows:
  - the settings.json values
  - where things live: profile, `browsing.sqlite3`, `chats.sqlite3`,
    traces, `policy.json`, downloads
  - the CEF version and app version
  - the policy, read-only, with a link to edit it in Focus → Safety
- "Open folder" buttons (`xdg-open`, folders only).
- Eval: `home-settings` (`js:` finds the traces path from the case's
  `UNIBROW_DATA_DIR`).

### H4.3 Trace and evals at a glance — todo
- The last 10 trace entries (kind, tool, time), with Open Trace.
- In a source checkout (where `crates/evals/results/` exists), the latest
  eval run's pass count and failing case names.
- Eval: `home-trace` (after an agent turn, home lists that tool name).

### H4.4 Shortcuts and help — todo
- Shortcut list generated from the key bindings registered in `main.rs`
  (the `gpui::KeyBinding::new` list), not typed by hand.
- Eval: covered by `home-guide` (Alt+5 shown).

---

## Phase H5: Making it yours

### H5.1 Layout — todo
- A "Customize" mode: show or hide each section, reorder them, choose
  compact or roomy. Saved in `~/.config/agent-unibrow/home.json`, a shell
  file kept separate from settings.json, so `agent-core` doesn't need to
  know about it. Saved through a validated `save_layout` command.
- Eval: `home-layout` (needs a harness option that writes home.json into
  the case's config dir, like `policy`; hidden sections are absent).

### H5.2 Pinned sites — todo
- Pinned tiles above Recent: add from home, from the page's context menu,
  or from a History row; drag to reorder. Stored in a `home_pins` table in
  a new `crates/browsing/src/home.rs` (url, title, position).
- Eval: `home-pins` (pin the fixture shop, relaunch, still pinned and
  first).

### H5.3 Theme — todo
- `/theme.css` is generated from `crates/shell/src/theme.rs`, so the page
  matches the shell. Options: accent color, light variant, background
  image (a file in `~/.config/agent-unibrow/home/`).
- Eval: `home-theme` (`js:` reads `--accent` = `theme::ACCENT`).

### H5.4 Your own CSS and page — todo
- `~/.config/agent-unibrow/home/user.css` is applied after the built-in
  CSS.
- An `index.html` in that folder replaces the built-in page and gets the
  same API, so people can build their own start page.
- Files are served only from that folder: canonicalized, no symlinks out,
  size cap.
- `unibrow://home/?default` and a "Reset to default" link always show the
  built-in page.
- Eval: `home-user-page` (the case writes a small index.html; home shows
  it; `?default` shows the built-in page).

### H5.5 Ask the agent to change the page — todo
- A `home_customize` tool (Stage effect) that can only edit `home.json`
  and `user.css`. Its approval card shows the change.
- Eval: `home-customize` ("hide the watches section on my home page" →
  `approval_requested`, then home.json has watches hidden).

---

## Suggested order

1. **H1** in full: the scheme, home as the start/new-tab page, and the
   lockdown evals before anything privileged exists.
2. **H2.1, H2.2, H2.4**: data and status (read-only).
3. **H3.1** with its allowlist eval, then **H3.2** (ask) and **H3.4**
   (focus/Jev).
4. **H4.1** once the other session's `explained.rs` work has landed.
5. **H5** as people ask for it; **H5.4** last.

## Open questions

1. URL and address bar: is `unibrow://home` the right name, and should the
   address bar show it or stay empty (Chrome-style)?
2. On launch: always open home, or restore the last tabs (there's no
   session restore today) with home only in new tabs?
3. Should "Ask the agent" on home send right away, or only fill the chat
   box?
4. Search: keep DuckDuckGo, or make the engine a setting (shared with
   `normalize_address`)?
5. Should the agent be fully blind to the home page (the plan), or allowed
   to read it?
6. H5.4: allow a full replacement page, or only layout and CSS?
7. Should eval results and other developer info appear only in source
   checkouts, or not at all?
8. Look: always the shell's dark palette, or follow the desktop's
   light/dark setting like reader mode does?
9. Should home hide the sidebar, so it works as a full-window launcher, or
   leave the sidebar as it is?
