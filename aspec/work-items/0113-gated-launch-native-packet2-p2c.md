# Work Item 0113 Packet 2C: native create-child custody and retention

Status: independent source-only adversarial test candidate. This packet does
not enable native launch, READY, receipt/release, provider cleanup, credentials,
Docker orchestration, ACP orchestration, or Apple provider acceptance.

## Authority and checkpoint

- Packet 2 concrete seam SHA-256
  `ef11f6698cdcadc21a63ea9d04b0394fcf0d3e61ff1782a27ec0d7fa29f8dec4`.
- Root source disposition SHA-256
  `a4fcc053f4c3d2417c3bee85b8ed75fb58e103f31bf9c60720a0154160c71f1b`.
- Concrete addendum SHA-256
  `eb00e22c1399637294f904d6dd630399e4fb3a2a44e59ba60ea7e8740f713266`.
- Identity packet SHA-256
  `ead26429abf89f0dab24cbe40421bbd93929105b5cb74e72bf586ffaea6f132e`.
- Committed 35-case matrix SHA-256
  `76d2b295463e6c94fb2d18082620665945a452cf54b93c7ea5df9f64f577f8b0`.
- Candidate base HEAD
  `5688b65cfbc90b2075877df8da77f16633dc912d`, tree
  `75be5ca6b04f303f9ef39debeec82e9630f4dd5a`, branch
  `feat/workspace-preflight`.
- Layer-placement full-gate evidence SHA-256
  `620f01bd32a714b24fd8675aa87f8131dcb2f6798ad51c4dcaec5e0ec7280805`.
- Isolated Rust 1.94 formatter evidence SHA-256
  `c77128a0232e57cd937780be8b8f476d696901242e902a7f419fc96c945270b8`.
- Root-reviewed formatter patch SHA-256
  `5dde4b36d413919c348448dd0ad6d1f596c2f7d0abb5dbf280ad02ec84b2735c`.
- Post-format Rust 1.94 full-gate evidence SHA-256
  `dccc75315eb180059efbfbb716910654d81e2802bd1c16a7d1a36c55a7f3d328`.
- Explicit-contract full-gate evidence SHA-256
  `28e9fea688a5e60fa6c30d78881567eead00bc1364f0b1138b52ed8d48b5caad`.
- Explicit-contract isolated Rust 1.94 formatter evidence SHA-256
  `d7204ef217584eed21cbb1c31f6a09f4e4e7bc2a76917e2332a8a8946603ad5f`.
- Root-reviewed explicit-contract formatter patch SHA-256
  `cc4d6846d1c0c3d1f7a2866b5d09dee8b6ab678982175a44995fa7edd63fce74`.

The existing Packet 1A and Packet 1B tests remain byte-exact. Packet 2C adds
fourteen Layer 1 tests in `engine::container`, one Layer 2 registry-owner test
in `command::dispatch`, and one test-only module declaration in each layer.
The split preserves all fifteen assertions while respecting the architecture
rule that Layer 1 cannot import Layer 2.

## Genuine local fixture contract

Every positive spawn uses the exact `process::test_support::spawn_fixture`
harness. The executable is the static absolute `/bin/sh`; the first argument
is a unique mode-0700 script in a fresh temporary directory. That script writes
one start marker, optionally writes a unique stdout value, and waits for a
unique mode-0600 release marker before returning its requested real exit code.
Its wait also has a finite approximately ten-second fallback, so even a failed
test whose release write cannot succeed cannot leave an immortal fixture.
Drop checks the release write and reports any failure. A fixed nonexistent
absolute executable exercises the OS no-child spawn error.

The harness receives a durable plan created through `load_startup_gate` and
`prepare_orchestrated_launch` over real private host-parent and guest-child
directories. Its adapter is an in-process bounded observation script. It makes
no model, network, Docker, Apple Containers, or provider subprocess request.
The adapter cannot create a child, PID, wait result, READY value, or successful
native outcome. Native progress and exit come only from the trusted local OS
child and lifecycle actor.

## Frozen observable assertions

