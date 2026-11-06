class Tilefeed < Formula
  desc "Dynamic vector tiles from PostGIS and GDAL files"
  homepage "https://github.com/muimsd/tilefeed"
  version "0.10.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.10.0/tilefeed-aarch64-apple-darwin.tar.gz"
      sha256 "959517a0b6cccc112ef0e1117b9f1c7b9f2bd70f8283f6246541225d8f92e947"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.10.0/tilefeed-x86_64-apple-darwin.tar.gz"
      sha256 "f9ab84ceb94d0bad2bbdbfbf5006c2cdf2f65485de6bf97e5b0cbe5849c53d4f"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.10.0/tilefeed-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "522db0c9df2ce759d1964583d4f70c2a653eeef7ed054d699456164dd992795d"
    end
    on_intel do
      url "https://github.com/muimsd/tilefeed/releases/download/v0.10.0/tilefeed-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "896b403773125a3e250bd8f3c458d1beb94d95e009644f2b9b17494334cc3649"
    end
  end

  def install
    bin.install "tilefeed"
  end

  test do
    assert_match "tilefeed", shell_output("#{bin}/tilefeed --help")
  end
end
