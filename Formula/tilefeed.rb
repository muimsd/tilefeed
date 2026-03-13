class Tilefeed < Formula
  desc "PostGIS vector tile pipeline with incremental MBTiles updates via LISTEN/NOTIFY"
  homepage "https://github.com/muimsd/tilefeed"
  version "0.7.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.7.1/tilefeed-aarch64-apple-darwin.tar.gz"
      sha256 "152f4dbbed44327e06c25aaebdff0d277d7ab30e38673423a7bfca1926c54d8d"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.7.1/tilefeed-x86_64-apple-darwin.tar.gz"
      sha256 "84e200e8c6e1e1065a557a6650e113104b4728bbf44580d354ba340660a22979"
    end
  end

  on_linux do
    url "https://github.com/muimsd/tilefeed/releases/download/v0.7.1/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "2e7afc9ee769008fbd02cca7fc56d071ee018e5cec6ddda8cae044032f1c9fc0"
  end

  def install
    bin.install "tilefeed"
  end

  test do
    assert_match "tilefeed", shell_output("\#{bin}/tilefeed --help")
  end
end
