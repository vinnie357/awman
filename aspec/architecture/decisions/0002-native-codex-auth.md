# Contained native Codex authentication

Status: Proposed; accepted on merge. Related: [Altana #230](https://github.com/vinnie357/altana/issues/230).
Basis: [accepted ADR 0001](https://github.com/vinnie357/awman/blob/0f26a8cc5b444161308ca88032557648f20dc3fe/aspec/architecture/decisions/0001-native-sdk-run-state.md) and [merged strict storage](https://github.com/vinnie357/awman/commit/8fc8cddc49dd00fcbd8eef904c002aeb9123c623).

## Why

An SDK owner needs isolated subscriber authentication before its first model execution.
Authentication success establishes preparation, not execution retirement or permission to publish run completion.
Automatic login during resume could silently change the account behind an existing conversation.

## Decision

Select Codex as the first connected production tracer; Claude and Agy remain in scope through later qualified profiles.
Apply the [accepted security rule](../security.md): Codex runs inside AWMan-managed containers; the host never invokes Codex.
Scope this decision to the AWMan-managed Apple native profile; it opens no Docker native route.
Use two connected Apple launches for a fresh owner: contained authentication, then the main execution.
Both reuse the protected control, READY, child-custody, retention, and exact-cleanup lifecycle.
The preparation custody and one-time lease handoff require implementation; existing lifecycle mechanisms alone do not prove this connection.

The fixed guest SDK home is `/data/awman-native-sdk/codex`.
Its sole RW host source is the validated canonical `state_path()` of the whole strict lease.
Only this exact leased directory is mounted as private SDK state; protected control bindings retain their existing contract.
Owner records, active/completed records, and the writer lock stay host-only outside that mount.
Strict activation must be durable before owner-specific SDK config/auth/settings access or writes, staging, mounts, or child launch.
Pure caller/profile eligibility checks may precede acquisition without owner-material access or writes.
Fresh owners receive an exclusively created minimal `config.toml` containing `cli_auth_credentials_store = "file"`; never overwrite existing config.
Do not copy operator home, settings, history, or credentials into the owner or either launch.
Existing workspace selection rules and Claude's refresh-token-free credential-file rule remain unchanged.
Do not inherit an alternate SDK home, credential source, API-key billing, or federation fallback.

### Contained authentication and handoff

A dedicated qualified guest launch runs only `codex login --device-auth` with guest `CODEX_HOME` fixed to the SDK home.
It receives neither the model question nor a review-source mount.
Preparation retains the original whole lease across the actual auth child, required readers, resources, and exact auth-container cleanup.
Forward login URLs and device challenges only through the user's interactive channel, never outer QA helpers or diagnostic logs.
Use actual provider expiration or user cancellation; an arbitrary wall-clock authentication timeout grants no retirement or retry authority.
Initial authentication retry requires actual child reap, clean required readers, resource release, and exact cleanup.
Retry retains the same live preparation lease and activation; it never replays the model question.
Unknown spawn, reader, resource, or cleanup custody retains that lease and unmatched activation through existing recovery custody.
Do not infer noncreation, clean up by name, or abandon a live auth helper through a terminal success/drop claim.
Abandonment leaves the unmatched run closed to resume.

Pure caller/profile eligibility may reject before acquisition without owner-specific SDK material access or writes.
Otherwise missing auth requires live owned `AUTH_REQUIRED` custody until the user completes or cancels and retirement is established.
Auth success advances preparation only; it never publishes a session association or completion.
After all auth retirement and exact cleanup predicates succeed, transfer the lease once to main `NativeSdkExecution` custody.
Then build and revalidate the main manifest and launch review through its own protected lifecycle.
Auth READY binds the exact SDK RW state; main READY additionally binds the selected review workspace RO.
Disable the generic `.codex` overlay for this route; neither stage broadens workspace mounts.

### Account continuity and qualified profile

The user explicitly chooses the account during first-owner authentication.
Resume uses only that owner's existing auth store and normal provider refresh.
Never automatically relogin, reseed credentials, switch account sources, or change billing mode during resume.
Missing or rejected resume auth returns actionable recovery-required state and retains the unmatched intent.
Reauthentication that could change an existing conversation's account requires a separate explicit recovery contract.
Add no durable account record, account-verification claim, or new state framework.
The pinned CLI's login status reports auth mode, not account identity; normal refresh has internal stored-account checks.

Qualify the guest image and exact CLI version before accepting either launch profile.
The profile explicitly checks SDK/config/auth selectors and fixes its environment and argv; unsupported combinations reject eligibility.
Check `CODEX_HOME`, `CODEX_SQLITE_HOME`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`, and provider/config/issuer overrides.
Check federation selectors `OPENAI_FEDERATION_RULE_ID`, `OPENAI_IDENTITY_TOKEN_FILE`, and `OPENAI_WORKLOAD_IDENTITY_CONTEXT`.
Trace [declared routing/client overrides](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/login/src/auth/manager.rs#L195-L202) before allowing them: `CODEX_REFRESH_TOKEN_URL_OVERRIDE`, `CODEX_REVOKE_TOKEN_URL_OVERRIDE`, and `CODEX_APP_SERVER_LOGIN_CLIENT_ID`.
Also trace [`OPENAI_API_KEY` readers](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/login/src/auth/manager.rs#L890-L905); a declaration alone does not establish effective Codex exec auth selection.
No unqualified selector may redirect owner state, select another credential source, or enable fallback billing/federation.
The source-supported `CODEX_ACCESS_TOKEN` path is PAT/agent-identity auth, not a generic subscriber OAuth-token handoff.

Main `NativeSdkExecution` alone authorizes completion.
Require actual successful wait, valid ACK, clean required readers, resource release, and exact Apple cleanup.
After visible completion uncertainty, retain `CompletedPublicationUncertain` custody and reconcile exact proof under ADR 0001 without repeating execution.
Existing production gates stay closed until the connected route and actual isolated two-turn qualification pass.
This decision supplies no implementation, auth invocation, runtime proof, or production/provider qualification.

## Acceptance

- Exercise the actual first and resume production entry paths against one physical owner and exact session.
- Prove activation precedes owner-specific SDK config/auth/settings access, staging, mounts, and both child launches; failed activation starts neither stage.
- Verify exclusive file-store config, auth-selector isolation, SDK-only RW mapping, workspace RO, and protected READY bindings.
- Exercise interactive auth, cancellation/expiration, required-reader/resource failure, and unknown custody without lease loss or premature success.
- Prove retry requires actual auth retirement and exact cleanup; resume never automatically logs in or changes account source.
- Exercise auth/main READY or cleanup failures: retain unmatched intent and exact custody; publish no unauthorized completion.
- Qualify two real Codex turns with exact ACK/session continuity, retained SDK mutations, actual waits/readers/resources, and exact cleanup.
- Preserve typed completion uncertainty and reconciliation; auth success alone cannot publish session or completion.

## Pinned public source facts

Codex [`rust-v0.162.1` CLI parsing/dispatch](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/cli/src/main.rs) and [device-login implementation](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/cli/src/login.rs#L300-L345) support `login --device-auth`.
The [config field](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/config/src/config_toml.rs#L261-L266) supports the explicit file credential store.
The [auth resolver](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/login/src/auth/manager.rs#L1394-L1497), [refresh checks](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/login/src/auth/manager.rs#L2666-L2703), and [login status](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/cli/src/login.rs#L421-L483) bound the account/auth claims above.
[Home selection](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/config/mod.rs#L4698-L4708), [SQLite selection](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/config/src/config_toml.rs#L335-L337), and [federation configuration](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/login/src/auth/workload_identity.rs#L226-L243) with its [literal declarations](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/protocol/src/shell_environment.rs#L4-L18) require explicit profile checks.
These are tagged-source facts, not qualification of an installed binary or guest image.
