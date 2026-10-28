#!/bin/sh
# Starts the default bundle. For custom output folders, run that folder's play directly.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PLAY="$ROOT/builds/mkds/play"
if [ ! -x "$PLAY" ]; then
    echo "No MKDSrust build found in builds/mkds. Run: $ROOT/build.sh /path/to/game.nds" >&2
    exit 1
fi
exec "$PLAY" "$@"
