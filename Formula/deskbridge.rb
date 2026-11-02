class Deskbridge < Formula
  desc "Small remote desktop"
  homepage "https://github.com/mikelittman/deskbridge"
  version "0.0.9"
  license "MIT"
  depends_on :macos
  depends_on arch: :arm64

  url "https://github.com/mikelittman/deskbridge/releases/download/v0.0.9/deskbridge-macos-arm64"
  sha256 "13de363a17ddda55dc3d3328e95d1010e3895c28d4cc99ebe2c355ad69bf646f"

  def install
    bin.install "deskbridge-macos-arm64" => "deskbridge"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/deskbridge version")
  end
end
