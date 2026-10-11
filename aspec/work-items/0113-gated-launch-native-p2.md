# Work Item: Gated native launch identity and custody

## Summary

Add independent adversarial P2 for AWMan's orchestrated Apple Containers launch profile. The tests freeze the exact host-parent/guest-child authority, secret parsing, provider identity, durable pre-spawn plan, child custody, protected receipt/release, exact cleanup, retention, and legacy compatibility contracts before production implementation.

Authoritative source packets:

- Concrete addendum SHA-256 `eb00e22c1399637294f904d6dd630399e4fb3a2a44e59ba60ea7e8740f713266`.
- Identity packet SHA-256 `ead26429abf89f0dab24cbe40421bbd93929105b5cb74e72bf586ffaea6f132e`.
- Split-control amendment SHA-256 `76eacc8614a4e1a18ce782b3c5f3cb37ba8b997452ad717ef8d88dd2e26b4b4b`.
- Apple stopped-schema evidence SHA-256 `bd7f4ebc8853b83f54c2207f6a5c0387466a2908cc108a6257d4b58cd111872e`.

This work item does not authorize a native implementation. The P2 author and production implementer must remain separate. Existing startup/bootstrap assertions remain frozen and unchanged.

## User story

As an orchestrator, I want AWMan to launch only the exact Apple container authorized by Store-owned pinned control directories, so an ambiguous create result, substituted control, foreign same-name object, failed receipt, or incomplete cleanup is retained for recovery rather than adopted, retried, or destroyed.

## Required P2 matrix

### A. Intention and pinned controls

1. A valid exact three-key, version-1 `launch-intent.json` loads from the held mode-0700 host parent, derives the domain-separated token digest, and returns no raw token owner.
2. The bounded reader accepts at most 512 bytes and rejects byte 513 before Serde; non-ASCII, BOM, NUL, backslash/escapes, duplicate/unknown keys, non-integer version, malformed names, non-lowercase token, and trailing non-whitespace fail with fixed non-secret diagnostics.
3. Returned/debug/error/argv/environment/overlay/bootstrap bytes contain no raw token, parent path, or token digest where the contract redacts it.
4. The literal `guest-control` child must be distinct, current-user owned, exact mode 0700, held/current identical, and opened no-follow relative to the held parent. Missing, link, file, broad mode, replacement, or alias fails before provider work.
5. Reserved gate names cannot be claimed by a manifest or binding-controlled control entry.
6. Orchestrated staging contains one read-only bootstrap overlay and one deferred revalidated RW child mount. No parent, intention, plan, receipt, request, or manifest is copied into the writable child.
7. Missing intention preserves the legacy one-directory RW mount and version-1 guest behavior.

### B. Provider identity and bounded calls

8. Apple image inspection accepts only `configuration.descriptor.digest` normalized as lowercase `sha256:<hex64>`.
9. Apple container inspection requires matching top-level and `configuration.id`, exact reserved label digest, `configuration.image.descriptor.digest`, a valid UTC `configuration.creationDate` at or after the key's whole-second floored lower bound, and recognized `status.state` (`running` or evidenced `stopped`). Missing, duplicate, malformed, mismatched, or unknown state never yields a matching inspection.
10. Docker normalization compares image `.Id` with container `.Image`; image references are never immutable identity.
11. Canonical inspection revisions contain only the fixed normalized tuple and a digest for unrecognized bounded state, never raw provider JSON.
12. Every image, absence, tuple, stop, and remove operation receives a fresh absolute deadline bounded by both the enclosing deadline and ten seconds. Provider subprocess stdout/stderr is bounded; timeout kills and reaps the subprocess.

### C. Planning and spawn boundary

