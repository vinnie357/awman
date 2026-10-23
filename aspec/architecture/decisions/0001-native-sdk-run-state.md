# Durable native SDK run state

Status: Proposed; accepted on merge. Related: [native SDK continuity](https://github.com/vinnie357/altana/issues/230).

## Why

A native session association identifies an SDK state owner, but cannot prove its latest execution retired safely.
An interrupted resumed run can leave an older valid association beside uncertain SDK mutations.
Association rename can also succeed before a later sync or validation reports failure.
We need durable evidence of the latest run without a journal, PID inference, or time-based expiry.

## Decision

Store two fixed private files beside the existing owner record: `active-run.json` and `completed-run.json`.
Both use schema version 1, the existing bounded record size, and existing no-follow, ownership and permission checks.
They reuse the exact `OwnerRecord`, including its generation, project/provider/model key and physical identity pins.
Do not introduce a second owner generation or relax existing state and association validation.

An activation contains `schema_version`, a new UUID `run_id`, `owner`, and `requested_session`.
`requested_session` is null for a first turn and the exact acquired session ID for a resume.
A completion repeats that activation and includes the actual acknowledged `session_id` associated with this owner.
A resumed acknowledgement must equal the requested session ID.
Reject unknown fields, malformed or oversized records, foreign owners, and unsafe or exchanged files and directories.

Hold the validated OS writer lock before reading or replacing either record.
Production resume acquisition requires the latest activation, its matching completion, and the exact session association.
Match the complete owner, run ID, requested session and acknowledged session under the held, revalidated lock.
A missing or unmatched record rejects resume; completion for run A cannot authorize activation B.
There is no PID, TTL, legacy-record, or missing-record fallback in production.

For every first or resumed run, atomically replace and sync `active-run.json` with a fresh run ID.
Complete its durability and pin checks before staging settings, mounting state, or starting execution.
Keep the prior completion until the current run completes; do not delete the active record.
Hold the lease and its writer lock through actual execution and resource retirement.
An execution, reader, or cleanup uncertainty leaves the activation unmatched and prevents resume.

`NativeSdkExecution` alone authorizes completion after actual successful child wait, valid native acknowledgement,
clean required readers, actual resource release, and exact backend cleanup establish retirement.
It publishes or validates the exact session association before atomically publishing the matching completion.
The crate-visible storage API trusts this caller boundary; storage cannot establish backend retirement itself.
Do not replace that boundary with a caller-supplied cleanup boolean or a new user-facing state framework.

Finalization returns `Completed` after publication durability and final identity checks succeed.
Failures before completion becomes visible return normal errors and preserve the active record.
If association rename succeeds but later association validation or sync fails, do not publish completion.
The visible association then has an unmatched activation, so production resume rejects it.

If completion rename becomes visible but a later sync or validation fails, return `CompletedPublicationUncertain`.
Retain the lease and distinguish publication durability uncertainty from unknown execution or cleanup safety.
Do not report rollback, delete evidence, or repeat execution to resolve publication uncertainty.
Reconciliation uses the retained validated lock and rereads the exact activation, completion, association and physical pins.
It may return `Completed` only after matching records, required syncs and final revalidation succeed.
Missing, changed, malformed or unsafe evidence remains rejected and retained for explicit recovery.

After restart, a valid visible matching completion may authorize resume under the acquired validated lock.
This is safe because only actual retired execution can emit that completion, even if its last durability acknowledgement failed.
A lost completion leaves an unmatched activation and quarantines the state instead of guessing that execution completed.

## Consequences

Two fixed bounded records avoid an unbounded journal and preserve the latest uncertain run across host interruption.
Conservative rejection can require explicit recovery even when execution retired safely but completion was lost.
Existing test-only association APIs without activation remain frozen compatibility coverage; production must always activate.
Storage tests prove storage ordering and validation, never actual child, reader or container cleanup.
The current prototype's zero exit and reader drain do not prove exact Apple backend cleanup.
Production continuity stays closed until isolated seed/mount/control, READY and exact cleanup gates are complete.
This decision records the protocol; it does not claim implementation, QA or managed execution qualification.

## Acceptance

- Two clean rounds advance activation A/completion A to activation B/completion B, resuming the same physical SDK state.
- Interrupted resume leaves B unmatched; older completion A and an existing association cannot authorize another resume.
- Failure after association rename preserves the association and unmatched activation, rejects resume, and retains state.
- Failure after visible completion rename returns `CompletedPublicationUncertain` and retains the lease without execution retry.
- Reconciliation checks exact records and pins; a matching synced completion resolves, while missing or changed evidence rejects.
- Restart accepts valid matching visible completion; lost completion quarantines the activation.
- Foreign generations, project/provider/model or session mismatch, exchanged pins, unsafe files and malformed records reject before staging.
