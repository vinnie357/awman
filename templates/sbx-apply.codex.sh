#!/usr/bin/env bash
# awman sbx startup script — agent: codex
# Rendered by DSbxKitEmitter at `awman ready`. Do not edit by hand inside a kit
# directory; edit the template and re-run `awman ready`.
set -euo pipefail

# Require jq — if absent, skip config (do not hard-fail sandbox boot).
if ! command -v jq >/dev/null 2>&1; then
  echo "awman: jq not found — skipping session config apply for codex." >&2
  exit 0
fi

SESSION_FILE="${WORKDIR:-/workspace}/.awman/session.json"

# If no session file is present there is nothing to apply.
if [ ! -f "$SESSION_FILE" ]; then
  echo "awman: no session file found at $SESSION_FILE — nothing to apply."
  exit 0
fi

# Validate schema version.
SCHEMA_VERSION="$(jq -r '.schema_version // empty' "$SESSION_FILE")"
if [ "$SCHEMA_VERSION" != "1" ]; then
  echo "awman: session.json has unsupported schema_version '${SCHEMA_VERSION}' (expected 1)." >&2
  echo "awman: please re-run \`awman ready\` to regenerate the session file." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# env_config entries are written to $HOME/.awman/env.sh (sourceable).
# ---------------------------------------------------------------------------
mkdir -p "$HOME/.awman"
ENV_SH="$HOME/.awman/env.sh"

printf '# awman — generated environment overrides for codex\n' > "$ENV_SH"
printf '# Source this file to apply session env_config values.\n' >> "$ENV_SH"

jq -r '
  .env_config // {} |
  to_entries[] |
  "export \(.key)=\(.value|@sh)"
' "$SESSION_FILE" >> "$ENV_SH" || true

# ---------------------------------------------------------------------------
# Dynamic session fields codex can express natively.
# ---------------------------------------------------------------------------
mkdir -p "$HOME/.codex"

# Model → ~/.codex/config.toml. The awman-managed line is rewritten on each
# startup; other config.toml content is preserved.
MODEL="$(jq -r '.model // empty' "$SESSION_FILE")"
if [ -n "$MODEL" ]; then
  CONFIG_TOML="$HOME/.codex/config.toml"
  touch "$CONFIG_TOML"
  # Drop any prior awman-managed model line, then append the current one.
  grep -v '^model = .* # awman-managed$' "$CONFIG_TOML" > "$CONFIG_TOML.tmp" || true
  mv "$CONFIG_TOML.tmp" "$CONFIG_TOML"
  printf 'model = "%s" # awman-managed\n' "$MODEL" >> "$CONFIG_TOML"
fi

# System prompt → developer instructions. The container path delivers this as
# `--config developer_instructions=<text>`; the native, mixin-safe equivalent is
# codex's home AGENTS.md, which is always read as developer guidance.
SYS_PROMPT="$(jq -r '.system_prompt_inline.text // empty' "$SESSION_FILE")"
if [ -n "$SYS_PROMPT" ]; then
  # `.system_prompt_inline.text` for codex is "developer_instructions=<text>";
  # strip the key prefix so only the prompt body lands in AGENTS.md.
  printf '%s\n' "${SYS_PROMPT#developer_instructions=}" > "$HOME/.codex/AGENTS.md"
fi

# Tool allow/deny lists are not expressible through codex config — warn rather
# than silently drop them.
if [ "$(jq -r '((.allowed_tools // []) + (.disallowed_tools // [])) | length' "$SESSION_FILE")" != "0" ]; then
  echo "awman: codex does not support allow/deny tool lists via config; ignoring them." >&2
fi

# Seeded prompt — staged for reference. Delivery happens host-side: awman
# writes the prompt into the agent's stdin at launch (mixin kits cannot take
# it positionally through Docker's built-in template).
SEEDED="$(jq -r '.seeded_prompt // empty' "$SESSION_FILE")"
if [ -n "$SEEDED" ]; then
  printf '%s' "$SEEDED" > "$HOME/.awman/seeded-prompt.txt"
fi

# Write a marker file so external tooling can confirm config was applied.
cp "$SESSION_FILE" "$HOME/.awman/session-applied.json"

echo "awman: codex session config applied from $SESSION_FILE."
