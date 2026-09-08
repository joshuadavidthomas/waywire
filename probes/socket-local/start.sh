#!/usr/bin/env bash
set -euo pipefail
: "${PUBLIC_URL:?PUBLIC_URL must be set}"
rate=${FRAME_RATE:?FRAME_RATE must be set}
case "$rate" in 30|60) ;; *) echo 'FRAME_RATE must be 30 or 60' >&2; exit 2;; esac
if [[ ${1:-} != --session ]]; then exec dbus-run-session -- "$0" --session; fi
runtime=/tmp/socket-local
export HOME=/home/sprite XDG_RUNTIME_DIR=$runtime WAYLAND_DISPLAY=wayland-0
export WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1 WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman
export XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=LXQt:labwc:wlroots
export XDG_CONFIG_HOME=$HOME/.config XDG_CONFIG_DIRS=/etc:/etc/xdg:/usr/share
export XDG_DATA_HOME=$HOME/.local/share XDG_DATA_DIRS=$HOME/.local/share:/usr/local/share:/usr/share
export XDG_CACHE_HOME=$HOME/.cache XDG_MENU_PREFIX=lxqt-
export QT_QPA_PLATFORMTHEME=lxqt QT_ACCESSIBILITY=1 QT_AUTO_SCREEN_SCALE_FACTOR=0
mkdir -p "$runtime" "$XDG_CONFIG_HOME" "$HOME/Desktop"
chmod 0700 "$runtime"
exec python3 /opt/socket-local/native.py
