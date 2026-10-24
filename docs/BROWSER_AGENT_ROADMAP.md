# Browser agent roadmap

What the agent needs so it can do almost anything a person can do in the
embedded browser: see the page, act on it, move between pages and tabs,
handle logins, files and dialogs, recover when things go wrong, and know
when to stop and hand control back.

Status: ✅ **COMPLETED** (built and tested) · ◐ partial (started; what is left is noted) · todo.

## Where things stand

| Area | Status | Notes |
| --- | --- | --- |
| Chromium embedded in the app (CEF 154, off-screen) | ✅ **COMPLETED** | Frames painted in GPUI; one window |
| User input to the page (mouse, wheel, keyboard) | ✅ **COMPLETED** | Key events carry X11 keycodes (US layout) |
| Toolbar: back, forward, reload, address bar | ✅ **COMPLETED** | |
| Persistent profile (cookies, logins) | ✅ **COMPLETED** | `~/.local/share/agent-unibrow/browser` |
| History, bookmarks, focus sites | ✅ **COMPLETED** | SQLite; agents can read history and focus |
| Agent backends (Claude Code, Codex, OpenCode, API) | ✅ **COMPLETED** | Tools served over local MCP |
| Policy, approval cards, trace | ✅ **COMPLETED** | Every tool call goes through `ToolHost` |
| Agent tools that act in the browser | ◐ partial | About 30 browser tools; see the phases below |

### Progress by phase

| Phase | Completed | Partial | Todo |
| --- | --- | --- | --- |
| 1. Foundation | ✅ all | | |
| 2. Full interaction | 2.1, 2.2, 2.4, 2.5, 2.6 | 2.3 | 2.7 |
| 3. Tabs, windows, navigation | ✅ all | | |
| 4. Dialogs, permissions, files | 4.1, 4.2, 4.3, 4.4, 4.5 | 4.6 | |
| 5. Accounts and handoff | 5.1, 5.2, 5.3, 5.4 | 5.5 | |
| 6. Safety and policy | ✅ all | | |
| 7. Reliability | ✅ all | | |
| 8. Focus sites | 8.1, 8.2, 8.3, 8.4 (decisions), 8.5, 8.6 | | 8.4 SubQ chat |
| 9. Extracting | ✅ 9.1, 9.2, 9.3, 9.4 | | |
| 10. Polish | zoom, reader, context menu, favicons, audio, video codecs, fullscreen, region shots, shortcuts | | translate; spellcheck, PiP and DevTools blocked by CEF |
| 11. Under the hood | throttling, crash recovery, memory caps | | GPU frame path, separate profiles |
| 12. Jev sites | 12.1, 12.2, 12.3, 12.6 | | 12.4, 12.5 |

## How the agent will see and drive pages

CEF exposes the full Chrome DevTools Protocol (CDP) in-process through
`BrowserHost::execute_dev_tools_method` and a DevTools message observer.
That gives the agent the same machinery Playwright and Puppeteer use,
without a separate Chrome or a debugging port:

- **Accessibility** (`getFullAXTree`): roles, names and states of everything
  on the page, which is the most reliable way to know what can be clicked.
- **DOM / DOMSnapshot**: structure, attributes, layout boxes, and stable
  `backendNodeId`s to target elements.
- **Input** (`dispatchMouseEvent`, `dispatchKeyEvent`, `insertText`): actions
  that behave like a real user, through the same input path as the UI.
- **Page / Runtime / Network**: load events, screenshots, script evaluation,
  request activity for "is the page done loading".

Rendering already gives us every frame as pixels, so screenshots for vision
come for free.

The `page-model` crate turns these into one structured view of the page
that every agent backend receives the same way.

---

## Phase 1: Foundation (see and act on one page)

The minimum for the agent to work in the browser at all.

