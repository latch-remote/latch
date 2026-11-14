#!/usr/bin/env bash
# macOS-native checks. Sources helpers without downloading or installing an app.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/scripts/install-macos-release.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

DIGEST="$(printf abc | shasum -a 256 | awk '{print $1}')"
cat > "$TMP/good.json" <<EOF
{"version":"6.0.0","tag_name":"v6.0.0","prerelease":false,"assets":[
{"name":"latch-macos-arm64.tar.gz","url":"https://github.com/latch-remote/latch-releases/releases/download/v6.0.0/latch-macos-arm64.tar.gz","size":3,"digest":"sha256:$DIGEST"},
{"name":"latch-host.exe","url":"https://github.com/latch-remote/latch-releases/releases/download/v6.0.0/latch-host.exe","size":3,"digest":"sha256:$DIGEST"}]}
EOF
read_manifest "$TMP/good.json" > "$TMP/asset"
[[ "$(sed -n '1p' "$TMP/asset")" == 'https://github.com/latch-remote/latch-releases/releases/download/v6.0.0/latch-macos-arm64.tar.gz' ]]
[[ "$(sed -n '2p' "$TMP/asset")" == 3 ]]
[[ "$(sed -n '3p' "$TMP/asset")" == "$DIGEST" ]]

reject_manifest() {
  if read_manifest "$TMP/bad.json" >/dev/null 2>&1; then
    echo "accepted invalid manifest ($1)" >&2
    exit 1
  fi
}

for key in version assets.0.digest assets.0.size; do
  cp "$TMP/good.json" "$TMP/bad.json"
  /usr/bin/plutil -remove "$key" "$TMP/bad.json"
  reject_manifest "missing $key, including a valid following asset hash"
done
for digest in sha256:abc "sha256:$(printf '%064d' 0 | tr 0 g)" md5:abc; do
  cp "$TMP/good.json" "$TMP/bad.json"
  /usr/bin/plutil -replace assets.0.digest -string "$digest" "$TMP/bad.json"
  reject_manifest "malformed hash"
done
for url in \
  'http://github.com/latch-remote/latch-releases/releases/download/v6.0.0/latch-macos-arm64.tar.gz' \
  'https://github.com.evil.example/asset' \
  'https://github.com@evil.example/asset' \
  'https://github.com/latch-remote/latch-releases/releases/latest/download/latch-macos-arm64.tar.gz' \
  'https://github.com/latch-remote/latch-releases/releases/download/v6.0.1/latch-macos-arm64.tar.gz' \
  'https://github.com/latch-remote/latch-releases/releases/download/v6.0.0/../latch-macos-arm64.tar.gz'; do
  cp "$TMP/good.json" "$TMP/bad.json"
  /usr/bin/plutil -replace assets.0.url -string "$url" "$TMP/bad.json"
  reject_manifest "unbound URL"
done
for version in v6.0.0 6.0.0/../latest 06.0.0 6.0.1; do
  cp "$TMP/good.json" "$TMP/bad.json"
  /usr/bin/plutil -replace version -string "$version" "$TMP/bad.json"
  reject_manifest "invalid or mismatched version"
done
for size in 0 -1 536870913; do
  cp "$TMP/good.json" "$TMP/bad.json"
  /usr/bin/plutil -replace assets.0.size -integer "$size" "$TMP/bad.json"
  reject_manifest "invalid size"
done
cp "$TMP/good.json" "$TMP/bad.json"
/usr/bin/plutil -replace assets.0.size -string 3 "$TMP/bad.json"
reject_manifest "size has wrong JSON type"
cp "$TMP/good.json" "$TMP/bad.json"
/usr/bin/plutil -replace assets.0.name -string ../latch-macos-arm64.tar.gz "$TMP/bad.json"
reject_manifest "path traversal"
cp "$TMP/good.json" "$TMP/bad.json"
/usr/bin/plutil -replace assets.1 -json "$(/usr/bin/plutil -extract assets.0 json -o - "$TMP/good.json")" "$TMP/bad.json"
reject_manifest "duplicate asset"

printf abc > "$TMP/download"
verify_download "$TMP/download" 3 "$DIGEST"
if verify_download "$TMP/download" 4 "$DIGEST"; then exit 1; fi
printf abd > "$TMP/download"
if verify_download "$TMP/download" 3 "$DIGEST"; then exit 1; fi

approved_download_url 'https://release-assets.githubusercontent.com/file?signature=public'
for url in 'http://github.com/file' 'https://github.com:443/file' \
  'https://github.com@evil.example/file' 'https://github.com.evil.example/file' \
  'https://github.com/\evil' $'https://github.com/file\r\nInjected: yes'; do
  if approved_download_url "$url"; then
    echo "accepted invalid redirect URL" >&2
    exit 1
  fi
done
echo 'release manifest and installer checks passed'
