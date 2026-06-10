#!/usr/bin/env bash
# awman sbx startup script — agent: copilot
# Rendered by DSbxKitEmitter at `awman ready`. Do not edit by hand inside a kit
# directory; edit the template and re-run `awman ready`.
set -euo pipefail

# Require jq — if absent, skip config (do not hard-fail sandbox boot).
if ! command -v jq >/dev/null 2>&1; then
  echo "awman: jq not found — skipping session config apply for copilot." >&2
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

printf '# awman — generated environment overrides for copilot\n' > "$ENV_SH"
printf '# Source this file to apply session env_config values.\n' >> "$ENV_SH"

jq -r '
  .env_config // {} |
  to_entries[] |
  "export \(.key)=\(.value|@sh)"
' "$SESSION_FILE" >> "$ENV_SH" || true

# ---------------------------------------------------------------------------
# Dynamic session fields copilot can express natively.
# ---------------------------------------------------------------------------

# System prompt → a custom-instructions directory referenced by
# COPILOT_CUSTOM_INSTRUCTIONS_DIRS (the container path's EnvFile mechanism).
SYS_PROMPT="$(jq -r '.system_prompt_inline.text // empty' "$SESSION_FILE")"
if [ -n "$SYS_PROMPT" ]; then
  INSTR_DIR="$HOME/.awman/copilot-instructions"
  mkdir -p "$INSTR_DIR"
  printf '%s\n' "$SYS_PROMPT" > "$INSTR_DIR/awman-context.md"
  printf 'export COPILOT_CUSTOM_INSTRUCTIONS_DIRS=%q\n' "$INSTR_DIR" >> "$ENV_SH"
fi

# Model and tool allow/deny lists are not expressible through a mixin-safe
# copilot config file — warn rather than silently drop them.
if [ -n "$(jq -r '.model // empty' "$SESSION_FILE")" ]; then
  echo "awman: copilot has no mixin-safe config for --model; ignoring the model override." >&2
fi
if [ "$(jq -r '((.allowed_tools // []) + (.disallowed_tools // [])) | length' "$SESSION_FILE")" != "0" ]; then
  echo "awman: copilot does not support allow/deny tool lists via config; ignoring them." >&2
fi

# Seeded prompt — staged for reference; awman delivers it via the agent's
# stdin at launch.
SEEDED="$(jq -r '.seeded_prompt // empty' "$SESSION_FILE")"
if [ -n "$SEEDED" ]; then
  printf '%s' "$SEEDED" > "$HOME/.awman/seeded-prompt.txt"
fi

# Write a marker file so external tooling can confirm config was applied.
cp "$SESSION_FILE" "$HOME/.awman/session-applied.json"

echo "awman: copilot session config applied from $SESSION_FILE."
