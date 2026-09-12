#!/usr/bin/env bash
set -euo pipefail

fail() { printf 'waywire launcher: %s\n' "$*" >&2; exit 1; }

readonly root=/opt/waywire/current
readonly runtime_dir=/tmp/waywire
readonly wayland_display=wayland-0
readonly wayland_socket="$runtime_dir/$wayland_display"

[ -n "${HOME:-}" ] || fail 'HOME is not set'
for command in dbus-run-session ffmpeg flock labwc lxqt-session python3 ss wayland-info; do
  command -v "$command" >/dev/null || fail "required command is missing: $command"
done
for binary in waywire-gateway waywire-streamd; do
  [ -x "$root/bin/$binary" ] || fail "paired binary is missing: $root/bin/$binary"
done

if [ "${1:-}" != --session ]; then
  [ "$#" -eq 1 ] || fail 'expected one public origin argument'
  exec dbus-run-session -- "$0" --session "$1"
fi
[ "$#" -eq 2 ] || fail 'unexpected session arguments'
readonly origin=$2
: "${DBUS_SESSION_BUS_ADDRESS:?dbus-run-session did not set DBUS_SESSION_BUS_ADDRESS}"

[ ! -L "$runtime_dir" ] || fail "refusing symlinked runtime directory: $runtime_dir"
install -d -m 0700 "$runtime_dir"
[ "$(stat -c %u "$runtime_dir")" = "$(id -u)" ] || fail "runtime directory belongs to another user: $runtime_dir"
chmod 0700 "$runtime_dir"
exec 9>"$runtime_dir/launcher.lock"
flock -n 9 || fail 'another desktop launcher holds the runtime directory'
if [ -e "$wayland_socket" ]; then
  if ss -H -xl | awk -v socket="$wayland_socket" '{for (i=1; i<=NF; i++) if ($i == socket) found=1} END {exit !found}'; then
    fail "refusing to replace a compositor listening at $wayland_socket"
  fi
  rm -f "$wayland_socket"
fi

export XDG_RUNTIME_DIR="$runtime_dir"
export WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1
export WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman
export WAYLAND_DISPLAY="$wayland_display"
export WAYWIRE_RESOLUTION="${WAYWIRE_RESOLUTION:-1920x1080}"
export WLR_NO_HARDWARE_CURSORS=0
export XCURSOR_THEME=breeze_cursors XCURSOR_SIZE=24
export XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=LXQt:labwc:wlroots
export XDG_CONFIG_HOME="$HOME/.config" XDG_CONFIG_DIRS=/etc:/etc/xdg:/usr/share
export XDG_DATA_HOME="$HOME/.local/share" XDG_DATA_DIRS="$HOME/.local/share:/usr/local/share:/usr/share"
export XDG_CACHE_HOME="$HOME/.cache" XDG_MENU_PREFIX=lxqt-
export QT_QPA_PLATFORMTHEME=lxqt QT_ACCESSIBILITY=1
export QT_AUTO_SCREEN_SCALE_FACTOR=0 QT_EXCLUDE_GENERIC_BEARER=1
mkdir -p "$XDG_CONFIG_HOME" "$HOME/Desktop"
if [ ! -d "$XDG_CONFIG_HOME/labwc" ]; then
  cp -a /usr/share/lxqt/wayland/labwc "$XDG_CONFIG_HOME/labwc"
fi
# Keep the launcher lock across exec. Python's child processes close inherited
# descriptors and its pidfds keep teardown safe after a child exits.
exec python3 "$root/bin/session.py" "$origin"
