# Gated native launch P2 freeze packet

Status: Packet 1A is applied and independently source-reviewed. Sixteen tests in four additive files are frozen at the current pins in the last amendment. A passing full post-formatter gate remains pending; earlier gate evidence and amendment history appear below.

## Authority

- AWMan worktree checkpoint for this source-pinned wiring patch: `eac5244b6f21947e622cf701cdaac6c228fc1834` on `feat/workspace-preflight`. It is a documentation-only descendant of the original Packet 1A baseline `c5fa6b19f35a11c88dd32570db2793a75a42f836`; the production source pins used by these tests are unchanged.
- Concrete P1: `eb00e22c1399637294f904d6dd630399e4fb3a2a44e59ba60ea7e8740f713266`.
- Identity P1: `ead26429abf89f0dab24cbe40421bbd93929105b5cb74e72bf586ffaea6f132e`.
- Split-control amendment: `76eacc8614a4e1a18ce782b3c5f3cb37ba8b997452ad717ef8d88dd2e26b4b4b`.
- Apple stopped-schema evidence: `bd7f4ebc8853b83f54c2207f6a5c0387466a2908cc108a6257d4b58cd111872e`.
- Packet 1A seam disposition: `aspec/reviews/gated-launch-native-p2-seam-disposition.md`.

The test author is retired from AWMan native production implementation. The future production author must not read these assertions. Existing startup/bootstrap tests are not replaced, edited, or reinterpreted by this packet.

Current source pins at `eac5244b6f21947e622cf701cdaac6c228fc1834`:

- `Cargo.toml`: `62b86612125c691ce5d42a4388e7e99ad9d20844a7843afe205b52cd38163502`
- `Cargo.lock`: `7ae63b0a4ba2f8d91a37e9f9e077d1e8ed1c1ba9b635d82f277d819e2f3d8798`
- `src/data/startup_gate.rs`: `5675a50f115c20eea9e75ef3ef5a67d0f1234b4493e08ff6201a06d62b573f46`
- `src/engine/container/startup_gate.rs`: `b5baff654db2f43e689601d3ecf6202ed4e1f6b2b997e67f835c27ddcb505fc9`
- `src/engine/container/apple.rs`: `d4a794ce32b2ae52082c6c5419efc5fef16a47bba8aa4153986d6b43ea4ec3df`
- `src/engine/container/mod.rs`: `e131d04237ce244f4ec8606506b6aa9cb8fbb60e78ed86a11c5576dbfe1e3c5d`

## Packet 1A files

- `src/data/startup_gate_native_p2_test.rs`: six loader-only tests for the bounded intention parser, validated identity and redaction, reserved names, unsafe guest rejection, and missing-intention legacy layout behavior.
- `src/engine/container/startup_gate_native_p2_test.rs`: two stager tests for split pinned topology, byte-exact approved request/manifest staging, static-directory and secret-surface exclusions, the deferred guest-child overlay, and legacy single-directory mounting.
- `src/engine/container/apple_gated_p2_test.rs`: strict Apple image/container JSON parser tests using the source-pinned configuration descriptor, label, creation, name, and running/stopped fields.
- `src/engine/container/gated_launch_p2_test.rs`: actual `GatedProviderAdapter` plus real filesystem tests for final image/absence ordering, canonical durable plan bytes and permissions, existing-plan retained recovery, no overwrite/no respawn, and deadline bounding.
- `build-wiring.patch`: an apply-checked unified patch against the pinned HEAD, containing the direct `zeroize` dependency, its existing-package `Cargo.lock` root dependency entry, new gated-launch module wiring, and additive test-module declarations. The production author may fill `gated_launch.rs` around the test declaration but may not edit frozen assertion files.
- `aspec/work-items/0113-gated-launch-native-p2.md`: complete 35-case matrix and packet boundaries. Packet 1A does not claim release, lifecycle, cleanup, or runtime acceptance.

Packet 1A observes exact bounded-parser outcomes, secret-free returned surfaces,
real split filesystem staging, strict Apple tuples, canonical plan state, and
no-respawn behavior. Filesystem state after return cannot itself prove memory
zeroization on unwind, `sync_all` ordering, or the atomic no-replace primitive;
the reviewer must verify those P1 mechanisms in the implementation, and later
execution evidence must cover their fault paths. This limitation is not a
release-readiness waiver.

## Review questions

The independent reviewer must answer:

