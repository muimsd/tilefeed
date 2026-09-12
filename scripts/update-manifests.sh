#!/usr/bin/env bash
#
# Regenerate every package manifest for a released version.
#
#   scripts/update-manifests.sh 0.8.1
#
# Downloads that version's release assets, checksums them, and rewrites the
# Homebrew formula, Scoop manifest, Chocolatey package, winget manifest and AUR
# PKGBUILD. Run by .github/workflows/release.yml after a release is published,
# and safe to run by hand afterwards to repair a manifest.
#
# Assets are cached in a temp directory; set KEEP_DOWNLOADS=1 to keep them.

set -euo pipefail

VERSION="${1:-}"
if [ -z "$VERSION" ]; then
    echo "usage: $0 <version>   (e.g. $0 0.8.1)" >&2
    exit 2
fi
VERSION="${VERSION#v}"

REPO="muimsd/tilefeed"
BASE_URL="https://github.com/${REPO}/releases/download/v${VERSION}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap '[ -n "${KEEP_DOWNLOADS:-}" ] || rm -rf "$WORK"' EXIT

# sha256 of a file, on both Linux (sha256sum) and macOS (shasum)
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# Download and checksum one asset. `curl -f` matters: without it a missing asset
# writes GitHub's HTML error page to the file and we would publish the checksum
# of that page as if it were the binary.
fetch() {
    local name="$1" dest="$WORK/$1"
    if ! curl -fsSL "${BASE_URL}/${name}" -o "$dest"; then
        echo "error: could not download ${BASE_URL}/${name}" >&2
        echo "       is v${VERSION} published, with all of its assets?" >&2
        exit 1
    fi
    case "$name" in
        *.tar.gz) file "$dest" | grep -q gzip || { echo "error: $name is not a gzip archive" >&2; exit 1; } ;;
        *.zip) file "$dest" | grep -q -i zip || { echo "error: $name is not a zip archive" >&2; exit 1; } ;;
    esac
    sha256 "$dest"
}

# In-place edit that behaves the same on GNU and BSD userlands
replace() {
    local pattern="$1" file="$2"
    perl -pi -e "$pattern" "$file"
}

echo "Updating manifests for v${VERSION}"

SHA_ARM_MAC=$(fetch "tilefeed-aarch64-apple-darwin.tar.gz")
SHA_INTEL_MAC=$(fetch "tilefeed-x86_64-apple-darwin.tar.gz")
SHA_LINUX=$(fetch "tilefeed-x86_64-unknown-linux-gnu.tar.gz")
SHA_WINDOWS=$(fetch "tilefeed-x86_64-pc-windows-msvc.zip")

# The AUR package builds from source, so it needs the tag's source tarball
SRC_TARBALL="$WORK/source.tar.gz"
if ! curl -fsSL "https://github.com/${REPO}/archive/v${VERSION}.tar.gz" -o "$SRC_TARBALL"; then
    echo "error: could not download the v${VERSION} source tarball" >&2
    exit 1
fi
SHA_SOURCE=$(sha256 "$SRC_TARBALL")

# --- Homebrew -------------------------------------------------------------
cat > "$ROOT/Formula/tilefeed.rb" <<EOF
class Tilefeed < Formula
  desc "PostGIS vector tile pipeline with incremental MBTiles updates via LISTEN/NOTIFY"
  homepage "https://github.com/${REPO}"
  version "${VERSION}"
  license "MIT"

  on_macos do
    on_arm do
      url "${BASE_URL}/tilefeed-aarch64-apple-darwin.tar.gz"
      sha256 "${SHA_ARM_MAC}"
    end
    on_intel do
      url "${BASE_URL}/tilefeed-x86_64-apple-darwin.tar.gz"
      sha256 "${SHA_INTEL_MAC}"
    end
  end

  on_linux do
    url "${BASE_URL}/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "${SHA_LINUX}"
  end

  def install
    bin.install "tilefeed"
  end

  test do
    assert_match "tilefeed", shell_output("#{bin}/tilefeed --help")
  end
end
EOF

# --- Scoop ----------------------------------------------------------------
cat > "$ROOT/bucket/tilefeed.json" <<EOF
{
    "version": "${VERSION}",
    "description": "PostGIS vector tile pipeline with incremental MBTiles updates via LISTEN/NOTIFY",
    "homepage": "https://github.com/${REPO}",
    "license": "MIT",
    "architecture": {
        "64bit": {
            "url": "${BASE_URL}/tilefeed-x86_64-pc-windows-msvc.zip",
            "hash": "${SHA_WINDOWS}"
        }
    },
    "bin": "tilefeed.exe",
    "checkver": {
        "github": "https://github.com/${REPO}"
    },
    "autoupdate": {
        "architecture": {
            "64bit": {
                "url": "https://github.com/${REPO}/releases/download/v\$version/tilefeed-x86_64-pc-windows-msvc.zip"
            }
        }
    }
}
EOF

# --- Chocolatey -----------------------------------------------------------
# Targeted edits, not regeneration: these files carry hand-maintained metadata.
replace "s|<version>.*</version>|<version>${VERSION}</version>|" \
    "$ROOT/packaging/chocolatey/tilefeed.nuspec"
replace "s|checksum64     = '.*'|checksum64     = '${SHA_WINDOWS}'|" \
    "$ROOT/packaging/chocolatey/tools/chocolateyinstall.ps1"

# --- winget ---------------------------------------------------------------
replace "s|^PackageVersion: .*|PackageVersion: ${VERSION}|" \
    "$ROOT/packaging/winget/muimsd.tilefeed.yaml"
replace "s|InstallerUrl: .*|InstallerUrl: ${BASE_URL}/tilefeed-x86_64-pc-windows-msvc.zip|" \
    "$ROOT/packaging/winget/muimsd.tilefeed.yaml"
replace "s|InstallerSha256: .*|InstallerSha256: ${SHA_WINDOWS}|" \
    "$ROOT/packaging/winget/muimsd.tilefeed.yaml"

# --- AUR ------------------------------------------------------------------
replace "s|^pkgver=.*|pkgver=${VERSION}|" "$ROOT/packaging/aur/PKGBUILD"
replace "s|^pkgrel=.*|pkgrel=1|" "$ROOT/packaging/aur/PKGBUILD"
replace "s|^sha256sums=.*|sha256sums=('${SHA_SOURCE}')|" "$ROOT/packaging/aur/PKGBUILD"

echo
echo "Updated for v${VERSION}:"
echo "  Formula/tilefeed.rb                            (macOS arm ${SHA_ARM_MAC:0:12}…)"
echo "  bucket/tilefeed.json                           (windows ${SHA_WINDOWS:0:12}…)"
echo "  packaging/chocolatey/tilefeed.nuspec"
echo "  packaging/chocolatey/tools/chocolateyinstall.ps1"
echo "  packaging/winget/muimsd.tilefeed.yaml"
echo "  packaging/aur/PKGBUILD                         (source ${SHA_SOURCE:0:12}…)"