### 1.1 CDP bridge — ✅ **COMPLETED**
- Send CDP commands and receive results and events from `rail-browser`.
- CEF runs on the UI thread and tools run on the agent thread, so
  requests go over a channel and answers come back the same way.
- Timeouts, and errors that read clearly in the trace.

### 1.2 Page model v1 — ✅ **COMPLETED** (outline, refs, paging; every action reports "Changes since you last looked": added, removed and changed lines, URL/title changes, bounded in size)
- Accessibility tree pruned to what matters: interactive elements,
  headings, landmarks, text.
- Each element gets a short **ref** (`e12`) mapped to its `backendNodeId`, so
  the agent says "click e12" instead of guessing coordinates.
- Visible text in reading order, with the current URL, title, scroll
  position and viewport size.
- Size budget: big pages are summarized and the rest is fetched on request.
- A screenshot of the viewport, attached when the model can use images.
- Diffs between steps: what changed after the last action.

### 1.3 Core browser tools — ✅ **COMPLETED**

| Tool | Does | Effect |
| --- | --- | --- |
| `browser_open` | Go to a URL (or search) | Read |
| `browser_read` | Get the page model | Read |
| `browser_screenshot` | Viewport or element screenshot | Read |
| `browser_click` | Click an element ref (or x,y as a fallback) | Read*/Commit |
| `browser_type` | Type into a field, optionally clearing it first | Stage |
| `browser_press` | Press keys or shortcuts (Enter, Tab, Ctrl+A) | Stage |
| `browser_scroll` | Scroll the page, an element, or to an element | Read |
| `browser_wait` | Wait for load, an element, text, or network idle | Read |
| `browser_back` / `browser_forward` / `browser_reload` | Navigation | Read |

\* Clicks are classed by what they hit: a link or tab is Read, a submit,
"Buy", "Send" or "Delete" button is Commit (see 6.1).

### 1.4 Shared page between you and the agent — ✅ **COMPLETED** (highlight and agent bar not yet seen on screen)
- The agent drives the page you're looking at, so you see every step.
- **Agent overlay**: highlight the element about to be clicked or typed
  into, and show a small "agent is working" bar with **Stop**.
- **Esc or Stop** cancels the agent's current step immediately.
- While the agent is driving, your own clicks pause it rather than racing
  it.

### 1.5 Trace with evidence — ✅ **COMPLETED** (after-action screenshots; before-shots todo)
- Every browser action logged with the element, the URL, and a small
  before/after screenshot.
- Click a trace row to see the page as the agent saw it.

---

## Phase 2: Full interaction (everything a person does on a page)

### 2.1 Forms — ✅ **COMPLETED** (`browser_fill_form` fills many fields at once, never submits and refuses sensitive fields; date/time/number inputs set reliably; contenteditable rich editors, Gmail- and Lexical-style, verified)
- Text inputs, text areas, number, email, password fields.
- Checkboxes, radio buttons, switches.
- **`<select>` dropdowns**: render CEF's popup layer (`PET_POPUP`), which is
  also needed for people to use them, plus a `browser_select` tool that
  picks options by label.
- Date and time pickers, sliders, color pickers.
- Rich text editors and `contenteditable` (email composers, docs).
- Read validation errors after submit and fix the fields.
- `browser_fill_form`: fill several fields in one call from a mapping.

### 2.2 Pointer actions — ✅ **COMPLETED** (hover, right/middle/double click; `browser_drag` with HTML5 drag interception; drops onto delete/trash/send ask first. Hand drags by the user of HTML5 items in the embedded view still unsupported)
- Double-click, right-click, middle-click.
- Hover (menus that open on hover, tooltips).
- Drag and drop: between elements, sliders, sortable lists, file drops
  (CEF `DragHandler` plus `drag_target_*`).
- Click-and-hold.

### 2.3 Keyboard — ◐ partial
- Shortcuts inside pages (Ctrl+C/V/X/Z/A, Tab navigation).
- Non-US layouts: derive physical key codes from the active layout rather
  than a US table.