1. Is each assertion entailed by the three authoritative P1 documents?
2. Are the tests satisfiable without exposing raw tokens or weakening opaque production authority?
3. Do the loader/stager tests exercise production filesystem behavior rather than a names-only fake?
4. Do Apple parser tests reject missing, mismatched, duplicate, malformed, and unknown-state inputs instead of accepting current permissive parsing?
5. Does the planning test call the actual orchestration function with the exact adapter seam and inspect the real durably published file?
6. Would the tests fail if split mounting used the parent, if an existing plan respawned, if identity fields were ignored, or if raw token material escaped?
7. Are any test-only accessors broader than the internal access production already needs for trusted identity injection and planning?
8. Does this candidate compile in principle after the missing P1 API is implemented, without depending on existing frozen assertions?

## Expected-red classification

The current checkpoint lacks the P1 layout, pinned authority, Apple gated parsers, gated provider types, planning API, and gated-launch module. Compilation failure for those missing production symbols is the expected red after review and application. Syntax errors, inaccessible seams unnecessary to production, failures in pre-existing tests, or mutations to existing frozen files are unexpected and return to this test author.

## Freeze rule

The four additive `*_p2_test.rs` files are frozen at the current hashes in the last amendment. The production implementer may update non-assertion fixtures or test-only module wiring only through an explicitly reviewed test-author follow-up. Packet 1B and later packets add new frozen files; they do not reopen Packet 1A assertions.

## Formatter-only freeze amendment

Root source-reviewed and approved the following three mechanical `rustfmt`
diffs. They change only import order and whitespace; all assertions, fixtures,
and behavior remain frozen. The original test hashes above remain the semantic
provenance pins:

- `src/data/startup_gate_native_p2_test.rs`: original
  `1abcbfd11b4fb3b9b7a52808222f4473d1fd0e5811d3f075a65c5e2d2b0180a9`;
  formatter diff
  `09c9f1748a8a0ccfe98e657c715b4ed525fe02079bba6f33cbf355dd2c3c0b4e`;
  formatted frozen file
  `e819322bd5f29068799ded5cd6e328a28213fa0f1dfb5b3b20e5a727bc691349`.
- `src/engine/container/apple_gated_p2_test.rs`: original
  `9a1db182ae058b0546b935ce8798a7927ec4f721f32a79de56d2538739cb2c04`;
  formatter diff
  `5bf0fb782bfd7a78469d44d1a43380b381f706d6a35b757ef53265450c9e79ce`;
  formatted frozen file
  `cf8e607cee32f8aae5278d5b7ae08c05667b71c0fcefad81d9e3ac44fcb21e97`.
- `src/engine/container/gated_launch_p2_test.rs`: original
  `c2fb51f830317410d280d5dc0602169ead0aa2ceb0c6d26f74efbb2a843473b4`;
  formatter diff
  `87cc2dcae2fbf472cd6ab7b6f876e4a0a021e42978bd9cdbe6262a8a78dbe1d4`;
  formatted frozen file
  `8979e3ed966889b0628fa899e097a7b0cb7fd0e409531e187023fb34428f12f8`.

Luna's Rust 1.94 `cargo test --no-run` evidence exited 101 while compiling
against the expected missing native P1 APIs; log SHA-256
`faf43cd980ce088080371c5eddff7bdf748f4554bfeecc58e552f60f5919a9c9`.
That is partial compile classification only. A complete `make pre-push` result
has not been established. A separate `cargo fmt --check` reported differences,
which this amendment corrected mechanically.

## Layer-relocation candidate amendment

A full real Rust 1.94 `make pre-push` run on applied Packet 1A exited 2 at the
architecture lint before native compilation. The lint located two forbidden
Layer 0 imports in `src/data/startup_gate_native_p2_test.rs`: the container
startup-gate module and container overlay types. Log SHA-256:
`d68505a84016c252aa12619c713a634a9fd8382ee2df209c88b90adff89e29b1`.
This is an unexpected test-placement failure. It provides no native compile or
runtime result, and a complete post-repair `make pre-push` result is not yet
established.

The source-only repair relocates every stager and overlay assertion into a new
child test module of `engine::container::startup_gate`. The data-layer file
retains loader identity, redaction, parser, reserved-name, guest-pin, and legacy
layout assertions and imports no engine module. The engine child uses only the
approved borrowed `orchestrated_parts` view of already-validated caps and the
non-secret identity; it adds no authority constructor or path-derived cleanup
authority. The old combined orchestrated and legacy tests are split by layer,
so Packet 1A grows from 14 to 16 tests without dropping a semantic assertion.

