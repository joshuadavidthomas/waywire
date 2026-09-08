#!/usr/bin/env bash
# Rebuild and verify the current paired Rust binaries against the retained native image.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
readonly root
bitrate=${SOCKET_TEST_BITRATE:-16000}
case "$bitrate" in 8000|16000) ;; *) echo 'test bitrate must be 8000 or 16000' >&2; exit 2 ;; esac
readonly bitrate
owner="socket-recheck-$(date -u +%Y%m%d-%H%M%S)-$$"
readonly owner
readonly result_root="$root/probes/socket-local/results/$owner"
mkdir -p "$result_root"

containers=()
units=()
profiles=()
cleanup() {
  local name unit profile
  trap - EXIT INT TERM
  for name in "${containers[@]}"; do
    if [ "$(docker inspect "$name" --format '{{index .Config.Labels "socket-local-owner"}}' 2>/dev/null || true)" = "$owner" ]; then
      docker logs "$name" >"$result_root/$name-container.log" 2>&1 || true
      docker rm -f "$name" >/dev/null 2>&1 || true
    fi
  done
  for unit in "${units[@]}"; do
    systemctl --user stop "$unit.service" >/dev/null 2>&1 || true
    systemctl --user reset-failed "$unit.service" >/dev/null 2>&1 || true
  done
  for profile in "${profiles[@]}"; do rm -rf -- "$profile"; done
}
trap cleanup EXIT INT TERM

command -v docker >/dev/null
command -v chromium >/dev/null
command -v systemd-run >/dev/null
pnpm build
pnpm test:rust:media
docker build --file "$root/probes/socket-local/Dockerfile" --iidfile "$result_root/image.txt" "$root"
image=$(<"$result_root/image.txt")
readonly image
sha256sum "$root/target/release/sprite-desktop-gateway" "$root/target/release/sprite-desktop-streamd" >"$result_root/binaries.sha256"

rates=(30 60)
if [ "$#" -gt 0 ]; then rates=("$@"); fi
for rate in "${rates[@]}"; do
  case "$rate" in 30|60) ;; *) echo 'test rate must be 30 or 60' >&2; exit 2 ;; esac
  name="$owner-$rate"
  unit="$owner-browser-$rate"
  port="18$rate"
  output="$result_root/$rate"
  profile="/tmp/$unit-profile"
  mkdir -p "$output" "$profile"
  containers+=("$name")
  units+=("$unit")
  profiles+=("$profile")

  if ss -H -ltn "sport = :$port" | read -r _; then echo "owned port $port is already in use" >&2; exit 1; fi
  docker run -d --name "$name" --label socket-local-owner="$owner" --stop-timeout 20 \
    -p "127.0.0.1:$port:8080" \
    -e "PUBLIC_URL=http://127.0.0.1:$port" -e "FRAME_RATE=$rate" -e "BITRATE=$bitrate" \
    -v "$root/target/release/sprite-desktop-gateway:/opt/socket-local/bin/sprite-desktop-gateway:ro" \
    -v "$root/target/release/sprite-desktop-streamd:/opt/socket-local/bin/sprite-desktop-streamd:ro" \
    -v "$root/probes/socket-local/container-entrypoint.sh:/opt/socket-local/container-entrypoint.sh:ro" \
    -v "$root/probes/socket-local/key-fixture.sh:/opt/socket-local/key-fixture.sh:ro" \
    -v "$root/probes/socket-local/key-fixture.py:/opt/socket-local/key-fixture.py:ro" \
    -v "$output:/evidence" --entrypoint /opt/socket-local/container-entrypoint.sh "$image" >/dev/null

  ready=false
  for _ in $(seq 1 150); do
    if curl --fail --silent --max-time 1 "http://127.0.0.1:$port/" >/dev/null; then ready=true; break; fi
    docker inspect "$name" --format '{{.State.Running}}' | rg -qx true || break
    sleep .2
  done
  $ready || { docker logs "$name"; exit 1; }

  systemd-run --user --unit "$unit" --property RuntimeMaxSec=15min \
    /usr/bin/chromium --headless=new --disable-gpu --no-first-run --no-default-browser-check \
    --remote-debugging-address=127.0.0.1 --remote-debugging-port=0 --user-data-dir="$profile" about:blank >/dev/null
  for _ in $(seq 1 100); do [[ -s "$profile/DevToolsActivePort" ]] && break; sleep .1; done
  [[ -s "$profile/DevToolsActivePort" ]] || { echo 'Chrome CDP endpoint timeout' >&2; exit 1; }
  cdp_port=$(awk 'NR==1 {print; exit}' "$profile/DevToolsActivePort")
  cdp_path=$(awk 'NR==2 {print; exit}' "$profile/DevToolsActivePort")
  [[ "$cdp_port" =~ ^[0-9]+$ && "$cdp_path" =~ ^/devtools/browser/ ]] || { echo 'invalid owned Chrome endpoint' >&2; exit 1; }
  browser_control=$(systemctl --user show "$unit.service" --property ControlGroup --value)
  [[ "$browser_control" == /user.slice/* ]] || { echo 'dedicated browser cgroup unavailable' >&2; exit 1; }
  browser_cgroup="/sys/fs/cgroup$browser_control"
  container_pid=$(docker inspect "$name" --format '{{.State.Pid}}')
  container_control=$(awk -F: '$1=="0" {print $3}' "/proc/$container_pid/cgroup")
  container_cgroup="/sys/fs/cgroup$container_control"
  [[ -r "$browser_cgroup/cpu.stat" && -r "$container_cgroup/cpu.stat" ]] || { echo 'CPU unavailable: cgroup cpu.stat is unreadable' >&2; exit 1; }

  timeout --signal=TERM --kill-after=15s 14m pnpm exec tsx "$root/probes/socket-local/verify.ts" \
    "ws://127.0.0.1:$cdp_port$cdp_path" "http://127.0.0.1:$port" "$name" "$output" \
    "$browser_cgroup" "$container_cgroup" "$rate" "$bitrate" | tee "$output/verify.log"

  docker logs "$name" >"$output/container.log" 2>&1
  if rg -n "video metadata/RTP correlation stalled|correlation stalled" "$output/container.log"; then
    echo 'late correlation failure found in container log' >&2
    exit 1
  fi
  systemctl --user stop "$unit.service"
  docker rm -f "$name" >/dev/null
done

printf 'socket local verification passed: %s\n' "$result_root"
