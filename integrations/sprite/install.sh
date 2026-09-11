#!/usr/bin/env bash
set -euo pipefail

readonly RELEASE='@WAYWIRE_VERSION@'
readonly ARCHIVE_SIZE='@WAYWIRE_ARCHIVE_SIZE@'
readonly ARCHIVE_SHA256='@WAYWIRE_ARCHIVE_SHA256@'
readonly OWNER_ID='dev.waywire.runtime'
readonly API_SOCKET='/.sprite/api.sock'
readonly INSTALL_RECORD='/var/lib/waywire/install.json'
readonly VERSION_FILE='/etc/waywire/version.json'
readonly RELEASE_DIR="/opt/waywire/releases/$RELEASE"
readonly CURRENT_LINK='/opt/waywire/current'
archive_path=
temporary=

fail() { printf 'waywire installer: %s\n' "$*" >&2; exit 1; }
notice() { printf 'waywire installer: %s\n' "$*"; }
failpoint() { [ "${WAYWIRE_FAILPOINT:-}" != "$1" ] || fail "test failpoint: $1"; }
cleanup() { [ -z "$temporary" ] || rm -rf "$temporary"; }
trap cleanup EXIT

while [ "$#" -gt 0 ]; do
  case "$1" in
    --archive)
      [ "$#" -ge 2 ] || fail '--archive requires a path'
      archive_path=$2
      shift 2
      ;;
    *) fail "unknown argument: $1" ;;
  esac
done

[[ "$RELEASE" != @* ]] || fail 'this is an unbuilt installer template'
[ "$(id -un)" = sprite ] || fail 'run this installer as the sprite user, not as root'
for command in sudo curl jq flock sha256sum stat tar gzip gpg ss dpkg sprite-env; do
  command -v "$command" >/dev/null || fail "required command is missing: $command"
done
sudo -n true 2>/dev/null || fail 'the sprite user needs passwordless sudo'
# shellcheck disable=SC1091 # fixed system file on the supported image
. /etc/os-release
[ "${ID:-}" = ubuntu ] && [ "${VERSION_CODENAME:-}" = resolute ] || fail 'only Ubuntu 26.04 (resolute) is supported'
[ "$(dpkg --print-architecture)" = amd64 ] || fail 'only amd64 is supported'
sudo install -d -m 1777 /run/lock
exec 9>/var/lock/waywire-install.lock
flock -x 9
[ -S "$API_SOCKET" ] || fail "Sprite API socket is missing: $API_SOCKET"
origin=$(sprite-env info | jq -er '.sprite_url | select(type == "string" and test("^https://[a-z0-9](?:[a-z0-9-]*[a-z0-9])?(?:\\.[a-z0-9](?:[a-z0-9-]*[a-z0-9])?)+$"))') ||
  fail 'sprite-env info did not return a canonical HTTPS origin'

temporary=$(mktemp -d)
staged=$temporary/release
mkdir "$staged"
if [ -n "$archive_path" ]; then
  [ -f "$archive_path" ] || fail "archive does not exist: $archive_path"
  archive=$archive_path
else
  fail '--archive is required'
fi
actual_size=$(stat -c %s "$archive")
[ "$actual_size" = "$ARCHIVE_SIZE" ] || fail "archive size mismatch: expected $ARCHIVE_SIZE, got $actual_size"
actual_sha=$(sha256sum "$archive" | awk '{print $1}')
[ "$actual_sha" = "$ARCHIVE_SHA256" ] || fail "archive SHA-256 mismatch: expected $ARCHIVE_SHA256, got $actual_sha"
tar -xzf "$archive" -C "$staged" --no-same-owner --no-same-permissions
for path in bin/waywire-gateway bin/waywire-streamd bin/desktop.sh bin/session.py manifest.json sources.lock; do
  [ -f "$staged/$path" ] || fail "archive is missing $path"
