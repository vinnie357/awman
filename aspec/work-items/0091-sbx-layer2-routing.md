# Work Item: Feature

Title: Route `awman chat` / `awman exec` / workflows to the sandbox runtime (`docker-sbx-experimental`)
Issue: https://github.com/connerhicks/awman/issues/16

**Prerequisite**: work item 0090 (Docker Sandbox Runtime — `DSbxBackend`). WI 0090 delivered the working engine tier (kit emission, lifecycle, credential injection, session config, `run_interactive`, `exec_args`) and the `awman ready` integration. This work item closes the remaining Layer-2 gap: the agent-launching commands still construct container-paradigm options unconditionally and error under the sandbox runtime.

## Summary

As of WI 0090, `awman ready` works end-to-end under `runtime: "docker-sbx-experimental"`, but `awman chat`, `awman exec`, and `awman exec workflow` still call `Engines::require_container_runtime()` (`src/command/commands/chat.rs`) and build `ResolvedAgentOptions::Container(...)` unconditionally — a sandbox-configured user gets a clear `EngineError::NotImplemented` ("this command does not yet route to the sandbox runtime") instead of a running agent. This work item teaches the Layer-2 commands to:

1. Detect the active runtime paradigm via `Engines` (the `sandbox_runtime: Option<Arc<SandboxRuntime>>` handle from WI 0089/0090) and construct `ResolvedAgentOptions::Sandbox(ResolvedSandboxOptions)` from the same flag/config inputs the container path consumes today (agent, workspace, seeded prompt, model, system prompts, tools, env, memory).
2. Route the launch through the `AgentRuntimeEngine` trait (`engines.runtime.build(...)` → `AgentInstance::run_with_frontend`), which WI 0090 already wired to `dsbx::run_interactive`.
3. Deliver seeded prompts for `kind: mixin` kits. WI 0090 writes the prompt into `<workspace>/.awman/session.json` and appends it positionally for `kind: agent` kits, but the mixin apply-scripts (`templates/sbx-apply.{claude,codex,gemini,copilot,opencode}.sh`) do not yet consume `seeded_prompt` (nor `model`, `system_prompt_*`, `allowed_tools`/`disallowed_tools` where the agent supports settings-file equivalents). Decide per agent: render into the agent's settings file (e.g. `$HOME/.claude/settings.json` supports `model` and permission rules), or invoke via `sbx exec` with the prompt as an argv tail per the WI 0090 spec.
4. Satisfy WI 0090's deferred checklist item: "`awman exec --runtime docker-sbx-experimental` no longer returns `EngineError::NotImplemented` — it actually runs." That item was deferred from WI 0090 (whose change-set scoped Layer 2 out) to this work item — see the History note in `0090-ghb16-docker-sandbox-runtime.md`.

## User Stories

### User Story 1:
As a: user who has configured `runtime: "docker-sbx-experimental"` and run `awman ready`

I want to:
run `awman chat <agent>` and `awman exec <agent> "<prompt>"` and have them launch microVM sandboxes

So I can:
actually use the sandbox runtime for day-to-day agent work, not just prepare it.

### User Story 2:
As a: user running multi-agent workflows under sbx

I want to:
`awman exec workflow` to create one persistent sandbox per agent and reuse it across steps

So I can:
pay the per-agent install cost once per worktree, as the persistent-sandbox model promises.

## Implementation Details:
- `src/command/commands/chat.rs` / `exec.rs` / `exec_workflow.rs`: branch on `engines.sandbox_runtime.is_some()` to build `SandboxOption` vectors instead of `ContainerOption` vectors; resolve via `ResolvedSandboxOptions::resolve` and launch via `engines.runtime.build(ResolvedAgentOptions::Sandbox(..))`.
- Re-attach: use `AgentRuntimeEngine::exec_args()` (already implemented for sbx with `--env` passthrough) and pass `COLUMNS`/`LINES` per the Issue #63 PTY-size workaround.
- Mixin seeded-prompt delivery: extend the five mixin `templates/sbx-apply.<agent>.sh` scripts (and bump `session.json` `schema_version` if their input contract changes) or switch delivery to `sbx exec` argv-tail. Keep the writer (`DSbxSessionConfig`) and the scripts versioned together.
- Background/non-interactive exec uses `SandboxBackend::start_sandbox` (`sbx create`) + `exec_in_sandbox`; note WI 0090's comment that the background create path cannot inject credentials (no sink) — decide where credential registration happens for that path (likely a pre-step with the command's sink, mirroring `run_interactive`).
- Layer discipline: Layer 2 talks only to `AgentRuntimeEngine` / `Engines` handles; no `DSbxBackend` or kit internals leak upward (WI 0089 invariant).

## Edge Case Considerations:
- `awman exec` with a seeded prompt against each kit kind (mixin vs agent) — both must deliver the prompt exactly once.
- Runtime switching mid-project: chat under Docker, then sbx, then Docker again (the WI 0090 switching test covers detection; extend it to a real launch path when env-gated).
- A stopped persistent sandbox must be restarted (`sbx run --name`, no `--kit`) by chat/exec, not recreated.
- Sandbox-unsupported flags surfaced exactly as in WI 0090 (`--cpus` warning, `--allow-docker` debug trace).

## Test Considerations:
- Unit: option-construction parity tests — for a fixed set of chat/exec flags, the sandbox option vector carries the same intent as the container vector (agent, prompt, model, tools, env classification).
- Env-gated integration (macOS arm64 + `AWMAN_TEST_SBX=1`): `awman exec` non-interactive prompt end-to-end; two-step workflow reusing one sandbox per agent (these were specified in WI 0090's test plan but blocked on this routing).
- Regression: all Docker/Apple chat/exec tests unchanged.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec, WI 0089 (`AgentRuntimeEngine` abstraction), and WI 0090 (dsbx engine tier).

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- Update `docs/16-runtimes.md`: remove the "chat/exec not yet routed" limitation once it ships; document mixin seeded-prompt behavior.
- **Never create work-item-specific docs**; keep implementation details in this spec or code comments.

See `CLAUDE.md` for more guidance on documentation standards.
