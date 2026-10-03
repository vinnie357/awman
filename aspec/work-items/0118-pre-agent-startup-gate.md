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
have size zero and null digest. Unknown fields are rejected. `manifest_id` is
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
4. For `read-only`, attempts to create one unpredictable probe file and
   requires denial. This supplements mountinfo; it does not replace it.
5. For `read-write`, creates, fsyncs and removes the probe successfully.
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
await_release(control_dir, ready, timeout,
              cancel_check=lambda: False,
              clock=time.monotonic, sleep=time.sleep)
run_gate(control_dir, original_argv, original_env, mountinfo_text,
         cancel_check=lambda: False, clock=time.monotonic,
         sleep=time.sleep, exec_fn=os.execvpe)
```

These parameters are ordinary Python call arguments used by unit tests, not
environment variables or command-line switches. The production `__main__`
path supplies the fixed operational paths, reads real `/proc/self/mountinfo`,
uses the real filesystem and monotonic clock, and retains `os.execvpe` as the
execution function. It exposes no test-mode environment or CLI bypass.
Internal protocol failures raise `GateError` with a stable string `code`;
cancellation while awaiting release uses exactly `code == "cancelled"` and
writes that same code to `failure.json`.

Any validation, probe, timeout or I/O failure atomically writes `failure.json`,
emits `AWMAN_STARTUP_GATE_FAILED <code>`, and exits nonzero without executing
the original argv. If the process is canceled, the existing container stop and
reap path remains authoritative; cleanup removes only awman-owned bootstrap
staging and never the caller's control directory or workspace.

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
