#!/usr/bin/env bash
# Run inside a disposable, installed Sprite. Ends every desktop session.
set -euo pipefail

api() {
  curl -fsS --max-time 5 --unix-socket /.sprite/api.sock "http://sprite/v1/services/$1"
}

curl -fsS http://127.0.0.1:8080/healthz | jq -e '.attached == 0' >/dev/null || {
  echo 'disconnect browser viewers before running this test' >&2
  exit 1
}

for child in Xvnc xfce4-session sprite-desktop-bridge; do
  service=sprite-desktop
  [ "$child" != sprite-desktop-bridge ] || service=sprite-desktop-bridge
  cgroup=/sys/fs/cgroup/svc.$service/cgroup.procs
  before=$(api "$service")
  old_launcher=$(jq -er '.state.pid' <<<"$before")
  mapfile -t old_pids <"$cgroup"
  target=
  for pid in "${old_pids[@]}"; do
    if [ "$child" = sprite-desktop-bridge ]; then
      [ "$pid" != "$old_launcher" ] || target=$pid
    elif [ -r "/proc/$pid/comm" ] && [ "$(</proc/"$pid"/comm)" = "$child" ]; then
      target=$pid
    fi
  done
  [ -n "$target" ] || { echo "missing $child in $service" >&2; exit 1; }
  # Check membership again immediately before signaling the captured PID.
  [ "$(</proc/"$target"/cgroup)" = "0::/svc.$service" ]
  started=$SECONDS
  kill -TERM "$target"
  # Health caches readiness for ten seconds. Wait it out so a cached result
  # from the dead Xvnc cannot pass this check.
  sleep 11
  recovered=false
  while (( SECONDS - started < 60 )); do
    sleep 1
    current=$(api "$service")
    if jq -e --argjson old "$old_launcher" '.state.status == "running" and .state.pid != $old' <<<"$current" >/dev/null &&
      curl -fsS --max-time 3 http://127.0.0.1:8080/healthz | jq -e '.rfb == "listening" and .attached == 0' >/dev/null &&
      (( SECONDS - started < 60 )); then
      recovered=true
      break
    fi
  done
  "$recovered" || { echo "$child did not recover within 60 seconds" >&2; exit 1; }
  for pid in "${old_pids[@]}"; do
    [ ! -e "/proc/$pid" ] || { echo "$child left old process $pid alive" >&2; exit 1; }
  done
  echo "$child: recovered in $((SECONDS - started))s; all old service processes are gone"
done
