# Gated native launch Packet 2C freeze candidate

Status: independent Sol test-author source candidate for Root's complete
assertion/fixture review, then Luna's revision-matched Rust 1.94 expected-red
classification. Nothing in this directory has been applied to the AWMan
worktree at these final post-format pins. Luna's predecessor application and
gate evidence are recorded below. This Sol author executed no formatter,
compiler, test, build, CI, scan, provider, network, model, or runtime workload.

## Authority

- Packet 2 concrete seam:
  `ef11f6698cdcadc21a63ea9d04b0394fcf0d3e61ff1782a27ec0d7fa29f8dec4`.
- Root Packet 2 source disposition:
  `a4fcc053f4c3d2417c3bee85b8ed75fb58e103f31bf9c60720a0154160c71f1b`.
- Luna source-pin record:
  `aae31f972ccb58263d98a167967de157de9e75bffbd91c512d40d3bb59a074e4`.
- Concrete addendum:
  `eb00e22c1399637294f904d6dd630399e4fb3a2a44e59ba60ea7e8740f713266`.
- Identity interface:
  `ead26429abf89f0dab24cbe40421bbd93929105b5cb74e72bf586ffaea6f132e`.
- Existing 35-case matrix:
  `76d2b295463e6c94fb2d18082620665945a452cf54b93c7ea5df9f64f577f8b0`.
- Base HEAD/tree:
  `5688b65cfbc90b2075877df8da77f16633dc912d` /
  `75be5ca6b04f303f9ef39debeec82e9630f4dd5a`.
- Layer-placement full-gate evidence:
  `620f01bd32a714b24fd8675aa87f8131dcb2f6798ad51c4dcaec5e0ec7280805`.
- Isolated Rust 1.94 formatter evidence:
  `c77128a0232e57cd937780be8b8f476d696901242e902a7f419fc96c945270b8`.
- Root-reviewed formatter patch:
  `5dde4b36d413919c348448dd0ad6d1f596c2f7d0abb5dbf280ad02ec84b2735c`.
- Post-format Rust 1.94 full-gate evidence:
  `dccc75315eb180059efbfbb716910654d81e2802bd1c16a7d1a36c55a7f3d328`.
- Explicit-contract full-gate evidence:
  `28e9fea688a5e60fa6c30d78881567eead00bc1364f0b1138b52ed8d48b5caad`.
- Explicit-contract isolated Rust 1.94 formatter evidence:
  `d7204ef217584eed21cbb1c31f6a09f4e4e7bc2a76917e2332a8a8946603ad5f`.
- Root-reviewed explicit-contract formatter patch:
  `cc4d6846d1c0c3d1f7a2866b5d09dee8b6ab678982175a44995fa7edd63fce74`.

Candidate source pins:

- `src/engine/container/gated_launch_p2c_test.rs` — 41,413 bytes,
  SHA-256 `433b055a0c26ec8412101d10aa1e6c8464c9bf6d8a87478f0979329abbf9321a`.
- `src/command/dispatch/gated_launch_retention_p2c_test.rs` — 1,265
  bytes, SHA-256
  `3c60a135eebbef3f55ab58e5590a9da75109b874ba607389d649130ba54a1dec`.
- `aspec/work-items/0113-gated-launch-native-packet2-p2c.md` — 14,395
  bytes, SHA-256
  `ccb940b437d9cab9e075ed2cd0731ca335cc9bef51f9dde334b4952e2493e7e7`.
- `container-mod.patch` — 339 bytes, SHA-256
  `602f1b3478063365484e7ee81eba7b0a295414aab2088ed7518c60b14169e6cb`.
- `dispatch-mod.patch` — 311 bytes, SHA-256
  `e5998e404768051e227df0e9104bd9dc02ecff42e47e40099ccf0254d72f5e5c`.

The container registration patch is based on the current
`src/engine/container/mod.rs` SHA-256
`8ec27920b16b2610f89a2285f59997a9dacc349aa98d97d1ecd183fdaf0197d1`.
It adds only `#[cfg(test)] mod gated_launch_p2c_test;` beside Packet 1B.
The dispatch registration patch is based on
`src/command/dispatch/mod.rs` SHA-256
`914071490045d96addc5b23e111a6cfb28f97b428fa3c404b3a282bba259bccf`
and adds only its `cfg(test)` child module declaration in rustfmt's lexical
position. The expected postimage is 75,562 bytes, SHA-256
`66f6f4522bc2d50cfe60cd777c23580ac49baa57eecd275d5fec241186a4de86`.

## Prior frozen preservation pins

The candidate does not copy or edit the existing frozen files. Application
must preserve these checkpoint bytes exactly:

- `src/engine/container/gated_launch_p2_test.rs`:
  `081c7825813332490120a9880d6ac728d9edcc0e650699442efc631abc6b1d48`.
- `src/engine/container/gated_launch_p2b_test.rs`:
  `047c370f3e2c94099c1af936e8361adbd0f9f54655398663d646d8e384d375ed`.
- `aspec/work-items/0113-gated-launch-native-p2.md`:
  `76d2b295463e6c94fb2d18082620665945a452cf54b93c7ea5df9f64f577f8b0`.
