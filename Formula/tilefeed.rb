class Tilefeed < Formula
  desc "Dynamic vector tiles from PostGIS and GDAL files"
  homepage "https://github.com/muimsd/tilefeed"
  version "0.12.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.12.0/tilefeed-aarch64-apple-darwin.tar.gz"
      sha256 "66302ff7dc589ee1960f5fb9e0f5745eda5572f51fc1cd1255073ec6ffb56ee2"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.12.0/tilefeed-x86_64-apple-darwin.tar.gz"
      sha256 "cef1767de0fd04307d498606d4d2ce7ba9707815285d427f923d5d9c2079c5dc"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.12.0/tilefeed-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "a0306de809c999302f003678eb61d80b8c845985acdcfddebb3c26b9bedb1ef9"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.12.0/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "a24b00245e507231dc80313db6584a01f2178e05514348bcd9d3ebf7a1965b1d"
    end
  end

  def install
    bin.install "tilefeed"
  end

  test do
    assert_match "tilefeed", shell_output("#{bin}/tilefeed --help")
  end
end
