#!/usr/bin/env bash
# Disposable two-process Rust trial. The gateway owns streamd; streamd owns FFmpeg.
set -euo pipefail

fail() { printf 'rust desktop trial: %s\n' "$*" >&2; exit 1; }

readonly root=/home/sprite/rust-desktop
readonly runtime_dir=/tmp/sprite-desktop-rust-session
readonly wayland_display=wayland-0
readonly wayland_socket="$runtime_dir/$wayland_display"
labwc_pid=
gateway_pid=
stage_timings_dir=

[ "$(id -un)" = sprite ] || fail 'run as the sprite user'
[ "$(</proc/self/cgroup)" = '0::/svc.rust-desktop' ] ||
  fail 'run only through the owned rust-desktop service'
for command in dbus-run-session ffmpeg flock jq labwc lxqt-session sprite-env ss wayland-info wlr-randr; do
  command -v "$command" >/dev/null || fail "required command is missing: $command"
done
for binary in sprite-desktop-gateway sprite-desktop-streamd; do
  [ -x "$root/bin/$binary" ] || fail "paired binary is missing: $root/bin/$binary"
done
[ ! -L "$root" ] || fail "refusing symlinked trial root: $root"
[ "$(stat -c %U "$root")" = sprite ] || fail "trial root is not owned by sprite: $root"
[ ! -L "$runtime_dir" ] || fail "refusing symlinked runtime directory: $runtime_dir"
install -d -m 0700 "$runtime_dir"
[ "$(stat -c %u "$runtime_dir")" = "$(id -u)" ] ||
  fail "runtime directory belongs to another user: $runtime_dir"
chmod 0700 "$runtime_dir"
exec 9>"$runtime_dir/launcher.lock"
flock -n 9 || fail 'another Rust desktop launcher holds the runtime directory'
if [ -e "$wayland_socket" ]; then
  if ss -H -xl | awk -v socket="$wayland_socket" '{for (i=1; i<=NF; i++) if ($i == socket) found=1} END {exit !found}'; then
    fail "refusing to replace a compositor listening at $wayland_socket"
  fi
  rm -f "$wayland_socket"
fi
if ss -H -ltn 'sport = :8080' | awk 'NR == 1 {found=1} END {exit !found}'; then
  fail 'refusing to replace a process listening on port 8080'
fi

export XDG_RUNTIME_DIR="$runtime_dir"
export WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1
export WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman
export WAYLAND_DISPLAY="$wayland_display"
export WLR_NO_HARDWARE_CURSORS=0
export XCURSOR_THEME=breeze_cursors XCURSOR_SIZE=24
export XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=LXQt:labwc:wlroots
export XDG_CONFIG_HOME="$HOME/.config" XDG_CONFIG_DIRS=/etc:/etc/xdg:/usr/share
export XDG_DATA_HOME="$HOME/.local/share" XDG_DATA_DIRS="$HOME/.local/share:/usr/local/share:/usr/share"
export XDG_CACHE_HOME="$HOME/.cache" XDG_MENU_PREFIX=lxqt-
export QT_QPA_PLATFORMTHEME=lxqt QT_ACCESSIBILITY=1
export QT_AUTO_SCREEN_SCALE_FACTOR=0 QT_EXCLUDE_GENERIC_BEARER=1
: "${DBUS_SESSION_BUS_ADDRESS:?run this launcher under dbus-run-session}"
mkdir -p "$XDG_CONFIG_HOME" "$HOME/Desktop"
if [ ! -d "$XDG_CONFIG_HOME/labwc" ]; then
  cp -a /usr/share/lxqt/wayland/labwc "$XDG_CONFIG_HOME/labwc"
fi
origin=$(sprite-env info | jq -er '.sprite_url | select(type == "string" and test("^https://[^/]+$"))') ||
  fail 'sprite-env info did not return a canonical HTTPS origin'

# shellcheck disable=SC2329 # invoked by EXIT trap
cleanup() {
  trap - EXIT
  trap '' INT TERM
  local pid
  for pid in "$gateway_pid" "$labwc_pid"; do
    [ -z "$pid" ] || kill -TERM "$pid" 2>/dev/null || true
  done
  sleep 2
  for pid in "$gateway_pid" "$labwc_pid"; do
    [ -z "$pid" ] || kill -KILL "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  if [ -n "$stage_timings_dir" ] &&
     [ "$(dirname "$stage_timings_dir")" = "$runtime_dir" ] &&
     [ ! -L "$stage_timings_dir" ] &&
     [ -d "$stage_timings_dir" ] &&
     [ "$(stat -c %u "$stage_timings_dir")" = "$(id -u)" ]; then
    rm -rf -- "$stage_timings_dir"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

stage_timings_dir=$(mktemp -d "$runtime_dir/stage-timings.XXXXXXXX")
[ ! -L "$stage_timings_dir" ] || fail 'refusing symlinked stage timing directory'
[ "$(stat -c %u "$stage_timings_dir")" = "$(id -u)" ] ||
  fail 'stage timing directory belongs to another user'
chmod 0700 "$stage_timings_dir"
export SPRITE_DESKTOP_STAGE_TIMINGS="$stage_timings_dir/stages.json"
export SPRITE_DESKTOP_GATEWAY_STAGE_TIMINGS="$stage_timings_dir/gateway.json"

labwc -C "$XDG_CONFIG_HOME/labwc" -S lxqt-session 9>&- >"$runtime_dir/labwc.log" 2>&1 &
labwc_pid=$!
ready=false
for _ in $(seq 1 100); do
  kill -0 "$labwc_pid" 2>/dev/null || { wait "$labwc_pid"; fail 'launched labwc exited'; }
  if [ -S "$wayland_socket" ] && wayland-info >/dev/null 2>&1; then
    ready=true
    break
  fi
  sleep 0.1
done
$ready || fail 'launched labwc did not make its private Wayland socket ready'
wlr-randr --output HEADLESS-1 --custom-mode 1280x720@60Hz

"$root/bin/sprite-desktop-gateway" \
  --listen 0.0.0.0:8080 \
  --streamd "$root/bin/sprite-desktop-streamd" \
  --public-url "$origin" \
  --frame-rate 60 \
  --bitrate 8000 \
  --xkb-layout "${XKB_DEFAULT_LAYOUT:-us}" 9>&- &
gateway_pid=$!
wait -n "$gateway_pid" "$labwc_pid"
exit 1
