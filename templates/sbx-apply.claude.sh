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
# Claude-specific config: write $HOME/.claude/settings.json, merging the
# passthrough agent_settings.claude block with the dynamic session fields
# claude can express natively (model and allow/deny tool permissions).
# ---------------------------------------------------------------------------
mkdir -p "$HOME/.claude"

MODEL="$(jq -r '.model // empty' "$SESSION_FILE")"
jq -n \
  --argjson base "$(jq '.agent_settings.claude // {}' "$SESSION_FILE")" \
  --arg model "$MODEL" \
  --argjson allowed "$(jq -c '.allowed_tools // []' "$SESSION_FILE")" \
  --argjson disallowed "$(jq -c '.disallowed_tools // []' "$SESSION_FILE")" '
  $base
  + (if $model != "" then { model: $model } else {} end)
  + (if ($allowed | length) > 0 or ($disallowed | length) > 0 then
       { permissions: (($base.permissions // {})
         + (if ($allowed | length) > 0 then { allow: $allowed } else {} end)
         + (if ($disallowed | length) > 0 then { deny: $disallowed } else {} end)) }
     else {} end)
' > "$HOME/.claude/settings.json"

# System prompt → global memory file (claude reads $HOME/.claude/CLAUDE.md).
# The container path uses --append-system-prompt-file; the closest native,
# mixin-launch-safe equivalent is the global memory file.
SYS_PROMPT="$(jq -r '.system_prompt_inline.text // empty' "$SESSION_FILE")"
if [ -n "$SYS_PROMPT" ]; then
  printf '%s\n' "$SYS_PROMPT" > "$HOME/.claude/CLAUDE.md"
fi

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

# Seeded prompt — staged for reference. Delivery happens host-side: awman
# writes the prompt into the agent's stdin at launch (mixin kits cannot take
# it positionally through Docker's built-in template).
SEEDED="$(jq -r '.seeded_prompt // empty' "$SESSION_FILE")"
if [ -n "$SEEDED" ]; then
  printf '%s' "$SEEDED" > "$HOME/.awman/seeded-prompt.txt"
fi

echo "awman: claude session config applied from $SESSION_FILE."
