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
