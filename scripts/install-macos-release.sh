#!/usr/bin/env bash
# Install the prebuilt Mac app from the latest GitHub release without the
# Gatekeeper "damaged" dialog: files fetched from the command line carry no
# quarantine flag, unlike a browser download. Usage, from a checkout or a
# copy of this file:
#
#   bash scripts/install-macos-release.sh
#
set -euo pipefail
# These helpers also run in the focused manifest checks when this file is sourced.
manifest_value() {
  /usr/bin/plutil -extract "$2" raw -expect "$3" -o - "$1"
}

approved_download_url() {
  case "$1" in
    https://github.com/*|https://release-assets.githubusercontent.com/*|https://objects.githubusercontent.com/*|https://github-releases.githubusercontent.com/*) ;;
    *) return 1 ;;
  esac
  [[ "$1" != *[$'\r\n\t ']* && "$1" != *'#'* && "$1" != *\\* ]]
}

# Follow redirects explicitly so every destination uses an approved HTTPS
# origin. --disable comes first to ignore curlrc credentials and options.
download_public() {
  local url="$1" dest="$2" limit="$3" status location hop
  for ((hop=0; hop<5; hop++)); do
    approved_download_url "$url" || { echo "unapproved release download origin" >&2; return 1; }
    status=$(/usr/bin/curl --disable --silent --show-error --proto '=https' \
      --connect-timeout 30 --max-time 120 --max-filesize "$limit" \
      --dump-header "$dest.headers" --output "$dest" --write-out '%{http_code}' "$url") || return 1
    case "$status" in
      200) rm -f "$dest.headers"; return 0 ;;
      301|302|303|307|308)
        location=$(awk 'tolower($0) ~ /^location:/ {
          sub(/^[^:]*:[ \t]*/, ""); sub(/\r$/, ""); value=$0; count++
        } END { if (count != 1) exit 1; print value }' "$dest.headers") || return 1
        case "$location" in
          https://*) url="$location" ;;
          //*) return 1 ;;
          /*) url="${url#https://}"; url="https://${url%%/*}$location" ;;
          *) return 1 ;;
        esac
        ;;
      *) echo "release download returned HTTP $status" >&2; return 1 ;;
    esac
  done
  echo "too many release redirects" >&2
  return 1
}

# Read typed values from the same asset object. A missing hash can never
# borrow the following asset's hash, even if the JSON uses one line.
read_manifest() {
  local file="$1" version tag count i name url size digest prerelease seen='|' found=0
  local selected_url='' selected_size='' selected_digest=''
  version=$(manifest_value "$file" version string) || return 1
  tag=$(manifest_value "$file" tag_name string) || return 1
  prerelease=$(manifest_value "$file" prerelease bool) || return 1
  [[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ && "$tag" == "v$version" && "$prerelease" == false ]] || return 1
  if /usr/bin/plutil -type draft "$file" >/dev/null 2>&1; then
    [[ "$(manifest_value "$file" draft bool)" == false ]] || return 1
  fi
  count=$(manifest_value "$file" assets array) || return 1
  [[ "$count" -gt 0 ]] || return 1
  for ((i=0; i<count; i++)); do
    name=$(manifest_value "$file" "assets.$i.name" string) || return 1
    url=$(manifest_value "$file" "assets.$i.url" string) || return 1
    size=$(manifest_value "$file" "assets.$i.size" integer) || return 1
    digest=$(manifest_value "$file" "assets.$i.digest" string) || return 1
    [[ "$name" =~ ^[a-zA-Z0-9][a-zA-Z0-9._-]*$ && "$seen" != *"|$name|"* ]] || return 1
    seen="$seen$name|"
    [[ "$url" == "https://github.com/latch-remote/latch-releases/releases/download/$tag/$name" ]] || return 1
    [[ "$size" =~ ^[0-9]+$ && "$size" -gt 0 && "$size" -le 536870912 ]] || return 1
    [[ "$digest" =~ ^sha256:[a-fA-F0-9]{64}$ ]] || return 1
    if [[ "$name" == latch-macos-arm64.tar.gz ]]; then
      found=$((found+1))
      selected_url="$url" selected_size="$size" selected_digest="${digest#sha256:}"
    fi
  done
  [[ "$found" == 1 ]] || return 1
  printf '%s\n%s\n%s\n' "$selected_url" "$selected_size" "$selected_digest"
}

verify_download() {
  local file="$1" size="$2" digest="$3" got
  [[ "$(stat -f %z "$file")" == "$size" ]] || return 1
  got=$(shasum -a 256 "$file" | awk '{print $1}') || return 1
  [[ "$(printf '%s' "$got" | tr 'A-F' 'a-f')" == "$(printf '%s' "$digest" | tr 'A-F' 'a-f')" ]]
}

if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
  return 0
fi

MANIFEST="https://github.com/latch-remote/latch-releases/releases/latest/download/release.json"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "checking the public release manifest"
download_public "$MANIFEST" "$TMP/release.json" 1048576
if ! read_manifest "$TMP/release.json" > "$TMP/asset"; then
  echo "release manifest has invalid version, URL, size, or SHA-256; refusing to install" >&2
  exit 1
fi
{
  IFS= read -r URL
  IFS= read -r SIZE
  IFS= read -r DIGEST
} < "$TMP/asset"
echo "downloading $URL"
download_public "$URL" "$TMP/latch-macos-arm64.tar.gz" "$SIZE"
if ! verify_download "$TMP/latch-macos-arm64.tar.gz" "$SIZE" "$DIGEST"; then
  echo "download does not match the manifest size and SHA-256; refusing to install" >&2
  exit 1
fi

tar xzf "$TMP/latch-macos-arm64.tar.gz" -C "$TMP"
APP="$TMP/Latch.app"
if [[ ! -x "$APP/Contents/MacOS/Latch" ]]; then
  echo "the download holds no Latch.app" >&2
  exit 1
fi
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true
codesign --verify --deep --strict "$APP"

DEST="/Applications/Latch.app"
if [[ ! -w /Applications ]]; then
  echo "this account cannot write to /Applications." >&2
  echo "Run the installer from an administrator account. Latch's login item and" >&2
  echo "updater expect the app in /Applications, so it is not installed elsewhere." >&2
  exit 1
fi
STAGE="/Applications/.Latch-install-$$.app"
rm -rf "$STAGE"
cp -R "$APP" "$STAGE"
# Swap last: a failed copy must not delete the live app.
if [[ -d "$DEST" ]]; then
  PREV="/Applications/.Latch-previous-$$.app"
  rm -rf "$PREV"
  mv "$DEST" "$PREV"
  if ! mv "$STAGE" "$DEST"; then
    mv "$PREV" "$DEST"
    echo "could not replace /Applications/Latch.app; previous copy restored" >&2
    exit 1
  fi
  rm -rf "$PREV"
else
  mv "$STAGE" "$DEST"
fi
# An install from before 4.1: stop its
# background job, drop its login item and remove its app, so only Latch is
# left. The new app registers its own login item when it first runs.
OLD_AGENT="gui/$(id -u)/dev.brolink.node"
launchctl bootout "$OLD_AGENT" >/dev/null 2>&1 || true
rm -f "$HOME/Library/LaunchAgents/dev.brolink.node.plist"
rm -rf /Applications/BroLink.app
# Retire the previous Latch login item; the app registers its current one.
launchctl bootout "gui/$(id -u)/com.bardbro.latch.node" >/dev/null 2>&1 || true
rm -f "$HOME/Library/LaunchAgents/com.bardbro.latch.node.plist"
# The background service keeps running the old binary until it restarts:
# replacing the bundle does not stop it, and launchd restarts it only after
# a failure. Restart it through launchd so it runs the new one now.
AGENT="gui/$(id -u)/io.github.MrBeldum.Latch.node"
if launchctl print "$AGENT" >/dev/null 2>&1; then
  launchctl kickstart -k "$AGENT" && echo "restarted the Latch background service"
fi
echo "installed $DEST"
open "$DEST"
