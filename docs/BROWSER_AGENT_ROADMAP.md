# Browser agent roadmap

What the agent needs so it can do almost anything a person can do in the
embedded browser: see the page, act on it, move between pages and tabs,
handle logins, files and dialogs, recover when things go wrong, and know
when to stop and hand control back.

Status: **done** (built and tested) · **partial** (started) · **todo**.

## Where things stand

| Area | Status | Notes |
| --- | --- | --- |
| Chromium embedded in the app (CEF 154, off-screen) | done | Frames painted in GPUI; one window |
| User input to the page (mouse, wheel, keyboard) | done | Key events carry X11 keycodes (US layout) |
| Toolbar: back, forward, reload, address bar | done | |
| Persistent profile (cookies, logins) | done | `~/.local/share/agent-unibrow/browser` |
| History, bookmarks, focus sites | done | SQLite; agents can read history and focus |
| Agent backends (Claude Code, Codex, OpenCode, API) | done | Tools served over local MCP |
| Policy, approval cards, trace | done | Every tool call goes through `ToolHost` |
| Agent tools that act in the browser | **partial** | Phase 1 built and tested on a local checkout page |

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

### 1.1 CDP bridge — done
- Send CDP commands and receive results and events from `rail-browser`.
- CEF runs on the UI thread and tools run on the agent thread, so
  requests go over a channel and answers come back the same way.
- Timeouts, and errors that read clearly in the trace.

### 1.2 Page model v1 — partial (outline, refs, paging done; diffs between steps todo)
- Accessibility tree pruned to what matters: interactive elements,
  headings, landmarks, text.
- Each element gets a short **ref** (`e12`) mapped to its `backendNodeId`, so
  the agent says "click e12" instead of guessing coordinates.
- Visible text in reading order, with the current URL, title, scroll
  position and viewport size.
- Size budget: big pages are summarized and the rest is fetched on request.
- A screenshot of the viewport, attached when the model can use images.
- Diffs between steps: what changed after the last action.

### 1.3 Core browser tools — done

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

### 1.4 Shared page between you and the agent — done (highlight and agent bar not yet seen on screen)
- The agent drives the page you're looking at, so you see every step.
- **Agent overlay**: highlight the element about to be clicked or typed
  into, and show a small "agent is working" bar with **Stop**.
- **Esc or Stop** cancels the agent's current step immediately.
- While the agent is driving, your own clicks pause it rather than racing
  it.

### 1.5 Trace with evidence — done (after-action screenshots; before-shots todo)
- Every browser action logged with the element, the URL, and a small
  before/after screenshot.
- Click a trace row to see the page as the agent saw it.

---

## Phase 2: Full interaction (everything a person does on a page)

### 2.1 Forms — partial (fields, checkboxes, radios, native dropdowns with popup rendering, `browser_select`; date pickers, rich editors and `browser_fill_form` todo)
- Text inputs, text areas, number, email, password fields.
- Checkboxes, radio buttons, switches.
- **`<select>` dropdowns**: render CEF's popup layer (`PET_POPUP`), which is
  also needed for people to use them, plus a `browser_select` tool that
  picks options by label.
- Date and time pickers, sliders, color pickers.
- Rich text editors and `contenteditable` (email composers, docs).
- Read validation errors after submit and fix the fields.
- `browser_fill_form`: fill several fields in one call from a mapping.

### 2.2 Pointer actions — partial (hover, right/middle/double click done; drag and drop todo)
- Double-click, right-click, middle-click.
- Hover (menus that open on hover, tooltips).
- Drag and drop: between elements, sliders, sortable lists, file drops
  (CEF `DragHandler` plus `drag_target_*`).
- Click-and-hold.

### 2.3 Keyboard — partial
- Shortcuts inside pages (Ctrl+C/V/X/Z/A, Tab navigation).
- Non-US layouts: derive physical key codes from the active layout rather
  than a US table.
- Typing non-English text and emoji through IME (`ime_commit_text`).

### 2.4 Clipboard — todo
- Copy from the page into the agent's context; paste content into fields.
- Read the system clipboard only when the user explicitly asks.

### 2.5 Scrolling and lazy content — todo
- Infinite scroll and "load more": scroll until the target appears or a
  limit is reached.
- Scroll inside panes (chat panels, modals), not just the page.
- Bring an element into view before acting on it.

### 2.6 Frames and shadow DOM — todo
- Act inside iframes (payment forms, embedded widgets) and open shadow
  roots, with refs that work across frames.

