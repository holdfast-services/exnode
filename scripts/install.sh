#!/bin/sh
# install.sh: install the exnode provider agent from a signed GitHub release.
#
# POSIX sh, not bash or fish, because it runs on fresh machines that may have
# neither.  Linux with systemd only.
#
# Do not pipe this into a shell (curl ... | sh).  Download it, read it, run it:
#     curl -fsSLO https://github.com/holdfast-services/exnode/releases/download/vX.Y.Z/install.sh
#     less install.sh
#     sudo sh install.sh
#
# Usage: sudo sh install.sh [--version vX.Y.Z] [--allow-unsigned]
#
# Downloads the exnode binary for this CPU, its packaging files, SHA256SUMS and
# the Sigstore bundle for SHA256SUMS.  Verifies the bundle with cosign (the
# signer must be this repository's release workflow at that tag), then every
# file against SHA256SUMS.  Without cosign it stops unless --allow-unsigned.
# Installs /usr/local/bin/exnode, creates the exnode user and its directories,
# installs and enables exnode.service, and prints the next steps.  It does not
# start the agent: the node has to be enrolled first.
set -eu

# The release workflow fills these in.  An unstamped copy needs EXNODE_REPO.
REPO="${EXNODE_REPO:-@REPO@}"
VERSION="${EXNODE_VERSION:-@VERSION@}"
ISSUER=https://token.actions.githubusercontent.com

die() { echo "install.sh: $*" >&2; exit 1; }
say() { echo "==> $*"; }

allow_unsigned=0
while [ $# -gt 0 ]; do
    case $1 in
        --version)        [ $# -ge 2 ] || die "--version needs a tag"; VERSION=$2; shift 2 ;;
        --allow-unsigned) allow_unsigned=1; shift ;;
        -h|--help)        sed -n '2,/^set -eu$/{/^set -eu$/d;s/^# \{0,1\}//;p;}' "$0"; exit 0 ;;
        *)                die "unknown option: $1 (try --help)" ;;
    esac
done

[ "$(id -u)" -eq 0 ] || die "run it as root: sudo sh install.sh"
[ "$(uname -s)" = Linux ] || die "exnode releases are for Linux"
for cmd in curl sha256sum awk install systemctl systemd-sysusers systemd-tmpfiles; do
    command -v "$cmd" >/dev/null 2>&1 || die "$cmd is required"
done
case $REPO in
    @*@)                               die "set EXNODE_REPO=owner/name: this copy was not stamped by a release" ;;
    *[!A-Za-z0-9._/-]* | */*/* | /* | */) die "bad repository '$REPO' (want owner/name)" ;;
    */*)                               ;;
    *)                                 die "bad repository '$REPO' (want owner/name)" ;;
esac
case $(uname -m) in
    x86_64 | amd64)  target=x86_64-unknown-linux-musl ;;
    aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
    *)               die "there is no exnode release for $(uname -m)" ;;
esac

releases=https://github.com/$REPO/releases
case $VERSION in
    @*@ | latest)  # GitHub redirects /releases/latest to /releases/tag/<tag>
        url=$(curl -fsS --proto '=https' -o /dev/null -w '%{redirect_url}' "$releases/latest") \
            || die "cannot reach $releases"
        VERSION=${url##*/} ;;
esac
case $VERSION in
    v[0-9]*) ;;
    *)       die "no release tag found (got '$VERSION')" ;;
esac
case $VERSION in *[!A-Za-z0-9._-]*) die "bad version '$VERSION'" ;; esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' INT TERM
files="exnode-$target exnode.service exnode.sysusers exnode.tmpfiles node.toml"
say "downloading exnode $VERSION ($target) from $releases"
for f in SHA256SUMS SHA256SUMS.sigstore.json $files; do
    curl -fsSL --proto '=https' --proto-redir '=https' --retry 3 -o "$tmp/$f" \
        "$releases/download/$VERSION/$f" || die "could not download $f"
done

