# Gated launch native P2 Packet 1B

Status: source-only independent test packet for Root review. No file has been
applied to the AWMan worktree, and no formatter, compiler, test, CI, runtime,
provider, or log workload has run.

## Authority and source lineage

- concrete gated-launch P1:
  `/private/tmp/awman-gated-launch-concrete-addendum.candidate.md`, SHA-256
  `eb00e22c1399637294f904d6dd630399e4fb3a2a44e59ba60ea7e8740f713266`;
- identity P1 SHA-256
  `ead26429abf89f0dab24cbe40421bbd93929105b5cb74e72bf586ffaea6f132e`;
- split-control P1 SHA-256
  `76eacc8614a4e1a18ce782b3c5f3cb37ba8b997452ad717ef8d88dd2e26b4b4b`;
- committed P2 matrix SHA-256
  `76d2b295463e6c94fb2d18082620665945a452cf54b93c7ea5df9f64f577f8b0`;
- packet base and frozen Packet 1A commit
  `69ecd05790a0f93a3b17a44222d4d24f42316696`.

The exact additive private seams are recorded separately in
`aspec/reviews/0113-packet1b-minimal-seams.md`. Docker identity parsing remains
a lower-level common contract. This packet does not enable or claim Docker
orchestrated launch; P1 still rejects that profile before agent setup or image
build.

## Cases 10–12 and 16

The new engine-layer module contains thirteen behavioral tests plus one ignored
subprocess fixture.

### Docker immutable identity — case 10

1. A Docker image-inspect `.Id` using uppercase `SHA256:` normalizes to the
   same lowercase immutable identity as container-inspect `.Image`. The
   matching inspection also requires exact name, token digest, creation lower
   bound, runtime ID, and recognized state.
2. A matching mutable image reference in `.Config.Image` cannot mask a
   different container `.Image`; the result is `ForeignOrAmbiguous`.

The assertions call the production-used Docker parsers. They do not exercise
the unsupported orchestrated Docker launch profile.

### Canonical sanitized revisions — case 11

3. Two Docker documents with the same normalized tuple but different image
   references, unknown fields, and raw JSON shapes produce one revision. A
   changed normalized runtime ID changes it.
4. The valid matching tuple includes a fractional Docker `.Created` value and
   pins the exact canonical SHA-256 bytes. This ensures canonical AutoSi UTC
   serialization preserves parsed fractional precision rather than applying
   the whole-second launch lower-bound floor.
5. An unrecognized state enters the canonicalizer only as its SHA-256 digest:
   equal state bytes produce equal revisions and different bytes produce a
   different revision. The revision API has no raw-JSON argument.

### Bounded provider subprocess — case 12

6. An expired absolute deadline returns `NotStarted(DeadlineExpired)` and the
   external fixture creates no PID marker.
7. Stdout beyond 256 KiB and stderr beyond 64 KiB independently cause the
   fixed overflow class, kill, actual wait, and an OS-level absent PID after
   return. Raw output is unavailable from the failure.
8. A small successful subprocess returns only after both bounded drains reach
   EOF and the child is actually waited; returned vectors stay within both
   fixed caps.
9. A timed-out live provider child is killed and reaped even when its
   descendant keeps inherited stdout/stderr open. An already exited and reaped
   provider parent with the same inherited-pipe condition returns bounded
   `DeadlineExceeded` rather than early `Completed` or an indefinite join.

The subprocess fixture is the current unit-test executable invoked by exact
ignored test name. Unix liveness uses `kill(pid, 0)` only after the production
runner returns. It invokes no Docker or Apple provider. The test covers normal
kill/wait and output-limit behavior; forced OS kill/wait failure and detached
custody are source-audit and later lifecycle-packet concerns, with no invented
fault selector.

### Immediate pre-spawn barrier — case 16

10. A real private parent/guest-control/request/manifest/intention fixture is
    loaded through the production loader. The first exact absence publishes a
    real durable plan; a second exact absence returns the private one-shot
    barrier, for exactly two absence calls.
11. Present, ambiguous, unavailable, and provider/name-mismatched absence
    observations return the fixed pre-spawn error and no barrier. An expired
    enclosing deadline performs no second inspection. Replacing either the
    pinned parent or guest-control current name fails `UnsafeControl` before a
    second provider inspection.

The test validates the production barrier and real held-control revalidation.
The token has no public fields or constructor and is not used as cleanup or
filesystem authority. Placement and consumption at all three actual spawn
calls remains a mandatory implementation source-audit item; this packet does
not claim that source placement before implementation exists.

## Existing Packet 1A freeze

Packet 1B adds one new test module and one `#[cfg(test)] mod` declaration. It
does not read, edit, or replace the four frozen Packet 1A test files. Their
current byte pins are:

- data loader tests:
  `7911c42a939dba206c9612accd88b25f2ef9881419a88d707cc86484d8524347`;
- engine stager tests:
  `cf55b156125014e4346311db37a8cc02ecb100123dd8a5df6f9307310bb3d130`;
- Apple parser tests:
  `94f26234c984a9869a8f63bab8ff204b0fc7c06d42f5fe024708ea80ba31c094`;
- plan tests:
  `081c7825813332490120a9880d6ac728d9edcc0e650699442efc631abc6b1d48`.

The build-wiring patch changes only `src/engine/container/mod.rs`; it preserves
all existing module declarations and production source.

## Truthful limits

Passing Packet 1B alone would not prove native launch readiness. It does not
cover the three child representations, actor binding, convergence, READY and
receipt, release credential gating, cleanup, or last-owner launch retention.
It does not force exceptional OS kill/reap failures. Those remain in later
P2 packets, implementation source audit, and independent runtime evidence.

## Fixture lineage amendment

Root's post-review source check located that the first candidate reused the
Docker-only canonical fixture name `awman-p2b-exact` in `launch-intent.json`,
while identity P1 requires orchestrated control names to begin
`awman-altana-`. The control fixture now uses the separate valid
`awman-altana-p2b-exact`. Docker parser and canonical-revision fixtures retain
the original name, so their expected canonical digest and assertions are
unchanged. This is a fixture-only correction with no production seam or
behavioral expectation change.