done
expected_gateway_sha=$(sha256sum "$staged/bin/waywire-gateway" | awk '{print $1}')
expected_streamd_sha=$(sha256sum "$staged/bin/waywire-streamd" | awk '{print $1}')
jq -e --arg release "$RELEASE" '
  .schema == 1 and .release == $release and (.source | type == "string") and
  .os == {id:"ubuntu",codename:"resolute",architecture:"amd64"} and
  .artifacts == ["waywire-gateway","waywire-streamd"] and
  .ports == {http:8080} and
  (has("services") | not)
' "$staged/manifest.json" >/dev/null || fail 'manifest release or platform definition is invalid'
jq -e '.schema == 1 and (.sources | type == "array") and (.sources | length == 2)' "$staged/sources.lock" >/dev/null || fail 'sources.lock is invalid'
bash -n "$staged/bin/desktop.sh"

# Verify package repositories and keys before changing persistent state.
while IFS=$'\t' read -r source_url suite; do
  curl -fsSL --connect-timeout 10 --max-time 30 -o /dev/null "$source_url/dists/$suite/Release" || fail "apt source is unreachable: $source_url ($suite)"
done < <(jq -r '.sources[] | .url as $url | .suites[] | [$url,.] | @tsv' "$staged/sources.lock")
ubuntu_keyring=$(jq -r '.sources[] | select(.name == "ubuntu") | .keyring' "$staged/sources.lock")
[ -r "$ubuntu_keyring" ] || fail "Ubuntu keyring is missing: $ubuntu_keyring"
ubuntu_fingerprints=$(gpg --no-options --homedir "$temporary" --batch --show-keys --with-colons "$ubuntu_keyring" | awk -F: '$1 == "fpr" {print $10}')
while read -r fingerprint; do
  printf '%s\n' "$ubuntu_fingerprints" | awk -v wanted="$fingerprint" '$0 == wanted {found=1} END {exit !found}' || fail "Ubuntu signing key fingerprint is missing: $fingerprint"
done < <(jq -r '.sources[] | select(.name == "ubuntu") | .key_fingerprints[]' "$staged/sources.lock")
mozilla_key_url=$(jq -r '.sources[] | select(.name == "mozilla") | .key_url' "$staged/sources.lock")
curl -fsSL --connect-timeout 10 --max-time 30 -o "$temporary/mozilla-key" "$mozilla_key_url" || fail 'Mozilla signing key is unreachable'
mozilla_fingerprints=$(gpg --no-options --homedir "$temporary" --batch --show-keys --with-colons "$temporary/mozilla-key" | awk -F: '$1 == "fpr" {print $10}')
while read -r fingerprint; do
  printf '%s\n' "$mozilla_fingerprints" | awk -v wanted="$fingerprint" '$0 == wanted {found=1} END {exit !found}' || fail "Mozilla signing key fingerprint mismatch: $fingerprint"
done < <(jq -r '.sources[] | select(.name == "mozilla") | .key_fingerprints[]' "$staged/sources.lock")

