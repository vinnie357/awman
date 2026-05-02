# Work Item: Task

Title: grand architecture refactor — TUI frontend (split out from the original 0069)
Issue: n/a — split-out portion of the grand architecture refactor described in `aspec/architecture/2026-grand-architecture.md`

## Required reading before starting

This work item is the TUI-frontend portion of the grand architecture refactor, originally bundled into `0069-grand-architecture-layer-3-frontends-and-binary.md`. That work item proved too large to land in a single pass, and was split into three smaller work items:

- `0069-…` — CLI frontend + Layer 4 binary + `Cargo.toml` swap (merged before this work item starts).
- `0070-…` (this work item) — TUI frontend.
- `0071-…` — Headless frontend.
- `0072-…` — Final parity validation, oldsrc removal, docs and aspec refresh.

The implementing agent **MUST** read `aspec/architecture/2026-grand-architecture.md`, the original `0069-…` (which already contains the TUI section §2 and the per-section addenda §7a–§7r), and the current state of `src/data/`, `src/engine/`, `src/command/`, and `src/frontend/cli/`.

## Scope

Build `src/frontend/tui/` per `0069-…` §2 and the §7 addenda. This includes:

- `mod.rs`, `app.rs`, `tabs.rs`, `command_box.rs`, `command_frontend.rs`, `per_command/`, `container_view.rs`, `workflow_view.rs`, `ready_view.rs`, `init_view.rs`, `claws_view.rs`, `dialogs/`, `text_edit.rs`, `pty.rs`, `keymap.rs`, `render.rs`, `hints.rs`, `user_message.rs`, `worktree_lifecycle_frontend.rs`.
- The behavioral parity checklist in `0069-…` §2.
- The §7a–§7r addenda (tab management, command-box autocomplete, workflow control board, stuck/yolo, step error, agent setup, mount scope, agent auth, config show, new-artefact dialogs, claws dialogs, quit/tab-close, PTY container view, status log, status-tab annotations, startup behavior, remote pickers, status TIPS / CLEAR_MARKER, init `--aspec`, work-items config).
- The §8 code-reuse policy (copy-and-adapt for pure presentation; reimplement for business-logic-entangled).

After this work item, `main.rs` MUST dispatch bare invocations to `tui::run` and the TUI MUST exhibit user-perceptible parity with the legacy TUI.

## What must NOT happen in this work item

- No business logic in `src/frontend/tui/`. If a frontend needs to make a decision that affects behavior, the missing surface is in Layer 2; add it there.
- No deletion of `oldsrc/`. That is `0072-…`.
- No edits inside `oldsrc/` other than possibly the `oldsrc/README.md` note.
- No new commands, new flags, or new user-visible behavior. This work item is *parity only*.

## Test Considerations

Same philosophy as `0069-…` §"Test Considerations": **only Layer 3 unit tests and pure-presentation snapshot tests**. The full parity test suite is `0072-…`'s responsibility.

## Codebase Integration

- Follow `aspec/architecture/2026-grand-architecture.md` as the source of truth.
- Follow `0069-…` §2, §7a–§7r, §8a–§8d for TUI specifics.
- Do not edit `oldsrc/` (other than the README note).
- Do not delete `oldsrc/` — that is `0072-…`.
- After this work item lands, the next agent picks up `0071-grand-architecture-headless-frontend.md`.
