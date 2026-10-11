# Work Item: Feature

Title: Opt-in pre-agent startup gate for verified workspace mounts
Issue: downstream Altana workspace orchestration

## Summary

Add an opt-in container startup gate that verifies a prepared workspace inside
the actual container, announces readiness, and waits for an external
orchestrator to release the agent. The original agent argv is executed only
after release. Legacy launches remain byte-for-byte unchanged when the gate is
absent.

This lets an orchestrator start several containers, verify every member sees
the intended workspace and access mode, and then release all agents. A failure,
timeout or cancellation exits without executing the agent.

## User Stories

### User Story 1

As an orchestration caller, I want an awman container to prove its prepared
workspace mount before the agent starts, so I can fail the whole group before
any member sends a request to a model.

### User Story 2

As an operator, I want the gate to preserve awman's normal credentials, PTY,
signals and agent argv after release, so opting into verification does not
change the agent session beyond delaying its start.

## CLI contract

`chat`, `exec prompt` and the agent steps of `exec workflow` gain these
catalogue-defined flags:

```text
--startup-gate-control <DIR>
--startup-gate-timeout <SECONDS>   default 120; valid range 1..=3600
```

`--startup-gate-timeout` without `--startup-gate-control` is a usage error.
`--allow-docker` with `--startup-gate-control` is also a usage error. The
combination is rejected before agent setup, image build, or launch because the
current privileged bootstrap restores the image user's group contract and
cannot preserve Docker's added socket group. Ungated `--allow-docker` remains
supported.

All three commands validate and snapshot the control request before agent
availability checks, image setup, runtime construction, or launch. A gated
workflow uses a single-attempt policy: its initial launch still occurs, but
restart and relaunch actions are neither advertised nor accepted, including an
action returned directly by a frontend. This includes returning to a previous
step: ordinary and failure control boards explain why the action is unavailable,
and an injected action is rejected before resetting either step or launching
again. A mid-step injected action still stops the currently owned execution
before returning the rejection. A public single-attempt run terminates on a
failed step without presenting the interactive failure board; the private
failure-handler regression covers only its injected-action validation boundary.
Ungated workflow retry behavior is unchanged.
The flags are command-mode only in this work item. API, remote, squad, TUI,
setup/teardown entries, ACP and sandbox-class (`sbx`) launches reject the gate
as unsupported before any container starts. Supporting them later requires an
explicit parity work item; no frontend silently ignores the flag.

The caller creates `DIR` with mode `0700`, owned by the current uid, not a
symlink, outside the selected source workspace. It contains a mode-`0600`
`request.json`:

```json
{
  "version": 1,
  "bindings": [
    {
      "id": "review-input",
      "workspace_path": "/review/input",
      "manifest_id": "64 lowercase hexadecimal characters",
      "manifest_file": "review-input.manifest.json",
      "access": "read-only"
    }
  ]
}
```

Each binding has a distinct safe id. `access` is `read-only` or `read-write`.
`workspace_path` must be an absolute,
normalized container path without NUL, `.` or `..` components and must not be
`/`, `/proc`, `/sys`, `/dev`, `/run`, `/etc`, `/home` or an ancestor of those
paths. Binding paths may not overlap. `manifest_file` is one plain basename;
its mode-`0600`, nonsymlink file lives in the control directory and is capped
at 8 MiB. Its exact v1 JSON wire format is:

```json
{
  "version": 1,
  "entries": [
    {"path":"README.md","kind":"file","size":123,"sha256":"64 lowercase hex"},
    {"path":"src","kind":"directory","size":0,"sha256":null}
  ]
}
```

The file is UTF-8 with no BOM. `entries` is strictly sorted by raw UTF-8 path
bytes and paths are unique, relative slash paths with no empty, `.`, `..`, NUL
or backslash component. `kind` is exactly `file` or `directory`; directories
have size zero and a null digest. Every entry must contain the `sha256` key;
files require 64 lowercase hexadecimal characters and directories require
explicit JSON `null`. Unknown fields are rejected. `manifest_id` is
the lowercase SHA-256 of the manifest file's exact raw bytes, with no JSON
re-serialization or canonicalization. It identifies the prepared base's
content; callers needing another source identity record it separately.

The gate creates these files atomically in `DIR`:

- `ready.json`: gate version, a cryptographically random 256-bit lowercase-hex
  nonce, awman container name, and every binding's id, manifest id, effective
  access, and workspace path. No credentials or host source paths.
- `release.json`: created by the caller to release the agent. It contains
  exactly `{"version":1,"nonce":"<ready nonce>"}`.