| Matrix case | Candidate assertion |
| --- | --- |
| 17 | PTY, one-shot piped, and persistent-piped each enter their real production spawn function with a fresh barrier and the same owned durable plan; after real bridged output synchronizes script execution, each unique script has started exactly once, bound a lifecycle slot, and publishes one actual exit. |
| 18 | Injected actor-thread startup failure is `BeforeCliStart`, obtains no slot authority, performs no second absence, and starts no fixture. Both disconnected and full bind channels return the exact PTY/piped/persistent raw-child kind and original bounded timestamp, with no bound slot. Each raw owner is synchronously inserted in retention before release and reaches actual `unreaped == 0`. Its post-reap marker count is at most one solely as a no-retry guard; user-code execution is not required to prove native spawn/custody. The after-bind bridge fault returns `Managed { execution: None, lifecycle }` for all three representations and the slot exposes that same lifecycle state. Its actor-published actual exit is preserved exactly, with one native reap and no guessed successful code or PTY signal. |
| 19 safe subset | A real zero exit followed by absence and a real nonzero exit followed by a matching tuple both return `Retained(SpawnResultUnknown)`, start once, and perform no destructive provider call. A live matching tuple returns only `StartedCreateObservation::Matching`. A genuinely missing executable is the sole fixture path with exact `BeforeCliStart(ContainerRuntimeUnavailable { binary })`, where `binary` equals the requested missing fixture executable; it has no child authority and no start marker. No diagnostic input or positive collision assertion exists in this packet. |
| 29 | Normal `AgentExecution::wait`, lifecycle legacy wait, bounded wait, pre-extracted cancel handle, direct cancel, and grace expiry agree on the actor's byte-identical actual exit. The lifecycle probe reports exactly one native reap. Provider stop/remove is never used as local-child cancellation. |
| 30 | Retention tickets are observable immediately after `retain` returns. `ContainerRuntime` owns the application registry; the `Arc<ContainerRuntime>` stored by `Engines` and cloned `Engines` values reach pointer-identical application registry Arcs through crate-private `ContainerRuntime::launch_retention()`, without a duplicate registry field on `Engines`. An old backend implementing only required `build` remains source-compatible: the new default rejects orchestrated input before calling it even when given a registry Arc, while legacy input still delegates once. |
| 31 | Injected registry thread failure returns `SupervisorThreadUnavailable` before any fixture can start. Poison recovery preserves the pre-poison ticket and accepts a second owner while marking supervisor failure. Registry insertion and lifecycle state query remain bounded while the actor is paused immediately before a native poll. |
| 32 | Last-owner Drop returns within the two-second shutdown grace plus test scheduling allowance for both managed and raw-unbound custody and destroys the wrapper Arc. A paused managed actor reports detached unreaped custody; raw-unbound custody may already be actually reaped by its permitted shutdown kill. After either local child is actually reaped, the snapshot distinguishes `unreaped == 0` from the still-unresolved gated entry: `retained == 1` and `worker_finished == false`. Local reap clears execution custody only and never discards the durable plan, inspection, or reason. Provider call history is unchanged. The worker-owned empty-registry drop helper exercises the real no-self-join path. |

The case-30 runtime/`Engines` test retains all three pointer-identity assertions in
`src/command/dispatch/gated_launch_retention_p2c_test.rs`; registry access now
uses each bundle's canonical `container_runtime`. The other fourteen tests remain
in `src/engine/container/gated_launch_p2c_test.rs`. A Rust 1.94
full-gate attempt against the original single-file placement stopped at the
architecture lint before formatting or compilation; evidence SHA-256
`756d7641ed4b006aa3a8a3a9100e1eda0d08cdf0424234620979e6d36d1e9fd7`.
After the Layer 2 relocation, a revision-matched full gate passed architecture
lint and stopped at `cargo fmt --check`. That formatter failure was an
unexpected candidate gate failure; compilation and the expected missing Packet
2 production API red were not reached. Luna then formatted isolated copies
with Rust 1.94. Root reviewed the exact patch and approved only its import
ordering, line wrapping, and lexical placement of the dispatch test module.
This candidate adopts the approved Layer 1 postimage
`2b400227f1251dac0fcb46fbd78955d2419ccfd2b1fdc8dbaebd67c21792a49b`
and the dispatch module postimage is expected to be
`66f6f4522bc2d50cfe60cd777c23580ac49baa57eecd275d5fec241186a4de86`
after applying the registration patch to the clean preimage. No assertion or
fixture behavior changed.

The fresh post-format gate passed architecture lint and `cargo fmt --check`,
then compilation reported eleven missing Packet 2 production API diagnostics
and `E0282` at the bounded waiter expression. The accepted seam fixes
`wait_actual_until` as
`Result<Option<AgentExitInfo>, EngineError>`. Because the authority import and
its method were unresolved in that same compilation, the closure had no
resolved method return type from which to infer the `spawn_blocking` output;
the `E0282` is therefore a missing-contract recovery cascade rather than a
conflicting candidate type. The candidate nevertheless makes the exact seam
type explicit on the closure, removing that inference dependency without
changing the awaited value or any assertion. This source conclusion still
requires a fresh Luna gate; it does not reclassify the unexecuted revised
candidate as an all-expected red.

The next full gate passed architecture lint but stopped at an unexpected
`cargo fmt --check` diff confined to that explicit closure type. Compilation,
the missing-production diagnostics, and tests were not reached in that run;
evidence SHA-256
`28e9fea688a5e60fa6c30d78881567eead00bc1364f0b1138b52ed8d48b5caad`.
Luna then ran Rust 1.94 rustfmt on an isolated copy. Root reviewed the exact
800-byte patch and approved its sole closure-layout change. This candidate
adopts the isolated 41,413-byte postimage
`433b055a0c26ec8412101d10aa1e6c8464c9bf6d8a87478f0979329abbf9321a`.
The explicit return type and every assertion remain unchanged. The isolated
formatter pass is formatting evidence only and a fresh full gate is still
required.

