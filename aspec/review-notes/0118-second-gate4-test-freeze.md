# Second Gate 4 regression freeze

Base revision: `3f465d31877131aaef4bad0e6d832caf41e9d3e4`.

The source-reviewed regression tests were frozen before implementation. They
cover frontend visibility, rejection of startup-gated Docker access, strict
manifest digest keys, command preflight ownership, single-attempt restart
closure, and target-identity verification. No test or CI command had been run
when these hashes were recorded.

- `src/command/dispatch/projections/raw_args.rs`:
  `97e0270b70ffabc5e6ee7c4b2f916410b9c97fec13be8d038af839a4397c820d`
- `src/command/dispatch/parsed_input.rs`:
  `201f1ebbec22a42f700b8cb3517396559dfd4b2a948cf0fcb2ab359fd817c7f6`
- `src/command/dispatch/projections/parity_test.rs`:
  `5f50334ad349be1bf9419756aae64b3c847500f20d7aa8cfb36a9225c937a0ba`
- `tests/data_layer/startup_gate.rs`:
  `63eeb044d3ff4752c25f73957e8e5548a60392eb9fa71bdde124a3c0bb5a1f14`
- `src/command/dispatch/mod.rs`:
  `914071490045d96addc5b23e111a6cfb28f97b428fa3c404b3a282bba259bccf`
- `src/engine/workflow/mod.rs`:
  `efe6b55d635be127c61cc602624201fb149c69255145fb8a0b1336f39558f61f`
- `tests/startup_gate_bootstrap_test.py`:
  `5477eeb94f61edf8bd2d54d5afeffd85b1d1b66bf075c4ed5d7e31b8096e9745`

The Python hash includes the independently approved resolve-once identity
amendment recorded in
`aspec/review-notes/0118-startup-gate-identity-test-refreeze.md`. Implementers
must not alter these assertions.

The accompanying contract and user documentation are:

- `aspec/work-items/0118-pre-agent-startup-gate.md`:
  `f34881ea8017f3eca003eb4beefc43e3399d8c741262a1066065ebbb73936713`
- `docs/03-agent-sessions.md`:
  `fb5b1616860596890648e6cbc57b371921d340d53b56e8b9e584ab24dd9554a7`

## Fixture compilation correction

The initial frozen `src/command/dispatch/mod.rs` hash was
`b37a310a93357caa56839f0358382e69813297dac63e8e75d4cd3cfef372ffd7`.
Pinned Rust compilation showed that `GenericArray` does not implement
`LowerHex` for `format!("{:x}", Sha256::digest(manifest))`. The independent
test author mechanically replaced that expression with an iteration over the
same digest bytes, formatting every byte as exactly two lowercase hexadecimal
digits and collecting the result into `String`. Fixture bytes and assertions
are unchanged. The corrected frozen hash is the one recorded above.

## Previous-step and identity-cleanup additive freeze

Immediately before this additive freeze, `src/engine/workflow/mod.rs` was
SHA-256
`af81517dc1d0c04bd2673fc4cc8bc4d6a613e226908e5af7686e935561c71e74`
and `tests/startup_gate_bootstrap_test.py` was SHA-256
`ebff4b92cf28528c511ad27c86ec6e76428f4a95929bdb4b54bb19c7cdf99669`.
Every existing assertion is preserved.

Four additive workflow regressions freeze single-attempt handling of
`CancelToPreviousStep`: the action is hidden with a reason on both ordinary
and failure boards; injected between-step and failure-handler actions are
rejected before state reset or another launch; and a bounded mid-step case
still cleans up its owned execution while preserving the previous persisted
step. The failure-handler test invokes the private handler after two explicit
steps. It tests that unit boundary and does not claim that a public
single-attempt run displays a failure board; the public path intentionally
terminates immediately after a failed step.

Five additive bootstrap regressions freeze the immutable four-field
`RuntimeIdentity` tuple record and process cleanup. Fork failure closes both
pipe ends without child operations. Parent wait failure kills and reaps the
exact child and closes both descriptors before normalizing an `OSError` to
`identity-probe`. The corresponding fork and wait interruption cases perform
the same owned cleanup and preserve `KeyboardInterrupt`.

The independent test author did not run tests, formatting, builds, or CI while
recording this additive freeze. Execution evidence belongs to the independent
execution role at the exact applied revision.

### Private failure-handler fixture compilation correction

The initial previous-step candidate `src/engine/workflow/mod.rs` was SHA-256
`1726d45562369b3664a8e9e8e7363b5918a37edf6792de47d50f5f75fa0137d5`.
Pinned Rust compilation showed that `Result::expect_err` would require the
private successful `IterationOutcome` type to implement `Debug`. The
independent test author replaced only that `expect_err` call with an explicit
match: the existing error value continues to the unchanged
`InvalidAdvanceAction` assertion, while any successful variant reaches one
static panic message without formatting the private value. Launch counts,
workflow states, board visibility, reasons, and every other assertion and
fixture remain unchanged. The corrected frozen workflow hash is the one
recorded above.


## Identity and child-cleanup additive candidate

The independent candidate was applied only after verifying that the existing
Python test file was exactly 67,262 bytes with the frozen SHA-256
`5477eeb94f61edf8bd2d54d5afeffd85b1d1b66bf075c4ed5d7e31b8096e9745` and
that the candidate began with those exact bytes. The old prefix and all its
assertions are unchanged. The resulting test file is 73,152 bytes with SHA-256
`b15e2f1e9e225d758a9a094c06c2433f3a9b822381caf4446c809eb5b72ce27e`.

