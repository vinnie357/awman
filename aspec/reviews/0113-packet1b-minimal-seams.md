# AWMan native Packet 1B minimal seam disposition candidate

Status: source-only candidate for Root review. This document amends only the
crate-private observation, provider-process, and immediate pre-spawn seams
needed to test cases 10–12 and 16 of the committed native P2 matrix. It does
not authorize production or worktree changes.

Authority:

- concrete gated-launch P1 SHA-256
  `eb00e22c1399637294f904d6dd630399e4fb3a2a44e59ba60ea7e8740f713266`;
- identity P1 SHA-256
  `ead26429abf89f0dab24cbe40421bbd93929105b5cb74e72bf586ffaea6f132e`;
- split-control P1 SHA-256
  `76eacc8614a4e1a18ce782b3c5f3cb37ba8b997452ad717ef8d88dd2e26b4b4b`;
- committed Packet 1A/P2 lineage HEAD
  `69ecd05790a0f93a3b17a44222d4d24f42316696`.

## Docker parsers

The Docker adapter uses these private parsers:

```rust
pub(crate) fn parse_gated_docker_image_inspection(
    bytes: &[u8],
) -> Result<ImmutableImageId, LaunchIdentityError>;

pub(crate) fn parse_gated_docker_launch_inspection(
    bytes: &[u8],
    key: &ProviderLaunchKey,
) -> ExactInspection;
```

The image parser returns only `ImageIdentityMismatch` or
`ProviderInspectionUnavailable` from the fixed P1 error vocabulary. Neither
parser preserves raw JSON in an error. They implement the P1 `.Id` versus
`.Image` normalization and never compare an image tag/reference as immutable
identity.

## Canonical observation revision

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InspectionObservationKind {
    Absent,
    Matching,
    PresentUnusable,
}

pub(crate) struct CanonicalInspectionRevisionInput<'a> {
    pub kind: InspectionObservationKind,
    pub provider: ProviderKind,
    pub exact_name: &'a ContainerName,
    pub runtime_id: Option<&'a str>,
    pub token_digest: Option<[u8; 32]>,
    pub immutable_image_id: Option<&'a ImmutableImageId>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub state: SanitizedProviderStateObservation,
}

pub(crate) fn canonical_inspection_revision(
    input: CanonicalInspectionRevisionInput<'_>,
) -> InspectionRevision;

impl InspectionRevision {
    pub(crate) fn as_bytes(&self) -> &[u8; 32];
}
```

The function hashes exactly one compact UTF-8 JSON object with no trailing
newline and this field order:

```json
{"version":1,"provider":"docker","exactName":"awman-example","runtimeId":"container-id","tokenDigest":"<hex64>","imageId":"sha256:<hex64>","createdAt":"2026-10-04T00:20:56Z","state":{"kind":"known","value":"running"},"observationKind":"matching"}
```

`provider` is `docker` or `apple-containers`. Optional normalized tuple fields
are JSON `null` when absent. `createdAt` uses
`chrono::SecondsFormat::AutoSi` with UTC `Z` and preserves every parsed
fractional-second digit through nanosecond precision. Only the
`created_not_before` lower bound is floored to a whole second; a Docker
`.Created` value is not truncated. Apple's evidenced whole-second value
naturally serializes with `Z` and no fraction. State encodings are exactly
`{"kind":"absent"}`, `{"kind":"known","value":"<P1 enum token>"}`, or
`{"kind":"unrecognized-digest","sha256":"<hex64>"}`. Observation-kind
tokens are `absent`, `matching`, and `present-unusable`.

Only normalized fields are arguments. Raw provider JSON, unknown fields,
diagnostics, and unrecognized state bytes cannot enter except through the
already computed state SHA-256. Both provider parsers and every absence or
present-unusable adapter result use this canonicalizer. `as_bytes` exposes
only the immutable digest value already needed by receipt serialization; the
constructor remains private.

## Bounded provider CLI

```rust
pub(crate) const MAX_PROVIDER_STDOUT: usize = 256 * 1024;
pub(crate) const MAX_PROVIDER_STDERR: usize = 64 * 1024;

pub(crate) struct BoundedProviderOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderCliStartFailure {
    DeadlineExpired,
    ResourceUnavailable,
    ExecutableUnavailable,
    SpawnFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderCliReapedFailureKind {
    DeadlineExceeded,
    StdoutLimitExceeded,
    StderrLimitExceeded,
    ReadFailed,
}

pub(crate) struct ProviderCliReapedFailure {
    pub kind: ProviderCliReapedFailureKind,
    pub status: std::process::ExitStatus,
}

#[must_use = "an unreaped provider CLI child remains owned recovery state"]
pub(crate) struct RetainedProviderCli {
    // References the prestarted custody actor that owns the exact invocation.
    // During native spawn it owns Command with no invented Child; after a
    // successful spawn it owns the exact Child and bounded drain states.
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct ProviderCliCustodyTicket(uuid::Uuid);

#[must_use]
pub(crate) enum RetainedProviderCliTermination {
    NotStarted(ProviderCliStartFailure),
    Reaped(ProviderCliReapedFailure),
    Retained(RetainedProviderCli),
}

impl RetainedProviderCli {
    pub(crate) fn retry_terminate(
        self,
        deadline: ProviderCallDeadline,
    ) -> RetainedProviderCliTermination;

