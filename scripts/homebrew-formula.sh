#!/usr/bin/env bash
# Generate the formula from release archives, including shell completions.
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
printf '%s\n' '  license "Apache-2.0"'
for os in macos linux; do
  echo "  on_$os do"
  for cpu in arm intel; do
    arch=arm64; [[ $cpu != intel ]] || arch=x64
    case "$os-$arch" in
      macos-arm64) target=aarch64-apple-darwin ;;
      macos-x64) target=x86_64-apple-darwin ;;
      linux-arm64) target=aarch64-unknown-linux-gnu ;;
      linux-x64) target=x86_64-unknown-linux-gnu ;;
    esac
    asset="ego-$target.tar.gz"
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
    bin.install "ego"
    bash_completion.install "completions/ego.bash" => "ego"
    zsh_completion.install "completions/_ego"
    fish_completion.install "completions/ego.fish"
    (pkgshare/"completions").install "completions/_ego.ps1", "completions/ego.elv"
  end

  def caveats
    <<~TEXT
      ego stores nothing until first use; user state goes to ~/.ego (or $EGO_HOME).
      First setup:
        1. Make one provider usable: export its API key (for example
           ANTHROPIC_API_KEY or OPENAI_API_KEY), run `ego auth login PROVIDER`
           for a subscription, or start a local Ollama.
        2. Run `ego setup` to review providers and save defaults, or run `ego`
           in a repository to choose a model.
    TEXT
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/ego --version")
  end
end
EOF