api() { curl --unix-socket "$API_SOCKET" -fsS 'http://sprite/v1/services'; }
running_artifacts_match() {
  local pid cgroup executable digest members gateway_pid='' streamd_pid=''
  members=$(sudo cat /sys/fs/cgroup/svc.waywire/cgroup.procs 2>/dev/null) || return 1
  while read -r pid; do
    [[ "$pid" =~ ^[1-9][0-9]*$ ]] || continue
    if $service_replacement_requested; then
      case " $previous_service_pids " in *" $pid "*) continue ;; esac
    fi
    cgroup=$(sudo cat "/proc/$pid/cgroup" 2>/dev/null) || continue
    [ "$cgroup" = '0::/svc.waywire' ] || continue
    # Sprite permits cmdline inspection but denies /proc/PID/exe, even to root.
    # The launcher resolves current before exec, so argv names this release.
    IFS= read -r -d '' executable < <(sudo cat "/proc/$pid/cmdline" 2>/dev/null) || continue
    case "$executable" in
      "$RELEASE_DIR/bin/waywire-gateway"|"$RELEASE_DIR/bin/waywire-streamd") ;;
      *) continue ;;
    esac
    digest=$(sudo sha256sum "$executable" 2>/dev/null | awk '{print $1}') || continue
    if [ "$digest" = "$expected_gateway_sha" ]; then
      [ -z "$gateway_pid" ] || return 1
      gateway_pid=$pid
    fi
    if [ "$digest" = "$expected_streamd_sha" ]; then
      [ -z "$streamd_pid" ] || return 1
      streamd_pid=$pid
    fi
  done <<<"$members"
  [ -n "$gateway_pid" ] && [ -n "$streamd_pid" ] || return 1
  printf '%s:%s\n' "$gateway_pid" "$streamd_pid"
}
services_raw=$(api) || fail 'could not list Sprite services'
live_services=$(jq -ce 'if type != "array" then error("expected service array") else [.[] | {name,cmd,args:(.args // []),http_port:(.http_port // null),needs:(.needs // []),env:(.env // {}),dir:(.dir // null)}] end' <<<"$services_raw") || fail 'Sprite services response is invalid'
target_service=$(jq -cn --arg origin "$origin" '{name:"waywire",cmd:"/opt/waywire/current/bin/desktop.sh",args:[$origin],http_port:8080,needs:[],env:{},dir:"/home/sprite"}')
current_service=$(jq -c '.[] | select(.name == "waywire")' <<<"$live_services")
record=
if sudo test -f "$INSTALL_RECORD"; then
  record=$(sudo cat "$INSTALL_RECORD")
  [ "$(sudo stat -c %u "$INSTALL_RECORD")" = 0 ] || fail 'install.json must be owned by root'
  jq -e --arg owner "$OWNER_ID" '.schema == 1 and .owner == $owner and (.state == "pending" or .state == "committed") and (.services | type == "array")' <<<"$record" >/dev/null || fail 'install.json is not owned by this runtime'
elif sudo test -e "$CURRENT_LINK" || sudo test -L "$CURRENT_LINK"; then
  fail 'existing runtime pointer has no ownership record'
fi
# The API's service PID may not exist in this process namespace after restart.
# Use actual cgroup members to distinguish replacement processes.
previous_service_pids=$(sudo cat /sys/fs/cgroup/svc.waywire/cgroup.procs 2>/dev/null | tr '\n' ' ' || true)
if [ -n "$current_service" ]; then
  if [ -z "$record" ] || ! jq -e --argjson current "$current_service" --argjson target "$target_service" '.services == [$current] or (.state == "pending" and $current == $target)' <<<"$record" >/dev/null; then
    fail 'service name is foreign: waywire'
  fi
fi
while IFS= read -r definition; do
  name=$(jq -r .name <<<"$definition")
  if [ "$(jq -r '.http_port != null' <<<"$definition")" = true ] && [ "$name" != waywire ]; then
    fail "foreign HTTP service owns the Sprite URL: $name"
  fi
done < <(jq -c '.[]' <<<"$live_services")
listeners=$(ss -H -ltne 'sport = :8080')
if [ -n "$listeners" ]; then
  if [ -z "$current_service" ] || [ -z "$record" ] || ! jq -e --argjson current "$current_service" '.services == [$current]' <<<"$record" >/dev/null; then
    fail 'port 8080 is occupied by an unmanaged process'
  fi
  jq -e 'any(.[]; .name == "waywire" and .state.status == "running")' <<<"$services_raw" >/dev/null || fail 'port 8080 has no running owner service'
  awk '{found=0; for (i=1; i<=NF; i++) if ($i == "cgroup:/svc.waywire") found=1; if (!found) exit 1}' <<<"$listeners" || fail 'port 8080 is occupied by an unmanaged process'
