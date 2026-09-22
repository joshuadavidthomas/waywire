#!/usr/bin/env bash
set -euo pipefail

fail() { printf 'waywire launcher: %s\n' "$*" >&2; exit 1; }

readonly root=/opt/waywire/current
readonly runtime_dir=/tmp/waywire

[ -n "${HOME:-}" ] || fail 'HOME is not set'
for command in dbus-run-session ffmpeg flock; do
  command -v "$command" >/dev/null || fail "required command is missing: $command"
done
for binary in waywire-gateway waywire-compositor; do
  [ -x "$root/bin/$binary" ] || fail "paired binary is missing: $root/bin/$binary"
done

[ "$#" -ge 1 ] || fail 'expected a public origin, optionally followed by an application and its arguments'
readonly origin=$1
shift
if [ "$#" -eq 0 ]; then
  set -- foot
fi

[ ! -L "$runtime_dir" ] || fail "refusing symlinked runtime directory: $runtime_dir"
install -d -m 0700 "$runtime_dir"
[ "$(stat -c %u "$runtime_dir")" = "$(id -u)" ] || fail "runtime directory belongs to another user: $runtime_dir"
chmod 0700 "$runtime_dir"
exec 9>"$runtime_dir/launcher.lock"
flock -n 9 || fail 'another desktop launcher holds the runtime directory'

export XDG_RUNTIME_DIR="$runtime_dir"
export WAYWIRE_RESOLUTION="${WAYWIRE_RESOLUTION:-1920x1080}"
unset WAYLAND_DISPLAY DISPLAY
export PATH="${WAYWIRE_XWAYLAND_PREFIX:-$HOME/.local/share/waywire-xwayland}/bin:$PATH"

# Gateway shutdown owns the compositor's process group, including its applications.
exec dbus-run-session -- "$root/bin/waywire-gateway" \
  --listen 0.0.0.0:8080 --compositor "$root/bin/waywire-compositor" \
  --public-url "$origin" --frame-rate 60 --bitrate 16000 \
  --xkb-layout "${XKB_DEFAULT_LAYOUT:-us}" -- "$@"
