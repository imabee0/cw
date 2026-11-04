#!/usr/bin/env bash
# Install the newest cw release into ~/.local/bin.
#
#   curl -fsSL https://raw.githubusercontent.com/imabee0/cw/main/install/cw.sh | bash
#
# Finds the newest `cw-vX.Y.Z` GitHub Release, downloads the archive for this
# OS and CPU, verifies its SHA256 against the published checksum, and installs
# the binary atomically. Re-running it upgrades in place. The asset names are
# the contract `.github/workflows/ci.yml` (build job) and `src/selfupdate.rs`
# share: cw-<target-triple>.tar.gz and cw-<target-triple>.tar.gz.sha256.
set -euo pipefail

repo="imabee0/cw"
component="cw"
bin_dir="$HOME/.local/bin"

die() {
  printf 'install %s: %s\n' "$component" "$*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not on PATH"
}

need curl
need tar
need uname

os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
  Linux/x86_64 | Linux/amd64) target="x86_64-unknown-linux-gnu" ;;
  Darwin/arm64 | Darwin/aarch64) target="aarch64-apple-darwin" ;;
  *) die "no prebuilt binary for $os/$arch; build from source: cargo install --git https://github.com/$repo --locked" ;;
esac

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  die "need sha256sum or shasum to verify the download"
fi

# Newest cw-vX.Y.Z by numeric version, not by publish date. The releases list
# omits drafts for unauthenticated callers; cw never publishes prereleases.
releases=$(curl -fsSL -H "Accept: application/vnd.github+json" \
  "https://api.github.com/repos/$repo/releases?per_page=100") ||
  die "could not list releases from api.github.com (offline, or rate-limited?)"
# grep -o splits every tag_name onto its own line whatever the JSON's layout.
version=$(printf '%s\n' "$releases" |
  grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' |
  sed -n "s/.*\"$component-v\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)\"\$/\1/p" |
  sort -t. -k1,1n -k2,2n -k3,3n | tail -n 1 || true)
[ -n "$version" ] || die "no $component-vX.Y.Z release found in $repo"

tag="$component-v$version"
archive="$component-$target.tar.gz"
base="https://github.com/$repo/releases/download/$tag"

work=$(mktemp -d "${TMPDIR:-/tmp}/$component-install.XXXXXX")
trap 'rm -rf "$work"' EXIT

printf 'Downloading %s %s for %s\n' "$component" "$version" "$target"
curl -fsSL -o "$work/$archive" "$base/$archive" || die "download failed: $base/$archive"
curl -fsSL -o "$work/$archive.sha256" "$base/$archive.sha256" || die "download failed: $base/$archive.sha256"

want=$(awk '{print $1; exit}' "$work/$archive.sha256")
got=$(sha256 "$work/$archive")
[ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch for $archive (expected $want, got $got)"
printf 'Verified sha256 %s\n' "$got"

mkdir -p "$work/x"
tar -xzf "$work/$archive" -C "$work/x"
[ -f "$work/x/$component" ] || die "$archive does not contain '$component'"

# Copy next to the destination, then rename over it: atomic, and safe even
# while an older cw is running.
mkdir -p "$bin_dir"
staged="$bin_dir/.$component.install-$$"
cp "$work/x/$component" "$staged"
chmod 0755 "$staged"
mv -f "$staged" "$bin_dir/$component"

printf 'Installed %s\n' "$("$bin_dir/$component" --version)"
printf '  -> %s\n' "$bin_dir/$component"
case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *)
    # shellcheck disable=SC2016 # a literal $PATH for the user to paste
    printf 'Note: %s is not on your PATH; add it, e.g.  export PATH="%s:$PATH"\n' "$bin_dir" "$bin_dir"
    ;;
esac
