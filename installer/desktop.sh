#!/usr/bin/env bash
set -euo pipefail

export DISPLAY=:1
export HOME=/home/sprite
# Ubuntu's Glycin loader keeps seccomp but skips the nested bubblewrap sandbox
# when SNAP is set. Sprites cannot create that nested user namespace.
export SNAP=/run/glycin-container

runtime_dir=/tmp/sprite-desktop
rfb_socket=$runtime_dir/rfb.sock
x_socket=/tmp/.X11-unix/X1
x_lock=/tmp/.X1-lock
xvnc_pid=
session_pid=

# XFCE agents can create new process groups. Sprite's service cgroup still
# contains them, so cleanup must cover that group rather than only our children.
service_cgroup=/sys/fs/cgroup/svc.sprite-desktop
[ "$(</proc/self/cgroup)" = "0::/svc.sprite-desktop" ] &&
  [ -r "$service_cgroup/cgroup.procs" ] || {
    echo 'run this launcher through the sprite-desktop service' >&2
    exit 1
  }

[ ! -L "$runtime_dir" ] || { echo "refusing a symlink at $runtime_dir" >&2; exit 1; }
mkdir -p "$runtime_dir"
[ "$(stat -c %u "$runtime_dir")" = "$(id -u)" ] || { echo "runtime directory belongs to another user" >&2; exit 1; }
chmod 0700 "$runtime_dir"
exec 9>"$runtime_dir/desktop.lock"
flock -n 9 || { echo 'another desktop launcher is running' >&2; exit 1; }

socket_is_listening() {
  ss -H -xl | awk -v path="$1" '$5 == path {found=1} END {exit !found}'
}

if ss -H -ltn 'sport = :5900' | awk 'NR == 1 {found=1} END {exit !found}'; then
  echo "refusing to replace a process already listening on port 5900" >&2
  exit 1
fi
for path in "$rfb_socket" "$x_socket"; do
  if [ -S "$path" ] && socket_is_listening "$path"; then
    echo "refusing to remove active foreign socket $path" >&2
    exit 1
  fi
done
if [ -f "$x_lock" ]; then
  lock_pid=$(tr -d '[:space:]' <"$x_lock")
  if [[ "$lock_pid" =~ ^[0-9]+$ ]] && kill -0 "$lock_pid" 2>/dev/null; then
    echo "display :1 belongs to a running process" >&2
    exit 1
  fi
fi
rm -f "$rfb_socket" "$x_socket" "$x_lock"

# shellcheck disable=SC2329 # invoked by trap
cleanup() {
  trap - EXIT
  trap '' INT TERM
  local pid remaining
  while read -r pid; do
    [ "$pid" = "$$" ] || kill -TERM "$pid" 2>/dev/null || true
  done <"$service_cgroup/cgroup.procs"
  sleep 3
  # A child can fork during the first scan. Rescan after killing its parent.
  for _ in {1..30}; do
    remaining=false
    while read -r pid; do
      if [ "$pid" != "$$" ]; then
        remaining=true
        kill -KILL "$pid" 2>/dev/null || true
      fi
    done <"$service_cgroup/cgroup.procs"
    "$remaining" || break
    sleep 0.1
  done
  wait 2>/dev/null || true
  # Xvnc removes its own sockets on normal exit. Startup checks and removes
  # stale paths after a crash; shutdown must not unlink a replacement server.
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

setsid Xvnc :1 \
  -geometry 1440x900 \
  -depth 24 \
  -rfbport 5900 \
  -localhost yes \
  -rfbunixpath "$rfb_socket" \
  -rfbunixmode 0600 \
  -SecurityTypes None \
  -AlwaysShared \
  -s 0 \
  -desktop sprite 9>&- &
xvnc_pid=$!

for _ in $(seq 1 100); do
  if [ -S "$x_socket" ] && [ -S "$rfb_socket" ]; then
    break
  fi
  if ! kill -0 "$xvnc_pid" 2>/dev/null; then
    wait "$xvnc_pid"
  fi
  sleep 0.1
done
if [ ! -S "$x_socket" ] || [ ! -S "$rfb_socket" ]; then
  echo "Xvnc did not create display :1 and its RFB socket" >&2
  exit 1
fi
chmod 0600 "$rfb_socket"
xset s off
xset s noblank
xset -dpms 2>/dev/null || true

setsid dbus-run-session -- startxfce4 9>&- &
session_pid=$!

# Either child exiting tears down the other. The service manager then starts a
# fresh display rather than leaving half a session alive.
set +e
wait -n "$xvnc_pid" "$session_pid"
status=$?
set -e
[ "$status" -ne 0 ] || status=1
exit "$status"
