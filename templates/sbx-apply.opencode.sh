#!/usr/bin/env bash
# awman sbx startup script — agent: opencode
# Rendered by DSbxKitEmitter at `awman ready`. Do not edit by hand inside a kit
# directory; edit the template and re-run `awman ready`.
set -euo pipefail

# Require jq — if absent, skip config (do not hard-fail sandbox boot).
if ! command -v jq >/dev/null 2>&1; then
  echo "awman: jq not found — skipping session config apply for opencode." >&2
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

printf '# awman — generated environment overrides for opencode\n' > "$ENV_SH"
printf '# Source this file to apply session env_config values.\n' >> "$ENV_SH"

jq -r '
  .env_config // {} |
  to_entries[] |
  "export \(.key)=\(.value|@sh)"
' "$SESSION_FILE" >> "$ENV_SH" || true

# ---------------------------------------------------------------------------
# Dynamic session fields opencode can express natively.
# ---------------------------------------------------------------------------

# Model → ~/.config/opencode/opencode.json (merged into any existing config).
MODEL="$(jq -r '.model // empty' "$SESSION_FILE")"
if [ -n "$MODEL" ]; then
  CONFIG_DIR="$HOME/.config/opencode"
  mkdir -p "$CONFIG_DIR"
  CONFIG="$CONFIG_DIR/opencode.json"
  EXISTING='{}'
  if [ -f "$CONFIG" ]; then
    EXISTING="$(jq '.' "$CONFIG" 2>/dev/null || echo '{}')"
  fi
  jq -n --argjson base "$EXISTING" --arg model "$MODEL" \
    '$base + { model: $model }' > "$CONFIG"
fi

# System prompt: opencode reads AGENTS.md from the workspace. The container path
# plants AGENTS.md into the mounted context directories, which the sandbox VM
# already sees through the workspace mount, so nothing inline is delivered here.
# If a future option carries inline text, surface it so it is not lost.
SYS_PROMPT="$(jq -r '.system_prompt_inline.text // empty' "$SESSION_FILE")"
if [ -n "$SYS_PROMPT" ]; then
  printf '%s\n' "$SYS_PROMPT" > "$HOME/.awman/system-prompt.md"
  echo "awman: opencode system prompt staged at ~/.awman/system-prompt.md (opencode reads AGENTS.md from the workspace)." >&2
fi

# Tool allow/deny lists are not expressible through a mixin-safe opencode config
# — warn rather than silently drop them.
if [ "$(jq -r '((.allowed_tools // []) + (.disallowed_tools // [])) | length' "$SESSION_FILE")" != "0" ]; then
  echo "awman: opencode does not support allow/deny tool lists via config; ignoring them." >&2
fi

# Seeded prompt — staged for reference; awman delivers it via the agent's
# stdin at launch.
SEEDED="$(jq -r '.seeded_prompt // empty' "$SESSION_FILE")"
if [ -n "$SEEDED" ]; then
  printf '%s' "$SEEDED" > "$HOME/.awman/seeded-prompt.txt"
fi

# Write a marker file so external tooling can confirm config was applied.
cp "$SESSION_FILE" "$HOME/.awman/session-applied.json"

echo "awman: opencode session config applied from $SESSION_FILE."
