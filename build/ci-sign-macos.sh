#!/bin/sh
# Import the Developer ID certificate from the environment and notarize one binary.
# Used by GitHub Actions. The certificate and the App Store Connect API key stay
# in repository secrets. They are not written into the repository.
set -eu

missing() {
  echo "missing secret: $1" >&2
  echo "Add it under GitHub Settings → Secrets and variables → Actions." >&2
  exit 1
}

[ -n "${APPLE_CERTIFICATE_BASE64:-}" ] || missing APPLE_CERTIFICATE_BASE64
[ -n "${APPLE_CERTIFICATE_PASSWORD:-}" ] || missing APPLE_CERTIFICATE_PASSWORD
[ -n "${APPLE_API_KEY:-}" ] || missing APPLE_API_KEY
[ -n "${APPLE_API_KEY_ID:-}" ] || missing APPLE_API_KEY_ID
[ -n "${APPLE_API_ISSUER:-}" ] || missing APPLE_API_ISSUER

bin=${1:?path to the unsigned deskbridge binary}
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)

keychain="$RUNNER_TEMP/deskbridge-signing.keychain-db"
keychain_password=$(openssl rand -base64 32)
certificate="$RUNNER_TEMP/developer-id.p12"

cleanup() {
  security delete-keychain "$keychain" >/dev/null 2>&1 || true
  rm -f "$certificate"
}
trap cleanup EXIT

printf '%s' "$APPLE_CERTIFICATE_BASE64" | base64 --decode > "$certificate"
security create-keychain -p "$keychain_password" "$keychain"
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
security import "$certificate" -P "$APPLE_CERTIFICATE_PASSWORD" -A -t cert -f pkcs12 -k "$keychain"
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$keychain_password" "$keychain"
security list-keychains -d user -s "$keychain"

export DESKBRIDGE_REQUIRE_NOTARIZATION=1
"$root/build/sign-macos.sh" "$bin"
