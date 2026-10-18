# Homepage roadmap

The page people land on when they open the app or a new tab: served by the
app itself, not from the web, and the front door to everything you can
adjust: the agent and model, focus sites and Jev, watches, playbooks,
safety settings, the guide, and the page itself.

Status: ✅ **COMPLETED** (built and tested) · ◐ partial (started; what is left is noted) · todo.

## Where things stand

| Area | Status | Notes |
| --- | --- | --- |
| First page on launch | ✅ | `unibrow://home/` (`HOME` in `crates/shell/src/browser_pane.rs`); `UNIBROW_START_URL` still overrides it (evals use this) |
| New tab (Ctrl+T, + button) | ✅ | Opens home with an empty address bar |
| Last tab closed | ✅ | Reopens home |
| Agent's new tab with no URL | ✅ | `about:blank`: the agent has nothing to do on home |
| App-served pages | ✅ H1 | `unibrow://` scheme (`rail-browser/src/app_pages.rs`), home page in `crates/shell/home/` |
| Places to tinker | ✅ in the sidebar | History (History/Bookmarks/Trace), Focus (sites, Learn with Jev, per-site View, watches, logins, safety), Explained (Alt+5), Model menu |

### Progress by phase

| Phase | Completed | Partial | Todo |
| --- | --- | --- | --- |
| H1. A local home page | H1.2–H1.5 | H1.1 | |
| H2. Live data | H2.1–H2.4 | | |
| H3. Acting from the page | H3.1–H3.3 | H3.4, H3.5 (no eval yet) | |
| H4. The tinkering hub | H4.1–H4.4 | | |
| H5. Making it yours | H5.1, H5.4, H5.5 | H5.2, H5.3 | |

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

### H1.1 The `unibrow://` scheme — ◐ partial
- Done: `rail_browser::app_pages` registers the scheme (standard, secure,
  display-isolated, fetch-enabled; not CORS-enabled) in every process
  through `App::on_register_custom_schemes`, and `app_pages::serve` adds the
  handler factory after `initialize`. The shell's `home::Home` implements
  `AppPages` (`get(host, path) -> Page`) with the files built in by
  `include_str!`. Every response carries the CSP above plus `nosniff` and
  `no-store`; unknown paths are a 404; paths with `..`, `%2e` or
  backslashes are refused.
- Unit tests: `app_pages` (routing, traversal, the CSP, which navigations
  may open the app's pages) and `home` (files served, nothing loaded from
  the web, no scripts).
- Left: a dark background for home tabs. A browser's background color is
  fixed when it's created, and the tab goes on to websites that expect
  white behind them, so this needs a per-navigation approach. The page
  loads from memory, so the white flash is very short.

### H1.2 Home on launch, new tab and last-tab-closed — ✅ **COMPLETED**
- `HOME` in `browser_pane.rs` becomes `unibrow://home/`.
  `UNIBROW_START_URL` still wins, so every existing eval keeps its start
  page.
