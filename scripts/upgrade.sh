#!/usr/bin/env bash
# Upgrades a systemd-installed loadbearer-fleet to a release: downloads it,
# checks it against SHA256SUMS, stops the service, replaces the binary and
# starts it again. There is no migration step — the index updates itself on
# first start.
#
#   sudo scripts/upgrade.sh            # the latest release
#   sudo scripts/upgrade.sh 0.8.0      # a given one
#
# BIN and SERVICE override where the binary lives and what the unit is called.
set -euo pipefail

REPO=issinoho/loadbearer-fleet
BIN=${BIN:-/usr/local/bin/loadbearer-fleet}
SERVICE=${SERVICE:-loadbearer-fleet}

die() { echo "upgrade: $*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || die "run as root (sudo $0 $*)"
for tool in curl sha256sum tar systemctl; do
    command -v "$tool" >/dev/null || die "needs $tool"
done

V=${1:-}
if [[ -z $V ]]; then
    V=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
        sed -n 's/.*"tag_name": *"v\([^"]*\)".*/\1/p')
    [[ -n $V ]] || die "couldn't find the latest release; name a version"
fi
V=${V#v}

current=$("$BIN" --version 2>/dev/null | awk '{print $2}' || true)
if [[ $current == "$V" ]]; then
    echo "already on $V"
    exit 0
fi
echo "upgrading ${current:-(not installed)} -> $V"

A=loadbearer-fleet-$V-x86_64-unknown-linux-gnu
U=https://github.com/$REPO/releases/download/v$V
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

curl -fsSLO "$U/$A.tar.gz" || die "no Linux archive for $V at $U"
curl -fsSLO "$U/SHA256SUMS" || die "no SHA256SUMS for $V"
sha256sum --ignore-missing -c SHA256SUMS || die "checksum mismatch — not installing"
tar xzf "$A.tar.gz"
[[ -x $A/loadbearer-fleet ]] || die "archive has no loadbearer-fleet binary"

# Refuse before stopping anything if the new binary won't even run here.
"$A/loadbearer-fleet" --version >/dev/null || die "new binary doesn't run on this machine"

systemctl stop "$SERVICE"
install -m755 "$A/loadbearer-fleet" "$BIN"
systemctl start "$SERVICE"

sleep 3
if systemctl is-active --quiet "$SERVICE"; then
    echo "$("$BIN" --version) is running"
else
    journalctl -u "$SERVICE" -n 30 --no-pager >&2
    die "$SERVICE didn't stay up — log above"
fi
