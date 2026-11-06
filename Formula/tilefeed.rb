class Tilefeed < Formula
  desc "Dynamic vector tiles from PostGIS and GDAL files"
  homepage "https://github.com/muimsd/tilefeed"
  version "0.11.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.11.0/tilefeed-aarch64-apple-darwin.tar.gz"
      sha256 "29db10dbabcc30767ef28c617880e8aef3c8dd085a1feaa607f586b3b31ca703"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.11.0/tilefeed-x86_64-apple-darwin.tar.gz"
      sha256 "9a4d036cec1754bd35da048f5e54e6765876d7632a884627863829ca14a9c75a"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.11.0/tilefeed-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "426092024d743145fe730dc81e71b09241f02ba81aa4397c6feaa59101b73b5c"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.11.0/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "ce50bb4d559768719c1f13efaf2d23a62082bfc3d67f3512c8fe01f69853ccd3"
    end
  end

  def install
    bin.install "tilefeed"
  end

  test do
    assert_match "tilefeed", shell_output("#{bin}/tilefeed --help")
  end
end