### 2.7 Canvas, maps and visual-only UI — todo
- When the accessibility tree is empty (canvas apps, maps, games), fall
  back to screenshots + coordinates, with a confirmation step after each
  action.

---

## Phase 3: Tabs, windows and navigation

### 3.1 Multiple tabs — partial (tab strip, Ctrl+T/W/Tab/L, `browser_tabs`, target=_blank links open tabs; agent-only background tabs todo)
- Real tabs in the UI (a tab strip), each its own `WebView`.
- Agent tools: `browser_tabs`, `browser_new_tab`, `browser_switch_tab`,
  `browser_close_tab`.
- Links that open new tabs open as tabs (currently they replace the page).
- Background tabs, so the agent can compare several pages without taking
  over the one you're on.

### 3.2 Popups — partial (`window.open` opens a tab; opener link for OAuth popups todo)
- OAuth and payment popups (`window.open`) open as real popup views and
  close themselves back into the opener, instead of replacing the page.

### 3.3 Navigation state — partial (load failures reported to the agent; per-tab history list todo)
- Session history list per tab (`navigation_entries`).
- Detect error pages, 404s, offline, and redirects to login.

### 3.4 Find in page — done (Ctrl+F bar with match count; `browser_find`)
- Ctrl+F for people; `browser_find` for the agent (CEF `find` / `FindHandler`).

---

## Phase 4: Dialogs, permissions, files and downloads

### 4.1 JavaScript dialogs — done
- `alert`, `confirm`, `prompt`, and "leave site?" (`JsdialogHandler`).
- Shown to the user as in-app cards; the agent can answer `alert`s, but
  `confirm`s that delete or pay go through approval.

### 4.2 Permission requests — partial (camera, mic, location, notifications denied with a notice; per-site allow todo)
- Location, notifications, camera, microphone, clipboard
  (`PermissionHandler`).
- Default deny for the agent; the user approves per site, remembered per
  site.

### 4.3 File uploads — done (desktop file picker for people; `browser_upload` with approval, hidden folders refused)
- File chooser (`DialogHandler`): the user picks files, or the agent
  attaches a file the user has explicitly allowed (e.g. a résumé in a
  known folder).
- Never browse the user's disk on its own.

### 4.4 Downloads — done (auto-save, toolbar list, `browser_downloads`, `browser_read_download` incl. PDF via pdftotext)
- `DownloadHandler`: save to `~/Downloads/agent-unibrow`, show progress in
  the UI, and report the saved path to the agent.
- Read downloaded PDFs, CSVs and text so the agent can use their contents.

### 4.5 Certificates and HTTP auth — todo
- Certificate errors shown to the user; the agent never clicks through.
- HTTP basic auth prompts handed to the user.

### 4.6 Print and PDF — partial (`browser_save_pdf`; system print dialog todo)
- Save page as PDF (`print_to_pdf`); print through the system dialog.

---

## Phase 5: Accounts, logins and handing control back

### 5.1 Password vault — todo
- Saved logins stored encrypted (keyring-backed).
- The agent can ask to "sign in to X"; the app fills the fields itself, so
  **the model never sees the password**.

### 5.2 Two-factor and verification — todo
- Detect 2FA, email or SMS codes, and device checks.
- Hand off: pause, highlight the field, tell the user what's needed, then
  resume once the page moves on.
- For email codes, the agent may read the code from Gmail with the
  user's approval.

### 5.3 CAPTCHAs and bot checks — todo
- Always hand to the user. No solving services, no evasion. If a site
  blocks the agent, say so and stop.

### 5.4 Take-over mode — todo
- One button: "I'll do this part". The agent pauses, the user acts, then
  hits "Continue" and the agent re-reads the page.

### 5.5 Session awareness — todo
- Notice when the user is logged out or a session expired, and say so
  instead of failing confusingly.

---

## Phase 6: Safety and policy for browser actions

### 6.1 Classifying actions by effect — done (button wording, form submits that post or carry sensitive fields, Enter in such forms, risky confirm dialogs)
- **Read**: navigate, read, scroll, search.
- **Stage**: type into fields, add to cart, save a draft.
- **Commit**: submit forms that send, pay, post, delete, book, sign up,
  change account settings. Always an approval card unless pre-allowed.
- Classified from the element (role, button text, form action) and the
  page (checkout, payment fields), with a cautious default.

### 6.2 Approval cards for browser actions — done
- Show what will happen: a screenshot of the form, the values filled in,
  and the button that will be pressed.

### 6.3 Site scope — todo
- Optional "focus only" mode: the agent can only act on focus sites,
  and anything else needs approval.