- Typing non-English text and emoji through IME (`ime_commit_text`).

### 2.4 Clipboard — ✅ **COMPLETED** (`browser_clipboard` copy/paste/show on the agent's own clipboard; the system clipboard is never read; Ctrl+C/V/X via browser_press refused)
- Copy from the page into the agent's context; paste content into fields.
- Read the system clipboard only when the user explicitly asks.

### 2.5 Scrolling and lazy content — ✅ **COMPLETED** (`browser_scroll` `load_more`/`min_items`: scrolls until nothing new loads, reports growth)
- Infinite scroll and "load more": scroll until the target appears or a
  limit is reached.
- Scroll inside panes (chat panels, modals), not just the page.
- Bring an element into view before acting on it.

### 2.6 Frames and shadow DOM — ✅ **COMPLETED** (same-process and cross-origin iframes stitched into the outline with clickable refs like `f1e8`; shadow DOM works; sensitive fields hidden in every frame. Max 12 frames; find/notes main frame only)
- Act inside iframes (payment forms, embedded widgets) and open shadow
  roots, with refs that work across frames.

### 2.7 Canvas, maps and visual-only UI — todo
- When the accessibility tree is empty (canvas apps, maps, games), fall
  back to screenshots + coordinates, with a confirmation step after each
  action.

---

## Phase 3: Tabs, windows and navigation

### 3.1 Multiple tabs — ✅ **COMPLETED** (tab strip, shortcuts, `browser_tabs`; the agent works in its own background tab (`new` with `background`, or `use`) while the user browses theirs; marked "agent" in the strip; the agent bar offers Show)
- Split view — ✅ **COMPLETED**: 2–4 tabs on screen in a grid (side by side, columns, 2×2), each laid out at its pane's size; Ctrl+click a tab or the toolbar split button; the clicked pane has the focus; the split stays a group when another tab is shown. The agent splits with `browser_tabs` (`split`/`unsplit`) and looks at up to 4 tabs at once with `browser_view_tabs` (screenshot, text and injection check per tab, without switching). Evals `split-view-user`, `agent-split-view`, `agent-views-background-tab`.
- Real tabs in the UI (a tab strip), each its own `WebView`.
- Agent tools: `browser_tabs`, `browser_new_tab`, `browser_switch_tab`,
  `browser_close_tab`.
- Links that open new tabs open as tabs (currently they replace the page).
- Background tabs, so the agent can compare several pages without taking
  over the one you're on.

### 3.2 Popups — ✅ **COMPLETED** (`window.open` popups keep `window.opener`, so OAuth sign-ins can post back; shown as tabs marked popup, closed when the page closes itself; `target=_blank` links stay plain tabs)
- OAuth and payment popups (`window.open`) open as real popup views and
  close themselves back into the opener, instead of replacing the page.

### 3.3 Navigation state — ✅ **COMPLETED** (load failures reported; right-click or long-press Back/Forward for the tab's history; `browser_history_list` for the agent)
- Session history list per tab (`navigation_entries`).
- Detect error pages, 404s, offline, and redirects to login.

### 3.4 Find in page — ✅ **COMPLETED** (Ctrl+F bar with match count; `browser_find`)
- Ctrl+F for people; `browser_find` for the agent (CEF `find` / `FindHandler`).

---

## Phase 4: Dialogs, permissions, files and downloads

### 4.1 JavaScript dialogs — ✅ **COMPLETED**
- `alert`, `confirm`, `prompt`, and "leave site?" (`JsdialogHandler`).
- Shown to the user as in-app cards; the agent can answer `alert`s, but
  `confirm`s that delete or pay go through approval.

### 4.2 Permission requests — ✅ **COMPLETED** (camera, mic, location, notifications ask the user with an Allow/Deny card, remembered per site; the agent can never answer; background pages deny)
- Location, notifications, camera, microphone, clipboard
  (`PermissionHandler`).
