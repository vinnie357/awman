//! Legacy banner string emitted by `headless start --refresh-key`. Captured
//! verbatim from `oldsrc/commands/headless/start.rs` so user-visible output
//! remains identical.

pub const NEW_API_KEY_BANNER: &str = "\
═══════════════════════════════════════════════════════════════════════════════
  amux headless: NEW API KEY
═══════════════════════════════════════════════════════════════════════════════

  This key will be shown ONCE. Save it now — amux only stores its hash.
";
