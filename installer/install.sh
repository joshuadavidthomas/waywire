#!/usr/bin/env bash
set -euo pipefail

readonly RELEASE='@SPRITE_DESKTOP_VERSION@'
readonly ARCHIVE_URL='@SPRITE_DESKTOP_ARCHIVE_URL@'
readonly ARCHIVE_SIZE='@SPRITE_DESKTOP_ARCHIVE_SIZE@'
readonly ARCHIVE_SHA256='@SPRITE_DESKTOP_ARCHIVE_SHA256@'
readonly OWNER_ID='dev.sprite-desktop.runtime'
readonly API_SOCKET='/.sprite/api.sock'
readonly INSTALL_RECORD='/var/lib/sprite-desktop/install.json'
readonly CONFIG_FILE='/etc/sprite-desktop/config.json'
readonly VERSION_FILE='/etc/sprite-desktop/version.json'
readonly RELEASE_DIR="/opt/sprite-desktop/releases/$RELEASE"
readonly CURRENT_LINK='/opt/sprite-desktop/current'
readonly V0_HASH='4d643f592c2f6d403db63907a1bfb2b5ec11f41ff88ff943c10a3962c480967c'
archive_path=
temporary=

fail() { printf 'sprite-desktop installer: %s\n' "$*" >&2; exit 1; }
notice() { printf 'sprite-desktop installer: %s\n' "$*"; }
failpoint() {
  if [ "${SPRITE_DESKTOP_FAILPOINT:-}" = "$1" ]; then
    fail "test failpoint: $1"
  fi
}
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
for command in sudo curl jq flock sha256sum stat tar gzip gpg ss dpkg; do
  command -v "$command" >/dev/null || fail "required command is missing: $command"
done
sudo -n true 2>/dev/null || fail 'the sprite user needs passwordless sudo'
# shellcheck disable=SC1091 # fixed system file on the supported Ubuntu image
. /etc/os-release
[ "${ID:-}" = ubuntu ] && [ "${VERSION_CODENAME:-}" = resolute ] || fail 'only Ubuntu 26.04 (resolute) is supported'
[ "$(dpkg --print-architecture)" = amd64 ] || fail 'only amd64 is supported'
# Fresh Sprites have /var/lock -> /run/lock without the target directory.
sudo install -d -m 1777 /run/lock
exec 9>/var/lock/sprite-desktop-install.lock
flock -x 9
[ -S "$API_SOCKET" ] || fail "Sprite API socket is missing: $API_SOCKET"

info=$(sprite-env info) || fail 'sprite-env info could not derive the canonical origin'
derived_origin=$(jq -er '.sprite_url | select(type == "string")' <<<"$info") || fail 'sprite-env info did not return sprite_url'
origin=${DESKTOP_ORIGIN:-$derived_origin}
if [ -n "${DESKTOP_ORIGIN:-}" ] && [ "$origin" != "$derived_origin" ]; then
  notice "warning: DESKTOP_ORIGIN differs from sprite-env info ($derived_origin)"
fi
jq -en --arg origin "$origin" '
  $origin | test("^https://[a-z0-9](?:[a-z0-9-]*[a-z0-9])?(?:\\.[a-z0-9](?:[a-z0-9-]*[a-z0-9])?)+$")
' | jq -e . >/dev/null || fail 'canonical origin must be an HTTPS origin without a port or path'

temporary=$(mktemp -d)
staged=$temporary/release
mkdir "$staged"
if [ -n "$archive_path" ]; then
  [ -f "$archive_path" ] || fail "archive does not exist: $archive_path"
  archive=$archive_path
else
  archive=$temporary/archive.tar.gz
  notice "downloading $ARCHIVE_URL"
  curl -fL --proto '=https' --tlsv1.2 --connect-timeout 10 --max-time 120 -o "$archive" "$ARCHIVE_URL" || fail 'release download failed'
fi
actual_size=$(stat -c %s "$archive")
[ "$actual_size" = "$ARCHIVE_SIZE" ] || fail "archive size mismatch: expected $ARCHIVE_SIZE, got $actual_size"
actual_sha=$(sha256sum "$archive" | awk '{print $1}')
[ "$actual_sha" = "$ARCHIVE_SHA256" ] || fail "archive SHA-256 mismatch: expected $ARCHIVE_SHA256, got $actual_sha"
tar -xzf "$archive" -C "$staged" --no-same-owner --no-same-permissions
for path in bin/sprite-desktop-bridge bin/desktop.sh manifest.json sources.lock; do
  [ -f "$staged/$path" ] || fail "archive is missing $path"