- Default deny for the agent; the user approves per site, remembered per
  site.

### 4.3 File uploads — ✅ **COMPLETED** (desktop file picker for people; `browser_upload` with approval, hidden folders refused)
- File chooser (`DialogHandler`): the user picks files, or the agent
  attaches a file the user has explicitly allowed (e.g. a résumé in a
  known folder).
- Never browse the user's disk on its own.

### 4.4 Downloads — ✅ **COMPLETED** (auto-save, toolbar list, `browser_downloads`, `browser_read_download` incl. PDF via pdftotext)
- `DownloadHandler`: save to `~/Desktop/unibrow-workspace/Downloads` (was `~/Downloads/agent-unibrow`), show progress in
  the UI, and report the saved path to the agent.
- Read downloaded PDFs, CSVs and text so the agent can use their contents.

### 4.5 Certificates and HTTP auth — ✅ **COMPLETED** (certificate errors blocked with no click-through, reported to the agent; HTTP sign-in shows an in-app username/password bar for the user, the agent is told to hand off)
- Certificate errors shown to the user; the agent never clicks through.
- HTTP basic auth prompts handed to the user.

### 4.6 Print and PDF — ◐ partial (`browser_save_pdf`; system print dialog todo)
- Save page as PDF (`print_to_pdf`); print through the system dialog.

---

## Phase 5: Accounts, logins and handing control back

### 5.1 Password vault — ✅ **COMPLETED** (keyring-backed; `browser_sign_in` fills on the saved site only; Focus tab UI)
- Saved logins stored encrypted (keyring-backed).
- The agent can ask to "sign in to X"; the app fills the fields itself, so
  **the model never sees the password**.

### 5.2 Two-factor and verification — ✅ **COMPLETED** (code fields detected and refused; `browser_handoff`; `browser_email_code` finds an emailed code in Gmail and shows it to the user with Copy and Fill, never to the model; one-digit code boxes hidden from page reads. SMS and authenticator codes stay with the user)
- Detect 2FA, email or SMS codes, and device checks.
- Hand off: pause, highlight the field, tell the user what's needed, then
  resume once the page moves on.
- For email codes, the agent may read the code from Gmail with the
  user's approval.

### 5.3 CAPTCHAs and bot checks — ✅ **COMPLETED** (detected and handed to the user)
- Always hand to the user. No solving services, no evasion. If a site
  blocks the agent, say so and stop.

### 5.4 Take-over mode — ✅ **COMPLETED** (Take over button, Continue to hand back)
- One button: "I'll do this part". The agent pauses, the user acts, then
  hits "Continue" and the agent re-reads the page.

### 5.5 Session awareness — ◐ partial (sign-in pages flagged with whether a saved login exists)
- Notice when the user is logged out or a session expired, and say so
  instead of failing confusingly.

---

## Phase 6: Safety and policy for browser actions

### 6.1 Classifying actions by effect — ✅ **COMPLETED** (button wording, form submits that post or carry sensitive fields, Enter in such forms, risky confirm dialogs)
- **Read**: navigate, read, scroll, search.
- **Stage**: type into fields, add to cart, save a draft.
- **Commit**: submit forms that send, pay, post, delete, book, sign up,
  change account settings. Always an approval card unless pre-allowed.
- Classified from the element (role, button text, form action) and the
  page (checkout, payment fields), with a cautious default.

### 6.2 Approval cards for browser actions — ✅ **COMPLETED**
- Show what will happen: a screenshot of the form, the values filled in,
  and the button that will be pressed.

### 6.3 Site scope — ✅ **COMPLETED** ("Only act on focus sites" toggle; reading anywhere is fine, acting elsewhere asks; enforced in `ToolHost` for every backend)
- Optional "focus only" mode: the agent can only act on focus sites,
  and anything else needs approval.
- Per-site rules: read-only sites, never-act sites.