Candidate lineage for Root review before worktree application:

- formatted data test before relocation:
  `e819322bd5f29068799ded5cd6e328a28213fa0f1dfb5b3b20e5a727bc691349`;
  loader-only candidate:
  `f2fa6e855407f2266862b13198ac0fac2b61d50527428f8d537df80d24e0e46f`.
- new engine stager test candidate:
  `3ac23df6937fe3f95ffa53de5c35fd41d3636df659d5022d8b4115cd65c1200f`.
- exact relocation and test-module wiring patch:
  `9eb45874373086b91f644ee45a5647eeeb4efa89dc16d9a5c208e6c94884db8b`.
- Apple identity and gated-plan tests remain byte-exact at
  `cf8e607cee32f8aae5278d5b7ae08c05667b71c0fcefad81d9e3ac44fcb21e97`
  and
  `8979e3ed966889b0628fa899e097a7b0cb7fd0e409531e187023fb34428f12f8`.

These candidate hashes become frozen only after Root approves the exact
relocation and authorizes application. The existing startup and bootstrap
tests remain immutable.

## Repository formatter amendment after layer relocation

Root source-reviewed all four diffs produced by the repository's absolute Rust
1.94 `cargo fmt`/`rustfmt` driver for edition 2021 with no configuration flags.
They change only import order and layout. All 16 assertions and fixtures remain
semantically frozen. The pre-format hashes above remain lineage pins; the
current formatted frozen pins are:

- `src/data/startup_gate_native_p2_test.rs`: pre-format
  `f2fa6e855407f2266862b13198ac0fac2b61d50527428f8d537df80d24e0e46f`;
  formatter diff
  `68aaaf3cf2aea118067c7ffaad76139deabf5cc6bade284534a9c9475e977ad9`;
  formatted file
  `7911c42a939dba206c9612accd88b25f2ef9881419a88d707cc86484d8524347`.
- `src/engine/container/startup_gate_native_p2_test.rs`: pre-format
  `3ac23df6937fe3f95ffa53de5c35fd41d3636df659d5022d8b4115cd65c1200f`;
  formatter diff
  `4884c2fcdb4a4da09afb98ed51fe35f94040f4dee1af3b231b6a35df382e951a`;
  formatted file
  `cf55b156125014e4346311db37a8cc02ecb100123dd8a5df6f9307310bb3d130`.
- `src/engine/container/apple_gated_p2_test.rs`: pre-format
  `cf8e607cee32f8aae5278d5b7ae08c05667b71c0fcefad81d9e3ac44fcb21e97`;
  formatter diff
  `e658c4f6d8b145ff5125ea7cc5b408ce1945282d6437851c275250e89d0ad8d4`;
  formatted file
  `94f26234c984a9869a8f63bab8ff204b0fc7c06d42f5fe024708ea80ba31c094`.
- `src/engine/container/gated_launch_p2_test.rs`: pre-format
  `8979e3ed966889b0628fa899e097a7b0cb7fd0e409531e187023fb34428f12f8`;
  formatter diff
  `1d08c318e283f12c7ce3a01ffeede2d01aa47046ba16754df978908caf3374aa`;
  formatted file
  `081c7825813332490120a9880d6ac728d9edcc0e650699442efc631abc6b1d48`.

This formatter amendment establishes no compile, test, runtime, or full-gate
result. The existing startup and Python bootstrap assertions remain unchanged.

## Current expected-red gate evidence

An actual Rust 1.94 full `make pre-push` run exited 2 after the architecture and
formatting checks passed. Compilation then reported 58 missing implementation
symbols, the expected red for this test-first packet, plus two unused `super`
warnings that follow from those missing symbols. Log SHA-256:
`7ab6c1020d8988f04720ba3009ccfc1d025d3072d441971c7787f531c0d5674a`.
This establishes expected-red compilation only; it is not production
implementation or runtime proof.

## Original-author fixture result-conversion amendment

Rust 1.94 type checking located one fixture-only mismatch in
`src/data/startup_gate_native_p2_test.rs`: `ControlFixture::rewrite_intent`
declares `Result<(), Box<dyn Error>>` but returned the narrower
`std::io::Result<()>` from `write_private` directly. The original Packet 1A
test author changed only that helper body to propagate the I/O result with `?`
and then return `Ok(())` in its declared error type. No assertion, expected
value, fixture bytes, production interface, or test coverage changed.

