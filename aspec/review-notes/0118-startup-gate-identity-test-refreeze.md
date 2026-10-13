# Startup-gate identity test refreeze record

The pre-correction `tests/startup_gate_bootstrap_test.py` is SHA-256
`0221cfdcb81bc6da0c3ec52d7ad7d1006abd6a94885104c9110315b7e4e8fecf`.
Its existing `agent-user:3456` assertion expected
`initgroups("agent-user", 3456)`. That expectation contradicted the preserved
container `USER` contract: Docker and OCI both specify that supplying an
explicit group ignores supplementary group memberships.

The only prior frozen semantic assertion changed by this test-author repair is:

- removed: `initgroups("agent-user", 3456)`;
- required: `setgroups([])` before `setgid(3456)` and `setuid(1234)`.

The added `geteuid()` and `getegid()` return values are fixture support for the
new final-identity verification and do not replace or weaken any assertion.
The additive identity table covers numeric UID with both a numeric explicit
group and a resolved named explicit group. All other assertions from the
pre-correction file remain unchanged. The final corrected candidate
`tests/startup_gate_bootstrap_test.py` is SHA-256
`f65df666c51bd20aa40c9d14cf626e8b6087f251a1e875d72ba98dc1cbc79685`.

## Resolve-once identity amendment

After the second Gate 4 rejection and operator approval of the repair plan and
reset, the P1 planner source-approved the independent test author's following
mechanism amendment to the frozen file above. For a passwd-backed user with no
explicit group, the bootstrap now resolves `getgrouplist("agent-user", 2345)`
before readiness and stores `[2345, 5678]`. After release it applies that exact
stored tuple with
`setgroups([2345, 5678])`, followed by `setgid(2345)` and `setuid(1234)`.
The former post-release `initgroups("agent-user", 2345)` assertion is removed
because it would resolve mutable guest account data after readiness. Final
group membership is unchanged.

All other assertions from the prior corrected file remain frozen. The
additive regressions require production `main` to pass the one resolved
identity into the gate, require binding verification as that identity before
`ready.json`, and reject a root-only binding for an unprivileged target in a
bounded child. Existing importable unit tests may omit the identity only to
retain their in-process verification mocks. The amended and additive candidate
`tests/startup_gate_bootstrap_test.py` is SHA-256
`ebff4b92cf28528c511ad27c86ec6e76428f4a95929bdb4b54bb19c7cdf99669`.
