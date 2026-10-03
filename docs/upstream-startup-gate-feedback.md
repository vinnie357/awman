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

## Ownership across mount namespaces

The opt-in startup gate exposed a concrete ownership mismatch on Apple
Container mounts. A control directory owned by host UID:GID `501:0` appeared
inside the guest as `0:0`. Passing the host numeric owner to the guest bootstrap
caused the correctly mounted, mode-protected `release.json` to fail with
`unexpected file owner`.

The approved correction treats host and guest ownership as separate
assertions:

- The host validates the caller-owned control directory and all host-observed
  handshake files against the current host user.
- The guest opens the actual mounted control directory once with no-follow
  directory semantics, validates mode `0700`, and derives its expected
  guest-visible UID/GID from that held descriptor.
- Guest handshake files are bounded, regular, single-link, mode-`0600`,
  same-owner objects opened and mutated relative to the held directory
  descriptor.
- No host numeric owner is accepted as guest authority.
- Exact schemas, nonce matching, binding and container identity,
  timeout/cancellation behavior, and the post-release nonroot agent exec remain
  unchanged.

This addresses namespace remapping without weakening either side. The host
still proves it is reading the caller-approved directory; the guest proves it
is operating on the mounted directory it actually received.
Descriptor-relative operations also prevent a later pathname swap from
redirecting ready, release, receipt, or failure handling.

Regression coverage includes differing host/guest numeric owners, wrong guest
UID or GID and mode, links and special files, inode replacement, control-path
replacement after open, wrong nonce, preservation of host-side exact handshake
checks, and the post-release unprivileged exec ordering.
