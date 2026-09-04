#!/usr/bin/env bash
# One disposable Sprite running the upstream demo. No runtime migration.
set -euo pipefail
export XDG_RUNTIME_DIR=/tmp/waymote-session
install -d -m 0700 "$XDG_RUNTIME_DIR"
export WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1
export WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman
export WAYLAND_DISPLAY=wayland-0
# startlxqtwayland forces software cursors on VMs, baking them into screencopy.
# Start this known labwc session directly so its headless cursor stays separate.
export WLR_NO_HARDWARE_CURSORS=0
export XCURSOR_THEME=breeze_cursors XCURSOR_SIZE=24
export XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=LXQt:labwc:wlroots
export XDG_CONFIG_HOME="$HOME/.config" XDG_CONFIG_DIRS=/etc:/etc/xdg:/usr/share
export XDG_DATA_HOME="$HOME/.local/share" XDG_DATA_DIRS="$HOME/.local/share:/usr/local/share:/usr/share"
export XDG_CACHE_HOME="$HOME/.cache" XDG_MENU_PREFIX=lxqt-
export QT_QPA_PLATFORMTHEME=lxqt QT_ACCESSIBILITY=1
export QT_AUTO_SCREEN_SCALE_FACTOR=0 QT_EXCLUDE_GENERIC_BEARER=1
: "${DBUS_SESSION_BUS_ADDRESS:?Run this launcher under dbus-run-session}"
mkdir -p "$XDG_CONFIG_HOME" "$HOME/Desktop"
if [ ! -d "$XDG_CONFIG_HOME/labwc" ]; then
  cp -a /usr/share/lxqt/wayland/labwc "$XDG_CONFIG_HOME/labwc"
fi
root=/home/sprite/waymote/waymote-server-0.1.2-linux-x86_64
labwc_pid=''
gateway_pid=''
# shellcheck disable=SC2329 # invoked by EXIT trap
cleanup() {
  trap - EXIT
  trap '' INT TERM
  for pid in "$gateway_pid" "$labwc_pid"; do
    [ -z "$pid" ] || kill -TERM "$pid" 2>/dev/null || true
  done
  sleep 2
  for pid in "$gateway_pid" "$labwc_pid"; do
    [ -z "$pid" ] || kill -KILL "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 143' TERM
trap 'exit 130' INT
labwc -C "$XDG_CONFIG_HOME/labwc" -S lxqt-session >"$XDG_RUNTIME_DIR/labwc.log" 2>&1 &
labwc_pid=$!
for _ in {1..100}; do
  [ ! -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] || break
  kill -0 "$labwc_pid"
  sleep 0.1
done
# Initial size only; the controlling browser resizes the output afterward.
wlr-randr --output HEADLESS-1 --custom-mode 1280x720@60Hz
"${WAYMOTE_GATEWAY:-$root/bin/waymote-gateway}" \
  -listen 0.0.0.0:8080 \
  -streamd "${WAYMOTE_STREAMD:-$root/bin/waymote-streamd}" \
  -public-url https://sprite-desktop-waymote-6ra.sprites.app \
  -frame-rate 60 -bitrate 8000 &
gateway_pid=$!
wait -n "$gateway_pid" "$labwc_pid"
exit 1