    pub(crate) fn transfer(self) -> ProviderCliCustodyTicket;
}

pub(crate) struct ProviderCliCustodyRegistry {
    // Application-lifetime custody for bounded helper processes only.
}

pub(crate) const MAX_PROVIDER_CUSTODY_SHUTDOWN: std::time::Duration =
    std::time::Duration::from_secs(2);

impl ProviderCliCustodyRegistry {
    pub(crate) fn try_new() -> Result<std::sync::Arc<Self>, ProviderCliStartFailure>;
}

#[must_use]
pub(crate) enum ProviderCliRunOutcome {
    Completed(BoundedProviderOutput),
    NotStarted(ProviderCliStartFailure),
    ReapedFailure(ProviderCliReapedFailure),
    RetainedFailure(RetainedProviderCli),
}

pub(crate) fn run_bounded_provider_cli(
    command: std::process::Command,
    deadline: ProviderCallDeadline,
    custody: &std::sync::Arc<ProviderCliCustodyRegistry>,
) -> ProviderCliRunOutcome;
```

`BoundedProviderOutput` has a custom redacted `Debug` that prints only status
and byte counts. The two vectors never exceed their constants. An already
expired deadline returns `NotStarted(DeadlineExpired)` before `spawn`.

The runner forces stdin null and stdout/stderr piped. Before calling `spawn`,
it allocates both bounded buffers, reserves a custody-registry ticket/capacity,
and starts the custody actor that owns the exact `Command` and single native
spawn attempt. Until the OS returns successfully, no `Child`, PID, or exit
status exists and none may be invented.
Buffer allocation, ticket reservation, or custody-thread failure at that point
is `NotStarted(ResourceUnavailable)`. `SpawnFailed` is reserved for an actual
OS spawn failure where no child exists. After `spawn`, startup or drain setup
failure must actually kill and wait or return `RetainedFailure` through the
already prepared custody. It can never report `NotStarted` after a child
exists. `ProviderCliCustodyRegistry::try_new` is fallible and runs before any
provider helper is spawned. If the caller's absolute deadline expires while the
actor is still inside the one native spawn attempt, the returned retained handle
owns that same in-flight invocation and no retry may spawn again. A later
`transfer()` is allocation-free and infallible because its ticket and capacity
were reserved in the handle's originating registry before the spawn attempt.

After spawn, stdout and stderr drain concurrently. Reaching either cap plus
one byte, reaching the absolute deadline, or a pipe read failure initiates
kill and actual wait. A verified wait returns `ReapedFailure` with fixed class
and status, and no raw output or PID. Any path that cannot verify wait returns
`RetainedFailure` with the exact child and drain custody. It cannot return an
ordinary error or drop the child.

The runner does not call `Command::output`. It does not block joining a drain
thread after the provider child has been reaped, because a descendant may
still hold an inherited pipe descriptor. Drain buffers remain bounded and
redacted. Provider adapters map `NotStarted` and verified reaped failures to
the fixed P1 error/outcome for their operation; they transfer
`RetainedFailure` into recovery ownership.

`Completed` requires both bounded drains to reach EOF and an actual child wait
before the absolute deadline. If the provider child has exited but a descendant
still holds either inherited pipe open, the runner never reports early
completion and never joins indefinitely. At the bounded deadline it closes or
detaches the bounded drain custody and returns
`ReapedFailure(DeadlineExceeded)` because the provider child itself was
actually reaped; no raw partial output enters that failure.

`retry_terminate` consumes the handle and returns actual `NotStarted` evidence
when its retained in-flight spawn later finishes with no child, verified reaped
evidence when a real child is waited, or the same custody in a new retained
value. `transfer()` consumes the handle into its already reserved originating
application-lifetime `ProviderCliCustodyRegistry` and returns only a non-secret
ticket. It accepts no destination registry, cannot silently reroute custody,
and never retries the spawn attempt. Last-owner registry/retained-handle
shutdown is bounded; if
the deadline expires, the custody worker detaches while still owning the child
until actual reap or process termination. This helper custody never calls a
provider inspect, stop, or remove operation and never creates provider-launch,
cleanup, absence, or trusted-stop authority. It is distinct from durable-plan
launch retention because a prelaunch image inspection has no launch plan.
The registry and retained-handle last-owner shutdown deadline is the earlier of
the enclosing deadline and `MAX_PROVIDER_CUSTODY_SHUTDOWN` (two seconds). A
deadline expiry detaches the custody worker without surrendering its child or
bounded drain ownership.

## Immediate pre-spawn barrier

```rust
#[must_use = "the validated barrier must be consumed by the matching spawn"]
pub(crate) struct ValidatedSpawnBarrier {
    // Private, non-Clone fields bind the plan file, held controls, full key,
    // and exact fresh absence observation.
}

pub(crate) fn validate_immediate_pre_spawn(
    plan: &DurableLaunchPlan,
    adapter: &dyn GatedProviderAdapter,
    enclosing: std::time::Instant,
) -> Result<ValidatedSpawnBarrier, LaunchIdentityError>;
```

The function revalidates the held/current parent and literal guest-control
child, then calls `inspect_name_absence` with a newly computed
`provider_call_deadline(enclosing)`. Only an exact `Absent` observation whose
provider and exact name match the plan returns the one-shot barrier. Present,
ambiguous, unavailable, pin substitution, or an expired deadline returns a
fixed pre-spawn error and no barrier.

The barrier exposes no fields, path, token, boolean, constructor, `Clone`, or
`Copy`. It captures the same plan-file pin, control authorities, and launch key
as the borrowed plan. The common real-spawn entry consumes the barrier and
the same owned `DurableLaunchPlan`; PTY, one-shot piped, and persistent piped
callers cannot invoke their actual provider spawn without that consumption.
The exact three call-site placements remain mandatory production source-audit
items; the private token does not replace that audit.

## Boundary

These additions are crate-private and production-used. They grant no public
constructor, filesystem authority, provider cleanup authority, raw-token
access, or Docker orchestrated support. Docker orchestrated launch continues
to reject before agent setup and image build; its parser tests establish only
the common lower-level identity contract.
