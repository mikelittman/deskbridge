class Deskbridge < Formula
  desc "Small remote desktop"
  homepage "https://github.com/mikelittman/deskbridge"
  version "0.0.10"
  license "MIT"
  depends_on :macos
  depends_on arch: :arm64

  url "https://github.com/mikelittman/deskbridge/releases/download/v0.0.10/deskbridge-macos-arm64"
  sha256 "9649730992f1a68145b7d4b1c2eb94f526f38df296260eb8818352ea3f929fe6"

  def install
    bin.install "deskbridge-macos-arm64" => "deskbridge"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/deskbridge version")
  end
end
