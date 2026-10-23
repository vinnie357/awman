# Gated launch production layer disposition

This supplements concrete P1 `eb00e22c1399637294f904d6dd630399e4fb3a2a44e59ba60ea7e8740f713266` without changing its public types, contracts, authority constructors, or frozen assertions.

`tools/architecture-lint.sh` requires `src/data` to import only the data layer. The loader-owned `StartupGateControlLayout`, pinned control objects, validated request snapshot and non-secret `GatedLaunchIdentity` therefore belong in the data layer. Their implementation must not import `engine::container`.

`ContainerName` currently lives at `src/engine/container/options.rs:38`. It is a pure String newtype with `new`, `as_str` and derived Debug/Clone/PartialEq/Eq. Move this exact type and its existing behavior into an appropriate data module and re-export that same type through the existing engine options/container path. Do not introduce a second nominal type or convert between duplicate authority representations. Existing callers retain the same public import path and behavior. Engine gated-launch code imports or re-exports the data-owned identity/control types rather than defining another copy.

Provider observation, deadlines, adapter, durable launch planning and provider effects remain in the engine layer. The data loader validates and returns pinned controls and non-secret identity; it performs no provider operation. This is an implementation placement clarification, not a permission to weaken layer enforcement or add a lint exception.

Implementers must preserve existing frozen assertion bytes and review production-only prefixes of files with inline test modules. The test author may exercise the existing engine re-export path. No test, runtime or CI result is asserted by this disposition.
