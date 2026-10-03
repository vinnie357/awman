# Proposed upstream startup-gate extension

Altana needs an opt-in pre-agent barrier for launching a group of containers
against prepared workspaces. The proposed AWMan extension adds a strict control
directory protocol: AWMan verifies each declared guest mount and complete
content manifest inside the real container, writes a nonce-bearing readiness
record, and waits for a nonce-matched release before executing the original
agent argv.

The change is deliberately narrow. Existing launches are unchanged when the
flag is absent. Docker and Apple Containers share one wrapper path; sandbox,
ACP, API, remote, squad, and interactive frontends reject the option until they
can provide equivalent semantics. The bootstrap uses fixed embedded source and
`/usr/bin/python3 -I -S`, preserves argv boundaries, restores the original
environment only for the final exec, and fails closed on timeout or
cancellation.

Upstream feedback requested: whether the v1 control protocol and CLI names are
suitable for long-term support, and whether a later frontend-parity proposal
should expose structured readiness events through a public API.

This proposal is based on upstream `0.12.0` commit `c730732b`. Upstream
`0.12.0` itself does not provide these flags. Until upstream adopts a stable
capability surface, downstream callers can query each supported command's
existing `--help` output for `--startup-gate-control`; no new speculative API
is required.