Source lineage for Root review before worktree application:

- current frozen data test:
  `7911c42a939dba206c9612accd88b25f2ef9881419a88d707cc86484d8524347`;
- fixture-corrected candidate:
  `5d0f872875cac1d6fab463d784d52a1ffea1d8726af5a2cfb8c8a32185ed59ba`.

This amendment records a source-only candidate. No formatter, compile, test,
runtime, or full-gate workload was executed, and nothing in the worktree was
changed. The candidate becomes the frozen data-test pin only after Root reviews
the exact two-line fixture conversion and authorizes application.

## Legacy external-test control-layout migration candidate

Revision-matched Rust 1.94 full-gate evidence showed that the pre-existing
external test `tests/engine/startup_gate.rs` still initialized and projected the
removed `StartupGateSpec.control_dir` field. The gate passed formatting and
library compilation reached the external-test target; all-target Clippy
remained blocked by this external-test compile failure. Raw-log SHA-256:
`6457c6877a54fd7cd7af853bd3278824c82b655f6b1704bd1242b635dda74bd5`.

This source-only fixture migration imports the current public
`StartupGateControlLayout`, wraps the same owned control path with
`StartupGateControlLayout::legacy`, serializes the exact existing fixture request
bytes, and fills `request_digest` with their real SHA-256. The former path
equality assertion now compares the public `Eq` control values, which preserves
the path check and also requires the exact legacy layout variant. Every other
fixture value, assertion, and expected byte remains unchanged; no production API
was widened.

Source lineage for Root review before worktree application:

- existing legacy external test:
  `8f80aa894fb2499b205eacda6e80e8682b5b345d39ba56727cb6fe1abeab6cd3`;
- migrated candidate:
  `d7c793e2065025a3995fda0d79a70f0bc0911ed260f6e18683a5e280f0c1032b`;
- preceding freeze record:
  `285bc43b0cfc5132db7e52f529bda953386d6446f9f6dcccca64d874b7193d0b`.

No formatter, compiler, test, full gate, probe, or scan was run while preparing
this candidate. It becomes an authorized fixture migration only after Root's
full source review.
# Gated launch native P2 Packet 1B freeze candidate

Status: source-only candidate for Root full test review. No packet file is in
the AWMan worktree. No formatter, compiler, test, CI, runtime, provider, or raw
execution-log workload has run.

## Authority

- concrete P1: `eb00e22c1399637294f904d6dd630399e4fb3a2a44e59ba60ea7e8740f713266`;
- identity P1: `ead26429abf89f0dab24cbe40421bbd93929105b5cb74e72bf586ffaea6f132e`;
- split-control P1: `76eacc8614a4e1a18ce782b3c5f3cb37ba8b997452ad717ef8d88dd2e26b4b4b`;
- committed matrix: `76d2b295463e6c94fb2d18082620665945a452cf54b93c7ea5df9f64f577f8b0`;
- packet base HEAD: `69ecd05790a0f93a3b17a44222d4d24f42316696`;
- Packet 1A freeze record:
  `e885b97d1bc2d2f79ccdfe0842c7b573670126fbf737b357a2abe51d613e2653`;
- production-layer disposition:
  `fe694574e789d9894ac06b5130bf5616ed3e38b7085b3f53230c6659e8204776`;
- Packet 1A seam disposition:
  `2f14713286540bd82b5fe266a70c1f759b2edd7b26eb8e36694775dd26db6cf1`.

Candidate Packet 1B pins:

- minimal seam disposition:
  `1e53c09756f4e8b0dd936b847e36ea5225c4b939791a0b9ee66212b2074439ff`;
- additive engine test:
  `1d7c049fb54f4961d128e258cc1f21606518a8f8c529f9d78b740d03c8939209`;
- module-wiring patch:
  `e502d9ceb9ccbb0f78d31514c83a5371a2df0c7858e29cc64c6de08b4ea988de`;
- Packet 1B work item:
  `60f89f4c38d73a17947b4957fe737eb33a66665df0aae0b019a3f97937535cb1`.

## Review requirements

Root must verify that the test source:

1. compares Docker image `.Id` only with container `.Image`, after exact
   lowercase `sha256:<hex64>` normalization;
2. treats image references as diagnostic and never immutable identity;
3. hashes only the fixed normalized tuple, preserves fractional creation time,
   ignores raw JSON, and digests an unrecognized state before canonicalization;