- Per-site rules: read-only sites, never-act sites.

### 6.4 Sensitive data — done (no typing into password/card/code fields; values hidden from page reads; masked in screenshots)
- Detect payment, password and ID fields. The agent never types card
  numbers or passwords itself; the vault fills them.
- Redact sensitive values in the trace and in anything sent to the model.

### 6.5 Prompt-injection defense — todo
- Page text is data, not instructions (already in the system prompt).
- Flag pages with instruction-like hidden text; treat cross-site
  instructions with extra suspicion; never follow instructions to
  exfiltrate data.

### 6.6 Limits — todo
- Step budgets per task, time limits, spend caps (from the purchase plan),
  and rate limits per site.

### 6.7 Honest identity — todo
- The agent doesn't disguise itself or evade bot detection. Sites that
  refuse agents are respected.

---

## Phase 7: Reliability (finishing tasks, not just starting them)

### 7.1 Act, then verify — todo
- Every action declares what should change (URL, element, text), and the
  agent checks that it did before moving on.

### 7.2 Waiting correctly — todo
- Wait on real signals (load, network idle, element visible, spinner
  gone), never fixed sleeps.

### 7.3 Recovering — todo
- Stale refs after re-render: re-read and re-find the element by role and
  name.
- Unexpected modals (cookie banners, newsletter popups): recognize and
  dismiss them.
- Retries with backoff, then a clear failure the user can act on.

### 7.4 Long tasks — todo
- Checkpoints, so a task can resume after an app restart.
- Running tasks in background tabs while the user keeps browsing.

### 7.5 Evals — todo
- Local test pages for each capability (forms, dropdowns, iframes, drag,
  dialogs, uploads, infinite scroll).
- Recorded tasks on focus sites, replayed nightly; pass rates tracked in
  `evals`.

---

## Phase 8: Knowing focus sites like the back of its hand

The idea: inside the finite set of focus sites, decision-making follows a
set path instead of starting from scratch each time.

### 8.1 Site maps — todo
- Per focus site: its pages, the elements that matter on each, and the
  paths between them, built from history and traces.

### 8.2 Playbooks — todo
- Recorded, reusable routes ("search a product → open the result → check
  the price → add to cart") that run step by step and are verified at
  each step.

### 8.3 Fast decisions (Jev) — todo
- Closed questions inside known pages ("which result matches?", "is this
  the checkout page?", "is the cart correct?") answered by a fast typed
  decision model, with its confidence in the trace.

### 8.4 Long context (SubQ) — todo
- Whole history, site maps and long pages in one context when a task
  needs the big picture.

### 8.5 Falling off the map — todo
- When a page doesn't match the site map (a redesign, a new modal), stop
  following the playbook, fall back to the general model, or hand off.

### 8.6 Per-site views — todo
- Filter history and the trace by focus site, to see everything that has
  happened on one site in one place.

---

## Phase 9: Extracting what's on the web

### 9.1 Reading — todo
- Readable article text (reader mode), tables to structured rows, lists,
  prices, dates, ratings.
- Every fact comes with the URL it came from.

### 9.2 Multi-page gathering — todo
- Pagination, "next" links, search result pages; stop when enough is
  found.

### 9.3 Comparing — todo
- Open several products or pages in background tabs and compare them in a
  table for the user.

### 9.4 Monitoring — todo
- Scheduled checks ("tell me when this price drops", "has my order
  shipped?") on focus sites.

---

## Phase 10: Human parity and polish

- Zoom (`set_zoom_level`) and Ctrl+/Ctrl- — todo
- Reader mode — todo
- Translate page — todo
- Spellcheck in fields (`replace_misspelling`) — todo
- Right-click context menus rendered in-app (`ContextMenuHandler`) — todo
- Keyboard shortcuts: Ctrl+L, Ctrl+T, Ctrl+W, Ctrl+Tab — todo
- Favicons and page titles in tabs, bookmarks and history — todo
- Audio and video: play, pause, mute (`set_audio_muted`) — todo
- Picture of a region ("screenshot this chart") — todo
- Developer tools for debugging pages (`show_dev_tools`) — todo

---

## Phase 11: Under the hood

- **GPU frame path**: dmabuf shared textures instead of CPU copies, if GPUI
  gains an external-texture API — todo
- **Throttle hidden pages**: lower frame rate for background tabs — todo
- **Renderer crash recovery**: detect a crashed page and offer reload — todo
- **Separate profiles**: an isolated profile for agent experiments — todo
- **Memory caps**: close idle background tabs — todo

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
