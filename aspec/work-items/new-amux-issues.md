# new-amux observed issues

### ISSUE-1

1.1: Calling `new spec` does not ask what kind of work item it should be, and therefore does not replace the type placeholder in the resulting file. Ensure it behaves just like old-amux.

1.2 Passing `--interview` to `new spec` does not ask for the work item's interview prompt. Ensure this works for all the `new *` commands

1.3 Passing `--interview` to `new spec` results in an agent container with no settings or auth passthrough. Ensure it launches correctly with auth, settings, and interview prompt for all of the `new *` commands

### ISSUE-2

2.1: `exec workflow` does not need to print the workflow status table before AND after the yolo countdown between steps. Just once, before yolo countdown.

2.2 the workflow status table isn't very nice looking, make it nicer (proper table formatting):

```
yolo: auto-advancing to next step...

   #  Step       Agent   Model             Status
  ──  ─────────  ──────  ────────────────
   1  implement  claude  claude-opus-4-7   ✓ Done
   2  tests      claude  default           · Pending
   3  docs       claude  claude-haiku-4-5  · Pending
   4  review     claude  claude-opus-4-7   · Pending
  ──  ─────────  ──────  ────────────────
  ```

  2.3 when `exec workflow` runs an interactive agent container (i.e. when --non-interactive is NOT passed), no prompt is passed to the agent. It should be authed, set up, interactive, and prompted with the correct prompt for the given workflow step (including work-item template substitutions if applicable.) Ensure workflow step agent containers are properly prompted for both interactive and non-interactive.

  2.4 The user input during the yolo countdown doesn't work, typing n, a, or p and pressing enter just causes the yolo countdown to start printing on a new line and nothing happens. Ensure user input and the "pretty" single-line countdown timer both work

  2.5 Triple check to ensure that all workflow agent containers that are supposed to run in a worktree get mounted correctly to the worktree and not the main repo path. This applies to both --worktree and --yolo when a workflow is run
