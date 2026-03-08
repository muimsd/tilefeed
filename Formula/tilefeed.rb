class Tilefeed < Formula
  desc "PostGIS vector tile pipeline with incremental MBTiles updates via LISTEN/NOTIFY"
  homepage "https://github.com/muimsd/tilefeed"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.1.0/tilefeed-aarch64-apple-darwin.tar.gz"
      sha256 "4982f81e92d9688c03be02573eeef693ed536cb1fe2e43eb9bc1a128e5ddadb2"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.1.0/tilefeed-x86_64-apple-darwin.tar.gz"
      sha256 "8439317426362ac1b563cf43f525f9200800f2b00f492c3e3b430876608d771f"
    end
  end

  on_linux do
    url "https://github.com/muimsd/tilefeed/releases/download/v0.1.0/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "f17a6607c562cd937689659325480868a7ab78d77b9064de2693e01dfe17282f"
  end

  def install
    bin.install "tilefeed"
  end

  test do
    assert_match "tilefeed", shell_output("#{bin}/tilefeed --help")
  end
end
