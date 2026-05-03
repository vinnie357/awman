# new-amux observed issues

### ISSUE-1
When running `ready`, new-amux has several issues:

1.1: running the local-agent check with greeting message, old-amux showed the greeting and agent's response. new-amux should as well. The greeting itself doesn't even seem to be sent because claude auth has not been refreshed after new-amux ready is run. Ensure this is not a no-op.
**FIXED**: `check_agent_greeting` now runs the **host-local agent binary** (e.g. `claude --print <greeting>`) in print/non-interactive mode. The full 50-greeting list and time-seeded selection logic from old-amux are ported into `engine::ready`. Both the greeting sent (`> greeting`) and the first line of the agent's response (`< response`) are shown to the user. Running the local binary refreshes OAuth tokens so container-mounted credentials are current. Per-agent command args match old-amux exactly (`claude --print`, `codex exec`, `opencode run`, `gemini -p`, `copilot -p -i`, `crush run`, `cline task`).

1.2 the base image and audit image steps should show as 'ready' in the output (assuming the images both exist) rather than 'skipped', that could be confusing to the user. Also, the Dockerfile checks (base and agent(s)) don't show in the summary table.
**FIXED**: When images already exist (`!needs_build`), `base_image` and `agent_image` are now set to `StepStatus::Done` instead of `Skipped`. The Dockerfile is also set to `Done` (not `Skipped`) when it already exists. A "Dockerfile" row has been added to the summary table in the CLI `report_summary`.

1.3 the summary table is malformed when apple-containers is the configured runtime:
┌─────────────────────────────────┐
│ Ready Summary (apple-containers) │ < see here the line is not aligned
├──────────────────┬──────────────┤
│ Base image       │ – skipped    │
│ Agent image      │ – skipped    │
│ Local agent      │ ✓ done       │
│ Audit            │ – skipped    │
│ Legacy migration │ – skipped    │
└──────────────────┴──────────────┘
**FIXED**: `render_summary_box` in `helpers.rs` now expands the `inner` width when the title is wider than the natural table width (label + value columns), preventing the top/bottom borders from being shorter than the title line.

### ISSUE-2
exec workflow issues:

2.1: running exec workflow with an agent: claude step is failing with no auth/setup despite auth/setup passthrough working fine in `chat` and `exec prompt`:

amux exec workflow ./aspec/workflows/implement-hard.toml --work-item 71
Not logged in · Please run /login
amux: workflow summary — 0/1 steps OK (1 failed)
Workflow ./aspec/workflows/implement-hard.toml completed (exit 1).
**FIXED**: `CommandLayerFactory::execution_for_step` in `exec_workflow.rs` now calls `auth_engine.resolve_agent_auth()` and injects the resulting keychain credentials as `ContainerOption::AgentCredentials`, matching the auth pattern used by `chat` and `exec_prompt`.
