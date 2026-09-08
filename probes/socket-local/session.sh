#!/usr/bin/env bash
set -euo pipefail
export QT_QPA_PLATFORM=wayland
mkdir -p "$HOME/.config" "$HOME/Desktop"
qterminal -e /opt/socket-local/key-fixture.sh &
terminal_pid=$!
wait "$terminal_pid"
