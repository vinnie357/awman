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
