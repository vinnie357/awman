# Packet 1A safe syscall amendment

Status: approved source-seam amendment for the Packet 1A production candidate.

Authority:

- concrete gated-launch P1 SHA-256
  `eb00e22c1399637294f904d6dd630399e4fb3a2a44e59ba60ea7e8740f713266`;
- identity P1 SHA-256
  `ead26429abf89f0dab24cbe40421bbd93929105b5cb74e72bf586ffaea6f132e`;
- Packet 1A production review
  `/private/tmp/awman-native-packet1a-production-root-review.md`;
- Luna Rust 1.94 execution log
  `/private/tmp/awman-packet1a-production-final-gate.log`, SHA-256
  `3e5fc147a430d331091db8d0ea453cf1e41c7ac9b45ba8ffb6f0c6cbe53270e5`;
- separate structured execution evidence
  `/private/tmp/awman-packet1a-production-final-gate-evidence.json`.

The crate retains `#![forbid(unsafe_code)]`. Packet 1A uses safe filesystem
wrappers and does not add an unsafe allowlist or an overwrite-capable rename
fallback.

## Approved APIs

The local primary sources for the resolved dependencies establish these APIs:

- `<cargo-registry>/nix-0.31.3/src/fcntl.rs:274-285`: `openat` accepts an
  `AsFd` directory and returns `OwnedFd`;
- `<cargo-registry>/nix-0.31.3/src/sys/stat.rs:248-268`: `fstatat` accepts an
  `AsFd` directory and `AtFlags`;
- `<cargo-registry>/nix-0.31.3/src/unistd.rs:1643-1655`: `unlinkat` accepts an
  `AsFd` directory and typed removal flags;
- `<cargo-registry>/nix-0.31.3/src/unistd.rs:1770-1773`: `geteuid` is a safe
  wrapper;
- `<cargo-registry>/rustix-1.1.4/src/fs/at.rs:282-309`:
  `renameat_with` accepts two `AsFd` directories and typed `RenameFlags` on
  Apple and Linux;
- `<cargo-registry>/rustix-1.1.4/src/backend/libc/fs/types.rs:518-523`: on
  Apple, `RenameFlags::NOREPLACE` maps to `RENAME_EXCL`;
- `<cargo-registry>/rustix-1.1.4/src/backend/libc/fs/syscalls.rs:595-626`: the
  Apple wrapper returns `NOSYS` when `renameatx_np` is unavailable and does not
  fall back to ordinary rename for flagged directory-relative calls.

`rustix = { version = "1.1", features = ["fs"] }` is a direct Unix-target
dependency. The resolved local version is 1.1.4. Its declared minimum Rust
version is 1.63; nix 0.31.3 declares 1.69. Both are compatible with the
project's pinned Rust 1.94 toolchain.

The implementation converts `OwnedFd` to `std::fs::File` through the safe
standard-library `From<OwnedFd>` implementation. Atomic publication uses only
`renameat_with(..., RenameFlags::NOREPLACE)`. `EXIST` maps to retained recovery;
every other failure maps to publication failure. Unsupported platforms retain
the fixed failure and never use check-then-rename or ordinary rename.

## Platform boundary

The held-handle implementation remains a Unix production seam. The portable
request/spec/layout/error contract and cfg platform selector live in
`src/data/startup_gate_native.rs`. Unix metadata, descriptor, nix, libc, and
rustix code lives only in `src/data/startup_gate_native_unix.rs`. The non-Unix
module `src/data/startup_gate_native_unsupported.rs` returns
`UnsupportedPlatform` from loading and revalidation.

Non-Unix pinned authority owners contain a private `std::convert::Infallible`
field and expose no constructor or deserializer. The crate-private
orchestrated-layout constructor is compiled only on Unix. Engine signatures
therefore remain type-correct without creating a value that could claim a
control pin, mount identity, launch plan, revalidation, or successful
publication on an unsupported platform. Plan operations have fixed failure
results and plan verification returns false.

A source-only spot check found other Unix integrations behind explicit target
guards, including `src/engine/container/attach_socket.rs:133-134`
(`#[cfg(unix)] mod unix_impl`) and
`src/data/fs/daemon_process.rs:287-293` (separate Unix and non-Unix process
liveness functions). No broader baseline Windows conclusion is inferred from
that spot check. A whole-repository cross-target compiler gate remains required
before any Windows-support claim. The portable split restores the startup-gate
module's intended `UnsupportedPlatform` behavior; it does not by itself prove
broader Windows support. No non-Unix path may gain orchestrated authority
through this amendment.