### 6.4 Sensitive data — ✅ **COMPLETED** (no typing into password/card/code fields; values hidden from page reads; masked in screenshots)
- Detect payment, password and ID fields. The agent never types card
  numbers or passwords itself; the vault fills them.
- Redact sensitive values in the trace and in anything sent to the model.

### 6.5 Prompt-injection defense — ✅ **COMPLETED** (pages scanned for instructions aimed at AI agents, hidden or visible; a "Prompt-injection warning" note names cross-site targets; the agent tells the user and ignores them. Not yet scanned in gather/compare)
- Page text is data, not instructions (in the system prompt).
- Flag pages with instruction-like hidden text; treat cross-site
  instructions with extra suspicion; never follow instructions to
  exfiltrate data.

### 6.6 Limits — ✅ **COMPLETED** (per-turn caps: 60 calls, 5 commits; purchase amount on approval cards; "always ask over $X" overrides always-allow)
- Step budgets per task, time limits, spend caps (from the purchase plan),
  and rate limits per site.

### 6.7 Honest identity — ✅ **COMPLETED** (stock Chrome UA, no webdriver spoofing; the agent never claims to be the user or a human)
- The agent doesn't disguise itself or evade bot detection. Sites that
  refuse agents are respected.

---

## Phase 7: Reliability (finishing tasks, not just starting them)

### 7.1 Act, then verify — ✅ **COMPLETED** ("Check:" lines after actions: clicks that did nothing, typed values that didn't stick, form errors after a submit; plus optional `expect`)
- Every action declares what should change (URL, element, text), and the
  agent checks that it did before moving on.

### 7.2 Waiting correctly — ✅ **COMPLETED** (load state, document ready, then network quiet; `browser_wait` for text)
- Wait on real signals (load, network idle, element visible, spinner
  gone), never fixed sleeps.

### 7.3 Recovering — ✅ **COMPLETED** (stale refs found again by role and name; covered elements named instead of clicked through; clicks during loading wait and retry once; anything that could commit is never retried)
- Stale refs after re-render: re-read and re-find the element by role and
  name.
- Unexpected modals (cookie banners, newsletter popups): recognize and
  dismiss them.
- Retries with backoff, then a clear failure the user can act on.

### 7.4 Long tasks — ✅ **COMPLETED** (`task_progress` plan per chat, shown as a checklist; survives crashes, restarts and turn caps)
- Checkpoints, so a task can resume after an app restart.
- Running tasks in background tabs while the user keeps browsing.

### 7.5 Evals — ✅ **COMPLETED** (`crates/evals/run_evals.py`, 53 cases over local test pages; nightly systemd timer files in `crates/evals/nightly/`, not installed)
- Local test pages for each capability (forms, dropdowns, iframes, drag,
  dialogs, uploads, infinite scroll).
- Recorded tasks on focus sites, replayed nightly; pass rates tracked in
  `evals`.

---

## Phase 8: Knowing focus sites like the back of its hand

The idea: inside the finite set of focus sites, decision-making follows a
set path instead of starting from scratch each time.

### 8.1 Site maps — ✅ **COMPLETED**
- URLs grouped into page patterns (`/dp/B0ABC12345` → `/dp/{id}`, query
  values dropped, tracking keys ignored), each with a kind: search,
  product, cart, checkout, sign-in, orders, account, home.
- Every page-changing action the agent takes on a focus site is recorded
  by role and name, not ref, with the page before and after (the
  `Recorded` wrapper in `rail-browser/src/tools/playbooks.rs`). Pages come
  from history, elements and paths from those actions.
- `site_map` tool; the focus line sent with each message shows playbook
  counts. Code: `crates/browsing/src/sitemap.rs`.

### 8.2 Playbooks — ✅ **COMPLETED**
- `playbook_save`: the latest run of actions on a site becomes a named
  route; literal values become parameters (`{query}`), including inside
  URLs. A search-result click that matched the query is saved as "the
  result matching the parameters".
