class Tilefeed < Formula
  desc "PostGIS vector tile pipeline with incremental MBTiles updates via LISTEN/NOTIFY"
  homepage "https://github.com/muimsd/tilefeed"
  version "0.8.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.8.1/tilefeed-aarch64-apple-darwin.tar.gz"
      sha256 "338edc964f2fe660b876212e3212947ae2084e0e25d3b766d25196b46b271c62"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.8.1/tilefeed-x86_64-apple-darwin.tar.gz"
      sha256 "61fa512819f66e1cbda06ea3636584e10568a2e810d95ca760c8d60e9cded698"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.8.1/tilefeed-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "ba8eb0b59648e8ca8a4897dcbb804152573a8d0af4f61d40d631cc2b123fede2"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.8.1/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0f5a741998da40d0d13ad1c7db629862cc8a6be00a9fe5d3e9e7eb2915a3a90a"
    end
  end

  def install
    bin.install "tilefeed"
  end

  test do
    assert_match "tilefeed", shell_output("#{bin}/tilefeed --help")
  end
end
