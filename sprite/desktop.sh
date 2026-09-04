#!/usr/bin/env bash
set -euo pipefail

export DISPLAY=:1
export HOME=/home/sprite
# Ubuntu's patched Glycin loader keeps seccomp but skips the nested bubblewrap
# sandbox, which Sprites cannot create, when SNAP is non-empty.
export SNAP=/run/glycin-container

rm -f /tmp/.X1-lock /tmp/.X11-unix/X1

Xvnc :1 \
  -geometry 1440x900 \
  -depth 24 \
  -rfbport 5900 \
  -localhost yes \
  -SecurityTypes None \
  -AlwaysShared \
  -desktop "sprite" &
XVNC_PID=$!
SESSION_PID=""

cleanup() {
  if [ -n "$SESSION_PID" ]; then
    kill -- "-$SESSION_PID" 2>/dev/null || true
  fi
  kill "$XVNC_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

for _ in $(seq 1 50); do
  if [ -S /tmp/.X11-unix/X1 ]; then
    break
  fi
  sleep 0.1
done

if [ ! -S /tmp/.X11-unix/X1 ]; then
  echo "Xvnc did not create display :1" >&2
  exit 1
fi

setsid dbus-run-session -- startxfce4 &
SESSION_PID=$!
wait "$XVNC_PID"
