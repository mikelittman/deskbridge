class Deskbridge < Formula
  desc "Small remote desktop"
  homepage "https://github.com/mikelittman/deskbridge"
  version "0.0.4"
  license "MIT"
  depends_on :macos
  depends_on arch: :arm64

  url "https://github.com/mikelittman/deskbridge/releases/download/v0.0.4/deskbridge-macos-arm64"
  sha256 "357f36ca10c00f185366abc5c225931624b75c119c78834e6431e2f41c7d9a3b"

  def install
    bin.install "deskbridge-macos-arm64" => "deskbridge"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/deskbridge version")
  end
end
