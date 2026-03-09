class Tilefeed < Formula
  desc "PostGIS vector tile pipeline with incremental MBTiles updates via LISTEN/NOTIFY"
  homepage "https://github.com/muimsd/tilefeed"
  version "0.4.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.4.0/tilefeed-aarch64-apple-darwin.tar.gz"
      sha256 "5ac1a7ea93ba1f61b7ba6812e94a3867accc2a142d2ad49423b1d49f4fdfb158"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.4.0/tilefeed-x86_64-apple-darwin.tar.gz"
      sha256 "9de10b553fd0c61aa274d2f249d5960b2ce3b0ff096c7ce53b70ce7a7bf7dfee"
    end
  end

  on_linux do
    url "https://github.com/muimsd/tilefeed/releases/download/v0.4.0/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "604eb82e44551c7eaaaa3af006d436b2b62d12689cc8128fcad824f2c749b3f5"
  end

  def install
    bin.install "tilefeed"
  end

  test do
    assert_match "tilefeed", shell_output("\#{bin}/tilefeed --help")
  end
end
