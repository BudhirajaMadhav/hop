#!/bin/sh
# Install Hop on a new Mac. No Rust needed:
#   curl -fsSL https://raw.githubusercontent.com/BudhirajaMadhav/hop/master/install.sh | sh
# ./install.sh target/Hop.app installs a local build instead (./bundle.sh install does this).
set -e
APP=/Applications/Hop.app
HOP=$APP/Contents/MacOS/hop
BIN=$HOME/.local/bin
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

[ "$(uname -m)" = arm64 ] || { echo "Hop needs an Apple silicon Mac." >&2; exit 1; }
[ "$(sw_vers -productVersion | cut -d. -f1)" -ge 26 ] || { echo "Hop needs macOS 26 or later." >&2; exit 1; }

SRC=$1
if [ -z "$SRC" ]; then
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  echo "downloading Hop"
  curl -fsSL -o "$TMP/Hop.tar.gz" https://github.com/BudhirajaMadhav/hop/releases/latest/download/Hop.tar.gz
  tar -xzf "$TMP/Hop.tar.gz" -C "$TMP"
  SRC=$TMP/Hop.app
fi

pkill -x hop || true # a running copy keeps the old code
rm -rf "$APP"
cp -R "$SRC" "$APP"
"$LSREGISTER" -u "$SRC" 2>/dev/null || true # links must never open the copy outside /Applications
"$LSREGISTER" -f "$APP"
mkdir -p "$BIN"
ln -sf "$HOP" "$BIN/hop"
case ":$PATH:" in *":$BIN:"*) ;; *) echo "Add $BIN to your PATH to use the hop command." ;; esac
# With curl | sh, stdin is the script, so the prompts read the keyboard from the terminal.
"$HOP" favorites </dev/tty
"$HOP" default </dev/tty
open "$APP"
echo "installed $APP and $BIN/hop"
