# Chats and steering roadmap

Conversations that survive crashes, more than one of them, and a way to
talk to the agent while it's working.

Status: ✅ **COMPLETED** (built and tested) · ◐ partial · todo.

## How it fits together

- **Store.** A `chats` crate keeps every chat in SQLite
  (`~/.local/share/agent-unibrow/chats.sqlite3`, WAL). Each message is
  written the moment it appears; the reply being streamed is saved about
  once a second. A crash loses at most the last second of a reply.
- **Resuming.** Every backend already keeps a conversation the model can
  pick up: Claude Code's session (`--resume`), Codex's thread
  (`exec resume`), OpenCode's session (`--session`), and the API backend's
  message list. The backend reports that handle as it changes; the chat
  stores it. Reopening a chat restarts the agent with it, so the model
  remembers everything, not just what's on screen.
- **Recap fallback.** If the handle can't be used (the chat was with a
  different backend, or the session is gone), the next message carries a
  recap of the chat so far.
- **One agent at a time.** The agent works in one chat; the browser is
  shared, so two agents driving it at once would collide. Switching chats
  while it's working is blocked with a note (stop it or let it finish).
- **Steering.** Every tool call goes through the app's `ToolHost`, for all
  four backends. Messages typed while the agent works are queued there and
  attached to the next tool result, so the model reads them mid-task and
  can change course. If it's only writing its answer, they go in as the
  next turn as soon as that answer ends.

## Phase C1: Chats that persist

### C1.1 Chat store — ✅ **COMPLETED**
- `chats` crate: chats (title, created, updated, open, last active,
  backend, resume handle) and items (kind, text, time), append-only.
- Streaming text updated in place at most once a second, and on every
  boundary (tool call, note, end of turn).

### C1.2 Resume handles from every backend — ✅ **COMPLETED**
- `AgentEvent::Resume(json)` from Claude Code (session id), Codex (thread
  id), OpenCode (session id), API (message list after each turn).
- `agent_core::spawn(.., resume)` starts a backend on that handle.
- A resume that fails (session deleted) reports `ResumeFailed`; the app
  resends with a recap.
- Tested end to end with Claude Code (`chat-survives-crash`). The Codex,
  OpenCode and API handles are saved and passed back the same way, but no
  eval runs on those backends yet.

### C1.3 Reopen on launch — ✅ **COMPLETED**
- The app reopens the chats that were open, on the one last used.

## Phase C2: Many chats

### C2.1 Chat strip — ✅ **COMPLETED**
- Open chats as tabs above the conversation, with + for a new chat and ×
  to close one (closing keeps it in the list).
- Titles come from the first message.

### C2.2 All chats — ✅ **COMPLETED**
- A list of every chat, newest first, to reopen one; delete with a
  confirm click. Deleting removes its messages from disk.

### C2.3 Switching — ✅ **COMPLETED**
- Switching stops nothing: it's blocked while the agent is working, with
  a note saying why. Idle, it restarts the agent on that chat's handle.
- Backend switches keep the chat; the next message carries a recap.

## Phase C3: Steering

### C3.1 Type while it works — ✅ **COMPLETED**
- The chat box stays open while the agent works ("Steer: add to what
  it's doing…"). Enter queues the message; it shows in the chat marked
  "queued" until the agent receives it, then "delivered".

### C3.2 Delivered mid-task — ✅ **COMPLETED**
- Queued messages ride along with the next tool result, framed as the
  user's words sent mid-task. The system prompt tells the model to act on
  them right away; they may change or cancel the plan.
- Anything still queued when a turn ends is sent as the next turn
  straight away.

### C3.3 Interrupt and send — ✅ **COMPLETED**
- "Interrupt" stops the current step and sends the message as a new turn
  at once, for when waiting for the next tool call is too slow.

### C3.4 Evals — ✅ **COMPLETED**
- `steer-mid-task`: during a 12-second wait the user changes the goal; the
  model must drop the original plan (no scroll) and answer the new one.
- `chat-survives-crash`: the app is killed (SIGKILL) after the first
  reply; relaunched on the same data, it reopens the chat on the same
  Claude Code session and remembers a code word from before the crash.

## Later

- Agents in background chats (needs per-chat browser tabs).
- ✅ **COMPLETED**: rename chats (double-click the title), search across chats (All chats), export a chat to Markdown.
- Per-chat trace files.
- ✅ **COMPLETED**: the change log (DeltaDB-style, step 1). Every change the
  file tools make (create, replace, edit, new folder, move, delete to the
  Trash) is kept in `chats.sqlite3` (`file_changes`) with the chat and the
  user's message it answered, the old contents of replaced files (up to
  5 MB), and an operation id: this install's replica id and a Lamport
  clock value, which chat items get too. A strip after each turn lists its
  changes with Undo; History → Changes lists all of them with their chat
  (blame) and Undo for one. Undo (`rail_files::undo`) only takes a change
  back if the file is still as the agent left it, never erases, and is
  logged itself; the agent hears about undone changes with the next
  message. Eval `change-log-undo`.
- Change log, step 2: playbook versioning (each saved playbook kept as a
  version with its op id, so a replay can say which version ran and an
  edit can be taken back).
