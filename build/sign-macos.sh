#!/bin/sh
# Sign and notarize the macOS release binary as a normal command-line tool.
set -eu
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
bin=${1:-$root/target/release/deskbridge}
profile=${DESKBRIDGE_NOTARY_PROFILE:-deskbridge}
identifier=com.deskbridge


if [ ! -f "$bin" ]; then
  echo "missing $bin (build it with cargo build --release)" >&2
  exit 1
fi
identity=$(security find-identity -p codesigning -v \
  | sed -n 's/.*"\(Developer ID Application:[^"]*\)".*/\1/p' \
  | head -1)
if [ -z "$identity" ]; then
  echo "no Developer ID Application certificate in the keychain" >&2
  exit 1
fi

if otool -L "$bin" | grep -q '@rpath/libswift_Concurrency.dylib'; then
  install_name_tool -change \
    @rpath/libswift_Concurrency.dylib \
    /usr/lib/swift/libswift_Concurrency.dylib \
    "$bin"
fi
otool -l "$bin" | awk '/cmd LC_RPATH/{p=1} p && /path /{print $2; p=0}' | while read -r rpath; do
  case "$rpath" in
    /usr/lib/swift) ;;
    *) install_name_tool -delete_rpath "$rpath" "$bin" ;;
  esac
done

echo "signing with $identity"
codesign --force --options runtime --timestamp \
  --sign "$identity" \
  --identifier "$identifier" \
  "$bin"
codesign --verify --strict --verbose=2 "$bin"
codesign -dv --verbose=2 "$bin" 2>&1 | head -12

if ! xcrun notarytool history --keychain-profile "$profile" >/dev/null 2>&1; then
  echo "signed. notarization profile '$profile' is not available."
  echo "one time: xcrun notarytool store-credentials $profile"
  exit 0
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
zip="$work/deskbridge.zip"
ditto -c -k --keepParent "$bin" "$zip"
echo "submitting for notarization"
xcrun notarytool submit "$zip" --keychain-profile "$profile" --wait
# stapler only embeds tickets in bundles, disk images, and packages.
# A notarized command-line tool is checked online, the same way other CLIs are.
echo "notarized: $bin"