- `failure.json`: stable failure code and non-secret message.

Awman refuses a control directory containing stale `ready.json`,
`release.json` or `failure.json`. One directory is single-use and belongs to
one container.

`/.awman/startup-gate` is a reserved operational namespace. User overlays,
configured overlays and requested bindings whose guest paths equal, contain or
are contained by that root are rejected before launch.

### Source-binding and interpreter trust boundary

Startup-gated source bindings are limited to the guest roots `/workspace`,
`/review`, `/work`, `/data`, `/mnt` and `/output`, including normalized paths
beneath those roots. A binding may not use an ancestor of an allowed root.
User overlays and configured overlays are rejected when their guest path
equals, contains or is contained by `/bin`, `/sbin`, `/usr`, `/lib`, `/lib64`,
`/etc`, `/proc`, `/sys`, `/dev` or `/.awman/startup-gate`. This validation is
performed on the complete effective overlay set before container launch.

For a gated launch, configured environment entries whose names begin with
`LD_` or `DYLD_` are rejected before launch. The bootstrap always starts as
`/usr/bin/python3 -I -S` with `PYTHONHOME` and `PYTHONPATH` absent from its
environment. The exact original environment, including any original Python
variables, is restored only for the final agent `exec`. These rules assume a
trusted base image: awman verifies that the selected container-class image is
one of its supported image templates, or otherwise rejects the gate. They do
not claim to make an attacker-controlled image trustworthy.

## Data and engine API

Layer 0 (`src/data/startup_gate.rs`) defines:

```rust
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupGateRequest {
    pub version: u32,
    pub bindings: Vec<StartupGateBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupGateBinding {
    pub id: String,
    pub workspace_path: String,
    pub manifest_id: String,
    pub manifest_file: String,
    pub access: StartupGateAccess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartupGateAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupGateSpec {
    pub control_dir: std::path::PathBuf,
    pub request: StartupGateRequest,
    pub timeout: std::time::Duration,
}

pub fn load_startup_gate(
    control_dir: &std::path::Path,
    timeout: std::time::Duration,
) -> Result<StartupGateSpec, StartupGateError>;
```

`load_startup_gate` uses `symlink_metadata`, validates uid and exact directory
and request-file modes on Unix, caps `request.json` at 4096 bytes, rejects
symlinks, hard-linked request/manifest files, duplicate ids or paths, path
overlap and stale status files, and canonicalizes only after those checks.
Non-Unix platforms return `UnsupportedPlatform` before launch.

Layer 1 adds one typed container option:

```rust
ContainerOption::StartupGate(StartupGateSpec)
```

and the corresponding field:

```rust
pub startup_gate: Option<StartupGateSpec>
```

on `ResolvedContainerOptions`. Duplicate options are an error. Sandbox
resolution rejects the option. Container-class resolution passes it unchanged
to `ContainerInstance`.

Layer 1 (`src/engine/container/startup_gate.rs`) defines:

```rust
pub const CONTAINER_GATE_ROOT: &str = "/.awman/startup-gate";

pub fn stage_startup_gate(
    spec: &StartupGateSpec,
) -> Result<StagedStartupGate, EngineError>;

pub struct StagedStartupGate {
    pub overlays: Vec<OverlaySpec>,
    pub wrapper_argv: Vec<String>,
    pub cleanup: StartupGateCleanup,
}

pub fn wrap_entrypoint(
    staged: &StagedStartupGate,
    original: &[String],
) -> Result<Vec<String>, EngineError>;
```

`stage_startup_gate` writes an awman-authored bootstrap into an awman-owned
mode-`0700` staging directory and mounts that directory read-only at
`/.awman/startup-gate/bin`. It mounts the caller's validated control directory
read-write at `/.awman/startup-gate/control`. These operational mounts are
separate from source overlays and do not broaden the project mount.

The bootstrap is fixed trusted Python source embedded in the awman binary. This
work item adds `python3-minimal` to every supported container-class agent image
template and smoke-tests `python3` in each image. A custom image without the
interpreter fails the gate before its agent runs. Python is chosen because the
gate needs `lstat`, link counts, binary-safe paths, SHA-256, atomic fsync/rename
and a monotonic clock; shell builtins cannot provide that contract robustly.
Awman invokes the trusted absolute `/usr/bin/python3` as
`/usr/bin/python3 -I -S /.awman/startup-gate/bin/bootstrap.py ...`. It removes
`PYTHONHOME` and `PYTHONPATH` from the bootstrap environment; isolated mode,
disabled site loading and the absolute script path prevent workspace imports,
user site hooks and path substitution. It accepts the original argv after
`--`, never evaluates or joins it, never invokes a shell with caller input,
and ends with the equivalent of:

```python
os.execvp(original_argv[0], original_argv)
```

where `"$@"` is the untouched original entrypoint and arguments. The wrapper
is inserted after the image in `build_run_argv`; Docker and Apple Containers
use the same construction. PTY/piped selection, environment, credentials,
labels, workdir and cleanup flags remain unchanged.

## Gate protocol

The workspace producer creates each canonical full-content manifest after
preparing its immutable base. Host-side awman validates each manifest digest
before launch. The manifest describes the prepared copy and is never added to
the caller's original source.

Inside the container the bootstrap:

1. Validates its compiled protocol version, binding count and argv boundary.
2. Parses `/proc/self/mountinfo`, finds the most-specific effective mount
   covering every binding, and requires `ro` or `rw` to match the request.
   A nested mount beneath a binding root that is absent from the request is an
   unexpected exposure and fails the gate.
3. Walks every binding without following symlinks, rejects symlinks, special
   files and multiply linked regular files, and compares the complete sorted
   tree, sizes and SHA-256 values with its canonical manifest. Missing, changed
   and extra content all fail. This is actual guest content verification, not
   marker-only evidence.
4. Resolves the image's final guest identity before readiness and verifies
   binding traversal and file access under that identity. Root-only access does
   not satisfy this check.
5. For `read-only`, attempts to create one unpredictable probe file as that
   identity and requires denial. This supplements mountinfo; it does not replace
   it. For `read-write`, it creates, fsyncs and removes the probe successfully.
6. Generates a cryptographically random 256-bit nonce, atomically writes one
   `ready.json` containing the nonce and every verified binding, and emits
   `AWMAN_STARTUP_GATE_READY <nonce>`.
7. Waits for a regular `release.json`, checking cancellation and the monotonic
   timeout. It rejects symlinks, unknown fields, wrong versions and a nonce
   that does not exactly match `ready.json`.
8. Removes `release.json`, emits `AWMAN_STARTUP_GATE_RELEASED <nonce>`, restores
   the original agent environment including any original Python variables, and
   `exec`s the original argv unchanged.

The fixed bootstrap reads files only to verify the caller-supplied manifests;
it does not execute repository files,
source shell configuration, install dependencies, contact the network or read
outside the declared workspace and control paths. Manifest identity is a
full guest-tree proof for the prepared copy at gate time. Later writable
bindings may intentionally diverge and must be versioned by the orchestrator.

The embedded source lives at
`src/engine/container/startup_gate_bootstrap.py` and is included verbatim in
the binary. It exposes these importable internal helpers for hermetic tests:

```python
parse_manifest(raw_bytes, expected_digest)
validate_mount(binding, mountinfo_text)
verify_tree(root_path, manifest)
probe_access(binding)
resolve_runtime_identity(run_as)
verify_binding_as(binding, manifest, runtime_identity)
await_release(control_dir, ready, timeout,
              cancel_check=lambda: False,
              clock=time.monotonic, sleep=time.sleep)
run_gate(control_dir, original_argv, original_env, mountinfo_text,
         cancel_check=lambda: False, clock=time.monotonic,
         sleep=time.sleep, exec_fn=os.execvpe, runtime_identity=None)
```

These parameters are ordinary Python call arguments used by unit tests, not
environment variables or command-line switches. The production `__main__`
path supplies the fixed operational paths, reads real `/proc/self/mountinfo`,
uses the real filesystem and monotonic clock, and retains `os.execvpe` as the
execution function. It exposes no test-mode environment or CLI bypass.
`run_gate` retains a default `runtime_identity=None` only so its existing
importable unit tests can exercise their in-process tree and access helpers.
The production `main` path always resolves and supplies an immutable runtime
identity before entering the gate.
`RuntimeIdentity` is an immutable tuple record with fields `uid`, `gid`,
`groups`, and `drop`; `groups` is an immutable tuple. `verify_binding_as` owns
both pipe descriptors and, after a successful fork, the exact child PID until
it has been reaped. Pipe, fork, wait, and read `OSError` failures become
`GateError("identity-probe")` only after owned descriptors are closed and any
live child is killed and reaped. A `BaseException` such as `KeyboardInterrupt`
performs the same cleanup and then propagates the original interruption.
Internal protocol failures raise `GateError` with a stable string `code`;
an explicitly supplied cooperative cancellation callback while awaiting
release uses exactly `code == "cancelled"` and writes that same code to
`failure.json`. The production entrypoint does not currently supply such a
callback.

