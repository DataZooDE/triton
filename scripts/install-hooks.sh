#!/usr/bin/env bash
# scripts/install-hooks.sh — enable the versioned git hooks in .githooks/.
#
# Git never installs hooks on clone, so run this once per checkout (fresh
# clone or existing one; linked worktrees share the setting). Idempotent.
# Undo with: git config --unset core.hooksPath
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

current="$(git config --get core.hooksPath || true)"
if [ "$current" != ".githooks" ]; then
  # Hooks in the previous location stop running; say so rather than
  # silently dropping someone's local hook.
  previous="${current:-$(git rev-parse --git-path hooks)}"
  for hook in "$previous"/*; do
    case "$hook" in *.sample) continue ;; esac
    if [ -f "$hook" ] && [ -x "$hook" ]; then
      echo "warning: $hook will no longer run (core.hooksPath -> .githooks)" >&2
    fi
  done
fi

git config core.hooksPath .githooks
echo "Installed: core.hooksPath=.githooks (pre-push runs scripts/check.sh pre-push)."
echo "Run the gate by hand with scripts/check.sh; skip the hook once with git push --no-verify."
