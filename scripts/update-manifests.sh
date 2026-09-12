#!/usr/bin/env bash
#
# Regenerate every package manifest for a released version.
#
#   scripts/update-manifests.sh 0.8.1
#
# Downloads that version's release assets, checksums them, and rewrites the
# Homebrew formula, Scoop manifest, Chocolatey package, winget manifest and AUR
# PKGBUILD. Run by .github/workflows/release.yml after a release is published,
# and safe to re-run by hand afterwards to repair a manifest.
#
# Every edit is verified to have matched something: a manifest that silently
# stops being updated is how the winget and AUR files sat at 0.1.0 with
# `__CHECKSUM__` placeholders through eight releases.
#
# Downloads go to a temp directory; set KEEP_DOWNLOADS=1 to keep them.

set -euo pipefail

VERSION="${1:-}"
if [ -z "$VERSION" ]; then
    echo "usage: $0 <version>   (e.g. $0 0.8.1)" >&2
    exit 2
fi
VERSION="${VERSION#v}"

# Prereleases must not overwrite the stable manifests, and makepkg rejects a
# pkgver containing a hyphen, so an -rc tag would also produce an unbuildable
# AUR package.
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "error: '$VERSION' is not a plain MAJOR.MINOR.PATCH version." >&2
    echo "       Package manifests track stable releases only." >&2
    exit 2
fi

REPO="${GITHUB_REPOSITORY:-muimsd/tilefeed}"
BASE_URL="https://github.com/${REPO}/releases/download/v${VERSION}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"

cleanup() {
    if [ -n "${KEEP_DOWNLOADS:-}" ]; then
        echo "Downloads kept in $WORK"
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

# sha256 of a file, on both Linux (sha256sum) and macOS (shasum)
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# Download one file and echo its sha256.
#
# `curl -f` matters: without it a missing asset writes GitHub's HTML error page
# to the file and we publish the checksum of that page as if it were the binary.
# Release assets are served through a CDN that can 5xx or briefly 404 right after
# publication, and this runs the moment the release job finishes, so retry.
fetch() {
    local url="$1" name="$2" dest="$WORK/$2"

    if ! curl -fsSL --retry 3 --retry-delay 5 --retry-all-errors "$url" -o "$dest"; then
        echo "error: could not download $url" >&2
        echo "       is v${VERSION} published, with all of its assets?" >&2
        exit 1
    fi

    # `file -b` omits the filename: `file x.zip` prints the name too, so grepping
    # its output for "zip" matches the name and would pass on an HTML error page.
    local kind
    kind="$(file -b "$dest")"
    case "$name" in
        *.tar.gz)
            case "$kind" in
                gzip*) ;;
                *) echo "error: $name is not a gzip archive (got: $kind)" >&2; exit 1 ;;
            esac
            ;;
        *.zip)
            case "$kind" in
                Zip*|*"Zip archive"*) ;;
                *) echo "error: $name is not a zip archive (got: $kind)" >&2; exit 1 ;;
            esac
            ;;
    esac

    sha256 "$dest"
}

# Apply a perl substitution in place, failing if it matched nothing.
#
# A silent no-op is the failure mode this script exists to prevent: the edit
# would be skipped, the file left stale, and the release published anyway.
# Matching (rather than changing) is the condition, so re-running for a version
# already written stays a no-op success.
replace() {
    local file="$1" pattern="$2"

    if ! perl -i -pe "BEGIN { \$n = 0 } \$n += ${pattern}; END { exit 3 unless \$n }" "$file"; then
        echo "error: nothing in ${file#"$ROOT"/} matched: $pattern" >&2
        echo "       the file's format changed; update this script." >&2
        exit 1
    fi
}

echo "Updating manifests for v${VERSION} (${REPO})"

