# Work Item: Feature

Title: Docker Sandbox Runtime (`docker-sbx`)
Issue: https://github.com/connerhicks/awman/issues/16

## Summary

Add Docker Sandboxes (`sbx`) as a third container runtime for awman, joining the existing `docker` and `apple-containers` backends. Docker Sandboxes (GA since January 30, 2026; CLI binary: `sbx`) runs AI coding agents inside isolated **microVMs** rather than Linux containers, providing hypervisor-grade isolation — each sandbox gets its own dedicated kernel, private Docker daemon, filesystem, and proxied network stack.

### What docker-sbx is

Docker Sandboxes is a purpose-built agent isolation product from Docker, distinct from Docker Desktop. It is not a general container runtime — it was designed specifically for running AI coding agents (Claude Code, Codex, Gemini CLI, etc.) in an environment where they can operate in YOLO mode without risking the host system. Isolation is via a custom VMM (not Firecracker) that uses native hypervisor APIs per platform: `Hypervisor.framework` on macOS, Windows Hypervisor Platform on Windows, and KVM on Linux. Each microVM runs Linux kernel 6.12 in an Ubuntu guest OS with 4 vCPUs and ~50% of host RAM (configurable). The `sbx` binary is standalone — Docker Desktop is not required.

### How it differs from Docker and Apple Containers