Any validation, probe, timeout or I/O failure atomically writes `failure.json`,
emits `AWMAN_STARTUP_GATE_FAILED <code>`, and exits nonzero without executing
the original argv. External host cancellation uses the existing container stop
and reap path and may leave no new failure record; cleanup removes only
awman-owned bootstrap staging and never the caller's control directory or
workspace.

## Credential refresh ordering

A gated launch must perform no host-side agent/model ping before release. Its
credential lease is marked `gate_pending` before registration with the global
refresh monitor; monitor ticks exclude such leases. Proactive refresh and the
synchronous pre-step refresh guard are skipped and reported through the
existing diagnostic channel. Credential staging that does not execute an agent
may proceed. Successful release atomically clears `gate_pending`; cancellation,
failure and timeout unregister the lease. Ordinary post-release auth-failure
behavior remains available. This work item does not add a fifth authorized
host-agent ping trigger.

## Command and frontend integration

Layer 2 parses and validates the gate before image build or container launch,
adds `ContainerOption::StartupGate`, and exposes gate readiness/failure as
structured command status in addition to the stable stderr markers. Layer 3
only parses/renders the catalogue fields.

When absent, no bootstrap file, operational overlay, argv wrapper, marker,
status or refresh-order change exists. Golden argv tests must prove the legacy
path is identical.

## Edge Case Considerations

- A release present before ready is stale input and is rejected host-side;
  empty releases and releases with the wrong nonce never start the agent.
- Cancellation while waiting never starts the agent.
- Timeout uses a monotonic clock and reports `timeout`, not a generic agent
  failure.
- Spaces, quotes, newlines and leading dashes in original agent argv remain
  distinct arguments.
- Workspace or manifest symlinks, hard-linked control files or workspace
  files, invalid UTF-8 JSON, unknown JSON fields, oversized request/manifest
  files and mode/owner mismatch
  fail closed.
- A read-only mount that is actually writable fails. A requested read-write
  mount that is not writable fails.
- Docker and Apple Container paths share the wrapper. Unknown container-class
  runtimes reject until they demonstrate equivalent overlay and argv behavior.
- Multiple gated containers use distinct control directories. The caller may
  wait until all are ready and release them as a group.

## Test Considerations

1. Layer 0 unit tests cover binding arrays, distinct ids, the exact manifest
   wire format/raw-byte digest, size, owner/mode, reserved-root overlap, stale
   files, path overlap, symlinks and timeout bounds.
2. Option-resolution tests cover duplicate gates and unchanged absent-gate
   behavior.
3. Golden `build_run_argv` tests prove the wrapper position and byte-for-byte
   preservation of hostile-but-valid original arguments.
4. Bootstrap integration tests use temporary read-only/read-write mounts and
   assert mountinfo selection, nested-mount rejection, complete hashes, extra
   files, links, isolated Python startup, ready nonce, matching/wrong/stale
   release, exec, timeout, cancellation and failure.
5. Docker and Apple fake-CLI tests receive the same gate overlays and wrapper.
6. CLI catalogue/parity tests cover both flags and every explicitly unsupported
   command/runtime.
7. Credential-refresh fakes prove synchronous guards and background monitor
   ticks cannot ping a `gate_pending` lease before release.
8. An end-to-end fixture starts two gated fake agents, proves neither agent
   entrypoint runs when only one gate is ready, releases both, then proves each
   original argv runs once.

No test uses a real model, network, operator repository or credential.

## Codebase Integration

- Layer 0: `src/data/startup_gate.rs` and exports.
- Layer 1: `src/engine/container/startup_gate.rs`, `options.rs`,
  `docker::build_run_argv`, and shared `ContainerInstance::run_with_frontend`.
- Layer 2: command flag resolution and refresh ordering.
- Layer 3: catalogue-driven CLI rendering only.
- Architecture remains Layer 0 → Layer 1 → Layer 2 → Layer 3.

## Documentation

After implementation, update `aspec/uxui/cli.md`, `docs/03-agent-sessions.md`,
`docs/04-security-and-isolation.md` and `docs/08-overlays.md`. Describe the
feature as an orchestrator startup gate, not as general repository attestation.

## Approved ownership boundary across host and guest namespaces

