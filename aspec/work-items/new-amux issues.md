# new-amux observed issues

### ISSUE-1
When running `ready`, new-amux has several issues:

1.1: ~~Running new-amux status command shows no containers running even when there are amux containers running. Ensure it's entirely working for both code and claw agent containers, review how old-amux did it.~~ **COMPLETED** — Docker backend now runs three `docker ps` queries (label filter + `name=amux-` + `name=nanoclaw`), merging and deduplicating by ID. Apple backend filter updated to also include `nanoclaw-*` containers.

### ISSUE-2
exec workflow issues:

2.1 ~~work item template value replacement isn't being done when `--work-item` is passed to `exec workflow`. Fix that and ensure every possible template insertion works (check the parsing logic for work item sections in old-amux and ensure it works for all the supported work item file types). All prompts passed to agent containers should have template values replaced with real work item values.~~ **COMPLETED** — `CommandLayerFactory` now carries a `WorkItemContext` loaded from the work-items directory (respecting `workItems.dir` repo config; falls back to `aspec/work-items/`). `substitute_prompt` is called for every step prompt, replacing `{{work_item_number}}`, `{{work_item}}`, `{{work_item_content}}`, and `{{work_item_section:[Name]}}`.

2.2 ~~when --work-item and --yolo are passed to `exec workflow`, the worktree and branch do not include the work item number, only the workflow name. Ensure the --work-item flag is fully implemented.~~ **COMPLETED** — When `--work-item` is supplied, `WorktreeLifecycle::for_work_item(number)` is used instead of `for_workflow(name)`, producing a branch like `amux/wi-<number>`.

2.3 ~~the --yolo flag passed to `exec workflow` seems to do nothing, there is no countdown after a workflow step ends in new-amux CLI.~~ **COMPLETED** — `WorkflowEngine` now has a `set_yolo(bool)` method; when enabled, `run_to_completion` replaces the inter-step user prompt with a 60-second countdown via `yolo_countdown_tick`. The CLI implementation displays a `\r`-overwritten countdown line, auto-advances on timeout, and (when a TTY is present) spawns a background stdin thread that maps `n`→advance-now / `a`/`p`→cancel/pause.
