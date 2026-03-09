class Tilefeed < Formula
  desc "PostGIS vector tile pipeline with incremental MBTiles updates via LISTEN/NOTIFY"
  homepage "https://github.com/muimsd/tilefeed"
  version "0.4.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.4.0/tilefeed-aarch64-apple-darwin.tar.gz"
      sha256 "3ae0cc21e917e56f042e26635b1ec6859514ecd514efb38d2d09960f4936f762"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.4.0/tilefeed-x86_64-apple-darwin.tar.gz"
      sha256 "095ef868403d22cce13564dfbb943653de1bfc0769f719785e16ec2bac636549"
    end
  end

  on_linux do
    url "https://github.com/muimsd/tilefeed/releases/download/v0.4.0/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "deef563642975f78823e4b1c24025888c471359d433cb6a588b7dc02b4df0646"
  end

  def install
    bin.install "tilefeed"
  end

  test do
    assert_match "tilefeed", shell_output("\#{bin}/tilefeed --help")
  end
end
