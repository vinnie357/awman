# Work Item: Task

Title: grand architecture refactor — Headless frontend (split out from the original 0069)
Issue: n/a — split-out portion of the grand architecture refactor described in `aspec/architecture/2026-grand-architecture.md`

## Required reading before starting

This work item is the Headless-frontend portion of the grand architecture refactor, originally bundled into `0069-grand-architecture-layer-3-frontends-and-binary.md`. That work item proved too large to land in a single pass, and was split into three smaller work items:

- `0069-…` — CLI frontend + Layer 4 binary + `Cargo.toml` swap (merged before this work item starts).
- `0070-…` — TUI frontend (must be merged before this work item starts).
- `0071-…` (this work item) — Headless frontend.
- `0072-…` — Final parity validation, oldsrc removal, docs and aspec refresh.

The implementing agent **MUST** read `aspec/architecture/2026-grand-architecture.md`, the original `0069-…` (which already contains the headless section §3 and the §7u headless-defaults addendum), the current state of `src/data/`, `src/engine/`, `src/command/`, `src/frontend/cli/`, and `src/frontend/tui/`, and the legacy `oldsrc/commands/headless/server.rs` end-to-end.

## Scope

Build `src/frontend/headless/` per `0069-…` §3 and the §7u addendum. This includes:

- `mod.rs`, `routes.rs`, `command_frontend.rs`, `container_log.rs`, `workflow_state.rs`, `user_message.rs`, `worktree_lifecycle_frontend.rs`, `auth.rs`, `errors.rs`, `defaults.rs`.
- **The HTTP API surface MUST NOT change.** Every path, every HTTP method, every request body schema, and every response body schema must be wire-identical to `oldsrc/commands/headless/server.rs`.
- The single `POST /v1/commands` endpoint dispatches through `Dispatch::run_command` instead of spawning a child `amux` process.
- `HeadlessStartCommandFrontend::serve_until_shutdown` (declared in Layer 2) is wired to `crate::frontend::headless::serve(...)` from the CLI frontend's impl in `src/frontend/cli/`.
- Behavioral parity checklist in `0069-…` §3.
- The §7u defaults table (every interactive frontend method must return a safe non-interactive default; each MAY be overridden by request body parameters).

After this work item, `amux headless start` MUST start the new headless server and serve the same HTTP API as the legacy server, but with `POST /v1/commands` dispatching through Layer 2 instead of spawning a child process.

## What must NOT happen in this work item

- No business logic in `src/frontend/headless/`. If a frontend needs to make a decision that affects behavior, the missing surface is in Layer 2; add it there.
- No deletion of `oldsrc/`. That is `0072-…`.
- **No changes to the headless HTTP API surface.** No route paths, no HTTP methods, no request body fields, no response body fields.

## Test Considerations

Same philosophy as `0069-…` §"Test Considerations": **only Layer 3 unit tests and pure-presentation snapshot tests** plus the route-parity assertion guard. The full parity test suite is `0072-…`'s responsibility.

## Codebase Integration

- Follow `aspec/architecture/2026-grand-architecture.md` as the source of truth.
- Follow `0069-…` §3 and §7u for headless specifics.
- Do not edit `oldsrc/` (other than the README note).
- Do not delete `oldsrc/` — that is `0072-…`.
- After this work item lands, the next agent picks up `0072-grand-architecture-finalize-and-remove-oldsrc.md`.
