//! Layer-0 path helpers for `amux claws`.
//!
//! Claws stores its per-repo state under `<HOME>/.amux/claws/<repo-hash>/`.
//! This module produces the canonical paths so Layer 1 / Layer 2 callers do
//! not need to hard-code the layout.

use std::path::{Path, PathBuf};

use crate::data::image_tags::repo_hash;

/// Resolve the claws data root: `<HOME>/.amux/claws`.
pub fn claws_root(home: &Path) -> PathBuf {
    home.join(".amux").join("claws")
}

/// Per-repo claws clone path: `<HOME>/.amux/claws/<repo-hash>`.
pub fn claws_clone_path(home: &Path, git_root: &Path) -> PathBuf {
    claws_root(home).join(repo_hash(git_root))
}

/// Per-repo claws config path: `<HOME>/.amux/claws/<repo-hash>/config.json`.
pub fn claws_config_path(home: &Path, git_root: &Path) -> PathBuf {
    claws_clone_path(home, git_root).join("config.json")
}

/// Per-repo claws controller container name.
pub fn claws_controller_name(git_root: &Path) -> String {
    format!("amux-claws-{}", repo_hash(git_root))
}

/// Per-repo claws controller image tag.
pub fn claws_image_tag(git_root: &Path) -> String {
    format!("amux-claws-{}:latest", repo_hash(git_root))
}