- Existing cumulative freeze record:
  `04bde33b564e434a75e89402fa80fa9c13c4d0ca34d146d9fc03987e3b8575af`.

## Candidate assertions

The two additive test files contain fifteen test functions: fourteen in Layer
1 `engine::container` and one in Layer 2 `command::dispatch`:

1. all three real production spawn functions use a fresh plan-backed barrier,
   start one genuine local child, bind one authority, bridge real output, and
   publish one actual exit;
2. lifecycle actor startup failure starts no child and performs no second
   absence;
3. disconnected/full bind faults return the exact raw-child kind for PTY,
   one-shot piped, and persistent-piped, followed by synchronous retention and
   actual eventual reap; a marker count of at most one is only a retry guard;
4. post-bind bridge failure returns managed pre-execution custody for all
   three representations and preserves the actor's exact actual exit without
   requiring exit zero or inferring a PTY signal;
5. zero/absence and nonzero/matching post-start observations retain without
   retry or destructive provider action;
6. a live matching tuple remains only a `StartedCreateObservation`;
7. a fixed missing executable produces the genuine no-child OS error;
8. pre-extracted cancel handle, bounded lifecycle waiter, legacy waiter, and
   ordinary execution wait agree byte-for-byte on one actual reap;
9. direct cancel and startup-grace cancellation use the same bound actor;
10. synchronous retention remains responsive during an actor poll pause and
    poison recovery preserves both tickets;
11. injected registry construction failure predates any fixture spawn;
12. the defaulted backend method rejects orchestrated input before invoking an
    old backend while legacy input delegates once;
13. runtime and engine clones expose the same application registry Arc;
14. managed last-owner Drop detaches a paused actor at the absolute deadline
    and later observes actual reap; and
15. raw-unbound Drop either actually reaps or detaches, never changes provider
    call history, and exercises the no-self-join helper.

The trusted fixture is `/bin/sh` plus a unique script pathname passed in argv.
Its release wait has an approximately ten-second finite fallback, and its Drop
checks and reports release-write failure. It never substitutes a Child, PID,
wait closure, exit result, or READY result.
Every native-success assertion comes from the production spawn function and
actual lifecycle actor. Error custody is transferred into the registry before
the test releases a faulted child. A fixture Drop also writes its unique
release marker so a failed assertion cannot leave a cooperative child waiting
forever.

## Root review questions before freeze

Root must read both complete test files, the packet spec, and both registration patches and
answer all of the following before authorizing application:

1. Does every success use `process::test_support::spawn_fixture` and one of
   the three actual production functions rather than reconstructing its spawn
   sequence?
2. Does each gated invocation obtain its `DurableLaunchPlan` only through the
   validated filesystem loader and planning function, preserving a clone only
   to retain error custody?
3. Can any test-only hook manufacture Child/PID/wait/exit/READY success, or do
   the hooks stop at the accepted actor-start, bind, post-bind bridge, poll
   pause, poison, registry-start, and self-drop boundaries?
4. Are bind-failure and post-bind-failure owners transferred before teardown,
   with exact child kind and managed/unbound distinction asserted, without
   requiring the raw bind-failure child to execute user code?
5. Would a second child spawn, raw-child drop, second waiter, synthetic exit,
   name-only provider cleanup, or strong registry cycle cause at least one
   assertion or mandatory source-audit question to fail?
6. Does case 19 remain conservative: no caller diagnostic bytes, no collision
   allowlist, no post-start noncreation, no READY/Created value, and no retry?
7. Do direct cancellation, cancel handle, grace, bounded wait, legacy wait,
   normal wait, retention, and shutdown all converge on one lifecycle actor
   and exactly one actual reap?
8. Do poison and actor-pause checks establish ownership and lock
   responsiveness without claiming an OS syscall timeout?
9. Does last-owner Drop destroy the wrapper Arc, return within the fixed grace,
   leave probe-visible detached custody when actual reap is still pending, and
   avoid provider operations/evidence?
10. Does the packet spec clearly separate executable observations from the
    mandatory all-three-callsite/source-owner audit and Packet 3 deferrals?
11. Are the existing Packet 1A/1B files and assertions untouched at the pins
    above?
12. Is every assertion satisfiable under the accepted seam, and would each
    test parse/type-check once only the missing production API exists?
13. Does Layer 1 now have no command/dispatch import, while the unchanged
    runtime/`Engines` identity assertions are registered only as a Layer 2
    dispatch child without an ignore, suppression, or shim?

## Mandatory source review paired with the tests

The exact audit is in the candidate packet spec. Root and the later production
reviewer must still locate the three adjacent barrier/spawn/bind sequences,
enumerate every native wait/kill, inspect every error owner, trace all registry
strong/weak references and Drop joins, validate the source-compatible backend
default, and trace chat/exec/workflow through the unchanged engine. Fixture
green cannot substitute for this audit.

At the current checkpoint, production still stores raw children in
`ContainerExecution`, waits them directly, uses name-only `stop_and_remove`,
extracts pipes through a borrowed Child, lacks the Packet 2 registry, and has
no Packet 2 test-support aliases. Those are expected missing-production reds,
not candidate acceptance evidence.

