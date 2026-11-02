class Deskbridge < Formula
  desc "Small remote desktop"
  homepage "https://github.com/mikelittman/deskbridge"
  version "0.0.11"
  license "MIT"
  depends_on :macos
  depends_on arch: :arm64

  url "https://github.com/mikelittman/deskbridge/releases/download/v0.0.11/deskbridge-macos-arm64"
  sha256 "d7843dfb9a8b646527f905cc6573df8e4f1b76a4fe310f3890d6d761c1bd93d5"

  def install
    bin.install "deskbridge-macos-arm64" => "deskbridge"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/deskbridge version")
  end
end