done
jq -e --arg release "$RELEASE" '
  .schema == 1 and .release == $release and
  .os == {id:"ubuntu",codename:"resolute",architecture:"amd64"} and
  (.services | length == 2) and
  (.services[0] == {name:"sprite-desktop",cmd:"/opt/sprite-desktop/current/bin/desktop.sh",args:[],http_port:null,needs:[],env:{},dir:"/home/sprite"}) and
  (.services[1] == {name:"sprite-desktop-bridge",cmd:"/opt/sprite-desktop/current/bin/sprite-desktop-bridge",args:["--config","/etc/sprite-desktop/config.json"],http_port:8080,needs:[],env:{},dir:"/home/sprite"})
' "$staged/manifest.json" >/dev/null || fail 'manifest release, platform, or service definitions are invalid'
jq -e '.schema == 1 and (.sources | type == "array") and (.glycin.version == "2.1.5+ds-0ubuntu0.1")' "$staged/sources.lock" >/dev/null || fail 'sources.lock is invalid or does not pin Glycin 2.1.5'

# Check every source and key before changing apt configuration or persistent files.
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
services_raw=$(api) || fail 'could not list Sprite services'
live_services=$(jq -ce 'if type != "array" then error("expected service array") else [.[] | {name,cmd,args:(.args // []),http_port:(.http_port // null),needs:(.needs // []),env:(.env // {}),dir:(.dir // null)}] end' <<<"$services_raw") || fail 'Sprite services response is invalid'
record=''
if sudo test -f "$INSTALL_RECORD"; then
  record=$(sudo cat "$INSTALL_RECORD")
  [ "$(sudo stat -c %u "$INSTALL_RECORD")" = 0 ] || fail 'install.json must be owned by root'
  jq -e --arg owner "$OWNER_ID" '.schema == 1 and .owner == $owner and (.state == "pending" or .state == "committed") and (.services | type == "array")' <<<"$record" >/dev/null || fail 'install.json is not owned by this runtime'
elif sudo test -e "$CURRENT_LINK" || sudo test -L "$CURRENT_LINK"; then
  fail 'existing runtime pointer has no ownership record'
fi
target_services=$(jq -c '.services' "$staged/manifest.json")

owned_definition() {
  local service=$1 definition=$2
  [ -n "$record" ] || return 1
  jq -e --arg name "$service" --argjson definition "$definition" '
    ([.services[]?] + [.previous_services[]?]) | any(.name == $name and . == $definition)
  ' <<<"$record" >/dev/null
}

v0_migration=false
while IFS= read -r definition; do
  name=$(jq -r .name <<<"$definition")
  case "$name" in
    sprite-desktop|sprite-desktop-bridge)
      owned_definition "$name" "$definition" || fail "service name is foreign: $name"
      ;;
    desktop)
      if [ "$(jq -r '.cmd == "/home/sprite/.local/bin/desktop.sh" and (.args | length == 0) and .http_port == null and .needs == [] and .env == {} and .dir == null' <<<"$definition")" = true ] &&
         [ -f /home/sprite/.local/bin/desktop.sh ] &&
         [ "$(sha256sum /home/sprite/.local/bin/desktop.sh | awk '{print $1}')" = "$V0_HASH" ]; then
        v0_migration=true
      else
        fail 'service desktop exists but is not the exact recognized v0 installation'
      fi
      ;;
  esac
  if [ "$(jq -r '.http_port != null' <<<"$definition")" = true ] && [ "$name" != sprite-desktop-bridge ]; then
    fail "foreign HTTP service owns the Sprite URL: $name"
  fi
done < <(jq -c '.[]' <<<"$live_services")

for port in 5900 8080; do
  listeners=$(ss -H -ltne "sport = :$port")
  [ -n "$listeners" ] || continue
  expected_name=sprite-desktop
  [ "$port" = 8080 ] && expected_name=sprite-desktop-bridge
  if [ "$port" = 5900 ] && $v0_migration; then expected_name=desktop; fi
  live_definition=$(jq -c --arg name "$expected_name" '.[] | select(.name == $name)' <<<"$live_services")
  if [ "$expected_name" != desktop ] && { [ -z "$live_definition" ] || ! owned_definition "$expected_name" "$live_definition"; }; then
    fail "port $port is occupied by an unmanaged process"
  fi
  service_pid=$(jq -r --arg name "$expected_name" '.[] | select(.name == $name) | .state.pid // 0' <<<"$services_raw")
  [ "${service_pid:-0}" -gt 0 ] || fail "port $port has no running owner service"
  # Sprite restricts /proc fd inspection even for root, so ss -p cannot
  # identify every listener. The kernel's socket cgroup names its service
  # directly, including children such as Xvnc.
  awk -v owner="cgroup:/svc.$expected_name" '{
    found=0
    for (i=1; i<=NF; i++) if ($i == owner) found=1
    if (!found) exit 1
  }' <<<"$listeners" || fail "port $port is occupied by an unmanaged process"
