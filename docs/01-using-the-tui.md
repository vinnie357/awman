# Using the TUI

amux has two execution modes:

- **TUI mode** — run `amux` with no arguments to open the interactive terminal UI. This is the primary interface for ongoing agent work: it supports multiple simultaneous sessions, live tab state, and a full in-process terminal emulator for agent output.
- **Command mode** — run `amux <subcommand>` directly from your shell. It executes the command and exits. Useful for scripting, CI, or quick one-off actions.

This guide covers TUI mode.

---

## Startup

When you run `amux` with no arguments, the TUI opens immediately in an alternate terminal screen. What happens next depends on your environment:

**Inside a Git repository:**

The TUI runs `amux ready` automatically on the first tab. This checks that your container runtime is available, that `Dockerfile.dev` and `.amux/Dockerfile.{agent}` exist, and that your agent image is built. If anything needs attention, `ready` will guide you through it. Once `ready` passes, the TUI shows the welcome message and waits for your first command.

**Outside a Git repository:**

If the working directory is not inside a Git repository, the TUI runs `amux status --watch` instead, streaming a live status view. This is useful for monitoring a headless server or checking the state of remote sessions. Most agent commands require a Git repo — navigate to one and open a new tab with **Ctrl+T**.

In both cases, terminal raw mode, alternate screen, and mouse capture are enabled on entry and restored unconditionally on exit, even if amux crashes.

---

## Layout

```
┌─ Tab 1: myproject ─────────┬─ Tab 2: myproject ──────────┐
│  implement 0001             │  chat                        │
└─────────────────────────────┴──────────────────────────────┘
┌─── ● running: implement 0001 ──────────────────────────────┐
│ $ docker run --rm -it ...                                   │
│                                                             │
│  ╭─ 🔒 Claude Code (containerized) ── myproj | 5% | 200mb ─╮│
│  │                                                          ││
│  │  [agent output here]                                     ││
│  │                                                          ││
│  ╰──────────────────────────────────────────────────────────╯│
│                                                             │
│  Ctrl-M toggle · Ctrl-W workflow · Ctrl-, config            │
└─────────────────────────────────────────────────────────────┘
┌─── command ──────────────────────────────────────────────────┐
│ > _                                                           │
└───────────────────────────────────────────────────────────────┘
  init  ·  ready  ·  implement  ·  chat  ·  specs
```

The TUI is composed of three areas:

- **Tab bar** (top) — one entry per open session, with colour-coded state
- **Execution window** (middle) — shows command output; overlaid by the container window when an agent is running
- **Command box** (bottom) — where you type subcommands

---

## The command box

The command box is where you interact with amux. Type any subcommand and press **Enter**.

| Key | Action |
|-----|--------|
| Type | Update input; suggestions appear below |
| **Enter** | Execute command |
| **Ctrl+Enter** or **Shift+Enter** | Insert a newline (multi-line input) |
| **← / →** | Move cursor within input |
| **Ctrl+← / Ctrl+→** | Move cursor by word |
| **Home / End** | Move cursor to start / end of input |
| **↑** | Focus the execution window (for scrolling) |
| **Backspace / Delete** | Edit input |
| **Ctrl+Backspace** | Delete previous word |
| **Tab** | Cycle to next autocomplete suggestion |
| **Shift+Tab** | Cycle to previous autocomplete suggestion |
| **Ctrl+C** | Close tab (multiple tabs) or open quit confirmation (single tab) |

### Autocomplete

As you type, matching command completions appear in the suggestion row below the command box:

```
> implement · init · status
```

When you type a partial command, the list narrows. Use **Tab** / **Shift+Tab** to cycle through suggestions and fill them into the input. Every command available in `amux` is also available in the TUI command box. Both `--flag value` and `--flag=value` forms are accepted. For example:

```
chat --agent codex
chat --agent=codex
implement 0042 --agent opencode --plan
```

When the input is empty or there are no matching completions, the suggestion row shows the current working directory of the active session instead:

```
CWD: /home/user/myproject
```

If a worktree is active for the session, it shows the worktree path:

```
Using Worktree: /home/user/myproject-worktree
```

If you type an unrecognised command, amux suggests the closest known one:

```
'implemnt' is not an amux command.  Did you mean: implement
```

### Quitting

Press **Ctrl+C** from the command box to open the quit confirmation dialog:

```
╭─── Quit amux? ───────────────────╮
│  Are you sure you want to quit?   │
│  [y/n]                            │
╰───────────────────────────────────╯
```

Press **y** to quit, **n** or **Esc** to cancel. With multiple tabs open, **Ctrl+C** instead shows a close-tab dialog:

```
╭─── Close tab? ──────────────────────────────╮
│  [q] Quit amux   [c] Close this tab   [n] Cancel │
╰──────────────────────────────────────────────╯
```

---

## The execution window

The execution window shows plain-text streaming output from commands — Docker build logs, status messages, error output. It is separate from the container window (see below).

### Scrolling

When the window is selected (press **↑** from the command box to select it):

| Key / Action | Effect |
|---|---|
| **↑ / ↓** | Scroll line by line |
| **PageUp / PageDown** | Scroll one full page |
| **b / e** | Jump to beginning / end |
| Mouse scroll | Scroll at any time |
| **Esc** | Return focus to command box |

### Status log

amux itself writes informational messages — not agent output, but messages from amux about what it is doing — into a per-tab **status log**. Examples include "container started", "worktree created", "auth token accepted", and error messages from failed commands.

The status log appears in the execution window. By default it is **collapsed**: only the most recent message is shown as a single line at the bottom of the output area.

Press **l** (lowercase L) while the execution window is focused to toggle between collapsed and expanded view. In expanded view the full message history is visible and scrollable, with color-coded level prefixes:

| Level | Colour |
|-------|--------|
| Info | Dark gray |
| Warning | Yellow |
| Error | Red |
| Success | Green |

