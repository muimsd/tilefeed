class Tilefeed < Formula
  desc "PostGIS vector tile pipeline with incremental MBTiles updates via LISTEN/NOTIFY"
  homepage "https://github.com/muimsd/tilefeed"
  version "0.6.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.6.0/tilefeed-aarch64-apple-darwin.tar.gz"
      sha256 "39194a159b7b1e0ef4dab36661314fa8e5d110948350420522abb57859a2717c"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.6.0/tilefeed-x86_64-apple-darwin.tar.gz"
      sha256 "577139a2337d41d92b5fc5dad51d33a8489ebdf403b352f6dca01f0fe5cb2eb3"
    end
  end

  on_linux do
    url "https://github.com/muimsd/tilefeed/releases/download/v0.6.0/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "051f343b3d8c3f6b64c049f7453e245c505b3c76e63211566ede8390dceb4946"
  end

  def install
    bin.install "tilefeed"
  end

  test do
    assert_match "tilefeed", shell_output("\#{bin}/tilefeed --help")
  end
end
