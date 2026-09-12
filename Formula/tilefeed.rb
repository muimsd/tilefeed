class Tilefeed < Formula
  desc "PostGIS vector tile pipeline with incremental MBTiles updates via LISTEN/NOTIFY"
  homepage "https://github.com/muimsd/tilefeed"
  version "0.9.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.9.0/tilefeed-aarch64-apple-darwin.tar.gz"
      sha256 "e1034abef51c6ca77cd2e3d8f4c9c78f2d29d518d4b6b3dd146c6e950b23d000"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.9.0/tilefeed-x86_64-apple-darwin.tar.gz"
      sha256 "75014c1b129b64a0999d07e3dcd1d918afbb9f75c6371669954fb801d9afe3b5"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.9.0/tilefeed-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "a04015c68efac5d44b6d80428ccd2cb867825d1431cd99ebc6f2f0725dace212"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.9.0/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "d2b4566c854beece6fce7b17f719970126801c3c69458db38008d6ba5c01cbbe"
    end
  end

  def install
    bin.install "tilefeed"
  end

  test do
    assert_match "tilefeed", shell_output("#{bin}/tilefeed --help")
  end
end
