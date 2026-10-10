#!/bin/bash
# Sign, notarise and staple the macOS app bundle on a real Mac over ssh
# (`make sign-macos`). Codesign and notarytool exist only on macOS, so the
# bundle, the entitlements and the notary key are copied to a temporary
# directory on the Mac, tools/macos-sign-remote.sh does the work there, and the
# stapled zip comes back into the build output. The temporary directory, key
# included, is removed from the Mac however the run ends.
#
# Usage: tools/macos-sign.sh <app bundle> <output dir>
# Machine, identity and key come from $MAC_SIGN_ENV (packaging/macos/sign.env,
# gitignored): MAC_SSH, SIGN_IDENTITY, NOTARY_KEY_FILE, NOTARY_KEY_ID and, for a
# Team API key, NOTARY_ISSUER. The entitlements are read from
# packaging/macos/gemstone.entitlements. $VERSION names the zip.
set -euo pipefail

APP=$1
OUT=$2
ENV=${MAC_SIGN_ENV:-packaging/macos/sign.env}
HERE=$(cd "$(dirname "$0")" && pwd)

[ -f "$ENV" ] || {
	echo "error: $ENV not found; create it with MAC_SSH, SIGN_IDENTITY, NOTARY_KEY_FILE," >&2
	echo "       NOTARY_KEY_ID and NOTARY_ISSUER (see the top of $0)" >&2
	exit 1; }
. "$ENV"
for v in MAC_SSH SIGN_IDENTITY NOTARY_KEY_FILE NOTARY_KEY_ID; do
	[ -n "${!v:-}" ] || { echo "error: $v is not set in $ENV" >&2; exit 1; }
done
[ -f "$NOTARY_KEY_FILE" ] || { echo "error: notary key $NOTARY_KEY_FILE not found" >&2; exit 1; }
ENTITLEMENTS="$HERE/../packaging/macos/gemstone.entitlements"
[ -f "$ENTITLEMENTS" ] || { echo "error: $ENTITLEMENTS not found" >&2; exit 1; }
[ -d "$APP" ] || { echo "error: no app bundle at $APP (run make build-macos)" >&2; exit 1; }

ZIP_NAME="Gemstone-DAW-${VERSION:-dev}-macos.zip"

CONF=$(mktemp)
REMOTE=$(ssh "$MAC_SSH" 'mktemp -d -t gemstone-sign')
trap 'rm -f "$CONF"; ssh "$MAC_SSH" "rm -rf $REMOTE"' EXIT
{
	printf 'SIGN_IDENTITY=%q\n' "$SIGN_IDENTITY"
	printf 'NOTARY_KEY_ID=%q\n' "$NOTARY_KEY_ID"
	printf 'NOTARY_ISSUER=%q\n' "${NOTARY_ISSUER:-}"
	printf 'ZIP_NAME=%q\n' "$ZIP_NAME"
} > "$CONF"

echo "── copying to the Mac ──────────────────────────────────────────────"
scp -q -r "$APP" "$MAC_SSH:$REMOTE/"
scp -q "$HERE/macos-sign-remote.sh" "$MAC_SSH:$REMOTE/sign.sh"
scp -q "$ENTITLEMENTS" "$MAC_SSH:$REMOTE/"
scp -q "$CONF" "$MAC_SSH:$REMOTE/sign.conf"
scp -q "$NOTARY_KEY_FILE" "$MAC_SSH:$REMOTE/AuthKey.p8"
ssh "$MAC_SSH" "chmod 600 $REMOTE/AuthKey.p8"

# -t, so that Ctrl-C here also stops the remote side.
ssh -t "$MAC_SSH" "/bin/bash $REMOTE/sign.sh"

scp -q "$MAC_SSH:$REMOTE/$ZIP_NAME" "$OUT/"
echo "Signed + notarised: $OUT/$ZIP_NAME"
