class Tilefeed < Formula
  desc "Dynamic vector tiles from PostGIS and GDAL files"
  homepage "https://github.com/muimsd/tilefeed"
  version "0.11.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.11.1/tilefeed-aarch64-apple-darwin.tar.gz"
      sha256 "61e40c496d482e241df4158e0e8ce6024ba45f7ab1b0199c0e32a1fc66f1fd4e"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.11.1/tilefeed-x86_64-apple-darwin.tar.gz"
      sha256 "42e97b08fcada126d6f6db29f64bc236bddfb6e9c278a6bae52bc10806a67040"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.11.1/tilefeed-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "eb40ef095f11cbc478e879cec3b586b3aaf2f1c6c73a4abaaeb72869e88e73f6"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.11.1/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "c185998055a754d70351c32630fab49cd914dde0f01e0ce6099a29bb7b8cadf4"
    end
  end

  def install
    bin.install "tilefeed"
  end

  test do
    assert_match "tilefeed", shell_output("#{bin}/tilefeed --help")
  end
end
