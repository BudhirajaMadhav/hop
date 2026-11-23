#!/bin/sh
# ./bundle.sh          build target/Hop.app
# ./bundle.sh install  build, then install it with ./install.sh (favorites, default browser)
# ./bundle.sh release  build, then publish target/Hop.tar.gz as a GitHub release for the one-line install
# After that, use the `hop` command (`hop help`).
set -e
cd "$(dirname "$0")"
cargo build --release
rm -rf target/Hop.app
mkdir -p target/Hop.app/Contents/MacOS
cp target/release/hop target/Hop.app/Contents/MacOS/hop
cp Info.plist target/Hop.app/Contents/Info.plist
codesign --force --sign - target/Hop.app
echo "built target/Hop.app"

case "${1:-}" in
  "") ;;
  install) ./install.sh target/Hop.app ;;
  release)
    COPYFILE_DISABLE=1 tar -czf target/Hop.tar.gz -C target Hop.app # no ._ metadata files
    gh release create "v$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml)" target/Hop.tar.gz --generate-notes ;;
  *) sed -n '2,5p' "$0" | sed 's/^# //'; exit 1 ;;
esac