done
if [ -n "$record" ]; then
  installed_release=$(jq -r .release <<<"$record")
  if [ "$installed_release" = "$RELEASE" ] && [ "$(jq -r .archive_sha256 <<<"$record")" != "$ARCHIVE_SHA256" ]; then
    fail 'this release is already recorded with a different archive; use a new version'
  fi
  "$staged/bin/sprite-desktop-bridge" --check-upgrade-from "$installed_release"
fi

# A pending run keeps its recorded config inputs. Fresh and committed runs use
# the currently derived canonical origin.
recovery=false
if [ -n "$record" ] && [ "$(jq -r .state <<<"$record")" = pending ]; then
  recovery=true
  jq -e --arg release "$RELEASE" --arg hash "$ARCHIVE_SHA256" '.release == $release and .archive_sha256 == $hash' <<<"$record" >/dev/null || fail 'finish the pending install with its original versioned installer and archive before upgrading'
  config_json=$(jq -c '.config' <<<"$record")
  [ "$(jq -r .origin <<<"$config_json")" = "$origin" ] || notice "recovering the pending target origin recorded by the prior run"
else
  config_json=$(jq -nc --arg origin "$origin" '{origin:$origin,socket_path:"/tmp/sprite-desktop/rfb.sock",ping_interval:"20s",keepalive:{task:false}}')
fi
config_rendered=$temporary/config.json
jq . <<<"$config_json" >"$config_rendered"
config_hash=$(sha256sum "$config_rendered" | awk '{print $1}')
"$staged/bin/sprite-desktop-bridge" --check-config --config "$config_rendered"
binary_version=$("$staged/bin/sprite-desktop-bridge" --version | awk '{print $1}')
[ "$binary_version" = "$RELEASE" ] || fail "bridge binary reports $binary_version, expected $RELEASE"
bash -n "$staged/bin/desktop.sh"

release_changed=true
if sudo test -d "$RELEASE_DIR" && sudo diff -qr "$staged" "$RELEASE_DIR" >/dev/null; then
  release_changed=false
fi
if [ -n "$record" ] && [ "$(jq -r .release <<<"$record")" != "$RELEASE" ]; then release_changed=true; fi
notice 'installation or upgrade may end an active desktop session'

notice 'configuring verified apt sources and desktop packages'
sudo install -d -m 0755 /etc/apt/keyrings /etc/sprite-desktop
sudo install -m 0644 "$temporary/mozilla-key" /etc/apt/keyrings/packages.mozilla.org.asc
jq -r '.sources[] | "Types: deb\nURIs: \(.url)\nSuites: \(.suites | join(" "))\nComponents: \(.components | join(" "))\nSigned-By: \(.keyring)\n"' "$staged/sources.lock" >"$temporary/apt.sources"
printf '%s\n' 'Package: firefox' 'Pin: origin packages.mozilla.org' 'Pin-Priority: 1001' '' 'Package: *' 'Pin: release a=resolute-proposed' 'Pin-Priority: -1' '' 'Package: libglycin-2-0 glycin-loaders glycin-thumbnailers' 'Pin: release a=resolute-proposed' 'Pin-Priority: 1001' >"$temporary/apt.preferences"
sudo install -m 0644 "$temporary/apt.sources" /etc/sprite-desktop/apt.sources
sudo install -m 0644 "$temporary/apt.preferences" /etc/sprite-desktop/apt.preferences
# Do not let unrelated repositories or pins supply the installer's packages.
# These options leave the user's global apt configuration untouched.
apt_options=(-o Dir::Etc::sourcelist=/etc/sprite-desktop/apt.sources -o Dir::Etc::sourceparts=- -o Dir::Etc::preferences=/etc/sprite-desktop/apt.preferences -o Dir::Etc::preferencesparts=)
sudo env DEBIAN_FRONTEND=noninteractive apt-get "${apt_options[@]}" update
mapfile -t packages < <(jq -r '[.sources[].packages[]] | unique[]' "$staged/sources.lock")
glycin_version=$(jq -r .glycin.version "$staged/sources.lock")
install_packages=()
for package in "${packages[@]}"; do
  case "$package" in
    libglycin-2-0|glycin-loaders|glycin-thumbnailers) install_packages+=("$package=$glycin_version") ;;
    *) install_packages+=("$package") ;;
  esac