- `TabOp::New` with no URL (the agent's background tab) opens
  `about:blank` instead. The agent has nothing to do on the home page.
- `normalize_address` (`rail-browser/src/lib.rs`) already passes
  `unibrow://…` through unchanged. Typing `home` alone stays a search.
- Eval: `home-launch` (`"page": "home"` leaves `UNIBROW_START_URL` unset,
  so it's the real launch path; title "Home", the dark background, the
  search box, and no DuckDuckGo).
- Eval: `home-newtab` (the new `newtab` pane step, the Ctrl+T path: a
  second tab on home with an empty address bar).

### H1.3 Browser chrome on internal pages — ✅ **COMPLETED**
- On home, the address bar is empty (`Chrome::shown_url`), the tab is
  titled "Home" with the app icon (`/favicon.svg`), and the ◎ Focus and
  bookmark toolbar buttons are hidden on every `unibrow://` page.
- Not recorded in history: `browsing::site_of` already returns `None` for
  anything other than http(s). Zoom is keyed by scheme (`zoom_key`).
- Eval: `home-not-history` (home, then a fixture page typed in the
  address bar with the new `open:` step, then a new tab; the new `visits`
  step lists exactly that one page).

### H1.4 Lockdown — ✅ **COMPLETED**
- `on_before_browse` (`handlers.rs`) lets a main frame go to `unibrow://`
  only on the app's own loads (address bar, new tab: explicit), reloads,
  Back/Forward, or from an app page; never a subframe
  (`app_pages::may_open`). The popup handler refuses new tabs to
  `unibrow://` unless an app page opens them.
- Every browser tool is wrapped (`NotOnAppPages` in
  `rail-browser/src/tools.rs`): no tool takes a `unibrow://` address, and
  tools that act on or read the open page (everything but open, tabs,
  back/forward/reload, wait, history list, downloads, compare, playbooks,
  and extract/gather given a URL) refuse there: "This tab is the app's
  home page, which is the user's, not a website. Open a site first…"
- Eval: `home-isolation`. A fixture `home-probe.html` tries a `fetch`, an
  iframe, `window.open`, a link click, a form POST to `/api/cmd` and
  `location.href`; all are blocked (Chromium's display isolation stops
  them before `on_before_browse` is reached) and the page stays put.
- Eval: `home-agent-refuses` ("type hello into the search box" on home:
  the tools refuse and the agent tells the user to type it or open a
  site).

### H1.5 First content — ✅ **COMPLETED**
- A static page in the app's palette, with no scripts (`crates/shell/home/`):
  - A search box: a plain `<form>` to DuckDuckGo (the CSP's only outside
    address, as `form-action`).
  - Four starter requests to type in the chat.
  - "Make it yours" cards: Model, Focus sites and Jev, History and Trace,
    Explained.
  - The main shortcuts (Ctrl+L, Ctrl+T, Ctrl+W, Ctrl+B, Alt+1–5, Esc).
- Eval: `home-search` (fills the box and clicks Search; the tab lands on
  `duckduckgo.com/?q=bass+nut`).

---

## Phase H2: Live data (read-only bridge)

### H2.1 `/api/state` — ✅ **COMPLETED**
- Built as planned, in `crates/shell/src/home.rs` (`home::state`). The
  shell hands the page its stores once (`home::attach`) and publishes what
  only it knows (`home::publish`). Data is served only to the home page's
  own main frame; any other caller gets 403. The page draws everything
  with `textContent`, never as HTML. The Jev status line is shared as
  `home::jev_line`, used by both the Focus tab and home.
- Along the way: history now breaks same-second ties newest first
  (`ORDER BY at DESC, id DESC`).
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
- Eval: `home-data` (start on `hist-a.html` with `localhost` in focus,
  then a new tab: Recent and Focus sites name it, and the status section
  is there).

### H2.2 Site icons — ✅ **COMPLETED**
- `GET /icon/<site>.png` from `Browsing::favicon` (BGRA → PNG with the
  `image` crate, already a shell dependency), with a letter tile as the
  fallback.
- Eval: `home-icons` (a page with a favicon, `polish.html`: its row on
  home shows the icon, not the letter).

### H2.3 Live refresh without polling — ✅ **COMPLETED**
- Also refreshed after a watch check, Jev learning progress, the agent
  starting, stopping or switching, a key changing, and every home
  command.
- When the shell sees `browsing_changed` (in `BrowserPane::tick`), a
  watch check, Jev learning progress, or a settings change, it calls
  `WebView::evaluate("window.home?.refresh()")` on tabs showing
  `unibrow://home`. The page then fetches `/api/state` again.
- Eval: `home-refresh` (home in tab 0, a page visited in tab 1, then back
  to tab 0: Recent lists it without a reload).

### H2.4 Agent, model and keys status — ✅ **COMPLETED**
- Gmail shows "signed in", "set up, not signed in" or "not set up"
  (`rail_api::google_signed_in` says whether a token exists, never what
  it is). Keys are checked off the UI thread at startup and when the API
  key changes (`Shell::check_home_keys`).
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
- Eval: `home-keys` (`JEV_API_KEY=sk-test-home-123`: the page says Jev
  is set, and the key's text is nowhere in it).

---

## Phase H3: Acting from the page (narrow, allowlisted)

### H3.1 Commands — ✅ **COMPLETED**
- Built with the list below, plus `open_folder`, `save_layout`,
  `reset_layout`, `pin`, `unpin` and `move_pin` (H4, H5). Each field is
  checked in `home::parse_cmd`, then against the app's state in
  `Shell::home_cmd` (a focus site, a model on offer). The token is in a
  `<meta>` tag of the served page and sent as `X-Unibrow-Token`. Refused
  commands are logged and traced ("Home: refused: …").
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
- Eval: `home-cmd-allowlist` (`set_focus_only` with the token → 400 and
  refused; any command without the token → 403; a focus site's View
  button works).

### H3.2 Ask the agent from home — ✅ **COMPLETED**
- Enter sends right away (open question 3: the default). The example
  buttons fill the box. They come from Explained (H4.1).
- An "Ask the agent" box. Enter sends it as a normal chat message, so Jev
  routing on Jev-ready sites, approvals and Stop all work as usual. The
  sidebar opens on Chat.
- Under the box: three example chips taken from Explained's examples (needs
  `explained.rs` to expose them; coordinate with the guide's owner).
- Eval: `home-ask` ("What is 2 + 2?" from home's box: the agent answers 4,
  traced as "Home: asked the agent").

### H3.3 Agent and model picker — ✅ **COMPLETED**
- Found and fixed along the way: test instances saved settings to the
  user's real `~/.config/agent-unibrow/settings.json`. They now read it
  but save to `$UNIBROW_DATA_DIR/settings.json`.
- The eval harness compared traces as `\u00b7`-escaped JSON, so no
  expectation with non-ASCII text (such as "·") could match. It now
  compares the text itself.
- Shows the same choices as `render_model_menu` (`agent_core::models`; it
  lists OpenCode models with `opencode models` the first time). Uses the
  same code path to save the settings and restart the agent.
- Eval: `home-model` (Haiku picked on home: the agent restarts on it, and
  home says "Claude Code · Haiku 5.5").

### H3.4 Focus sites and Jev — ◐ partial
- Built: each focus site row has Learn with Jev / Jev on-off / Retry Jev
  and View. Left: the `home-jev-learn` eval.
- Each focus site is a card: open it, see its Jev status, and use Learn
  with Jev / Jev on-off (`jev_worker::Job`, progress pushed via H2.3).
  "View" opens the sidebar on that site's View (8.6).
- Eval: `home-jev-learn` (with `UNIBROW_TEST_JEV_LEARN`, like the existing
  Jev cases; clicking Learn on the fixture shop's card shows "Jev ready"
  after refresh).

### H3.5 Watches and playbooks — ◐ partial
- Built: watches show the last value and check, with Check now (marks it
  due; the checker looks within half a minute) and Remove. Playbooks show
  per site with Run… (fills the chat). Left: the `home-watch` eval.
- Watches: label, last value, last check, Check now, Remove.
- Playbooks: name, site, runs and last failure (from `Playbook`). "Run…"
  fills the chat with "Run my playbook <name> for …", because parameters
  need the user's words and replays are an agent tool.
- Eval: `home-watch` (seed a watch on `watch.html` with an agent turn
  first; on home, Check now → `wait_trace` for the check).

---

## Phase H4: The tinkering hub

### H4.1 The guide on home — ✅ **COMPLETED**
- `explained::guide_json()` gives every guide page (tab, sections, first
  line), every example request and the shortcuts. Explained also gets a
  "The home page" section.
- Explained's sections as cards, so the page doubles as the "how to use
  this" screen. Needs `explained.rs` to expose its `GUIDE` data (e.g. a
  `guide_json()`), so the text stays in one place. "More" opens the
  sidebar's Explained tab on that page (`show`).
- Eval: `home-guide` (8 cards, Alt+5 among the shortcuts, and a card
  opens Explained on its page).

### H4.2 Settings and files — ✅ **COMPLETED**
- At `unibrow://home/settings` rather than a separate `unibrow://settings`,
  so it shares home's origin: its styles and its command token. It shows
  every file's path, the settings, the safety rules (read-only) and the
  versions (the app, and the CEF build's folder name). Open folder works
  for data, traces, downloads and config; test instances only log it.
- A second page from the same handler. It shows:
  - the settings.json values
  - where things live: profile, `browsing.sqlite3`, `chats.sqlite3`,
    traces, `policy.json`, downloads
  - the CEF version and app version
  - the policy, read-only, with a link to edit it in Focus → Safety
- "Open folder" buttons (`xdg-open`, folders only).
- Eval: `home-settings` (the case's own traces and history paths; Open
  Traces is traced).

### H4.3 Trace and evals at a glance — ✅ **COMPLETED**
- The last 10 trace entries (kind, tool, time), with Open Trace.
- In a source checkout (where `crates/evals/results/` exists), the latest
  eval run's pass count and failing case names.
- Eval: `home-activity` (after a turn asked from home, Recent activity
  shows "Turn finished").

### H4.4 Shortcuts and help — ✅ **COMPLETED**
- From Explained's shortcut list rather than the key bindings themselves,
  so the guide and home always agree. The bindings are separate Action
  types, so a shared table would need a bigger change.
- Shortcut list generated from the key bindings registered in `main.rs`
  (the `gpui::KeyBinding::new` list), not typed by hand.
- Eval: covered by `home-guide` (Alt+5 shown).

---

## Phase H5: Making it yours

### H5.1 Layout — ✅ **COMPLETED**
- "Customize this page": tick sections on or off, move them up or down,
  Roomy or Compact, accent color, Save or Reset. `home.json` lives in the
  config folder (a test instance's data folder). Unknown sections and
  bad colors are dropped when it's read (`home::Layout`).
- A "Customize" mode: show or hide each section, reorder them, choose
  compact or roomy. Saved in `~/.config/agent-unibrow/home.json`, a shell
  file kept separate from settings.json, so `agent-core` doesn't need to
  know about it. Saved through a validated `save_layout` command.
- Eval: `home-layout` (the new `home_layout` harness option: hidden
  sections hidden, Focus first, the accent applied).

### H5.2 Pinned sites — ◐ partial
- Built: `home_pins` table (`crates/browsing/src/home_pins.rs`, at most
  24), Pin on Recent rows, tiles with ← → and × instead of dragging.
  Left: pinning from the page's context menu or a History row.
- Pinned tiles above Recent: add from home, from the page's context menu,
  or from a History row; drag to reorder. Stored in a `home_pins` table in
  a new `crates/browsing/src/home.rs` (url, title, position).
- Eval: `home-pins` (Pin on a Recent row puts it in Pinned).

### H5.3 Theme — ◐ partial
- Built: `/theme.css` from `theme.rs` with the user's accent and density.
  Left: a light variant and a background image (a picture in the home
  folder already works through user.css and `/user/<file>`).
- `/theme.css` is generated from `crates/shell/src/theme.rs`, so the page
  matches the shell. Options: accent color, light variant, background
  image (a file in `~/.config/agent-unibrow/home/`).
- Eval: covered by `home-layout` (`--accent` follows home.json).

### H5.4 Your own CSS and page — ✅ **COMPLETED**
- The folder is `home/` in the config folder. Files are served at
  `/user/<name>`: plain names only, no links out of the folder, 4 MB each,
  common web types. The built-in page is `unibrow://home/default` (a path
  rather than `?default`, since app-page routing ignores queries).
- `~/.config/agent-unibrow/home/user.css` is applied after the built-in
  CSS.
- An `index.html` in that folder replaces the built-in page and gets the
  same API, so people can build their own start page.
- Files are served only from that folder: canonicalized, no symlinks out,
  size cap.
- `unibrow://home/?default` and a "Reset to default" link always show the
  built-in page.
- Eval: `home-user-page` (the new `home_files` harness option: a page of
  your own with its own CSS file, and `/default` with user.css applied).

### H5.5 Ask the agent to change the page — ✅ **COMPLETED**
- `home_customize` is a Commit tool rather than Stage, since Stage
  tools run without asking in this app's policy. The approval card spells
  out the change ("Home page: hide watches").
- A `home_customize` tool (Stage effect) that can only edit `home.json`
  and `user.css`. Its approval card shows the change.
- Eval: `home-customize` ("Hide the watches section on my home page": the
  approval card, then home.json has watches hidden).

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