The status log is per-tab and accumulates for the lifetime of the session. It does not include agent output (that lives in the container window's scrollback).

### Border colours

| Colour | Meaning |
|--------|---------|
| Blue | Running (selected) |
| Grey | Running (unselected) or idle |
| Green | Completed successfully |
| Red | Completed with error |

---

## The container window

Whenever amux launches a container to run a code agent, a **container window** appears overlaying the execution window. This window contains a full terminal emulator — all keyboard input, ANSI colour codes, cursor movement, and interactive TUI apps (like Claude Code's own UI) work exactly as they would in a real terminal.

```
╭─ 🔒 Claude Code (containerized) ── myproject | 5% | 200mb ──╮
│                                                               │
│  [agent output — full terminal emulation]                    │
│                                                               │
╰───────────────────────────────────────────────────────────────╯
  Ctrl-M toggle  ·  scroll ↕ history  ·  drag select  ·  Ctrl+Y copy
```

The title bar shows the container name, live CPU usage, memory, and total runtime. Stats are polled from the container runtime every 5 seconds.

### Keyboard and mouse

When the container window is visible and maximized, almost all keyboard input is forwarded to the agent:

| Key / Action | Effect |
|---|---|
| Type | Sent directly to the agent |
| **Esc** | Forwarded to the agent (`\x1b`) — for vim, fzf, REPLs, and other interactive programs |
| **Tab / Shift+Tab** | Forwarded to the agent |
| **Ctrl+M** | Toggle: minimize the container window (agent keeps running) |
| Mouse scroll | Scroll terminal scrollback (5 lines per tick) |
| Mouse drag | Select text (highlighted with inverted colours) |
| **Ctrl+Y** | Copy the current selection to clipboard (ANSI stripped) |

> **Note on Ctrl+M:** `Ctrl+M` produces the same byte (`\r`) as carriage return in many terminals. amux intercepts Ctrl+M before it reaches the agent, so agents cannot receive a raw `\r` from this key combination. In practice this is not a problem — agents use Enter (which produces `\r\n` or `\n`) for line input, not Ctrl+M.

Scrollback holds up to 10,000 lines by default. While scrolled, the title bar shows `↑ scrollback (N / M lines)` where `N` is your current offset and `M` is the total depth. Scroll back to the bottom to return to the live view.

**Ctrl+Y** with no active selection forwards the key to the agent instead of copying.

### Minimizing and restoring

Press **Ctrl+M** to minimize the container window. The agent keeps running. The window collapses to a 1-line status bar:

```
─ 🔒 claude | myproject | 5% | 200mb | 1m 23s ─────────────────
```

From the minimized state:

| Key | Effect |
|-----|--------|
| **Ctrl+M** | Restore the container window |
| **↑ / ↓** | Scroll the execution window (behind the status bar) |
| **b / e** | Jump to beginning / end of execution window |
| **Esc** | Return focus to command box |

### When the container exits

The container window closes and a summary bar appears:

```
── claude · myproject-12345 · avg CPU 4.2% · 210MiB · 1m 47s · exit 0 ──
```

This summary persists until a new container is launched.

---

## Config dialog

Press **Ctrl+,** from anywhere in the TUI to open the config dialog instantly — even while an agent is running or the container window is maximized. You can also type `config show` in the command box and press **Enter**. Either way opens the same large centered modal overlay for viewing and editing all configuration fields without leaving the TUI.

```
╭─── Configuration ────────────────────────────────────────────────────────╮
│                                                                            │
│  Field                       Global              Repo        Effective     │
│ ─────────────────────────────────────────────────────────────────────────  │
│  default_agent               claude (built-in)   N/A         claude        │
│  runtime                     docker (built-in)   N/A         docker        │
│▶ terminal_scrollback_lines   10000 (built-in)    5000        5000          │
│  yolo_disallowed_tools       (empty)             (not set)   (empty)       │
│  env_passthrough             (empty)             (not set)   (empty)       │
│  agent                       N/A                 codex       codex         │
│  auto_agent_auth_accepted    N/A                 true        true          │
│                                                                            │
│  Accepted values: positive integer                                         │
│                                                                            │
│  ↑↓ navigate · e edit · Ctrl+Enter save · Esc close                       │
╰────────────────────────────────────────────────────────────────────────────╯
```

### Navigation and editing

| Key | Action |
|-----|--------|
| **↑ / ↓** | Move between rows |
| **← / →** | Move between columns (Global, Repo, Effective) |
| **e** | Enter edit mode for the selected field |
| **Enter** (edit mode) | Confirm the new value and exit edit mode |
| **Esc** (edit mode) | Cancel edit without saving |
| **Ctrl+Enter** | Save all pending changes to the appropriate config files |
| **Esc** (navigation) | Close the dialog and return to the previous view |
| **Ctrl+,** | Close the dialog (same as Esc in navigation mode) |

When a row is selected, a hint line below the table shows the accepted values for that field (e.g. `claude | codex | opencode | maki | gemini`).

Fields marked `(read-only)` — such as `auto_agent_auth_accepted` — are skipped during navigation for edit purposes. Their values are shown but cannot be changed from this dialog.

### Scope and saving

The dialog loads both config files when it opens. Each edit targets the repo config by default; global-only fields (like `runtime` and `default_agent`) write to the global config. Changes are not written to disk until you press **Ctrl+Enter**. Pressing **Esc** without saving discards all edits made in this session.

---

## Multi-tab support

Press **Ctrl+T** to open a new tab. Each tab has its own working directory, execution window, and container session. Tabs run independently in the background when you switch away.

```
Ctrl+T          open a new tab (prompts for working directory)
Ctrl+A          switch to the previous tab
Ctrl+D          switch to the next tab
Ctrl+C, Ctrl+T  (multiple tabs open) close current tab
```

The tab bar shows each tab's project name, current or last command, and an arrow (`➡`) on the active tab. The active tab's bottom border is suppressed so it visually opens into the content area.

Tab names are truncated at 14 characters with `…`. The tab bar distributes width according to the number of open tabs:

| Open tabs | Each tab gets |
|-----------|--------------|
| 1 | ¼ of terminal width |
| 2 | ½ of terminal width |
| 3 | ¾ ÷ 3 of terminal width |
| 4+ | full width ÷ n |

### Tab colours

| Colour | Meaning |
|--------|---------|
| Grey | Idle or completed |
| Blue | Running (no container) |
| Green | Running with active container |
| Purple / Magenta | Running a claws (nanoclaw) session, **or** permanently bound to a remote headless session |
| Red | Exited with error |
| Yellow | Container silent for >10 seconds (stuck warning) |
| Alternating Yellow / Purple | Background yolo countdown in progress: tab label alternates between `⚠️ yolo in Ns` and `🤘 yolo in Ns` every 2 seconds (see [Yolo Mode](05-yolo-mode.md#background-yolo-countdown)) |

### Remote-bound tabs

When `remote.defaultAddr` is set in `~/.amux/config.json`, opening a new tab with **Ctrl+T** offers an option to bind the tab to a remote headless session. A **remote-bound tab** forwards every command you type to the remote host via the headless API — no `remote run` prefix or session flags needed.

Remote-bound tabs are **purple** in the tab bar. The tab label shows `host:port` of the remote host instead of the local directory name. When a workflow runs on the remote session, the workflow state strip appears automatically and updates every 5 seconds.

For full details on creating remote-bound tabs, the create-session sub-modal, and workflow strip behavior, see [Remote Mode: Remote-bound TUI tabs](09-remote-mode.md#remote-bound-tui-tabs).

---

### Stuck detection

If a running container produces no output for more than 10 seconds, the tab turns yellow and the subcommand label gains a `⚠️` prefix (e.g. `⚠️ implement 0001`). The warning clears automatically when you:

- Switch to the yellow tab
- Press any key while the tab is active
- Scroll with the mouse wheel

**Active-tab suppression:** On the currently active tab, any keypress or mouse scroll also resets the stuck timer directly. If you are actively reading or scrolling through output, the tab will not turn yellow or show any stuck indicator — the timer only starts when both the container and the user have been idle for 10 seconds. Background tabs are not affected by this; they use output time alone to determine stuck state.

For workflow tabs, amux goes further: the [workflow control board](04-workflows.md#workflow-control-board) opens automatically so you can act without having to notice the yellow indicator. In yolo mode, background tabs show a live countdown directly in the tab bar instead of a dialog. See [Workflows](04-workflows.md) and [Yolo Mode](05-yolo-mode.md) for details.

---

## Reference: all keyboard shortcuts

| Key | Context | Action |
|-----|---------|--------|
| **Ctrl+T** | Anywhere | Open new tab |
| **Ctrl+A** | Anywhere | Switch to previous tab |
| **Ctrl+D** | Anywhere | Switch to next tab |
| **Ctrl+M** | Anywhere | Toggle container window (minimize / restore / hide) |
| **Ctrl+C** | Single tab open | Open quit confirmation |
| **Ctrl+C** | Multiple tabs open | Open close-tab dialog |
| **Ctrl+W** | Workflow running | Open workflow control board |
| **Ctrl+,** | Anywhere | Open / close config dialog |
| **Enter** | Command box | Execute command |
| **Ctrl+Enter** or **Shift+Enter** | Command box | Insert newline |
| **Tab** | Command box | Cycle to next autocomplete suggestion |
| **Shift+Tab** | Command box | Cycle to previous autocomplete suggestion |
| **↑** | Command box | Focus execution window |
| **← / →** | Command box | Move cursor |
| **Ctrl+← / Ctrl+→** | Command box | Move cursor by word |
| **Home / End** | Command box | Move cursor to start / end |
| **Ctrl+Backspace** | Command box | Delete previous word |
| **Esc** | Execution window | Return focus to command box |
| **↑ / ↓** | Execution window | Scroll output line by line |
| **PageUp / PageDown** | Execution window | Scroll output by page |
| **b** | Execution window | Jump to beginning |
| **e** | Execution window | Jump to end (live view) |
| **l** | Execution window | Toggle status log collapsed / expanded |
| **Ctrl+Y** | Execution window (text selected) | Copy selection to clipboard |
| **Esc** | Container window maximized | Forwarded to agent (`\x1b`) |
| **Ctrl+Y** | Container window maximized (text selected) | Copy selection to clipboard |
| **Ctrl+M** | Container window maximized | Minimize container window |
| Mouse scroll | Container window | Scroll scrollback history |
| Mouse drag | Container window | Select text |
| **y** | Quit dialog | Quit amux |
| **n / Esc** | Quit dialog | Cancel |
| **q** | Close-tab dialog | Quit amux |
| **c** | Close-tab dialog | Close current tab only |
| **n / Esc** | Close-tab dialog | Cancel |
| **↑ / ↓** | Config dialog | Navigate between fields |
| **← / →** | Config dialog | Navigate between columns |
| **e** | Config dialog | Enter edit mode for selected field |
| **Enter** | Config dialog (edit mode) | Confirm value and exit edit mode |
| **Esc** | Config dialog (edit mode) | Cancel edit without saving |
| **Ctrl+Enter** | Config dialog | Save all changes to config files |
| **Esc** | Config dialog (navigation) | Close dialog |

---

[← Getting Started](00-getting-started.md) · [Next: Agent Sessions →](02-agent-sessions.md)