Five additional tests cover identity record mutation rejection and cleanup
when fork/wait operations fail or are interrupted. These five additions are a
test candidate, not a frozen claim that the current implementation passes;
the old frozen prefix remains unchanged.

## CLI conflict error-shape fixture correction

The initial parity regression artifact had SHA-256
`5f50334ad349be1bf9419756aae64b3c847500f20d7aa8cfb36a9225c937a0ba`.
Focused execution reported one unexpected failure at the new gated
`--allow-docker` parity assertion: it expected `MutuallyExclusive`, while the
established catalogue conflict path returns `InvalidFlagValue` with the
conflicting flag and the reason `--<flag> conflicts with --<other>`.

The independent test author corrected only that error-shape assertion. It now
accepts either catalogue traversal direction while requiring the flag to be
exactly `allow-docker` or `startup-gate-control` and requiring the corresponding
reason to name both flags in the established order. The three chat, prompt, and
workflow CLI cases, Clap rejection, ungated pass cases, and every other fixture
and assertion remain unchanged. The corrected parity artifact has SHA-256
`04233d17ec0e5f624f0d4b2ba5b767426176712ad60779d92992a877b8c7c81a`.

This record makes no green claim. The correction aligns the fixture with WI 43's
pre-dispatch usage-error requirement and the source-reviewed catalogue error
contract; it does not authorize a production change.

## API flag-accessor visibility fixture correction

The API frontend fixture previously had SHA-256
`11aa48a0d2765a541d32a1ba5f1d681ab670976858910a417caaece20ae0cb0c`.
In the initial 2,686-test full-suite run, 2,676 tests passed, seven were ignored,
and three legacy API accessor fixtures failed separately because `background`,
`port`, and `workdirs` were rejected as unknown on `api start`.

Those positive fixtures contradicted the existing catalogue assertion
`api_start_flags_are_cli_only`, which requires every `api start` flag to be
CLI-only. The independent test correction preserves accessor coverage using
flags declared visible to all frontends: `squad start --background` for bool,
`squad start --port` for `u16`, including an out-of-range rejection, and repeated
`exec prompt --overlay` values for the multi-value accessor. An additive table
requires API-mode `api start` requests to reject `background`, `port`, and
`workdirs` as exact `UnknownFlag` errors with command path `api start` and the
original flag name. No flag is silently deleted.

The corrected API frontend fixture has SHA-256
`3bf3e0fe6275d9d0eef7756d90dd66900027a1c2a2e6edba2bb3cd02696e0b6d`.
Catalogue visibility, production filtering, startup, workflow, and parity
assertions are unchanged. This record makes no focused-test or full-gate green
claim. If rustfmt changes only this candidate's whitespace, the independent
fixture owner authorizes that mechanical layout change provided every token and
semantic assertion remains unchanged and the resulting hash is recorded.


## Mechanical Rust formatting amendment

The first pinned `make pre-push` attempt stopped at `cargo fmt --check` with
EXIT 2 before the later gate steps ran. Rustfmt reported three layout-only
hunks in the additive `src/engine/workflow/mod.rs` tests and one layout-only
hunk in `src/command/dispatch/projections/raw_args.rs`. The frozen workflow
file before formatting was SHA-256
`0a85502404772421aacaabc2c3bb3f9b45178077ab35b3dac742a676998f58ec`; after
formatting it is SHA-256
`f422a43040668148db748170f8a3125a40fbf8d8a12db4ae0e2118a468620b71`. The
raw-args file before formatting was SHA-256
`f33b9418ee3379cafd26b9c9b38009e08991a57077c3a897cb59bd2323debec1`; after
formatting it is SHA-256
`372502871a403f1f08a523aeab6f056d5cd0bb2adca0d9df7235116ab765d2da`.

To verify the workflow test assertions and fixtures were untouched, the three
reported formatted text blocks were replaced in-memory with their exact
pre-format layouts; the reconstructed bytes matched the complete pre-format
workflow SHA-256 exactly. The same exact reconstruction check matched the
pre-format raw-args SHA-256 after reversing its one approved production
formatting hunk. Pinned Rust 1.94 rustfmt `--check` passes for both files. No
other file was formatted by this amendment.


## API command-frontend fixture formatting amendment

After the approved API fixture correction, the focused
`frontend::api::command_frontend` test filter passed 20/20. The subsequent
pinned `make pre-push` stopped at `cargo fmt --check` with EXIT 2 before
clippy/tests. Rustfmt reported three whitespace-only layout changes in the
new API fixture assertions in `src/frontend/api/command_frontend.rs`. The
pre-format full-file SHA-256 was
`3bf3e0fe6275d9d0eef7756d90dd66900027a1c2a2e6edba2bb3cd02696e0b6d`; after
formatting it is SHA-256
`027bb5153daaf692a4174e7a5b8e3a5b4c0e3ac73b7458cc00ad193e138ee103`.
Replacing exactly the three rustfmt-formatted lines with their pre-format line
wrapping reconstructs the original full-file hash byte-for-byte. This
confirms the formatter changed no tokens, literals, control flow, fixtures, or
assertions. Pinned Rust 1.94 rustfmt check now passes for this file.
