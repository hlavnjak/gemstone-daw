#!/bin/bash
# Wrap the notarised app zip from `make sign-macos` into the release zip
# (`make release`):
# a folder named NAME with the app plus the files in EXTRA_DIR (licence,
# readme). It is done on the Mac with ditto, because a bundle unpacked and
# re-zipped on Linux can lose what its signature covers; the notarisation
# ticket is stapled into the app, so a re-zip keeps it. The Mac is the one
# MAC_SSH in $MAC_SIGN_ENV names, as for signing.
#
#   macos-release-zip.sh NOTARISED_ZIP EXTRA_DIR NAME OUT_ZIP
set -euo pipefail

[ $# -eq 4 ] || { echo "usage: $0 NOTARISED_ZIP EXTRA_DIR NAME OUT_ZIP" >&2; exit 1; }
SRC=$1 EXTRA=$2 NAME=$3 OUT=$4
ENV=${MAC_SIGN_ENV:-packaging/macos/sign.env}
[ -f "$ENV" ] || { echo "error: $ENV not found (see packaging/macos/sign.env.example)" >&2; exit 1; }
. "$ENV"
[ -n "${MAC_SSH:-}" ] || { echo "error: MAC_SSH is not set in $ENV" >&2; exit 1; }

REMOTE=$(ssh "$MAC_SSH" 'mktemp -d -t resynth-release')
trap 'ssh "$MAC_SSH" "rm -rf $REMOTE"' EXIT
scp -q "$SRC" "$MAC_SSH:$REMOTE/app.zip"
scp -q "$EXTRA"/* "$MAC_SSH:$REMOTE/"
ssh "$MAC_SSH" "set -e; cd $REMOTE
	mkdir '$NAME'
	ditto -x -k app.zip '$NAME/'
	rm app.zip
	for f in *; do [ \"\$f\" = '$NAME' ] || mv \"\$f\" '$NAME/'; done
	app=\$(ls -d '$NAME'/*.app)
	codesign --verify --strict --deep \"\$app\"
	xcrun stapler validate \"\$app\" >/dev/null
	spctl --assess --type execute \"\$app\"
	ditto -c -k --keepParent '$NAME' out.zip"
scp -q "$MAC_SSH:$REMOTE/out.zip" "$OUT"
echo "macOS release zip: $OUT"
