#!/usr/bin/env bash
# Package a built target without changing the executable consumers download.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
target=${1:?usage: package-release.sh <target> <platform-name>}
name=${2:?usage: package-release.sh <target> <platform-name>}
exe=ego
suffix=
case "$target" in *-windows-*) exe=ego.exe; suffix=.exe ;; esac
asset="ego-$name$suffix"
archive="ego-$target"
mkdir -p dist
stage=$(mktemp -d "dist/.package.XXXXXX")
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/$archive"
cp "target/$target/release/$exe" README.md "$stage/$archive/"
cp "target/$target/release/$exe" "dist/$asset"
if [[ -n $suffix ]]; then
  (cd "$stage" && 7z a -tzip "../$archive.zip" "$archive" > /dev/null)
  archive+=.zip
else
  tar -czf "dist/$archive.tar.gz" -C "$stage" "$archive"
  archive+=.tar.gz
fi
(cd dist
  for file in "$asset" "$archive"; do
    if command -v sha256sum > /dev/null; then
      sha256sum "$file" > "$file.sha256"
    else
      shasum -a 256 "$file" > "$file.sha256"
    fi
    cat "$file.sha256"
  done
)
