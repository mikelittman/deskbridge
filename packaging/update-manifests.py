#!/usr/bin/env python3
"""Write the Homebrew formula and Scoop manifest for the release in dist/."""

import json
import os
from pathlib import Path

root = Path(__file__).resolve().parents[1]
sums = (root / "dist" / "SHA256SUMS").read_text()
hashes = {}
for line in sums.splitlines():
    digest, name = line.split()
    hashes[name] = digest

tag = os.environ["TAG"]
version = os.environ["VERSION"]
mac = hashes["deskbridge-macos-arm64"]
windows = hashes["deskbridge-windows-x86_64.exe"]

formula = f"""class Deskbridge < Formula
  desc "Small remote desktop"
  homepage "https://github.com/mikelittman/deskbridge"
  version "{version}"
  license "MIT"
  depends_on :macos
  depends_on arch: :arm64

  url "https://github.com/mikelittman/deskbridge/releases/download/{tag}/deskbridge-macos-arm64"
  sha256 "{mac}"

  def install
    bin.install "deskbridge-macos-arm64" => "deskbridge"
  end

  test do
    assert_match version.to_s, shell_output("#{{bin}}/deskbridge version")
  end
end
"""
(root / "Formula" / "deskbridge.rb").write_text(formula)

manifest = {
    "version": version,
    "description": "Small remote desktop",
    "homepage": "https://github.com/mikelittman/deskbridge",
    "license": "MIT",
    "architecture": {
        "64bit": {
            "url": f"https://github.com/mikelittman/deskbridge/releases/download/{tag}/deskbridge-windows-x86_64.exe",
            "hash": windows,
        }
    },
    "bin": [["deskbridge-windows-x86_64.exe", "deskbridge"]],
    "checkver": {"github": "https://github.com/mikelittman/deskbridge"},
    "autoupdate": {
        "architecture": {
            "64bit": {
                "url": "https://github.com/mikelittman/deskbridge/releases/download/v$version/deskbridge-windows-x86_64.exe"
            }
        }
    },
}
(root / "bucket" / "deskbridge.json").write_text(json.dumps(manifest, indent=2) + "\n")
