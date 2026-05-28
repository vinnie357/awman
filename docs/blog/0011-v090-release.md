# awman 0.9: from issue to merged PR

In the [last post](https://blog.cohix.network/amux-is-becoming-awman-an-agentic-workflow-manager-for-the-entire-software-development-lifecycle/), I announced that `amux` was becoming `awman` — the Agentic Workflow Manager. The rename reflected a shift in focus: the tool had grown from a terminal multiplexer for agents into something closer to an SDL pipeline, orchestrating the entire process from spec to merged PR. This release gets the new name finalized (the script below will install it and clean up amux for you if you had it installed). It also brings things a bit closer to that full end-to-end workflow manager vision with setup/teardown steps for workflows. These steps all get executed in your project's base container image for safety, and can be given overlays (i.e. directories, env vars, or agent skills from the host machine). This allows you to do things like run tests to validate the agent workflow didn't break anything, push branches, and create PRs. Since setup/teardown steps are NOT agent-run (they're just shell commands executed in a container) you can safely give them access to credentials like SSH keys and GitHub tokens without fear of an agent misusing them.

The other big change is to what was formerly called headless mode, and now is called API mode. The implementation has shifted from a simple send-command-run-workflow design to a job queue design that allows multiple workflows or prompts to be queued for a given session, and the awman API server will execute them automatically, while providing status, structured workflow overviews, streaming logs, etc via the API. `awman remote` also gets much simpler with `awman remote session start|kill` and `awman remote exec workflow|prompt` as concrete commands instead of the old "pass an unstructured command string and we'll see what happens" approach. API mode is coming along really well, and it's especially useful with `--yolo` and setup/teardown steps, letting you truly go from an issue to a PR with all the steps along the way fully controlled by the developer.

---

```sh
# install or upgrade
curl -s https://prettysmart.dev/install/awman.sh | sh
```

---

## Agents as investigators

One thing that surprised me while building the Antigravity integration was how good code agents are at *investigation*, not just implementation.

The Antigravity CLI's file-based oauth authentication method is not documented anywhere. The official docs describe API key auth and the interactive OAuth browser flow, but there's a third path: if you've authenticated once, `agy` reads an OAuth token from a file on disk under `~/.gemini/antigravity-cli/`. I didn't find this in any documentation, a code agent did by running `strace` on the `agy` process, watching the file descriptors, and tracing exactly which files it tried to open during startup. The agent identified the token path, confirmed the format, and I used that to wire up awman's auth passthrough so Antigravity sessions inside containers can reuse your host credentials without the browser flow. Pretty neat.

This is the kind of task agents excel at: patient, thorough, systematic investigation that a human *could* do but rarely *would* do unprompted. You'd have to look up how to strace the binary (unless you know that by heart somehow?), then sift through all the syscalls, then correlate the file reads with the auth flow and.. and.. and.. An agent does it because you asked "spike on antigravity agent auth passthrough" and it has no fatigue when taking the tedious approach. It just does the menial crap you'd rather not. On the flip side, they do tend towards guessing at solutions with no proof unless you explicitly ask them to do deep dives, so ymmv.

I've seen similar results when agents investigate build failures, trace race conditions, or audit dependency trees. The pattern is the same: you describe what you're trying to understand, and the agent does the kind of methodical, exhaustive search that humans tend to shortcut. It changes the economics of investigation since the thing that was too tedious to bother with is now close to free.

## What ships in v0.9

**Setup and teardown phases.** Workflows can now define what happens before and after the agent steps. Clone a repo, create a branch, install dependencies in setup. Run tests, commit, push, and open a PR in teardown. All of it runs inside containers. Here's a real workflow that I use with awman — `dependency-upgrade-pr.toml`:

```toml
title = "Dependency Upgrade"

[[setup]]
type = "run_shell"
command = "git status"

[[step]]
name = "security-audit"
prompt = "Check dependencies for security issues and upgrade any affected packages..."

[[step]]
name = "version-audit"
depends_on = ["security-audit"]
prompt = "Review all dependencies for available updates, upgrade sequentially..."

[[teardown]]
type = "run_shell"
command = "make test"

[[teardown]]
type = "commit_changes"
message = "Update all available dependencies"
add_all = true

[[teardown]]
type = "push_branch"
overlays = ["ssh()"]

[[teardown]]
type = "create_pull_request"
title = "Security and Dependency Upgrades"
overlays = ["env(GITHUB_TOKEN)"]
```

That's a complete workflow you can point at any project. `awman exec workflow dependency-upgrade-pr.toml` and walk away. One agent audits for security vulnerabilities, a second one reviews and upgrades everything else, teardown runs the tests, commits, pushes, and opens a PR. The `overlays` on the last two steps give the teardown container SSH access for the push and a GitHub token for the PR — nothing else gets those credentials.

**Per-step overlays.** Each workflow step — including setup and teardown — can declare its own `overlays` array. Mount SSH keys, pass specific environment variables, or attach named skills to individual steps without leaking resources across step boundaries. This is how you give the push step SSH access without exposing your keys to the agent that's writing code.

**Queue-and-worker execution.** The API server now runs an async job queue backed by SQLite. Submit workflows via the API, workers pick them up. Sessions can be pointed at a local directory or a remote git repo that awman clones for you. Run multiple workflows in parallel on a beefy server while you work from your laptop.

**Antigravity agent.** Google's Antigravity 2.0 CLI (`agy`) is now a supported agent alongside Claude, Codex, and the rest. Config and OAuth passthrough work automatically. The `gemini` agent name still works but emits a deprecation warning.

## What else changed

The rename from `amux` to `awman` is complete. Config auto-migrates on first run. "Headless mode" is now "API mode." Markdown workflow files are gone — TOML and YAML only. Yolo mode got a unified `ContainerIo` interface, better stuck detection, and throttled countdown messages. The API frontend is hardened and restricted to `exec workflow` and `exec prompt`. Full details in the [release notes](https://github.com/prettysmartdev/awman/blob/main/docs/releases/v0.9.0.md).

---

Source and issues at [github.com/prettysmartdev/awman](https://github.com/prettysmartdev/awman). More at [prettysmart.dev](https://prettysmart.dev). Feedback and contributions welcome.