| Dimension | Docker (`docker`) | Apple Containers (`container`) | Docker Sandbox (`sbx`) |
|---|---|---|---|
| Isolation unit | Linux container (shared kernel) | Lightweight VM per container | MicroVM per sandbox session |
| Host escape risk | Container escape = host root | Hypervisor CVE required | Hypervisor CVE required |
| Image store | Shared with host | Shared with host | Private per VM; host images not visible |
| Image source | Local or registry | Local or registry | **Registry only** (or per-VM socket load) |
| Image reference | `org/img` auto-resolves to `docker.io` | Same | **Full domain required**: `docker.io/org/img` |
| Volume mounts | `-v host:container` bind mounts | `-v host:container` bind mounts | virtiofs passthrough; workspace appears at **identical absolute path** inside VM |
| User home config | Mountable anywhere | Mountable anywhere | **Not accessible** — only mounted workspace paths are visible |
| Env vars | `-e KEY=VALUE` or `--env-file` | Same | **Not inherited from host shell** — must use keychain secrets or Kit YAML |
| Networking | Host network or bridge; raw TCP/UDP | Native macOS network per container | All traffic through HTTP/HTTPS proxy only; raw TCP/UDP/ICMP blocked |
| Docker-in-Docker | Requires `--privileged` | Limited | Fully supported (private Docker daemon per VM) |
| Startup time | Milliseconds | ~1 second | 2–5 seconds cold start |
| Stats | `docker stats` | `container stats` | `sbx ls` (status only; no per-resource metrics) |
| macOS arch support | x86_64 + arm64 | arm64 (Apple Silicon) | **arm64 only** (Apple Silicon) |
| Linux support | Full | No | x86_64 only; **virtiofs file-creation bug** (Issue #51) makes it effectively unusable |
| Windows support | x86_64 | No | x86_64 |
| Platform required | Docker daemon | macOS 26 Tahoe | `sbx login` + Docker account (free tier OK) |

### Platform availability

- **macOS**: arm64 (Apple Silicon) only — no Intel Mac support
- **Windows**: x86_64 via Windows Hypervisor Platform
- **Linux**: x86_64 via KVM, but **a confirmed virtiofs bug (Issue #51, open as of June 2026)** prevents agents from creating any new files inside the workspace. This makes `docker-sbx` on Linux effectively non-functional for coding agents. Linux support should be gated with a clear user-facing error until this bug is resolved upstream.

### Key operational differences from Docker/Apple Containers

**Image management**: awman currently builds agent images locally with `docker build` and uses them by tag. With sbx, the per-sandbox Docker daemon is **private and isolated** — it cannot see host-local images. Two options exist:
1. **Registry push** (documented, clean): After `awman ready` builds the image, push it to a Docker Hub org or private registry. The sandbox pulls it on first use. Requires registry credentials configured via `sbx secret set --registry`.
2. **Per-VM socket load** (undocumented, no registry required): The `sandboxd` daemon at `~/.docker/sandboxes/sandboxd.sock` exposes an HTTP/JSON API over a Unix socket. Creating a VM returns a `socketPath` pointing to the VM's private Docker daemon socket. `docker --host unix://<socketPath> load` can inject a locally-built image directly. This approach depends on an undocumented, reverse-engineered API that is subject to change without notice.

**Recommendation**: Implement registry-push as the primary path (documented, stable) and the per-VM socket load as an opt-in fallback for users without a registry. The implementing agent must ask the developer which path to prioritize before building the image-management phase.

**Credential/auth passthrough**: awman currently mounts agent settings directories (`~/.claude/`, `~/.codex/`, etc.) as read-only overlays. This pattern is **impossible with sbx** — only workspace paths (the mounted project directory) are accessible inside the VM. The sbx ecosystem provides two alternatives:
1. **`sbx secret set -g <service>`**: Stores credentials in the OS keychain (macOS Keychain, Windows Credential Manager, Linux Keyring). Sbx automatically maps well-known services to their environment variable names: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GH_TOKEN`, `GEMINI_API_KEY`, `AWS_ACCESS_KEY_ID`, etc. These are injected at VM boot, not exposed as raw values inside the VM.
2. **Kit YAML `environment.proxyManaged`**: Credentials are held by the host-side proxy and injected into HTTP headers, never entering the VM guest address space.

Agent settings files (e.g., `.claude/settings.json`, MCP server configs) that are not API keys cannot be passed through sbx's standard mechanisms. These files would need to either be present in the workspace (project-level), bundled into the template image, or handled via a Kit that installs them during sandbox startup. This is a **major UX gap** relative to Docker and Apple Containers. The implementing agent must discuss this limitation with the developer and agree on the fallback behavior before implementing.

**Seeded prompts and system prompts**: awman uses a mix of file-mount + CLI flag, env-var + file-mount, and inline flag strategies for system/seeded prompts (see `ContainerOption::SystemPromptFile`, `SystemPromptEnvFile`, `SystemPromptInline`). Since workspace files are accessible inside sbx at their exact host paths, file-based prompt delivery strategies work as-is — the host path equals the container path in sbx, so no path translation is needed. Inline flags also work normally as part of the agent entrypoint.

**Container listing and stats**: awman polls `docker ps --filter label=awman=true` for running container stats. With sbx, the equivalent is `sbx ls --json` (if available) or parsing `sbx ls` output. There is no `docker stats`-equivalent memory/CPU metrics API exposed by sbx at the sandbox level; `ContainerStats` values will be unavailable or must be obtained by talking to the per-VM Docker socket. This is an acceptable degradation — the TUI stats panel can show sandbox status (running/stopped) rather than resource metrics.

**Interactive/PTY and re-attach**: `sbx exec -it <sandbox-name> <cmd>` opens an interactive PTY session. `sbx run <agent>` attaches to a running agent. A **known PTY bug (Issue #63)** means `stty size` reports `0 0` inside sbx exec sessions — the host terminal size is not propagated, which causes TUI applications inside the sandbox to misrender. The implementing agent should document this limitation and consider whether a workaround (e.g., passing `COLUMNS` and `LINES` as explicit env vars via the exec command) is feasible.

---

## User Stories

### User Story 1

As a: security-conscious developer

I want to: configure awman to run my coding agents using Docker Sandboxes (`docker-sbx`) instead of Docker containers

So I can: benefit from microVM-grade isolation when running agents in YOLO/auto mode, ensuring that even a misbehaving agent cannot escape to my host filesystem, host Docker daemon, or local network beyond what the sbx network proxy permits.

### User Story 2

As a: user who has switched to docker-sbx

I want to: use awman's full workflow suite — multi-agent workflows, seeded prompts, context overlays, and system prompts — with as few behavioral changes as possible

So I can: leverage the same awman workflow files I already have, with clear documentation of any features that behave differently or are unavailable under the sbx runtime.

### User Story 3

As a: user who tries to configure `docker-sbx` on an unsupported platform or with an unsupported feature

I want to: receive a clear, specific error message explaining what is not supported and why (rather than a panic or a silent fallback to a different runtime)

So I can: understand exactly what I need to change in my setup or workflow to move forward.

---

## Implementation Details

### Phase 0 — Research gate (before writing any code)

Before implementing any code, the implementing agent must do the following and report findings to the developer for approval:

1. Verify the current state of the virtiofs file-creation bug on Linux (Issue #51 in `docker/sbx-releases`). Confirm whether it is fixed in the latest `sbx` release. If fixed, Linux support can proceed. If still open, Linux must be blocked with a clear error message.
2. Verify whether `sbx ls --json` (or any machine-readable output format) exists and what its schema is. This determines how awman lists running sbx sandboxes.
3. Verify whether the per-VM Docker socket approach (via `sandboxd.sock`) still works as documented by Rivet in February 2026 — the API is undocumented and may have changed. Specifically, confirm that creating a VM via `POST /vm` to `~/.docker/sandboxes/sandboxd.sock` still returns `socketPath` in the response.
4. Determine whether `sbx secret set` for ANTHROPIC_API_KEY and other common agent credentials actually injects those keys automatically into `sbx run claude` (i.e., that the secret injection is agent-aware or always-on), or whether additional Kit YAML configuration is required. This directly impacts the credential-passthrough strategy.
5. Determine the exact behavior of `sbx run` when no `--name` is given — does it generate a name deterministically or randomly? This impacts awman's container naming strategy.
6. Confirm that `sbx exec -it <name> <cmd>` works with awman's PTY approach (portable-pty + subprocess). Specifically, confirm that the PTY terminal size bug workaround of passing `COLUMNS` and `LINES` as env vars to `sbx exec` is effective.

**Do not proceed past Phase 0 without developer approval of the findings.**

### Phase 1 — Config and runtime detection

Add `"docker-sbx"` as a recognized `runtime` value in `GlobalConfig`.

Extend `ContainerRuntime::detect` in `src/engine/container/runtime.rs` to match the new value:

```rust
Some("docker-sbx") => {
    // Linux: block until virtiofs bug is confirmed fixed
    if cfg!(target_os = "linux") {
        return Err(EngineError::BackendUnsupportedOnPlatform {
            backend: "docker-sbx".into(),
            platform: "linux (virtiofs file-creation bug not yet resolved upstream)".into(),
        });
    }
    // macOS: arm64 only
    #[cfg(target_os = "macos")]
    if std::env::consts::ARCH != "aarch64" {
        return Err(EngineError::BackendUnsupportedOnPlatform {
            backend: "docker-sbx".into(),
            platform: "macOS x86_64 (Intel — docker-sbx requires Apple Silicon)".into(),
        });
    }
    Backend::DockerSbx
}
```

Update `display_name()` to return `"Docker Sandboxes"` for the new backend. Update `runtime_name()` to return `"docker-sbx"`.

Update `cli_binary()` to return `"sbx"` for the sbx backend.

Add `Backend::DockerSbx` to the internal `Backend` enum.

### Phase 2 — `SbxBackend` implementation

Create `src/engine/container/sbx.rs` with a new `SbxBackend` struct that implements `ContainerBackend`.

**`build()` — constructing a `ContainerInstance`:**

The sbx backend builds a `SbxContainerInstance` analogous to `DockerContainerInstance`. The instance holds the resolved options and generates the `sbx run` or `sbx create` argv when executed.

The argv shape for `sbx run` differs from `docker run` in the following ways:
- No `-e KEY=VALUE` flag for env vars — sbx does not accept env vars via `run` flags; env must be handled via keychain secrets or Kit YAML
- No `-v host:container` bind mounts — workspace is passed as a positional path argument; additional read-only paths use the `:ro` suffix form
- Image/template is `--template <fully-qualified-registry-ref>`, not a positional argument; must include full domain (`docker.io/org/image:tag`)
- Name is `--name <sandbox-name>`
- Memory override is `--memory <size>g`
- No CPU limit flag
- No `--label` flag for awman session labels
- Agent entrypoint is a positional command that sbx passes to the agent runner inside the VM

**Mapping `ResolvedContainerOptions` to sbx argv:**

| awman Option | sbx Handling |
|---|---|
| `Image` | `--template docker.io/<registry-org>/<tag>` (requires registry-qualified name) |
| `Entrypoint` | Positional args after the workspace path(s) |
| `Overlay(spec)` | Additional workspace paths appended to sbx positional args with `:ro` if read-only; **only works if the host path is a subdirectory of or equal to the workspace** |
| `EnvPassthrough` | Not supported via sbx CLI; must have been pre-configured via `sbx secret set`; emit a warning if awman is asked to pass env vars that sbx cannot inject |
| `EnvLiteral` | Not supported via sbx CLI for arbitrary keys; only well-known service credentials are auto-injected; document clearly |
| `SeededPrompt` | Passed as CLI arg to agent entrypoint (works normally) |
| `Interactive` | Controls whether to use `sbx run` (attach) or `sbx create` (background) |
| `AllowDocker` | No-op — sbx VMs always have a private Docker daemon; note this in docs |
| `WorkingDir` | The primary workspace path positional arg; sbx starts agent in this directory |
| `Name` | `--name <name>` |
| `Memory` | `--memory <n>g` |
| `Cpu` | **Not supported** — sbx does not expose CPU limits; log a warning and continue |
| `AgentSettingsPassthrough` | **Not supported** — home-dir overlays outside workspace are inaccessible; emit an error or warning; discuss fallback with developer in Phase 0 |
| `AgentCredentials` | **Partially supported** — sbx auto-injects well-known API keys if configured via `sbx secret set`; awman should verify required secrets are present at session start |
| `SessionLabel` | **Not supported** — sbx has no label mechanism; use naming convention for attribution |
| `SystemPromptFile` | Supported — host path equals container path in sbx virtiofs; no translation needed |
| `SystemPromptEnvFile` | Supported — same path identity |
| `SystemPromptInline` | Supported — passed as CLI flag to agent |
| `DisallowedTools` / `AllowedTools` | Passed through as agent CLI flags in entrypoint |
| `Model` | Passed through as agent CLI flag |
| `KeepContainer` | `sbx stop` instead of `sbx rm` after session ends (stopped sandboxes preserve installed packages) |

**`list_running()` and `list_running_all()`:**

Shell out to `sbx ls` and parse output. During Phase 0, determine whether a machine-readable (`--json`) flag is available. If so, parse JSON. If not, parse the tabular output. Filter by name prefix (`awman-`) or a naming convention established for sbx sandboxes. Return `ContainerHandle` with the sandbox name as the ID.

**`stats()`:**

`ContainerStats` (CPU%, memory MB) cannot be obtained from the sbx CLI without accessing the per-VM Docker socket. Implement a degraded path that returns a `ContainerStats` with zeroed resource metrics and a status indicator (running/stopped). If the per-VM Docker socket approach is validated in Phase 0, implement an optional enhanced stats path using `bollard` against the per-VM socket.

**`stop()`:**

Shell out to `sbx stop <name>`. Note: this pauses the VM; `sbx rm <name>` deletes it. awman's `stop()` semantics map to `sbx stop` (pause), consistent with `docker stop`. Add a `remove()` equivalent that calls `sbx rm`.

**`exec_args()`:**

Returns argv for `sbx exec -it <name> <entrypoint...>`. The PTY terminal size workaround (appending `COLUMNS` and `LINES` env vars) should be applied if confirmed effective in Phase 0.

**`image_home_dir()`:**

Not applicable — sbx does not expose image inspection. Return `None`. The home dir inside sbx templates is known from the official base images (`docker/sandbox-templates:claude-code` uses `/home/user`). awman should assume a default home and allow override via config rather than probing the image.

**`start_background()`:**

Use `sbx create <agent> <workspace>` to start a sandbox without attaching. Return the sandbox name as the container ID. Override the default background impl rather than inheriting it (the default uses `docker run -d` which has no sbx equivalent).

**`is_available()` (on `ContainerRuntime`):**

Shell out to `sbx version` or `sbx ls` with a 10-second timeout. A non-zero exit code or binary-not-found means sbx is unavailable.

### Phase 3 — Image management for sbx

This phase has two sub-paths; the developer must choose one (or both) in Phase 0.

**Sub-path A — Registry push (primary):**

Extend `ContainerRuntime::build_image()` to accept an optional `push_registry: Option<&str>` parameter (or add a new `push_image()` method). After building the image locally with `docker build`, `docker tag <local-tag> <registry-ref>` and `docker push <registry-ref>`. The registry org and credentials are read from global config. The `ImageRef` used by the sbx backend must include the full domain-qualified registry reference.

Add a `sbx_registry_org` field to `GlobalConfig` (or extend existing config) to store the registry org for sbx image pushes:

```json
{
  "runtime": "docker-sbx",
  "sbx_registry_org": "docker.io/myorg"
}
```

`awman ready` should prompt for this value when initializing sbx support and validate that `docker push` succeeds before completing the ready check.

**Sub-path B — Per-VM socket load (opt-in, no registry required):**

During `awman chat` or `awman exec`, before spawning the sbx sandbox, create a temporary sandbox via `POST /vm` to `~/.docker/sandboxes/sandboxd.sock`, receive the `socketPath`, run `docker save <tag> | docker --host unix://<socketPath> load` to inject the locally-built image, then proceed with `sbx run`. This avoids a registry requirement at the cost of using an undocumented API.

Gate this sub-path behind a `sbx_image_transfer: "local"` config option. Default to Sub-path A.

### Phase 4 — `awman ready` integration

Update the ready engine to handle sbx:

1. Check that `sbx` binary is in PATH and `sbx login` has been completed (probe via `sbx ls`).
2. If Sub-path A: check that `sbx_registry_org` is configured, verify `docker push` access works for a test tag.
3. If Sub-path B: verify that `~/.docker/sandboxes/sandboxd.sock` exists and is connectable.
4. Build the agent image as usual via `docker build`. Push to registry if Sub-path A, or skip if Sub-path B (the load happens at run time).
5. Run the auto-auth pre-flight (Phase 5) for all required agent credentials. Prompt the user for any missing keys using the existing ready frontend, then call `sbx secret set -g <service>` for each one. Report which secrets were registered and which were skipped.
6. Report the same per-phase status messages as Docker/Apple Containers ready, with sbx-specific notes where behavior differs.

### Phase 5 — Credential passthrough strategy

#### Auto-auth: awman-managed `sbx secret set`

For well-known API keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GH_TOKEN`, `GEMINI_API_KEY`, `AWS_ACCESS_KEY_ID`, etc.), awman handles credential injection automatically rather than requiring the user to pre-configure `sbx secret set` manually. The strategy:

**At `awman ready` time**: awman reads each required credential from the host (keychain or environment, using the same `AuthEngine` resolution path used for Docker/Apple Containers), then calls `sbx secret set -g <service>` for each one. This populates the sbx keychain so that any sandbox launched for this agent will have its credentials auto-injected by sbx at VM boot. The ready frontend reports which secrets were registered successfully and which were missing.

**At agent launch time (pre-flight)**: Immediately before spawning the sbx sandbox — in `SbxBackend::build()` or the `ContainerExecution::run` path — awman re-runs `sbx secret set -g <service>` for each credential required by the agent being launched. This ensures that:
- Rotated credentials (e.g., a refreshed `ANTHROPIC_API_KEY`) are always up to date in the sbx keychain without requiring the user to manually re-run `awman ready`.
- Credentials added after the last `awman ready` are picked up automatically.
- The sbx sandbox always launches with fresh secrets regardless of when ready was last run.

The per-launch injection is fast (a few subprocess calls to `sbx secret set`) and should not meaningfully affect session startup time relative to the 2–5 second VM cold-start.

**Service-to-credential mapping**: The agent matrix (`src/engine/agent/agent_matrix.rs`) already knows which credentials each agent requires. The sbx backend should consult the agent matrix to determine which `sbx secret set` calls to make, using a new mapping from awman credential names to sbx service names:

| awman credential | sbx service name |
|---|---|
| `ANTHROPIC_API_KEY` | `anthropic` |
| `OPENAI_API_KEY` | `openai` |
| `GH_TOKEN` / `GITHUB_TOKEN` | `github` |
| `GEMINI_API_KEY` | `google` |
| `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` | `aws` |
| `GROQ_API_KEY` | `groq` |
| `MISTRAL_API_KEY` | `mistral` |

This mapping should live in `SbxBackend` or a dedicated `sbx_auth.rs` module, not in the agent matrix itself (the mapping is sbx-specific and must not leak into shared data structures).

If a credential is not available on the host (not in keychain, not in environment), awman should warn at launch time — the same behavior as today when a Docker container is launched without a required key — rather than silently proceeding and letting the agent fail inside the VM.

#### Agent settings files

For agent settings files (`.claude/settings.json`, MCP configs, etc.): awman cannot mount `~/.claude/` as an overlay since sbx only mounts workspace paths. Two sub-options:
  - **Project-level config**: awman detects that the agent config is project-level (already in workspace) and no extra action is needed.
  - **Kit-based injection**: awman generates a Kit YAML that includes `commands.install` steps to copy the required config files from a workspace-accessible staging location into the correct location inside the VM. This staging location must itself be within the workspace.
  - **Unsupported, clear error**: awman emits a clear warning that agent home-dir settings are not available under sbx, and instructs the user to use project-level configuration.
  The implementing agent must recommend one of these and get developer approval before implementing.

### Phase 6 — Documentation

Update `docs/` to reflect docker-sbx as a supported runtime. Do not create a work-item doc — update the existing runtime/configuration docs to describe `docker-sbx` alongside `docker` and `apple-containers`, including its limitations and setup requirements.

---

## Edge Case Considerations

### Platform and architecture guards

- **Linux**: Block `docker-sbx` at `ContainerRuntime::detect` with a descriptive error until the virtiofs file-creation bug (Issue #51) is confirmed fixed. Recheck during Phase 0.
- **Intel Mac**: Block `docker-sbx` on `x86_64` macOS with a clear message: "Docker Sandboxes requires Apple Silicon (arm64). Intel Macs are not supported."
- **`sbx` binary not in PATH**: `is_available()` must return `false` with a helpful error telling the user to install sbx via `brew install docker/tap/sbx`.
- **`sbx` not logged in**: `sbx ls` will fail with an auth error. Detect this case and surface a message: "Run `sbx login` to authenticate Docker Sandboxes."

### Image management

- **Image not in registry**: If Sub-path A is used and the image was never pushed, `sbx run --template` will fail when the sandbox tries to pull. awman should check at session start whether the required image tag is accessible (e.g., via `docker manifest inspect`), and fail early with a message directing the user to run `awman ready`.
- **Registry auth for private images**: `sbx secret set --registry <domain>` must be called for any non-public registry. awman should detect a pull failure and emit a prompt.
- **Image tag format without full domain**: awman's existing `project_image_tag()` produces `awman-myproj-claude:latest` without a registry domain. For sbx, this must be prefixed with the configured registry org. Do not silently rewrite the tag — make the mapping explicit and testable.
- **Stale image in VM cache**: Once pulled, the image is cached in the VM's private Docker daemon. If the user runs `awman ready --no-cache` and pushes a new image to the registry, existing stopped sandboxes retain the old image. The user must `sbx rm <name>` to clear the cache, which also destroys any in-sandbox state. Document this clearly.

### Credential and overlay passthrough

- **Auto-auth failure — credential not found on host**: If the auto-auth pre-flight cannot resolve a required credential (not in keychain, not in environment), awman must warn at launch time with the same message style used for Docker/Apple Containers. Never silently launch a sandbox that will fail due to a missing key.
- **Auto-auth failure — `sbx secret set` subprocess error**: If `sbx secret set -g <service>` exits non-zero (e.g., sbx keychain is locked, sbx daemon is unresponsive), treat it as a launch-blocking error — do not proceed with `sbx run` and leave the user with a clear error message and the exact `sbx secret set` command they can run manually to diagnose.
- **Credential rotation**: Because awman re-runs `sbx secret set` at every launch, rotated credentials are picked up automatically. No user action is required between key rotations beyond updating the host keychain. Document this as a feature of the auto-auth strategy.
- **`AgentSettingsPassthrough` option with sbx backend**: awman currently passes agent settings (e.g., `~/.claude/`) as overlay mounts. When the sbx backend receives `ContainerOption::AgentSettingsPassthrough`, it must detect that the overlay host path is outside the workspace and either error, warn, or implement the agreed fallback. Never silently drop overlays.
- **`EnvPassthrough` for non-service env vars**: If awman is asked to pass through an env var that is not in the sbx service mapping, it cannot be injected via the auto-auth mechanism. Emit a warning listing the variables that cannot be passed and explain that they must be configured via `sbx secret set-custom` (experimental as of June 2026) or Kit YAML.
- **`EnvLiteral` with sensitive values**: awman must not log or expose literal credential values. Existing masking behavior must apply to the sbx path as well. The auto-auth path reads credentials from the host keychain and pipes them to `sbx secret set` via stdin — never via argv — to avoid credential leakage in process listings.

### Networking and `--allow-docker`

- **`AllowDocker` option**: In Docker/Apple Containers mode, this mounts the host Docker socket into the container. With sbx, every sandbox already has a private Docker daemon — `--allow-docker` is effectively always on. The sbx backend should treat `AllowDocker` as a no-op and emit a debug trace noting this. The agent inside the VM can use `docker` against the private daemon without any special flags.
- **Network proxy and HTTP clients**: All traffic from the sbx VM goes through the Docker-managed proxy on `host.docker.internal:3128`. Agents that use raw TCP (e.g., SSH-based Git operations, databases) will not work unless the network policy permits them. awman's `awman ready` for sbx should warn that only HTTP/HTTPS egress is available by default.

### PTY and interactive sessions

- **Terminal size not propagated (Issue #63)**: `sbx exec -it` does not forward the host terminal window size. TUI applications inside the VM (the agent's interactive mode) may misrender. As a workaround, awman can pass `--env COLUMNS=<width>` and `--env LINES=<height>` (obtained from the host PTY at session start) to `sbx exec`. Verify this is effective in Phase 0.
- **Re-attach to existing sandbox**: `sbx exec -it <name> bash` can re-attach to a running sandbox; awman's TUI re-attach flow uses `exec_args()` for this. Confirm the sbx backend's `exec_args()` returns the correct argv.
- **Seeded prompt delivery**: awman writes the seeded prompt to a temp file and mounts it, or passes it inline. With sbx, inline delivery via CLI flag is the most reliable path since temp files outside the workspace are not mountable.

### Lifecycle

- **Sandbox naming collisions**: `sbx run --name` will fail if a sandbox with that name already exists. awman's naming (`awman-<session>-<hash>`) must remain unique. On collision, surface a clear error rather than letting the sbx error propagate raw.
- **Stopped vs. removed sandboxes**: `sbx stop` (paused) and `sbx rm` (deleted) are different. awman's session cleanup logic must call `sbx rm` (not just `sbx stop`) when a workflow or session ends completely, to avoid accumulating stopped sandboxes that consume disk space.
- **`sbx reset` danger**: `sbx reset` removes all sandboxes and clears the image cache. awman should never call this command automatically.
- **Port mappings lost on stop**: If awman or a workflow step uses port forwarding via `sbx ports`, the mapping is lost when the sandbox stops. awman should document that port-based workflows require the sandbox to remain running for the duration.

### Multi-agent workflows

- **Concurrent sbx sandboxes**: Each workflow step that requires a container spawns a separate sbx sandbox. This is architecturally correct but incurs a 2–5 second cold-start overhead per step. Multi-step workflows will be noticeably slower than Docker equivalents. Document this and consider whether a "persistent sandbox" mode (reuse a single sandbox across steps via `sbx exec`) is worth implementing in a follow-on work item.
- **Sandbox naming per step**: Each workflow step sandbox must have a unique, deterministic name incorporating the session ID and step index to avoid collisions in concurrent multi-agent scenarios.

---

## Test Considerations

### Unit tests

- **`SbxBackend::build()` argv construction**: For each `ContainerOption`, verify the generated `sbx run` / `sbx create` argv is correct. Use the same pattern as existing Docker backend tests — no live sbx binary needed.
- **Unsupported option handling**: Verify that options unsupported by sbx (e.g., `AgentSettingsPassthrough` with a non-workspace path, `EnvLiteral` with a non-service key, `Cpu`) produce the expected warning/error rather than silently being dropped.
- **Platform guard in `ContainerRuntime::detect`**: Unit test that `"docker-sbx"` returns `BackendUnsupportedOnPlatform` on Linux (and x86_64 macOS), consistent with the existing test for `"apple-containers"` on non-macOS.
- **ImageRef registry prefix enforcement**: Test that a bare `awman-myproj-claude:latest` tag is rejected (or correctly prefixed) when building sbx options, while `docker.io/myorg/awman-myproj-claude:latest` is accepted.
- **`list_running()` output parsing**: Table-driven tests for `sbx ls` output parsing, including edge cases (no sandboxes running, sandbox names with hyphens, malformed output).
- **Auto-auth service mapping**: Unit test the awman-credential → sbx-service-name mapping table for every known credential. Verify that an unmapped credential name produces a warning rather than an error (it may still be valid, just not auto-injectable).
- **Auto-auth credential not found**: Verify that the pre-launch credential resolution path returns a clear warning (not a panic) when a required key is absent from both the host keychain and environment.
- **Auto-auth stdin pipe — no argv leakage**: Verify that `sbx secret set` is invoked with the credential value piped via stdin and never as a positional argument or environment variable that would appear in a process listing.

### Integration tests

- Gate all sbx integration tests behind `#[cfg(target_os = "macos")]` (or equivalent for macOS arm64) and an env var guard (`AWMAN_TEST_SBX=1`) so they are opt-in and don't run in standard CI.
- **Sandbox create/list/stop/remove lifecycle**: Spawn a real sbx sandbox with a minimal template, verify it appears in `sbx ls`, stop it, verify it shows as stopped, remove it.
- **`is_available()` with sbx installed vs. not installed**: Test both paths.
- **Image load via per-VM socket (Sub-path B only)**: If implemented, integration test that `docker --host unix://<socketPath> load` successfully makes an image available inside the VM.

### End-to-end tests

- `awman ready` with `runtime: "docker-sbx"` completes without error on a configured macOS arm64 machine (opt-in, env-gated).
- `awman exec` with a simple non-interactive prompt runs and returns output through the sbx backend.
- Multi-agent workflow with two sequential sbx steps completes and produces the expected artifacts.

### Regression tests

- All existing `docker` and `apple-containers` backend tests must continue to pass — this change adds a new backend and must not touch the existing ones.
- `ContainerRuntime::detect` with `runtime: "blarg"` (unknown) still falls back to Docker with a warning.

---

## Codebase Integration

- **`src/engine/container/sbx.rs`**: New file, mirrors the structure of `docker.rs` and `apple.rs`. Add `pub(super) struct SbxBackend` implementing `ContainerBackend`. Do not make any type `pub` beyond `pub(super)` — the backend is invisible outside `src/engine/container/`. Optionally extract the auto-auth logic into `src/engine/container/sbx_auth.rs` if the mapping table and subprocess calls grow complex enough to warrant a separate module.
- **Auto-auth integration point**: The auto-auth pre-flight (`sbx secret set` calls) must run in the `ContainerRuntime` or `ContainerExecution` execution path — not inside `SbxBackend::build()`, which only constructs options. The right place is wherever awman resolves `AgentCredentials` options before launching a container. The sbx backend should override or extend that path to call `sbx secret set -g <service>` for each credential rather than (or in addition to) emitting `-e KEY=VALUE` argv. Credentials must be read from the host via `AuthEngine` and piped to `sbx secret set` stdin.
- **`src/engine/container/mod.rs`**: Add `mod sbx;` alongside the existing `mod docker;` and `mod apple;`.
- **`src/engine/container/runtime.rs`**: Add `Backend::DockerSbx` to the internal enum, wire it in `detect()`, update `display_name()`, `runtime_name()`, `cli_binary()`, `build_image()` (for registry push path), and `is_available()`.
- **`src/engine/container/options.rs`**: No new `ContainerOption` variants are needed. The sbx backend handles the existing options with a different mapping — adding sbx-specific options would leak runtime concerns into the shared option set, violating the layer-0 contract.
- **`src/data/config/global.rs`**: Add `sbx_registry_org: Option<String>` field. This is the only data-layer change needed for Phase 1.
- **`src/engine/ready/`**: Update the ready phases to handle sbx-specific checks (sbx binary, sbx login status, registry config) without branching excessively — the existing ready phases use trait-based frontend delegation, so add sbx checks as new ready phase steps that are only active when the runtime is sbx.
- **Layer discipline**: `SbxBackend` lives in Layer 1 (engine). It must not import from Layer 2 or Layer 3. All subprocess invocations use `std::process::Command` as in the existing backends. No new async dependencies should be introduced in `SbxBackend` — keep it sync, consistent with the existing Docker and Apple backends.
- **Error messages**: All sbx-specific errors and warnings must be user-facing and actionable. Avoid leaking raw `sbx` CLI error output — wrap it in `EngineError::Container(...)` with a context string. Follow the existing error wrapping patterns in `docker.rs`.
- **Naming convention**: awman sandboxes in sbx should be named `awman-<session_label>` using the same `generate_container_name()` utility already used for Docker containers, since sbx also uses `--name` for named sandboxes.
- **`bollard` crate (optional)**: If Sub-path B (per-VM socket) or enhanced stats are implemented, `bollard` is the appropriate crate for speaking the Docker API over a Unix socket. Add it to `Cargo.toml` only if this path is chosen. Do not add it speculatively.
- **Read the grand architecture doc**: Before implementing, re-read `aspec/architecture/2026-grand-architecture.md` in full. Pay particular attention to the Layer 1 `ContainerRuntime` design: factory pattern, `ContainerInstance` trait, `ContainerExecution` flow, and the principle that backends are invisible outside `src/engine/container/`.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** — the runtimes section (whichever doc describes `docker` and `apple-containers`) should gain a `docker-sbx` entry describing setup, limitations, and the `sbx_registry_org` config field
- **Create a new user guide only if needed** — if no existing doc covers runtime selection, create `docs/XX-runtimes.md` as a user guide covering all three runtimes
- **Never create work-item-specific docs** — no "WI 0089 implementation guide" in published docs
- **Keep all technical/implementation details in this work item spec or code comments**, not in `docs/`
- **Docs are for end users** — the limitation table (what works, what doesn't, on which platforms) belongs in user docs in plain language, not in the work item

See `CLAUDE.md` for more guidance on documentation standards.