13. Final image resolution and exact first absence happen before durable plan publication.
14. `launch-plan.json` is exact seven-key compact canonical JSON plus one LF, private 0600, synced, no-replace, parent-synced, and contains the token digest but never the raw token.
15. Existing valid, invalid, unsafe, unreadable, or colliding plan returns retained recovery and performs no provider spawn.
16. A second control-pin validation and exact absence happen immediately before each real spawn. Any present, ambiguous, unavailable, or substituted state starts no child.
17. PTY, one-shot piped, and persistent-piped representations each spawn at most once and transfer the sole child owner to the prestarted lifecycle actor before bridge work.
18. Actor thread startup failure occurs before provider spawn. Bind channel failure returns the exact unbound raw child. Post-bind bridge failure returns managed lifecycle custody.
19. Post-spawn absence is never noncreation. Only a proven no-child local spawn failure or a reaped allowlisted name-collision exit is `NotCreated`; all other uncertain/nonmatching outcomes retain without retry.

### D. Protected readiness and credentials

20. READY/failure/release/`.released` are fixed child-relative reads; request, plan, receipt, and launch failure are fixed parent-relative operations.
21. A strict guest READY plus two matching provider inspections is required before atomic private `launch.json` publication. Guest READY alone is not protected readiness.
22. Receipt publication or post-publication pin uncertainty retains. A matching pre-release failure appends `launch-failure.json` without replacing the receipt.
23. Unsafe control replacement prevents failure publication through either the old path or replacement object, but still fails the in-process supervisor and AWMan process.
24. The receipt stays provisional until a valid child `.released`. AWMan process exit before release, including exit zero and no failure file, invalidates it.
25. Credential leases carry the validated request snapshot and both pins, stay pending through provisional receipt, and enable refresh only after matching `.released`.

### E. Cleanup and retention

26. Initial exact absence returns absence evidence only, never trusted stop evidence and never retry authority.
27. Matching cleanup re-inspects the complete tuple immediately before stop and remove, uses a fresh bounded deadline for each operation, requires strict stopped state, obtains the actor's actual reap result, removes, and proves final absence.
28. Foreign, ambiguous, unavailable, tuple change, timeout, stop/remove/reap failure, unknown stopped state, or still-present object retains and performs no later destructive step.
29. Grace expiry, explicit cancel, cancel handle, normal completion, supervisor recovery, and legacy wait share one lifecycle authority; no second raw-child waiter exists.
30. Retention insertion is synchronous before outward error. Engine/runtime/command bundles share one registry Arc; retained executions hold only weak registry backreferences.
31. Registry construction failure occurs before launch. Poison recovery preserves ownership. No registry or lifecycle mutex is held across wait/termination/join.
32. Last-owner Drop is bounded, never self-joins, detaches custody at deadline, and never calls provider inspect/stop/remove or fabricates cleanup/trusted-stop evidence.

### F. Compatibility and unsupported modes

33. Orchestrated mode rejects Docker, ACP, sandbox, `allow_docker`, caller name, reserved-label override, duplicate name, and a caller request for `remove_on_exit == true` before agent setup/image build; after validation it forces the trusted `KeepContainer` setting.
34. Trusted injection adds exactly one validated name and one lowercase reserved digest label after caller validation and forces `KeepContainer`.
35. Legacy gated and ungated Docker/Apple behavior, schemas, name generation, and current startup bootstrap remain unchanged.

## Packet sequence

- Packet 1A freezes the directly observable portions of cases 1–9 and 13–15 plus deadline construction from case 12: strict intent/control topology, Apple identity parsing, and durable plan/no-respawn behavior through the actual loader, stager, parser, adapter, and planning seams. Its source review also pins the single zeroizing allocation and held-parent sync/no-replace mechanisms that a returned filesystem state cannot observe directly; later execution evidence must still exercise the approved implementation and fault paths.
- Packet 1B completes cases 10–12 and 16: Docker identity normalization, canonical observation revisions, bounded external CLI kill/reap, and the immediate pre-spawn absence/control barrier.
- Packet 2 freezes cases 17–19 and 29–32: all child representations, sole ownership, retention registry, and bounded detached custody.
- Packet 3 freezes cases 20–28 and 33–35: protected receipt/failure/release, credentials, exact cleanup, option restrictions, and compatibility.

No packet alone proves release readiness. Runtime proof is delegated after all frozen packets receive source review and production reaches a single exact revision.

## Documentation

This work item is internal protocol verification. User-facing documentation changes belong to the later complete gated-launch feature, not this P2 packet.