4. checks the exact 256-KiB/64-KiB limits and actual reap rather than trusting a
   self-reported boolean;
5. starts no process for an expired deadline, and never expects raw provider
   output in an error;
6. distinguishes a reaped provider parent from descendant-held drain EOF and
   imposes a bounded return without an unbounded join;
7. reaches the immediate barrier through the actual loader, held controls,
   durable plan, adapter, and fresh second absence rather than constructing
   authority in the test;
8. rejects present, ambiguous, unavailable, mismatched, and substituted inputs
   without obtaining a spawn token;
9. makes no Docker-orchestrated support or full native readiness claim; and
10. preserves all four Packet 1A test hashes and changes no existing assertion.

## Expected-red boundary

After approved application, missing Packet 1B production types and functions
are expected compilation reds. Test syntax/import failures, architecture-layer
violations, modifications to Packet 1A, real provider invocation, leaked child
processes, or a test exceeding the repository time budget are unexpected.

## Freeze rule

After Root and the authorized independent reviewer approve the exact source,
the new test and wiring hashes are frozen. The native production implementer
must not read or modify this test. Formatter-only or fixture-only changes
require this original P2 author, a located reason, source review, and appended
lineage hashes.

## Fixture correction pending source review

The initial test candidate
`1d7c049fb54f4961d128e258cc1f21606518a8f8c529f9d78b740d03c8939209`
used the Docker canonical fixture name in the real orchestrated loader fixture.
Identity P1 requires the `awman-altana-` prefix. The original P2 author changed
only the launch-intent fixture to the separate valid
`awman-altana-p2b-exact`; the Docker fixture name, canonical digest,
assertions, production seams, and build wiring are unchanged. Root withdrew
the initial approval pending review of this exact fixture-only delta.
The corrected test SHA-256 is
`9e3f21f0242c00f7ff57e7a01a532455c5076ad18c42826ad50824288aa70b53`;
the amended work-item SHA-256 is
`f62019d8bedb17c62a195ae4816c11d3ee7242563b007138c46ae58ed9f506ae`.

## Packet 1A data-fixture lineage update pending application

Packet 1B was authored against the then-current formatted Packet 1A data-test
pin
`7911c42a939dba206c9612accd88b25f2ef9881419a88d707cc86484d8524347`.
Rust 1.94 later reached a fixture-only return-type mismatch in
`ControlFixture::rewrite_intent`. The original Packet 1A author changed only the
helper body to propagate `write_private(...)` with `?` and return `Ok(())` in
its declared `Result<(), Box<dyn Error>>` type. The source-approved corrected
Packet 1A data-test pin is
`5d0f872875cac1d6fab463d784d52a1ffea1d8726af5a2cfb8c8a32185ed59ba`;
Root's durable source review is
`/private/tmp/awman-native-p2-fixture-result-root-review.md`, SHA-256
`765d8cf066dbc923ba34b2d915c83b6e30eab2d49b3f54125e36f83f3eb98d98`.

Accordingly, review requirement 10's old `7911...` data-test preservation pin
is historical after that separately authorized Packet 1A fixture application.
The requirement then means preserving `5d0...` plus the other three Packet 1A
test pins byte-exact. Packet 1B's test remains byte-exact at
`9e3f21f0242c00f7ff57e7a01a532455c5076ad18c42826ad50824288aa70b53`;
this appendix changes no Packet 1B source, assertion, fixture, seam, or wiring.

## Packet 1B Rust 1.94 formatter lineage pending review

At revision `62e2b02a2949802e4aa5b1e4daef2240ce14a70d`, an actual Rust 1.94
`make pre-push` run stopped at `cargo fmt --check` before Packet 1B API
compilation. The structured evidence identified eleven formatting hunks solely
in `src/engine/container/gated_launch_p2b_test.rs`; the raw-log SHA-256 is
`399b06bea4fc2a321e7bfda8d68a9b8119eca2180c16ae38d229fad173588e1e`.
No expected missing-API red or runtime result was reached.

The original Packet 1B test author ran only Rust 1.94's
`rustfmt 1.8.0-stable (4a4ef493e3 2026-03-02)` with edition 2021 against a
temporary copy of that one test. The source lineage is:

- pre-format test:
  `9e3f21f0242c00f7ff57e7a01a532455c5076ad18c42826ad50824288aa70b53`;
- exact formatter diff:
  `63311c7d4134d2fadc4859044810dbe48dcf3f79fd88ddd3a2b443d8dd0132dc`;
