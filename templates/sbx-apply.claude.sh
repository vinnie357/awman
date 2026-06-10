#!/usr/bin/env bash
# awman sbx startup script — agent: claude
# Rendered by DSbxKitEmitter at `awman ready`. Do not edit by hand inside a kit
# directory; edit the template and re-run `awman ready`.
set -euo pipefail

# Require jq — if absent, skip config (do not hard-fail sandbox boot).
if ! command -v jq >/dev/null 2>&1; then
  echo "awman: jq not found — skipping session config apply for claude." >&2
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
# Claude-specific config: write $HOME/.claude/settings.json
# ---------------------------------------------------------------------------
mkdir -p "$HOME/.claude"
jq -r '.agent_settings.claude // {}' "$SESSION_FILE" > "$HOME/.claude/settings.json"

# ---------------------------------------------------------------------------
# Write env_config entries to $HOME/.awman/env.sh (sourceable).
# This file is idempotent — it is fully rewritten on every startup.
# ---------------------------------------------------------------------------
mkdir -p "$HOME/.awman"
ENV_SH="$HOME/.awman/env.sh"

# Emit the file header.
printf '# awman — generated environment overrides for claude\n' > "$ENV_SH"
printf '# Source this file to apply session env_config values.\n' >> "$ENV_SH"

# Append one export line per env_config entry (value is shell-quoted by jq @sh).
jq -r '
  .env_config // {} |
  to_entries[] |
  "export \(.key)=\(.value|@sh)"
' "$SESSION_FILE" >> "$ENV_SH" || true

echo "awman: claude session config applied from $SESSION_FILE."