fi
recovery=false
release_changed=true
if [ -n "$record" ]; then
  installed_release=$(jq -r .release <<<"$record")
  if [ "$installed_release" = "$RELEASE" ] && [ "$(jq -r .archive_sha256 <<<"$record")" != "$ARCHIVE_SHA256" ]; then
    fail 'this release is already recorded with a different archive; use a new version'
  fi
  if [ "$(jq -r .state <<<"$record")" = pending ]; then
    recovery=true
    # A corrected release may replace an owned pending install. Ownership and
    # service definitions were checked above; same-version hashes must match.
  fi
  [ "$installed_release" != "$RELEASE" ] || release_changed=false
fi

notice 'configuring verified apt sources and desktop packages'
sudo install -d -m 0755 /etc/apt/keyrings /etc/waywire
sudo install -m 0644 "$temporary/mozilla-key" /etc/apt/keyrings/packages.mozilla.org.asc
jq -r '.sources[] | "Types: deb\nURIs: \(.url)\nSuites: \(.suites | join(" "))\nComponents: \(.components | join(" "))\nSigned-By: \(.keyring)\n"' "$staged/sources.lock" >"$temporary/apt.sources"
printf '%s\n' 'Package: firefox' 'Pin: origin packages.mozilla.org' 'Pin-Priority: 1001' >"$temporary/apt.preferences"
sudo install -m 0644 "$temporary/apt.sources" /etc/waywire/apt.sources
sudo install -m 0644 "$temporary/apt.preferences" /etc/waywire/apt.preferences
apt_options=(-o Dir::Etc::sourcelist=/etc/waywire/apt.sources -o Dir::Etc::sourceparts=- -o Dir::Etc::preferences=/etc/waywire/apt.preferences -o Dir::Etc::preferencesparts=)
sudo env DEBIAN_FRONTEND=noninteractive apt-get "${apt_options[@]}" update
mapfile -t packages < <(jq -r '[.sources[].packages[]] | unique[]' "$staged/sources.lock")
sudo env DEBIAN_FRONTEND=noninteractive apt-get "${apt_options[@]}" install -y --no-install-recommends "${packages[@]}"
# shellcheck disable=SC2016 # dpkg-query expands its format variables
packages_json=$(printf '%s\n' "${packages[@]}" | sort -u | xargs dpkg-query -W -f='${binary:Package}\t${Version}\n' | jq -Rn '[inputs | split("\t")] | map({key:.[0],value:.[1]}) | from_entries')
jq -n --arg release "$RELEASE" --arg source "$(jq -r .source "$staged/manifest.json")" --arg observed_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson packages "$packages_json" '{release:$release,source:$source,observed_at:$observed_at,packages:$packages}' >"$temporary/version.json"