## Layer-placement correction

Luna applied the preceding single-file candidate byte-exact and ran the real
Rust 1.94 full gate. Architecture lint stopped the gate before formatting,
Clippy, compilation, or tests because the Layer 1 container test imported
Layer 2 `command::dispatch::Engines`. Structured evidence SHA-256:
`756d7641ed4b006aa3a8a3a9100e1eda0d08cdf0424234620979e6d36d1e9fd7`.

This correction moves the complete
`runtime_and_engine_clones_share_the_application_registry_arc` test, with every
assertion unchanged, into the new Layer 2 dispatch child. It removes only the
now-unused Layer 2 and `ContainerRuntime` imports from the Layer 1 file and adds
the two-line test-module registration in dispatch. It adds no architecture
ignore, lint suppression, shim, production API, or altered expected value. The
fifteen-test total remains fourteen Layer 1 plus one Layer 2. The gate result
is an unexpected placement defect record, not missing-API red evidence.

Luna's next revision-matched full gate passed architecture lint and stopped at
`cargo fmt --check`; structured evidence SHA-256
`620f01bd32a714b24fd8675aa87f8131dcb2f6798ad51c4dcaec5e0ec7280805`.
That result is an unexpected candidate formatter gate failure. Clippy,
compilation, tests, and the expected missing Packet 2 production API red were
not reached. Luna subsequently ran Rust 1.94 rustfmt against isolated copies;
structured evidence SHA-256
`c77128a0232e57cd937780be8b8f476d696901242e902a7f419fc96c945270b8`.
Root read and approved the exact formatter patch. It only sorts one import,
wraps Layer 1 lines, and moves the dispatch test declaration to lexical order;
the Layer 2 test and container module are byte-identical. This candidate adopts
the approved Layer 1 postimage and matching dispatch registration position.
No assertion, fixture, or runtime behavior changed. Luna must rerun the full
gate after deliberate application of these final pins.

That post-format rerun passed architecture and formatting, then stopped during
compilation with twelve diagnostics. Eleven identify the deliberately missing
Packet 2 production modules, aliases, fields, and methods. The twelfth is
`E0282` at the bounded lifecycle waiter; structured evidence SHA-256
`dccc75315eb180059efbfbb716910654d81e2802bd1c16a7d1a36c55a7f3d328`.
The accepted seam declares `wait_actual_until` to return exactly
`Result<Option<AgentExitInfo>, EngineError>`, while that compilation could not
resolve the imported authority or method supplying the closure return type.
This identifies the inference diagnostic as a missing-contract recovery
cascade. The revised Layer 1 source states that exact return type on the
`spawn_blocking` closure. The resulting value, timeout behavior, and all exit
equality/reap assertions are unchanged. At that point no formatter or compiler
had run on the revised source, so no all-expected-red claim was available.

The next revision-matched gate passed architecture lint and stopped at an
unexpected `cargo fmt --check` closure-layout diff before compilation;
structured evidence SHA-256
`28e9fea688a5e60fa6c30d78881567eead00bc1364f0b1138b52ed8d48b5caad`.
That run neither confirms removal of `E0282` nor adds expected missing-API red
evidence. Luna then formatted an isolated copy successfully; evidence SHA-256
`d7204ef217584eed21cbb1c31f6a09f4e4e7bc2a76917e2332a8a8946603ad5f`.
Root read the exact 800-byte formatter patch and approved its sole closure
layout change. The candidate adopts that exact 41,413-byte postimage. The
explicit `Result<Option<AgentExitInfo>, EngineError>` contract, timeout,
returned value, and all assertions are unchanged. The isolated pass is not a
full-gate result; the formatted candidate remains uncompiled and requires a
fresh Luna full gate.

## Known limits

- Real local child execution certifies local custody only; it is not a real
  Apple/Docker create, provider tuple, AGY, model, or network test.
- Exact complete invocation-bound stderr and the independently evidenced
  collision allowlist remain Packet 3 prerequisites. This packet deliberately
  writes no positive collision test.
- READY, receipt, release, credential activation, exact provider cleanup,
  unsupported mode enforcement, and legacy compatibility stay in Packet 3.
- The accepted narrow test-support surface does not expose a pre-bind cancel
  callback or `ContainerInstance` registry field. Those two facts remain
  explicit production source-audit obligations rather than widened test-only
  authority.
- Application registry identity is observed through runtime and `Engines`;
  instances are audited structurally and exercised indirectly by the exact
  fixture harness.

## Freeze and execution order

This record is a candidate, not a freeze verdict. Root first reviews the full
source. If approved, an authorized applicator copies both new test sources and
the spec, applies both registration patches, and records a fresh applied manifest.
Luna then runs the exact Rust 1.94 expected-red/full-gate workloads on that
revision and returns structured evidence. The approved isolated formatting
does not substitute for that fresh gate. Any further formatter-only or
fixture-only change returns to this original test-author role with exact
preimage/diff/postimage hashes. A separate Sol production author may begin only
after the independent assertion review and Luna classification. The production
author must not edit either frozen test file.
