# ed22 Startup fallback test freeze

Status: source-reviewed and applied at AWMan
`ed22d32e42d8b23b210f74c8d265dfb0ec84c4fe`. The additive tests have not yet
been formatted, compiled, or executed. Luna owns those steps.

## Contract and source pins

- P1 repair contract:
  `/private/tmp/awman-ed22-gate4-repair-contract.md`
- First formal Gate 4 rejection record:
  <https://github.com/vinnie357/awman/pull/1#issuecomment-5974301260>
- Original complete `src/command/startup.rs`:
  `85bc4148ab1c8ffaeb48525186afda9e0371ddac2d60705f0674ff70df835230`
- Applied test candidate before mechanical formatting:
  `e1c59e8bc1680132c7188b03835bc73655ba327d95c733a1e68cc4b7a3b29602`
- Source-reviewed patch:
  `d7e76fe74c378e52f21b4ff8cbe24d2f5e0992876c5c31c5fa2bbaf1a5f972d5`

The original 138-line production source is an exact byte prefix. Only one
`cfg(test)` module was appended.

## Frozen behavior

The five tests exercise the public `Startup::run` boundary and require:

1. default config startup for `config show` uses detected Docker;
2. on Linux, configured unavailable Apple Containers for `config show` reuses
   the detected Docker fallback;
3. unknown runtime remains fatal for a CLI invocation;
4. unknown runtime for a bare TUI retains inert Docker engines and the fatal
   modal text; and
5. on Linux, a runtime-required `status` invocation still rejects unavailable
   Apple Containers.

`Startup::run` calls migration helpers which read process HOME and legacy
environment values. Each assertion body therefore runs as the sole exact test
inside a bounded child test process. The child alone receives a private
HOME/USERPROFILE and has all six legacy AMUX variables removed. Entry and
completion files prevent filter mismatch, skip, or early return from certifying
the assertions. The parent environment and current directory are unchanged.

## Awaiting Luna evidence

Luna should first run Rust formatting limited to the additive test region and
return the exact diff/hash for independent refreeze. Then, on Linux with pinned
Rust 1.94, it should run the five exact Startup tests. The unavailable-Apple
ordinary-config case must produce the expected behavioral red at this source;
the other four cases are compatibility baselines. A Darwin skip is not runtime
evidence. Full pre-push follows only after the independently authored repair.

The separate generated-project Python dependency probe is not established by
these unit tests and requires its own actual-image execution record.

## Linux startup regression evidence

The frozen additive suffix remains byte-identical at SHA-256
`6c33b4c60d93ebb6a1ae80c26f4cc4e2b59b54ee5204c576f8e028ab682c0a20`.
The original complete independent test candidate was
`e1c59e8bc1680132c7188b03835bc73655ba327d95c733a1e68cc4b7a3b29602`;
the current complete `src/command/startup.rs`, including the separately authored
production repair, is
`109ed45e6cb08a847547748bb46f35f7509b7aee3be73fae1fec46f59f9e5793`.
No test assertion changed between the behavioral red and fixed execution.

The pinned Linux baseline passed four compatibility cases and produced the one
expected ordinary-config fallback behavior red. Its execution log SHA-256 was
`bb4022056c53d73623e1584979d4f304adde6a23765ecc6c0c94ab1ac2302b72`.
After the production repair, the same five cases passed under pinned Rust 1.94
in the isolated Linux source environment. The structured execution record is
`/private/tmp/awman-linux-startup-fixed-evidence.md`, SHA-256
`3e9ba7e3e750b059cea0c715bce436923beddea1f53de26abeed9857a55fa83e`.
That record is Linux startup behavior evidence for these five public
`Startup::run` cases; it is not Darwin or full native-runtime acceptance.

## Generated-project dependency evidence

The generated-image dependency check is separate from the five startup unit
cases. Both executions used frozen bootstrap SHA-256
`837bb45dec306300fa75f217863a8bc995c42d96a0e0912b9954e6d33d62916a`
and source-reviewed helper SHA-256
`428459ee1e5a97ae737694bb0135ad0fc597c174ef33cb8612b6736aba784539`.
The helper invokes no model or provider and accepts a dependency verdict only
after the unique local tag, actual named probe container image descriptor and
post-probe tag all match the same built-image digest.

