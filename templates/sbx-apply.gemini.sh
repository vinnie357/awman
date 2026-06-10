#!/usr/bin/env bash
# awman sbx startup script — agent: gemini
# Rendered by DSbxKitEmitter at `awman ready`. Do not edit by hand inside a kit
# directory; edit the template and re-run `awman ready`.
set -euo pipefail

# Require jq — if absent, skip config (do not hard-fail sandbox boot).
if ! command -v jq >/dev/null 2>&1; then
  echo "awman: jq not found — skipping session config apply for gemini." >&2
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

printf '# awman — generated environment overrides for gemini\n' > "$ENV_SH"
printf '# Source this file to apply session env_config values.\n' >> "$ENV_SH"

jq -r '
  .env_config // {} |
  to_entries[] |
  "export \(.key)=\(.value|@sh)"
' "$SESSION_FILE" >> "$ENV_SH" || true

# ---------------------------------------------------------------------------
# Dynamic session fields gemini can express natively.
# ---------------------------------------------------------------------------
mkdir -p "$HOME/.gemini"

# Model → ~/.gemini/settings.json (merged into any existing settings).
MODEL="$(jq -r '.model // empty' "$SESSION_FILE")"
if [ -n "$MODEL" ]; then
  SETTINGS="$HOME/.gemini/settings.json"
  EXISTING='{}'
  if [ -f "$SETTINGS" ]; then
    EXISTING="$(jq '.' "$SETTINGS" 2>/dev/null || echo '{}')"
  fi
  jq -n --argjson base "$EXISTING" --arg model "$MODEL" \
    '$base + { model: $model }' > "$SETTINGS"
fi

# System prompt → file referenced by GEMINI_SYSTEM_MD (the container path's
# EnvFile delivery mechanism). Write the file and export the env var.
SYS_PROMPT="$(jq -r '.system_prompt_inline.text // empty' "$SESSION_FILE")"
if [ -n "$SYS_PROMPT" ]; then
  SYS_FILE="$HOME/.gemini/system.md"
  printf '%s\n' "$SYS_PROMPT" > "$SYS_FILE"
  printf 'export GEMINI_SYSTEM_MD=%q\n' "$SYS_FILE" >> "$ENV_SH"
fi

# Tool allow/deny lists are not expressible through gemini config — warn.
if [ "$(jq -r '((.allowed_tools // []) + (.disallowed_tools // [])) | length' "$SESSION_FILE")" != "0" ]; then
  echo "awman: gemini does not support allow/deny tool lists via config; ignoring them." >&2
fi

# Seeded prompt — staged for reference; awman delivers it via the agent's
# stdin at launch.
SEEDED="$(jq -r '.seeded_prompt // empty' "$SESSION_FILE")"
if [ -n "$SEEDED" ]; then
  printf '%s' "$SEEDED" > "$HOME/.awman/seeded-prompt.txt"
fi

# Write a marker file so external tooling can confirm config was applied.
cp "$SESSION_FILE" "$HOME/.awman/session-applied.json"

echo "awman: gemini session config applied from $SESSION_FILE."
