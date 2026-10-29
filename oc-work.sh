#!/usr/bin/env bash
PROFILE="work"   # use "personal" in the other script

export OPENCODE_CONFIG_DIR="$HOME/.config/opencode-$PROFILE"
export XDG_DATA_HOME="$HOME/.local/share/opencode-$PROFILE"
export XDG_STATE_HOME="$HOME/.local/state/opencode-$PROFILE"
export OPENCODE_DB="$XDG_DATA_HOME/opencode.db"

# Open the interactive OpenCode environment
exec opencode "$@" --standalone