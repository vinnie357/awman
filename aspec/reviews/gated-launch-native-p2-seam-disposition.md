# Gated native launch Packet 1A seam disposition

This disposition supplements the immutable concrete P1 lineage at SHA-256
`eb00e22c1399637294f904d6dd630399e4fb3a2a44e59ba60ea7e8740f713266`.
It narrows only the production-used seams needed to exercise the concrete P1;
it grants no new authority constructor or path-based cleanup authority.

## Approved Apple parsing seams

The Apple provider module may keep these parsing functions private to the
module and its child test module:

```rust
fn parse_gated_image_inspection(
    bytes: &[u8],
) -> Result<ImmutableImageId, AppleProviderError>;

fn parse_gated_launch_inspection(
    bytes: &[u8],
    key: &ProviderLaunchKey,
) -> ExactInspection;
```

These functions parse actual bounded provider output. Parsing never grants
launch, stop, remove, cleanup, or control-directory authority.

## Approved fixed-name predicate

The startup-gate data module may keep this validation predicate private to the
module and its child test module:

```rust
fn is_reserved_gate_name(name: &str) -> bool;
```

The loader uses this predicate to reject manifest-controlled claims on every
fixed parent or child basename. It creates no identity or authority and does
not expose a path. Packet 1A checks the complete fixed-name set directly and
also drives a digest-valid `bootstrap.py` manifest claim through the real
loader so the integration case cannot pass because a claimed file is absent
or malformed.

## Approved opaque-layout borrow

The startup-gate layout may expose this crate-private borrowing accessor:

```rust
pub(crate) fn orchestrated_parts(
    &self,
) -> Option<(
    &Arc<OrchestratedControlAuthority>,
    &GatedLaunchIdentity,
)>;
```

The accessor returns only an existing validated opaque authority and the
non-secret identity derived by the loader. A caller may clone that `Arc` and
identity. It cannot reconstruct authority, mint identity, or receive a raw
token or path tuple. The raw launch token is zeroized before the loader
returns. Every pre-effect and post-effect held/current pin check required by
the concrete P1 remains mandatory. Returned values cannot be converted into
path-based cleanup authority.

An interim proposal to return borrowed paths and `RawLaunchToken` was
withdrawn after re-reading concrete P1 sections 4 and 5. This final shape
replaces that proposal.