Numeric ownership is namespace-specific. On the verified Apple Container path,
the host sees a control directory and its records as UID:GID `501:0`, while the
guest sees the same mounted objects as `0:0`. Passing a host UID/GID through a
bootstrap argument therefore rejects a legitimate mount and cannot serve as a
trustworthy guest ownership assertion.

Host and guest validate ownership independently:

- Host validation remains unchanged. The caller control directory must be a
  current-host-user-owned, nonsymlink directory with mode `0700`.
  Host-created and host-read protocol records remain regular, single-link,
  bounded, mode-`0600` files owned by that host identity.
- The fixed guest bootstrap receives no host numeric UID/GID argument.
- At bootstrap start, the guest opens the mounted control directory once with
  directory and no-follow semantics. It validates the held object is a
  directory with mode `0700`, records its guest-visible UID and GID, and keeps
  the descriptor open through release, failure, or final exec.
- Guest control records are accessed by fixed basenames relative to that held
  descriptor. `ready.json`, `release.json`, `.released`, and `failure.json`
  must be regular, single-link, bounded files with mode `0600` and the same
  guest-visible UID and GID as the held control directory.
- Guest reads use no-follow and nonblocking flags, compare pre-open metadata
  with `fstat`, and reject replacement, symlink, hardlink, FIFO, device,
  socket, wrong owner, wrong mode, empty or oversized records as required by
  the existing protocol.
- Guest atomic writes create unpredictable exclusive temporary basenames
  relative to the held descriptor, fsync the file, replace within the same
  held directory using descriptor-relative rename, and fsync the directory.
  Cleanup and release removal are descriptor-relative.
- A pathname replacement after the control descriptor is acquired cannot
  redirect any protocol read, write, rename, unlink, or sync.
- The approved read-only bootstrap/request/manifest mount keeps its separate
  guest-visible directory-owner validation. Its ownership is not inferred from
  the writable control mount.

The host lease verifier continues to validate host ownership, mode, link
count, bounded no-follow reads, request/ready/receipt schemas, container
identity, binding identity, and nonce. Guest-derived ownership does not weaken
or replace those checks.

The request, manifest, ready, release, receipt, and failure schemas do not
change. The ready nonce remains a cryptographically random 256-bit lowercase
hexadecimal value. Release must contain the exact ready nonce. Binding
identities and authoritative container identity remain exact. A deprecated
pure-test ownership argument may remain temporarily for source compatibility,
but production ignores it as authority and derives guest ownership from the
held control descriptor on every path.

Regression coverage must prove the staged production argv contains no host
numeric owner; a correct guest namespace writes and consumes owner-matched
records; wrong guest UID or GID, mode, links, FIFO, oversize and inode
replacement fail closed; replacing the control pathname cannot redirect
ready, release, receipt or failure operations; host-side exact verification is
unchanged; and the configured unprivileged identity transition occurs only
after the matching release.

## Approved final image-user identity semantics

The startup gate preserves the selected image's Linux `USER` contract when it
returns from the privileged bootstrap to the original agent. User and group
components may each be a decimal ID or a name; a digit-leading token that is
not wholly decimal remains a name and is resolved through the guest account
database.

When the image user omits a group and resolves through the guest password
database, the bootstrap applies that account's primary and supplementary group
memberships before setting its final UID. When the image user supplies either
a numeric or named group, the bootstrap resolves that group independently,
clears supplementary groups, then sets exactly the requested GID and UID. It
verifies the effective GID and UID before executing the original argv. Unknown
users or groups fail before any supplementary-group, GID, UID, or agent-exec
effect.

This follows the Docker [`USER` instruction reference](https://docs.docker.com/reference/dockerfile/#user),
which specifies that an explicit group is the user's only group membership,
and the OCI Image Configuration [`config.User` contract](https://github.com/opencontainers/image-spec/blob/main/config.md#properties),
which says an explicit group ignores supplementary groups while an omitted
group uses the account's default and supplementary memberships. A wholly
numeric UID without a guest password entry remains supported when an explicit
GID is present. The default GID for a numeric UID with no explicit group is not
expanded by this clarification and still requires backend-parity evidence.

User, group, and supplementary memberships are resolved exactly once before
binding verification and `ready.json`. The immutable numeric UID, primary GID,
and supplementary-group tuple is used both by the target-identity binding
verifier and by final exec after release. A passwd-backed user with no explicit
group uses `getgrouplist` during that resolution and `setgroups` with the
stored tuple after release; it does not call `initgroups` or repeat name lookup
after readiness. This changes the previously frozen mechanism assertion from a
post-release `initgroups` call while preserving the previously approved final
Docker/OCI membership semantics.
