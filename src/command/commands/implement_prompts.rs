//! Prompt templates for `ImplementCommand`. The literal string is preserved
//! from `oldsrc/commands/implement.rs` so user-visible prompts remain stable.

/// Default single-step prompt used when `--workflow` is omitted.
/// `{{work_item_number}}` is substituted at command-build time.
pub const DEFAULT_IMPLEMENT_PROMPT: &str =
    "Implement work item {{work_item_number}}. Iterate until build/tests/docs succeed.";

/// Substitute the canonical placeholder.
pub fn render_default_prompt(work_item: &str) -> String {
    DEFAULT_IMPLEMENT_PROMPT.replace("{{work_item_number}}", work_item)
}
