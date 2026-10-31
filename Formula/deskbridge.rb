class Deskbridge < Formula
  desc "Small remote desktop"
  homepage "https://github.com/mikelittman/deskbridge"
  version "0.0.7"
  license "MIT"
  depends_on :macos
  depends_on arch: :arm64

  url "https://github.com/mikelittman/deskbridge/releases/download/v0.0.7/deskbridge-macos-arm64"
  sha256 "e284752e05a1086dca488f4e7d5af93fa519570fbfb162d982e094f63bcf2874"

  def install
    bin.install "deskbridge-macos-arm64" => "deskbridge"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/deskbridge version")
  end
end