- formatted candidate:
  `b9de2b38197d21e381fa1b7a9f6d0c39c29fd2173176960f2cdf1aee0cf2ea59`;
- cumulative freeze before this appendix:
  `434a3f70a21d60dc96068291edfb08648746d6ae32f810e0276c4c7066e4272d`.

The diff changes import ordering and layout only. Assertions, fixture values,
canonical bytes and digest, production seams, Packet 1A tests, the external
legacy fixture, and Packet 1B wiring remain unchanged. This formatted candidate
and lineage appendix await Root source review before worktree application. No
compiler, test, full gate, provider, runtime, stage, commit, or push followed the
temporary formatting operation.

## Origin-only retained-custody contract amendment

Root's production source review found that `transfer(self, registry)` silently
ignored an unrelated destination registry. The durable corrective review
SHA-256 is
`fd82c9a607a5e40f2135dfb2b79a789b0eb651736547aecb3fbc06ecc10f1678`.

The amended private seam exposes only
`transfer(self) -> ProviderCliCustodyTicket`. It consumes into the retained
handle's already reserved originating registry without allocation, accepts no
caller-selected destination, and never retries spawn. The prestarted actor owns
the exact `Command` during its one native spawn attempt; no `Child`, PID, or exit
status exists until the OS returns successfully. If a caller deadline first
returns retained custody and that same in-flight attempt later fails with no
child, `RetainedProviderCliTermination::NotStarted(ProviderCliStartFailure)`
records the actually observed terminal result. It does not fabricate reaped
status or leave resolved no-child custody perpetually retained.

The formatted Packet 1B test contains no `transfer`, `retry_terminate`, or
retained-termination callsite, consistent with its explicit deferral of forced
OS spawn/kill/reap faults. It therefore remains byte-exact; no process success or
custody behavior was invented. The lineage is:

- Packet 1B test:
  `b9de2b38197d21e381fa1b7a9f6d0c39c29fd2173176960f2cdf1aee0cf2ea59`;
- preceding minimal seam:
  `1e53c09756f4e8b0dd936b847e36ea5225c4b939791a0b9ee66212b2074439ff`;
- amended minimal seam:
  `7279443236ee02d6561a8bac35d978590231b7e161728fb26e7ff089f925cf36`;
- preceding amended work item:
  `f62019d8bedb17c62a195ae4816c11d3ee7242563b007138c46ae58ed9f506ae`;
- amended work item:
  `661925fbd50a983f6602aa2279ad6140f4558010076f539088788256da24392a`;
- cumulative freeze before this appendix:
  `f5059a0bf52bc66cb5d18ea0de2762f79e08ef5535481eb22da7f6583178aa42`.

This source-only amendment awaits Root's full diff review. It changes no test,
assertion, fixture, canonical digest, build wiring, production source, or runtime
claim. No formatter, compiler, test, full gate, provider, runtime, stage, commit,
or push was run.

## Rust 1.94 fixture-only zombie-process lint amendment

The post-application Rust 1.94 gate reached Clippy and reported
`clippy::zombie_processes` at the two deliberate descendant spawns inside the
single ignored `provider_cli_fixture_child` subprocess fixture. The structured
evidence SHA-256 is
`102c41729f61f627d00f5f68e391d2b17d0c738d4bbb5745ec1105c268b51af2`.

The original independent test author added one function-scoped
`#[allow(clippy::zombie_processes)]` with a source comment explaining the exact
fixture need. Both modes intentionally let the descendant hold inherited stdout
and stderr until its natural one-second exit: one keeps the fixture parent alive,
and one lets the fixture parent exit first. Waiting in the fixture would erase
the condition exercised by the bounded drain behavior. No module/crate lint
suppression, process behavior, timeout, byte, assertion, fixture mode, or
expected value changed.

Lineage:

- preceding formatted Packet 1B test:
  `b9de2b38197d21e381fa1b7a9f6d0c39c29fd2173176960f2cdf1aee0cf2ea59`;
- fixture-only amended test:
  `047c370f3e2c94099c1af936e8361adbd0f9f54655398663d646d8e384d375ed`;
- cumulative freeze before this appendix:
  `515bd8990778b1b46fe9310debb9009137e345123965a832b52efc0049bedc55`.

This source-only amendment awaits Root's full diff review before worktree
application. No formatter, compiler, test, full gate, provider, runtime, stage,
commit, or push followed the edit.
