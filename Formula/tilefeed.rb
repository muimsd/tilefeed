class Tilefeed < Formula
  desc "Dynamic vector tiles from PostGIS and GDAL files"
  homepage "https://github.com/muimsd/tilefeed"
  version "0.13.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.13.1/tilefeed-aarch64-apple-darwin.tar.gz"
      sha256 "5714da5f9fda995820375ba2bafd16783b1c992ef8f4d3981d41dfaa7b994b2e"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.13.1/tilefeed-x86_64-apple-darwin.tar.gz"
      sha256 "c97cfe290bca080414bac3277dc9e5d452391effb04cc6f82384c9ebf80fad77"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.13.1/tilefeed-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "56f3f4d256d13c6958897c064f856c460f34e6529906f742098f4eb0f08ce9d0"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.13.1/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "af473dab5b578371c0062f2e3d13ea39d1116f9fd1b09a26e09d6fe37634288d"
    end
  end

  def install
    bin.install "tilefeed"
  end

  test do
    assert_match "tilefeed", shell_output("#{bin}/tilefeed --help")
  end
end
