#!/usr/bin/env bash
# Generate the formula from the same plain-binary checksums as the release.
set -euo pipefail
tag=${1:?usage: homebrew-formula.sh <tag> <owner/repo> <asset-directory>}
repository=${2:?}
directory=${3:?}
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo 'Homebrew formula requires a stable vX.Y.Z tag' >&2; exit 2;
}
[[ $repository =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || exit 2
printf '%s\n' 'class Ego < Formula' '  desc "Provider-agnostic coding agent harness"'
printf '  homepage "https://github.com/%s"\n' "$repository"
printf '  version "%s"\n' "${tag#v}"
printf '%s\n' '  license "MIT"'
for os in macos linux; do
  echo "  on_$os do"
  for cpu in arm intel; do
    arch=arm64; [[ $cpu != intel ]] || arch=x64
    asset="ego-$os-$arch"
    read -r checksum filename < "$directory/$asset.sha256"
    [[ $checksum =~ ^[0-9a-fA-F]{64}$ && $filename == "$asset" ]] || {
      echo "Invalid checksum file for $asset" >&2; exit 2;
    }
    printf '    on_%s do\n' "$cpu"
    printf '      url "https://github.com/%s/releases/download/%s/%s"\n' "$repository" "$tag" "$asset"
    printf '      sha256 "%s"\n' "$checksum"
    echo '    end'
  done
  echo '  end'
done
cat <<'EOF'

  def install
    bin.install cached_download => "ego"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/ego --version")
  end
end
EOF