done
sudo env DEBIAN_FRONTEND=noninteractive apt-get "${apt_options[@]}" install -y --allow-downgrades --no-install-recommends "${install_packages[@]}"
while read -r package; do
  [ "$(dpkg-query -W -f='${Version}' "$package")" = "$glycin_version" ] || fail "$package is not the locked Glycin version $glycin_version"
done < <(jq -r '.glycin.packages[]' "$staged/sources.lock")

# Seed only absent user preferences.
if [ ! -e /home/sprite/.config/xfce4/helpers.rc ]; then
  install -d -m 0755 /home/sprite/.config/xfce4
  printf 'WebBrowser=firefox\n' > /home/sprite/.config/xfce4/helpers.rc
fi

version_rendered=$temporary/version.json
# dpkg-query expands the dollar expressions in its own format string.
# shellcheck disable=SC2016
packages_json=$(printf '%s\n' "${packages[@]}" | sort -u | xargs dpkg-query -W -f='${binary:Package}\t${Version}\n' | jq -Rn '[inputs | split("\t")] | map({key:.[0],value:.[1]}) | from_entries')
version_changed=true
if sudo test -f "$VERSION_FILE" && sudo jq -e --arg release "$RELEASE" --argjson packages "$packages_json" '.release == $release and .packages == $packages and (try ((.observed_at | fromdateiso8601 | todateiso8601) == .observed_at and .observed_at != "0001-01-01T00:00:00Z") catch false)' "$VERSION_FILE" >/dev/null; then
  # The redirect writes the sprite-owned temporary file; sudo reads the root file.
  # shellcheck disable=SC2024
  sudo cat "$VERSION_FILE" >"$version_rendered"
  version_changed=false
else
  jq -n --arg release "$RELEASE" --arg observed_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson packages "$packages_json" '{release:$release,observed_at:$observed_at,packages:$packages}' >"$version_rendered"
fi
previous_services='[]'
if [ -n "$record" ]; then
  if [ "$(jq -r .state <<<"$record")" = committed ]; then
    previous_services=$(jq -c .services <<<"$record")
  else
    previous_services=$(jq -c '.previous_services // []' <<<"$record")
  fi
fi
pending=$(jq -n --arg owner "$OWNER_ID" --arg release "$RELEASE" --arg hash "$config_hash" --arg archive "$ARCHIVE_SHA256" --argjson config "$config_json" --argjson services "$target_services" --argjson previous "$previous_services" '{schema:1,owner:$owner,state:"pending",release:$release,archive_sha256:$archive,config_hash:$hash,config:$config,services:$services,previous_services:$previous}')
sudo install -d -o root -g root -m 0755 /etc/sprite-desktop /var/lib/sprite-desktop /opt/sprite-desktop
printf '%s\n' "$pending" >"$temporary/install.json"
sudo install -o root -g root -m 0644 "$temporary/install.json" "$INSTALL_RECORD.tmp"
sudo mv "$INSTALL_RECORD.tmp" "$INSTALL_RECORD"
failpoint after-pending