pending_service=$target_service
[ -z "$current_service" ] || pending_service=$current_service
pending=$(jq -n --arg owner "$OWNER_ID" --arg release "$RELEASE" --arg archive "$ARCHIVE_SHA256" --argjson service "$pending_service" '{schema:1,owner:$owner,state:"pending",release:$release,archive_sha256:$archive,services:[$service]}')
sudo install -d -o root -g root -m 0755 /var/lib/waywire /opt/waywire /opt/waywire/releases "$RELEASE_DIR" "$RELEASE_DIR/bin"
printf '%s\n' "$pending" >"$temporary/install.json"
sudo install -o root -g root -m 0644 "$temporary/install.json" "$INSTALL_RECORD.tmp"
sudo mv "$INSTALL_RECORD.tmp" "$INSTALL_RECORD"
failpoint after-pending
for path in bin/waywire-gateway bin/waywire-streamd bin/desktop.sh bin/session.py manifest.json sources.lock; do
  mode=444
  [[ "$path" != bin/* ]] || mode=555
  if ! sudo cmp -s "$staged/$path" "$RELEASE_DIR/$path" ||
     [ "$(sudo stat -c '%u:%g:%a' "$RELEASE_DIR/$path" 2>/dev/null || true)" != "0:0:$mode" ]; then
    release_changed=true
    sudo install -o root -g root -m "$mode" "$staged/$path" "$RELEASE_DIR/$path.new"
    sudo mv -f "$RELEASE_DIR/$path.new" "$RELEASE_DIR/$path"
  fi
done
sudo install -o root -g root -m 0644 "$temporary/version.json" "$VERSION_FILE.tmp"
sudo mv "$VERSION_FILE.tmp" "$VERSION_FILE"
sudo ln -sfn "releases/$RELEASE" /opt/waywire/current.new
sudo mv -Tf /opt/waywire/current.new "$CURRENT_LINK"

request=$(jq -c 'del(.name)' <<<"$target_service")
service_replacement_requested=false
if [ "$current_service" != "$target_service" ]; then
  curl --unix-socket "$API_SOCKET" -fsS -X PUT -H 'Content-Type: application/json' -d "$request" 'http://sprite/v1/services/waywire' -o /dev/null || fail 'could not create waywire service'
  service_replacement_requested=true
elif $recovery || $release_changed || [ "$(curl -fsS --max-time 2 http://127.0.0.1:8080/healthz 2>/dev/null || true)" != ok ]; then
  curl --unix-socket "$API_SOCKET" -fsS -X POST 'http://sprite/v1/services/waywire/restart' -o /dev/null || fail 'could not restart waywire service'
  service_replacement_requested=true
fi
failpoint after-service-1
healthy=false
for _ in $(seq 1 120); do
  candidate_services=$(api 2>/dev/null) || { sleep 1; continue; }
  jq -e 'any(.[]; .name == "waywire" and .state.status == "running")' <<<"$candidate_services" >/dev/null || { sleep 1; continue; }
  candidate_pair=$(running_artifacts_match) || { sleep 1; continue; }
  [ "$(curl -fsS --max-time 2 http://127.0.0.1:8080/healthz 2>/dev/null || true)" = ok ] || { sleep 1; continue; }
  # Keep both replacement executables stable across a full observation
  # interval; an old listener can answer while replacement starts.
  sleep 1
  api 2>/dev/null | jq -e 'any(.[]; .name == "waywire" and .state.status == "running")' >/dev/null || continue
  confirmed_pair=$(running_artifacts_match) || continue
  [ "$confirmed_pair" = "$candidate_pair" ] || continue
  [ "$(curl -fsS --max-time 2 http://127.0.0.1:8080/healthz 2>/dev/null || true)" = ok ] || continue
  healthy=true
  break
done
$healthy || fail 'waywire did not run the staged gateway and streamd within 120 seconds'
final=$(api | jq -ce '[.[] | {name,cmd,args:(.args // []),http_port:(.http_port // null),needs:(.needs // []),env:(.env // {}),dir:(.dir // null)} | select(.name == "waywire")]')
jq -en --argjson actual "$final" --argjson expected "[$target_service]" '$actual == $expected' >/dev/null || fail 'live waywire service does not match the installer definition'
final_listeners=$(ss -H -ltne 'sport = :8080')
[ "$(awk 'NF {count++} END {print count+0}' <<<"$final_listeners")" = 1 ] || fail 'waywire does not own exactly one HTTP listener'
awk '{found=0; for (i=1; i<=NF; i++) if ($i == "cgroup:/svc.waywire") found=1; if (!found) exit 1}' <<<"$final_listeners" || fail 'waywire HTTP listener is outside its owned service cgroup'
failpoint before-commit
committed=$(jq -c --argjson service "$target_service" '.state="committed" | .services=[$service]' <<<"$pending")
printf '%s\n' "$committed" >"$temporary/install-committed.json"
sudo install -o root -g root -m 0644 "$temporary/install-committed.json" "$INSTALL_RECORD.tmp"
sudo mv "$INSTALL_RECORD.tmp" "$INSTALL_RECORD"
notice "$RELEASE installed and healthy at $origin"
notice 'URL policy must remain auth=sprite and privateAccess=admins.'
