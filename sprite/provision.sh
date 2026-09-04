#!/usr/bin/env bash
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive

sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  tigervnc-standalone-server tigervnc-common \
  xfce4 xfce4-terminal dbus-x11 xubuntu-wallpapers \
  fonts-dejavu-core xdg-utils curl ca-certificates

# Ubuntu 26.04 shipped Glycin 2.1.1, whose image-loader sandbox cannot
# create a nested user namespace in a Sprite. The verified 2.1.5 SRU can
# keep its seccomp boundary while running without bubblewrap when SNAP is set.
. /etc/os-release
if [ "${VERSION_CODENAME:-}" = "resolute" ]; then
  case "$(dpkg --print-architecture)" in
    amd64 | i386) ubuntu_mirror="http://archive.ubuntu.com/ubuntu" ;;
    *) ubuntu_mirror="http://ports.ubuntu.com/ubuntu-ports" ;;
  esac
  printf 'Types: deb\nURIs: %s\nSuites: resolute-proposed\nComponents: main\nSigned-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\n' "$ubuntu_mirror" \
    | sudo tee /etc/apt/sources.list.d/resolute-proposed.sources >/dev/null
  printf 'Package: *\nPin: release a=resolute-proposed\nPin-Priority: 400\n\nPackage: libglycin-2-0 glycin-loaders glycin-thumbnailers\nPin: release a=resolute-proposed\nPin-Priority: 1001\n' \
    | sudo tee /etc/apt/preferences.d/resolute-proposed >/dev/null
fi

sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://packages.mozilla.org/apt/repo-signing-key.gpg \
  | sudo tee /etc/apt/keyrings/packages.mozilla.org.asc >/dev/null
echo "deb [signed-by=/etc/apt/keyrings/packages.mozilla.org.asc] https://packages.mozilla.org/apt mozilla main" \
  | sudo tee /etc/apt/sources.list.d/mozilla.list >/dev/null
printf 'Package: *\nPin: origin packages.mozilla.org\nPin-Priority: 1000\n' \
  | sudo tee /etc/apt/preferences.d/mozilla >/dev/null
sudo apt-get update
sudo apt-get install -y firefox
if [ "${VERSION_CODENAME:-}" = "resolute" ]; then
  sudo apt-get install -y libglycin-2-0 glycin-loaders glycin-thumbnailers
fi

mkdir -p "$HOME/.vnc" "$HOME/.local/bin" "$HOME/.config/xfce4"
printf 'WebBrowser=firefox\n' >"$HOME/.config/xfce4/helpers.rc"
install -m 0755 "$HOME/desktop.sh" "$HOME/.local/bin/desktop.sh"