- `playbook_run`: replays without the model through the same tools, so
  purchases and other commits still ask; site scope applies; at most 40
  steps. Run stats and the last failure are kept per playbook.
- Versions (the change log's step 2): saving under a name in use keeps the
  old steps as an earlier version, removing keeps the last version, and
  each replay records which version ran (`playbook_runs`); the tools say
  "version N". The site view lists versions and removed playbooks with
  Restore, which saves the old version as the newest. Eval
  `playbook-versions`.
- Evals: `playbook-replay` (recorded searching "TUSQ nut", replayed for
  "bone nut" after a crash and restart, picks the bone result).

### 8.3 Fast decisions (Jev) — ✅ **COMPLETED**
- Closed questions during replays ("Which search result matches?") go
  through the `Decider` trait, with the answer and confidence in the step
  log. Jev (TypeSafe's decision model, `rail_browser::jev`) answers them when
  a key is saved (keyring: service `agent-unibrow-models`, account `jev`, or
  `JEV_API_KEY`); `WordMatch` stands in without one or if Jev can't be
  reached. Test instances use Jev only with `UNIBROW_ALLOW_JEV=1`.

### 8.4 Long context (SubQ) — ✅ **COMPLETED** (decisions); chat todo
- SubQ Snap (`subq-snap-preview`, keyring account `subq`) takes the same
  typed questions as Jev over up to 384K tokens, for $0.05 per million
  input tokens. Every decision goes to Jev when it fits (it's quicker) and
  to SubQ when it doesn't: whole pages, long lists. Jev measured: 18.6K
  tokens fine, 55K refused; SubQ read 275K in 4 s. A Jev "too long" error
  is retried on SubQ. Either key alone works.
- Used by 12.4 (product questions answered from the whole page).
- Todo: SubQ's long-context chat model (`subq-preview`, early access) for
  whole history and site maps in one context. The saved key isn't allowed
  to use it yet (`403 model_not_allowed`).

### 8.5 Falling off the map — ✅ **COMPLETED**
- Before each step: is this the kind of page the recording was on? After
  it: did it land where the recording did (or, for picked results, on the
  same kind of page)? Otherwise the replay stops, records why, and returns
  the page so the model carries on.
- Eval: `playbook-off-map` (the shop's search moves to a new page between
  runs; the replay stops at the right step and the agent finishes).

### 8.6 Per-site views — ✅ **COMPLETED**
- Focus → View: the site's playbooks (steps, runs, last failure,
  remove), pages, the agent's recent actions there, and history.

---

## Phase 9: Extracting what's on the web

### 9.1 Reading — ✅ **COMPLETED**
- `browser_extract`: `article` (readable text with headings, byline and
  date; navigation, sidebars and hidden text left out), `tables` (data
  tables as rows, ARIA grids too), `items` (search results and product
  grids: title, link, price, rating), `links`, and `summary` (product facts
  from JSON-LD or microdata: price, availability, rating, reviews; prices
  shown; spec pairs).
- Works on the current page or any URL in a background page. Every result
  starts with its source URL, and ends with a reminder that page text is
  data, not instructions.

### 9.2 Multi-page gathering — ✅ **COMPLETED**
- `browser_gather` follows `rel=next`, "Next" links, numbered pagination
  or "Load more" buttons; stops at `max_pages`, `max_items`, the `until`
  text, or when there's nothing new. Duplicates dropped.

### 9.3 Comparing — ✅ **COMPLETED**
- `browser_compare` loads 2–8 URLs in background pages at once and returns
  one table (name, price, availability, rating, brand, plus requested
  `fields`) and each page's details.
- Background pages (also the start of 3.1's agent-only tabs): hidden, share
  the profile, never touch the user's tabs, close when the tool finishes or
  is stopped; dialogs and popups in them are declined. The agent bar shows
  "reading N pages in the background".

### 9.4 Monitoring — ✅ **COMPLETED**
- `browser_watch` (add, list, remove, check): price watches (drops, rises,
  changes) and text watches (appears, disappears, or a selector's text
  changes), every 15 minutes to 7 days.
- A checker thread re-checks due watches in background pages with the HTTP
  cache off, and tells the user by desktop notification and a note in
  Chat. The first check is the baseline. Watches are listed in Focus.
- Evals: `article-extract`, `gather-listing`, `compare-products`,
  `watch-price` (the price on the served page is changed after the turn and
  the watch must fire).

---

## Phase 10: Human parity and polish

- Zoom (Ctrl+= / Ctrl+- / Ctrl+0, Ctrl+wheel, per site, toolbar badge; agent clicks stay accurate) — ✅ **COMPLETED**
- Reader mode (toolbar Reader button, clean article view) — ✅ **COMPLETED**
- Screenshot button (toolbar camera: saves the visible page as a PNG in ~/Desktop/unibrow-workspace/Screenshots and copies it with wl-copy/xclip; eval `screenshot-button`) — ✅ **COMPLETED**
- Workspace folder and sidebar tab (`~/Desktop/unibrow-workspace` with Screenshots, Downloads, Pages, Chats and Files; the Workspace tab lists it with thumbnails, open, rename, delete to the Trash, show in folder; Alt+6; eval `workspace-tab`) — ✅ **COMPLETED**
- Agent saves screenshots to the workspace (`browser_save_screenshot`: the visible page, one element or a region, as PNG in Screenshots; sensitive fields masked; evals `agent-saves-screenshot`, `agent-saves-element-shot`, `agent-saved-shot-masked`) — ✅ **COMPLETED**
- Sidebar pop out (⤢ / Ctrl+Shift+B: the sidebar becomes a "Sidebar" tab, first in the tab strip, shown full size in a centered column; web tabs keep running live off screen; the tab's × docks it again; pages come forward when one needs the user or a sidebar link opens a page; eval `sidebar-pop-out`) — ✅ **COMPLETED**
- Translate page — skipped: Chromium's translate sends page text to Google. Option: the agent translates the extracted article into chat or reader view — todo
- Spellcheck in fields — blocked: CEF 154 off-screen browsers never report misspellings (dictionary downloads, prefs set); menu wiring ready if it ever does
- Right-click context menus rendered in-app (links, images, editing, navigation, save as PDF, view source) — ✅ **COMPLETED**
- Keyboard shortcuts: Ctrl+L, Ctrl+T, Ctrl+W, Ctrl+Tab, Ctrl+F, Esc — ✅ **COMPLETED**
- Favicons and page titles in tabs, History and Bookmarks — ✅ **COMPLETED**
- Audio: ♪ indicator and mute per tab — ✅ **COMPLETED** (play/pause controls todo; indicator can linger on Web Audio pages)
- Video: H.264/AAC/HLS through Arch's codec-enabled CEF build (X, Reddit, Vimeo, news players); Widevine; fullscreen fills the window, Escape leaves — ✅ **COMPLETED**. Picture-in-picture can't work with off-screen rendering
- Picture of a region (`browser_screenshot` with `ref` or `clip`; Save screenshot in the context menu) — ✅ **COMPLETED**
- Developer tools for debugging pages — blocked: CEF 154 only opens DevTools as a separate native window (no off-screen mode); a remote-debugging port would let any local program drive the browser, so not done

---

## Phase 11: Under the hood

- **GPU frame path**: dmabuf shared textures instead of CPU copies, if GPUI
  gains an external-texture API — todo
- **Throttle hidden pages**: hidden tabs and background pages at 1 fps — ✅ **COMPLETED**
- **Renderer crash recovery**: "This page crashed" overlay with Reload, hung-page Wait/Stop, the agent is told — ✅ **COMPLETED**
- **Separate profiles**: an isolated profile for agent experiments — todo
- **Memory caps**: background tabs idle 30 min are discarded and reload when shown — ✅ **COMPLETED**

---

## Phase 12: Jev sites (requests on focus sites without Claude)

Jev answers typed questions (pick one, yes/no, score) in a fraction of a
second for a fraction of a cent. It doesn't write text, so it can't replace
Claude everywhere; but most requests about a known shop are one of a few
kinds, and each step of those is a closed question.

### 12.1 Learn with Jev — ✅ **COMPLETED**
- Focus → "Learn with Jev" on any focus site (or Relearn in its View).
  The app explores the site in hidden pages; Jev makes every judgment:
  which text box is the site's search (or the user's own searches there,
  from history), which of the site's links names something to search for,
  whether the page that comes back is search results, whether a result is
  a product page, which link is the cart.
- What it learns is saved per site (`jev_sites` table): the search address
  with `{q}`, result and product URL patterns, whether product pages carry
  price/stock data, the cart address, and a log of each finding with
  Jev's probability. The Focus list shows "Jev ready: find · open · price ·
  stock · cart"; the button switches Jev off and on for the site.

### 12.2 Requests without Claude — ✅ **COMPLETED**
- A message that names a Jev-ready site, or is sent while the page is on
  one, goes to Jev first. One Jev call reads it: the kind of request
  (find, price, stock, open, cart, or something else), which phrase of the
  message names the product (a choice among its phrases), and whether it
  means the product already open.
- The app then does it in the user's tab: searches with the learned
  address, lets Jev pick the matching result, opens it, reads price and
  stock from the page's product data (Jev decides stock when the page
  has none), and replies from a template, e.g. "Bone Jazz Bass nut is $9.50
  on …, in stock." The chat notes "Answered by Jev · N decisions · no
  Claude tokens".
- Anything else (buying, comparing, reviews, questions, several things at
  once), or a request Jev isn't sure about, goes to Claude, with a note
  saying why. Claude is told what Jev answered meanwhile.
- Evals: `jev-price`, `jev-find` (no agent calls at all), `jev-hands-off`
  (a "which suits a 1962 Jazz Bass" question goes to Claude).

### 12.3 Decisions in playbooks — ✅ **COMPLETED**
- See 8.3: replays use Jev for "which result is it?".

### 12.6 Product questions from the page — ✅ **COMPLETED**
- "Does it come with glue?", "is it pre-slotted?": a new request kind. The
  product page (the open one, or the one the request names) is split into
  passages; one decision over the whole page picks the passage that
  answers it, and whether the answer is yes or no. The reply quotes it:
  "Yes. The … page says: “Pre-slotted for your convenience …”". Long pages
  go to SubQ (8.4). If no passage clearly answers, Claude takes it.
- A model number in the request ("PT-1214-00") decides the search pick
  when one result has it, and the search is retried with it alone until
  one does.
- Evals: `jev-question` (spec table, Jev), `subq-long-page` (an answer in
  the middle of 900 Q&As and 500 reviews, SubQ). Checked live on
  sweetwater.com.

### 12.4 More request kinds — todo
- Add to cart (with a confirmation card), reorder from order history,
  track an order, a watch ("tell me when it drops") set up by Jev.

### 12.5 Savings shown — todo
- Tokens and cost per request on each focus site, with Jev and without,
  in Focus → View.

---

## Suggested order

1. **Phase 1** in full. Nothing else works without seeing and acting on a
   page, the overlay, and Stop.
2. **6.1, 6.2 and 6.4** from safety, before the agent can submit anything
   real.
3. **2.1 forms and select popups**, **3.1 tabs**, **4.1 dialogs**: what most
   real tasks hit first.
4. **5.x logins and handoff**, then **7.x reliability** with evals running.
5. **Phase 8** once there's enough history and trace on focus sites to
   build site maps from.
6. Everything else as tasks need it.