## Mandatory production source audit

The independent source reviewer must audit the production implementation in
addition to the executable assertions. The current checkpoint still has the
old raw-child implementation, so these are required postimplementation
questions rather than acceptance claims:

1. At all three production sites, locate final command preparation, fresh
   Packet 1B barrier, consumption of the same owned plan, the one real spawn,
   infallible piped-handle extraction where applicable, immediate bind, slot
   bind, post-spawn pin/tuple validation, and bridge setup. There may be no
   fallible operation or callback between successful spawn and custody.
2. Enumerate every production native `try_wait`, wait, and kill. After bind,
   only the lifecycle actor may perform them. Test helpers, bridge readers,
   retention, cancellation, and cleanup must not introduce a second waiter.
3. Trace every `SpawnStageError` construction. A proven missing-executable OS
   spawn failure is exact `BeforeCliStart(ContainerRuntimeUnavailable {
   binary })`, preserving the requested binary; bind failures own `Unbound`;
   every later error owns `Managed`, including post-spawn control change and
   PTY bridge failure.
4. Trace chat, exec prompt, and the single-attempt exec workflow through the
   unchanged common `AgentEngine` call path. Confirm the existing native
   orchestrated rejection remains enabled.
5. Audit `ContainerBackend::build_with_launch_retention`: its default must
   reject orchestrated input before calling an old backend even when an Arc is
   supplied. Docker and Apple overrides must share build logic, reject missing
   retention before lease/image/provider effects, and never allocate a
   per-build registry.
6. Trace the application owner through `ContainerRuntime`, both
   `Engines::from_detected` and `Engines::for_daemon`, instances, executions,
   gate supervision, cleanup, and retained entries. Only runtime/Engines may
   keep strong application Arcs; execution/supervisor/cleanup backreferences
   must be weak.
7. Audit the registry worker and Drop: move entries to worker-local custody
   before lifecycle work; hold no registry/lifecycle/control mutex across
   polling, termination, timed waits, or join; never self-join; join another
   thread only after `is_finished`; detach at the one absolute deadline; and
   never call provider inspect/stop/remove or create evidence. A gated local
   reap clears only execution custody: snapshot accounting must then report
   `unreaped == 0`, retain the unresolved entry, and keep the worker alive.
8. Confirm `AgentEngine` native rejection and Docker/profile guards still
   block production native invocation. The `/bin/sh` fixture selection must
   exist only in `cfg(test)` support and must not alter `ContainerCli::DOCKER`
   or `ContainerCli::APPLE`.

Checkpoint locations that the implementation replaces or extends are
`process.rs:402-629` (three spawn paths), `process.rs:667-776` (raw owners,
wait, and name-only cancel), `io_bridge.rs:304-384` (pipe extraction),
`runtime.rs:153-212` (runtime construction/build), `backend.rs:16-24`
(required build method), `command/dispatch/mod.rs:84-209` (application engine
owners), `gated_launch.rs:305-502` (barrier), and `agent/mod.rs:211-222`
(native rejection).

## Known limits and deferrals

- The fixture proves production function placement and real local-child
  custody. It does not prove Apple/Docker provider invocation, name
  exclusivity, tuple truth, AGY behavior, or network behavior.
- Packet 2 has no caller diagnostic input. Positive case-19 collision proof is
  deferred to Packet 3 until complete bounded stderr is bound to this exact
  invocation/lifecycle and an independently evidenced allowlist.
- Matching observation is tuple convergence only. This packet constructs no
  `ProtectedReady` and no `GatedSpawnOutcome::Created`.
- Before-bind cancellation's lack of provider/local-child action and the exact
  instance registry field are source-audit obligations because the accepted
  narrow test-support interface exposes neither a pre-bind cancel callback nor
  instance internals.
- A polling pause proves scheduling, lock release, and custody. It does not
  claim an OS syscall was interrupted or bounded.
- Packet 3 retains cases 20-28 and 33-35. No packet alone establishes release
  readiness.

## Expected red

At the pinned checkpoint, missing Packet 2 production modules, aliases, types,
fixture harness, lifecycle methods, registry methods, runtime field, and
defaulted backend method are expected compile failures once compilation is
reached. The post-format gate established those missing-API diagnostics but
also produced the bounded-waiter inference cascade described above, so it is a
mixed compile-red record rather than all-expected-red evidence. The explicit
contract postimage has not yet been compiled. Syntax/type errors in the
revised candidate, mutation of prior frozen assertions, an actual provider
request, an unowned child, or a non-bounded failure path is unexpected and
returns to this independent test author.