identity="https://github.com/$REPO/.github/workflows/release.yml@refs/tags/$VERSION"
if command -v cosign >/dev/null 2>&1; then
    cosign verify-blob --bundle "$tmp/SHA256SUMS.sigstore.json" \
        --certificate-identity "$identity" --certificate-oidc-issuer "$ISSUER" \
        "$tmp/SHA256SUMS" >/dev/null || die "SHA256SUMS is not signed by $identity; not installing"
    say "signature verified: SHA256SUMS was signed by $identity"
elif [ "$allow_unsigned" -eq 1 ]; then
    echo "WARNING: cosign is not installed, so the release signature was NOT checked." >&2
    echo "WARNING: the checksums below catch a corrupt download, not a forged one." >&2
else
    die "cosign is not installed, so the release signature cannot be checked.
Install cosign (https://docs.sigstore.dev/cosign/system_config/installation/) and run this again,
or pass --allow-unsigned to rely on checksums alone (they catch corruption, not forgery)."
fi
for f in $files; do
    want=$(awk -v f="$f" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/SHA256SUMS")
    got=$(sha256sum "$tmp/$f" | awk '{ print $1 }')
    [ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch for $f; not installing"
done
say "checksums match SHA256SUMS"

install -m 0755 "$tmp/exnode-$target" /usr/local/bin/.exnode.new
mv -f /usr/local/bin/.exnode.new /usr/local/bin/exnode   # atomic, even while it runs
mkdir -p /etc/sysusers.d /etc/tmpfiles.d
install -m 0644 "$tmp/exnode.sysusers" /etc/sysusers.d/exnode.conf
install -m 0644 "$tmp/exnode.tmpfiles" /etc/tmpfiles.d/exnode.conf
install -m 0644 "$tmp/exnode.service" /etc/systemd/system/exnode.service
systemd-sysusers /etc/sysusers.d/exnode.conf
systemd-tmpfiles --create /etc/tmpfiles.d/exnode.conf
[ -e /etc/exnode/node.toml ] || install -m 0640 -o root -g exnode "$tmp/node.toml" /etc/exnode/node.toml

# Ollama's own settings: hide a custom model directory from exnode, and warn
# now about an Ollama open to the network (the agent would refuse to serve).
set -f
for kv in $(systemctl show -p Environment --value ollama.service 2>/dev/null || true); do
    case $kv in
        OLLAMA_MODELS=/*)
            dir=${kv#OLLAMA_MODELS=}
            case $dir in *[!A-Za-z0-9._/-]*) continue ;; esac
            mkdir -p /etc/systemd/system/exnode.service.d
            printf '[Service]\nInaccessiblePaths=-%s\n' "$dir" > /etc/systemd/system/exnode.service.d/10-ollama-models.conf
            say "hiding Ollama's model directory $dir from exnode" ;;
        OLLAMA_HOST=127.* | OLLAMA_HOST=localhost* | OLLAMA_HOST=http://127.* | OLLAMA_HOST=http://localhost*) ;;
        OLLAMA_HOST=*)
            echo "WARNING: ollama.service sets $kv; exnode refuses to serve an Ollama that other" >&2
            echo "WARNING: machines can reach.  Set OLLAMA_HOST=127.0.0.1:11434." >&2 ;;
    esac
done
set +f

systemctl daemon-reload
systemctl enable exnode.service
if systemctl is-active --quiet exnode.service; then
    systemctl restart exnode.service
    say "exnode was running; restarted it on $VERSION"
fi

cat <<EOF

exnode $VERSION is installed and enabled.  Next:
  1. Set hub_url and your [[models]] in /etc/exnode/node.toml.
  2. Keep the backend on loopback (Ollama: OLLAMA_HOST=127.0.0.1:11434).
  3. Enroll with the one-time token from the console (--token - reads it from stdin):
       sudo -u exnode /usr/local/bin/exnode init --config /etc/exnode/node.toml --token qd_reg_...
  4. Check:  sudo -u exnode /usr/local/bin/exnode doctor --config /etc/exnode/node.toml
  5. Start:  sudo systemctl start exnode && journalctl -u exnode -f
EOF