# Replace each verified file by rename, never overwrite a running executable
# or remove the directory selected by current. A pending rerun repairs forward.
sudo install -d -o root -g root -m 0755 /opt/sprite-desktop/releases "$RELEASE_DIR" "$RELEASE_DIR/bin"
for path in bin/sprite-desktop-bridge bin/desktop.sh manifest.json sources.lock; do
  mode=0444
  [[ "$path" != bin/* ]] || mode=0555
  if ! sudo cmp -s "$staged/$path" "$RELEASE_DIR/$path"; then
    sudo install -o root -g root -m "$mode" "$staged/$path" "$RELEASE_DIR/$path.new"
    sudo mv -f "$RELEASE_DIR/$path.new" "$RELEASE_DIR/$path"
  fi
  sudo chown root:root "$RELEASE_DIR/$path"
  sudo chmod "$mode" "$RELEASE_DIR/$path"
done
config_changed=true
if sudo test -f "$CONFIG_FILE" && sudo cmp -s "$config_rendered" "$CONFIG_FILE"; then config_changed=false; fi
if $config_changed; then
  sudo install -o root -g root -m 0644 "$config_rendered" "$CONFIG_FILE.tmp"
  sudo mv "$CONFIG_FILE.tmp" "$CONFIG_FILE"
fi
if $version_changed; then
  sudo install -o root -g root -m 0644 "$version_rendered" "$VERSION_FILE.tmp"
  sudo mv "$VERSION_FILE.tmp" "$VERSION_FILE"
fi
sudo chown root:root "$CONFIG_FILE" "$VERSION_FILE"
sudo chmod 0644 "$CONFIG_FILE" "$VERSION_FILE"
sudo ln -sfn "releases/$RELEASE" /opt/sprite-desktop/current.new
sudo mv -Tf /opt/sprite-desktop/current.new "$CURRENT_LINK"

if $v0_migration; then
  curl --unix-socket "$API_SOCKET" -fsS -X DELETE 'http://sprite/v1/services/desktop' -o /dev/null || fail 'could not remove recognized v0 service'
fi
prehealthy=false
health=$(curl -fsS --max-time 2 http://127.0.0.1:8080/healthz 2>/dev/null || true)
if jq -e --arg release "$RELEASE" '.release == $release and .bridge == "ok" and .rfb == "listening"' <<<"$health" >/dev/null 2>&1; then
  prehealthy=true
fi
index=0
while IFS= read -r target; do
  index=$((index + 1))
  name=$(jq -r .name <<<"$target")
  request=$(jq -c 'del(.name) | if .http_port == null then del(.http_port) else . end' <<<"$target")
  current=$(jq -c --arg name "$name" '.[] | select(.name == $name)' <<<"$live_services")
  definition_changed=true
  if [ -n "$current" ] && jq -en --argjson current "$current" --argjson target "$target" '$current == $target' >/dev/null; then definition_changed=false; fi
  if $definition_changed; then
    curl --unix-socket "$API_SOCKET" -fsS -X PUT -H 'Content-Type: application/json' -d "$request" "http://sprite/v1/services/$name" -o /dev/null || fail "could not create service $name"
  elif $recovery || $release_changed || $version_changed || ! $prehealthy || { [ "$name" = sprite-desktop-bridge ] && $config_changed; }; then
    curl --unix-socket "$API_SOCKET" -fsS -X POST "http://sprite/v1/services/$name/restart" -o /dev/null || fail "could not restart service $name"
  fi
  failpoint "after-service-$index"
done < <(jq -c '.[]' <<<"$target_services")

healthy=false
for _ in $(seq 1 120); do
  health=$(curl -fsS --max-time 2 http://127.0.0.1:8080/healthz 2>/dev/null || true)
  if jq -e --arg release "$RELEASE" '.release == $release and .bridge == "ok" and .rfb == "listening"' <<<"$health" >/dev/null 2>&1; then
    healthy=true
    break
  fi
  sleep 1
done
$healthy || fail 'services did not report a healthy target release within 120 seconds'

final_services=$(api)
final_normalized=$(jq -ce '[.[] | {name,cmd,args:(.args // []),http_port:(.http_port // null),needs:(.needs // []),env:(.env // {}),dir:(.dir // null)} | select(.name == "sprite-desktop" or .name == "sprite-desktop-bridge")] | sort_by(.name)' <<<"$final_services")
expected_normalized=$(jq -c 'sort_by(.name)' <<<"$target_services")
jq -en --argjson actual "$final_normalized" --argjson expected "$expected_normalized" '$actual == $expected' >/dev/null || fail 'live service definitions do not match the release manifest'
ss -H -ltn 'sport = :5900' | awk '{print $4}' | awk 'NF && $0 !~ /127\.0\.0\.1:5900$/ && $0 !~ /\[::1\]:5900$/ {bad=1} END {exit bad}' || fail 'RFB has a non-loopback listener'
[ "$(ss -H -ltn 'sport = :8080' | awk 'END {print NR}')" = 1 ] || fail 'bridge does not own exactly one HTTP listener'
[ -S /tmp/sprite-desktop/rfb.sock ] && [ "$(stat -c %a /tmp/sprite-desktop/rfb.sock)" = 600 ] || fail 'RFB Unix socket is missing or has the wrong mode'

failpoint before-commit
committed=$(jq -c '.state="committed" | del(.previous_services)' <<<"$pending")
printf '%s\n' "$committed" >"$temporary/install-committed.json"
sudo install -o root -g root -m 0644 "$temporary/install-committed.json" "$INSTALL_RECORD.tmp"
sudo mv "$INSTALL_RECORD.tmp" "$INSTALL_RECORD"
notice "$RELEASE installed and healthy at $origin"
notice 'Verify URL auth from outside the Sprite: auth=sprite, private_access=admins, and unauthenticated HTTP/WebSocket requests redirect to Fly login.'