Before the template dependency repair, the unmodified generated project built
successfully and the actual image probe failed with
`ModuleNotFoundError: No module named 'json'`. The built/probed descriptor was
`sha256:3f68557f09d24d184d9fba6debcd42b7083f4e6b64456c8cca6748e32a2e6aed`.
The helper's structured classification was the expected missing-standard-library
dependency red. The separately recorded outer helper exit was `1`, but the
helper's reviewed return predicate accepts that classification when its image,
cleanup and timeout conditions hold. Its cause is therefore unresolved pending
further diagnosis; it is not attributed to the probe exit. Luna's correction
records the outer command exit and structured probe exit as separate
observations without rerunning the helper:
`/private/tmp/awman-generated-project-dependency-evidence-correction.md`,
SHA-256
`b212864cf20e1c80b0f11ac1cc670648e04c27f36b675347b751ef18cbda645f`.
The causal sentence in the earlier evidence document is explicitly superseded
by this source disposition while its raw structured fields and artifact pins
remain provenance. That earlier record is
`/private/tmp/awman-generated-project-dependency-baseline.md`, SHA-256
`c35900c1076efa958698448645b531ed428103e6eef85f8dedec3fdd3beca972`.
Its retained result JSON SHA-256 is
`0f79cfcf4730355a76f0074312d17a61a6afc648a2811790aa90ed850106c60e`.

After the template repair, the same frozen bootstrap probe built and passed
against actual image descriptor
`sha256:a6854a845dc9d7a0a8d78afdf0cf048f88f5ee893765de891cfcc6679b866260`.
The helper, build and probe all returned `0`; the actual container identity and
post-probe tag matched, and exact named-container/image cleanup was confirmed.
The complete structured record is
`/private/tmp/awman-generated-project-dependency-fixed-evidence.md`, SHA-256
`46b46233eba2b7da70abf432a30e0e43b9cd7209ead59f7300afb62b5428c3f7`.
Its retained result JSON SHA-256 is
`ebb29ed65f5d2a7fcaa6db594fc8855c710a395c1b3df1a489ed96379433f00e`.

Earlier COPY-context failures, the Apple 16 KiB Dockerfile limit and the bare
digest remote-resolution 401 remain classified as unexpected infrastructure or
fixture-path evidence. They are not reused as dependency reds and are not
silently converted into acceptance. These records establish the Linux five-case
startup repair and generated-image bootstrap imports only. They do not establish
full native runtime, provider, model, mount, gate, or multi-member acceptance.

## Rust 1.94 test-format refreeze

Pinned Rust 1.94 formatting later produced four test-only whitespace reflows in
`src/command/startup.rs`, at the isolated-child entry lookup, child-status
assertion, completion write, and unknown-runtime exact-test constant. The
independent test author reviewed the exact formatter delta and applied only
those four hunks. No assertion token, literal, fixture action, production line,
or behavioral requirement changed.

The formatter-supplied test-only delta is retained at
`/private/tmp/awman-startup-fmt194-tests-only.diff`, SHA-256
`ac10d7188361409bdaf2ca170e90c821a0d8af90fe8cd4d78cd31df19288fe44`.
Its normalized applicable patch is
`/private/tmp/awman-startup-fmt194-tests-only.normalized.diff`, SHA-256
`8548ee6db195cef6f30bd8f5b589ab553d83921d4c3d7b4167a3e85885e3813c`.
The refrozen complete `cfg(test)` suffix is SHA-256
`56c378a1daba2a0d1eae7de40347db113fe3b12faa965ca65e9a1fede840f43e`.
The complete `src/command/startup.rs`, including the separately authored
production repair and these test-only reflows, is SHA-256
`cb296627630f3dee6d1fdd40df6ecd9327b3e5f13617df884974b3979e7006ed`.

The Linux five-case evidence above remains evidence for the earlier frozen test
suffix `6c33b4c60d93ebb6a1ae80c26f4cc4e2b59b54ee5204c576f8e028ab682c0a20`
and complete source snapshot
`109ed45e6cb08a847547748bb46f35f7509b7aee3be73fae1fec46f59f9e5793`.
It is not relabeled as execution evidence for the mechanically formatted
snapshot. A fresh Rust 1.94 gate supplies that later execution evidence.
