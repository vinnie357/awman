# new-amux issues

# Engines

ENG-1: When a workflow has two "parallel" steps (i.e. multiple steps that depends-on the same former step)ew-amux completes the first of the group and then considers the workflow complete and runs the post-workflow worktree flow. Ensure that "parallel groups" are handled correctly by the engine logic.

# Commands

COM-1: When a workflow using a worktree ends, and the dialog in the TUI presents the option to press m to 'merge into <current branch>', nothing happens and the worktree is left with uncommitted files and not merged into the current branch. Ensure the flow correctly then lists uncommitted files and asks for a commit message, then confirms to merge into current-branch. Ensure the gitengine portions all work, all git commands and their outputs are printed to the exection window, and that all options presented in all of the frontend dialogs in the pre- and post- workflow worktree flows work correctly as the user expects.
