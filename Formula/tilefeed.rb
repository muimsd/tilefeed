class Tilefeed < Formula
  desc "Dynamic vector tiles from PostGIS and GDAL files"
  homepage "https://github.com/muimsd/tilefeed"
  version "0.13.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.13.0/tilefeed-aarch64-apple-darwin.tar.gz"
      sha256 "2c2d2dcf39f247cff43eacb155d75f944e11c0f789184c653e323b544d5fbfae"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.13.0/tilefeed-x86_64-apple-darwin.tar.gz"
      sha256 "44204d783340fb9f69ed7d7a1d18830919b03e053d6b746c9714823cede5b6b9"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.13.0/tilefeed-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "4ba2a47585538a455db2afcca213495a1e2d013e65b466c07ca7ed69f1226fd2"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.13.0/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "6e2553109a0cc2ec9184f039bd76b363ae56361800d064000614fbb07ce1c8a1"
    end
  end

  def install
    bin.install "tilefeed"
  end

  test do
    assert_match "tilefeed", shell_output("#{bin}/tilefeed --help")
  end
end