SHA_ARM_MAC=$(fetch "${BASE_URL}/tilefeed-aarch64-apple-darwin.tar.gz" "tilefeed-aarch64-apple-darwin.tar.gz")
SHA_INTEL_MAC=$(fetch "${BASE_URL}/tilefeed-x86_64-apple-darwin.tar.gz" "tilefeed-x86_64-apple-darwin.tar.gz")
SHA_ARM_LINUX=$(fetch "${BASE_URL}/tilefeed-aarch64-unknown-linux-gnu.tar.gz" "tilefeed-aarch64-unknown-linux-gnu.tar.gz")
SHA_INTEL_LINUX=$(fetch "${BASE_URL}/tilefeed-x86_64-unknown-linux-gnu.tar.gz" "tilefeed-x86_64-unknown-linux-gnu.tar.gz")
SHA_WINDOWS=$(fetch "${BASE_URL}/tilefeed-x86_64-pc-windows-msvc.zip" "tilefeed-x86_64-pc-windows-msvc.zip")

# The AUR package builds from source, so it checksums the tag's source tarball
SHA_SOURCE=$(fetch "https://github.com/${REPO}/archive/v${VERSION}.tar.gz" "source.tar.gz")

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
    on_arm do
      url "${BASE_URL}/tilefeed-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "${SHA_ARM_LINUX}"
    end
    on_intel do
      url "${BASE_URL}/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "${SHA_INTEL_LINUX}"
    end
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

# --- winget ---------------------------------------------------------------
# Generated whole, like the two above: patching installer entries in place would
# rewrite an arm64 block's URL and hash to the x64 ones the day someone adds it.
cat > "$ROOT/packaging/winget/muimsd.tilefeed.yaml" <<EOF
PackageIdentifier: muimsd.tilefeed
PackageVersion: ${VERSION}
PackageName: tilefeed
Publisher: muimsd
License: MIT
LicenseUrl: https://github.com/${REPO}/blob/main/LICENSE
ShortDescription: PostGIS vector tile pipeline with incremental MBTiles updates via LISTEN/NOTIFY
PackageUrl: https://github.com/${REPO}
Tags:
  - postgis
  - vector-tiles
  - mbtiles
  - gis
  - cli
Installers:
  - Architecture: x64
    InstallerType: zip
    InstallerUrl: ${BASE_URL}/tilefeed-x86_64-pc-windows-msvc.zip
    InstallerSha256: ${SHA_WINDOWS}
    NestedInstallerType: portable
    NestedInstallerFiles:
      - RelativeFilePath: tilefeed.exe
        PortableCommandAlias: tilefeed
PackageLocale: en-US
ManifestType: singleton
ManifestVersion: 1.6.0
EOF

# Targeted edits below, not regeneration: these files carry hand-maintained
# metadata. Each is a separate `replace` so a format change names the line that
# stopped matching rather than failing as one opaque block.

# --- Chocolatey -----------------------------------------------------------
replace "$ROOT/packaging/chocolatey/tilefeed.nuspec" \
    "s|<version>[^<]*</version>|<version>${VERSION}</version>|"
replace "$ROOT/packaging/chocolatey/tools/chocolateyinstall.ps1" \
    "s|^(\\s*checksum64\\s*=\\s*)'[^']*'|\${1}'${SHA_WINDOWS}'|"


# --- AUR ------------------------------------------------------------------
replace "$ROOT/packaging/aur/PKGBUILD" "s|^pkgver=.*|pkgver=${VERSION}|"
replace "$ROOT/packaging/aur/PKGBUILD" "s|^pkgrel=.*|pkgrel=1|"
replace "$ROOT/packaging/aur/PKGBUILD" "s|^sha256sums=.*|sha256sums=('${SHA_SOURCE}')|"

echo
echo "Updated for v${VERSION}:"
echo "  Formula/tilefeed.rb                              (macOS arm ${SHA_ARM_MAC:0:12}…, linux arm ${SHA_ARM_LINUX:0:12}…)"
echo "  bucket/tilefeed.json                             (windows ${SHA_WINDOWS:0:12}…)"
echo "  packaging/chocolatey/tilefeed.nuspec"
echo "  packaging/chocolatey/tools/chocolateyinstall.ps1 (windows ${SHA_WINDOWS:0:12}…)"
echo "  packaging/winget/muimsd.tilefeed.yaml            (windows ${SHA_WINDOWS:0:12}…)"
echo "  packaging/aur/PKGBUILD                           (source ${SHA_SOURCE:0:12}…)"
