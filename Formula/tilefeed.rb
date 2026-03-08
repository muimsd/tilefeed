class Tilefeed < Formula
  desc "PostGIS vector tile pipeline with incremental MBTiles updates via LISTEN/NOTIFY"
  homepage "https://github.com/muimsd/tilefeed"
  version "0.3.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.3.0/tilefeed-aarch64-apple-darwin.tar.gz"
      sha256 "60255aca0da44ce157d78f896b50eee1281f034b91d1854407b3c9c055f52647"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.3.0/tilefeed-x86_64-apple-darwin.tar.gz"
      sha256 "ac871bb6edaaee2ba4a6a1ca0d4cf07b3be2278f07135d0c025c4d93eeec6234"
    end
  end

  on_linux do
    url "https://github.com/muimsd/tilefeed/releases/download/v0.3.0/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "9572c0bc6dbfa67ded6dd2bab0be9f92e82dd36a7d08e593219100131033c226"
  end

  def install
    bin.install "tilefeed"
  end

  test do
    assert_match "tilefeed", shell_output("\#{bin}/tilefeed --help")
  end
end
