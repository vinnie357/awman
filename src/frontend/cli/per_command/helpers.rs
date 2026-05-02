//! Shared helpers for CLI per-command frontend impls.

use super::super::output::stdin_is_tty;

/// Prompt the user with `[Y/n]` or `[y/N]` when stdin is a TTY.
/// Returns `default_yes` immediately when stdin is not a TTY.
pub fn yes_no(prompt: &str, default_yes: bool) -> bool {
    if !stdin_is_tty() {
        return default_yes;
    }
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    eprintln!("amux: {prompt} {suffix}");
    let mut buf = String::new();
    if std::io::stdin().read_line(&mut buf).is_err() {
        return default_yes;
    }
    match buf.trim() {
        "y" | "Y" => true,
        "n" | "N" => false,
        _ => default_yes,
    }
}
