#!/bin/sh
# install.sh: install the exnode provider agent from a provider bundle or from
# a signed GitHub release.
#
# POSIX sh, not bash or fish, because it is the first thing to run on a fresh
# machine, which may have neither; every Linux has a POSIX sh.  Linux with
# systemd only.
#
# The three-step install uses a bundle; nothing more is downloaded:
#     curl -fsSLO https://github.com/holdfast-services/exnode/releases/latest/download/exnode-linux-x86_64.tar.gz && tar xzf exnode-linux-x86_64.tar.gz
#     sudo ./exnode/install.sh
#     (answer the setup questions)
# Use exnode-linux-aarch64.tar.gz on 64-bit ARM.
#
# Or download only this script, read it and run it; it downloads the rest.
# Do not pipe it into a shell (curl ... | sh):
#     curl -fsSLO https://github.com/holdfast-services/exnode/releases/download/vX.Y.Z/install.sh
#     less install.sh
#     sudo sh install.sh
#
# Usage: sudo sh install.sh [--version vX.Y.Z] [--allow-unsigned] [--no-setup]
#
# Bundle mode, when the exnode binary sits next to this script: it copies the
# bundle's files to a private temp directory and installs those.  Download
# mode, otherwise: it downloads the binary for this CPU, its packaging files,
# SHA256SUMS and the Sigstore bundle for SHA256SUMS (--version picks the
# release).  Either way it verifies SHA256SUMS with cosign (the signer must be
# this repository's release workflow at that tag), then every file against
# SHA256SUMS.  Without cosign, or for a bundle without a signature, it stops
# unless --allow-unsigned, which leaves only the checksums.
#
# It installs /usr/local/bin/exnode, creates the exnode user and its
# directories, and installs and enables exnode.service.  Then, when stdin is a
# terminal, it runs `exnode setup`, which asks the questions, enrolls the node
# and starts it.  Otherwise, or with --no-setup, it prints the command.  On an
# upgrade of a node that is already set up and enrolled it restarts the agent
# and skips setup.
set -eu

# The release workflow fills these in.  An unstamped copy needs EXNODE_REPO
# to download, and cannot check a bundle's signature.
REPO="${EXNODE_REPO:-@REPO@}"
VERSION="${EXNODE_VERSION:-@VERSION@}"
ISSUER=https://token.actions.githubusercontent.com

die()  { echo "install.sh: $*" >&2; exit 1; }
say()  { echo "==> $*"; }
warn() { echo "WARNING: $*" >&2; }

allow_unsigned=0 run_setup=1 want_version=
while [ $# -gt 0 ]; do
    case $1 in
        --version)        [ $# -ge 2 ] || die "--version needs a tag"; want_version=$2; shift 2 ;;
        --allow-unsigned) allow_unsigned=1; shift ;;
        --no-setup)       run_setup=0; shift ;;
        -h|--help)        sed -n '2,/^set -eu$/{/^set -eu$/d;s/^# \{0,1\}//;p;}' "$0"; exit 0 ;;
        *)                die "unknown option: $1 (try --help)" ;;
    esac
done

[ "$(id -u)" -eq 0 ] || die "run it as root: sudo $0"
[ "$(uname -s)" = Linux ] || die "exnode releases are for Linux"

# A bundle when the binary sits next to this script.
here=$(cd "$(dirname "$0")" 2>/dev/null && pwd -P) || here=
mode=download
if [ -n "$here" ] && [ -f "$here/exnode" ] && [ ! -L "$here/exnode" ]; then
    mode=bundle
fi

needed="sha256sum awk od install mktemp systemctl systemd-sysusers systemd-tmpfiles"
if [ "$mode" = download ]; then needed="curl $needed"; fi
for cmd in $needed; do
    command -v "$cmd" >/dev/null 2>&1 || die "$cmd is required"
done
case $(uname -m) in
    x86_64 | amd64)  target=x86_64-unknown-linux-musl  elf=3e00 ;;
    aarch64 | arm64) target=aarch64-unknown-linux-musl elf=b700 ;;
    *)               die "there is no exnode release for $(uname -m)" ;;
esac

stamped=1
case $REPO in
    @*@)                                  stamped=0 ;;
    *[!A-Za-z0-9._/-]* | */*/* | /* | */) die "bad repository '$REPO' (want owner/name)" ;;
    */*)                                  ;;
    *)                                    die "bad repository '$REPO' (want owner/name)" ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' INT TERM

# Not signed, or not checkably: go on only with --allow-unsigned.
unsigned() {
    if [ "$allow_unsigned" -eq 1 ]; then
        warn "$1, so the release signature was NOT checked."
        warn "the checksums below catch a corrupt download, not a forged one."
    else
        die "$1, so the release signature cannot be checked.
$2
or pass --allow-unsigned to rely on checksums alone (they catch corruption, not forgery)."
    fi
}
no_cosign_fix="Install cosign (https://docs.sigstore.dev/cosign/system_config/installation/) and run this again,"

# $1: SHA256SUMS, $2: its Sigstore bundle.  REPO and VERSION name the signer.
verify_signature() {
    identity="https://github.com/$REPO/.github/workflows/release.yml@refs/tags/$VERSION"
    cosign verify-blob --bundle "$2" \
        --certificate-identity "$identity" --certificate-oidc-issuer "$ISSUER" \
        "$1" >/dev/null || die "SHA256SUMS is not signed by $identity; not installing"
    say "signature verified: SHA256SUMS was signed by $identity"
}

# Every file in $@ must be listed in $tmp/SHA256SUMS and match.
verify_sums() {
    for f in "$@"; do
        want=$(awk -v f="$f" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/SHA256SUMS")
        got=$(sha256sum "$tmp/$f" | awk '{ print $1 }')
        [ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch for $f; not installing"
    done
    say "checksums match SHA256SUMS"
}

if [ "$mode" = bundle ]; then
    [ -z "$want_version" ] || die "--version is for download mode; a bundle installs the exnode it holds"
    case $VERSION in v[0-9]*) shown=$VERSION ;; *) shown="(unreleased build)"; stamped=0 ;; esac
    case $VERSION in *[!A-Za-z0-9._@-]*) die "bad version '$VERSION'" ;; esac
    say "installing exnode $shown from the bundle in $here"
    # Check and install a private copy, so nothing can change in between.
    files="exnode install.sh exnode.service exnode.sysusers exnode.tmpfiles node.toml.example"
    for f in SHA256SUMS $files; do
        [ -f "$here/$f" ] || die "the bundle has no $f; download and extract it again"
        cp "$here/$f" "$tmp/$f"
    done
    if [ -f "$here/SHA256SUMS.sigstore.json" ]; then cp "$here/SHA256SUMS.sigstore.json" "$tmp/"; fi
    if ! command -v cosign >/dev/null 2>&1; then
        unsigned "cosign is not installed" "$no_cosign_fix"
    elif [ ! -f "$tmp/SHA256SUMS.sigstore.json" ]; then
        unsigned "this bundle has no signature (SHA256SUMS.sigstore.json)" \
            "Download the bundle from the release page and extract it again,"
    elif [ "$stamped" -eq 0 ]; then
        unsigned "this bundle's install.sh was not stamped by a release, so it cannot tell whose signature to expect" \
            "Use a released bundle,"
    else
        verify_signature "$tmp/SHA256SUMS" "$tmp/SHA256SUMS.sigstore.json"
    fi
    verify_sums $files
    bin=$tmp/exnode
    # the ELF machine field: x86-64 is 0x3e, aarch64 0xb7 (little-endian)
    got_elf=$(od -An -tx1 -j18 -N2 "$bin" | tr -d ' \n')
    [ "$got_elf" = "$elf" ] || die "this bundle's exnode is not for $(uname -m); download exnode-linux-${target%%-*}.tar.gz"
else
    [ "$stamped" -eq 1 ] || die "set EXNODE_REPO=owner/name: this copy was not stamped by a release"
    [ -z "$want_version" ] || VERSION=$want_version
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
    shown=$VERSION
    files="exnode-$target exnode.service exnode.sysusers exnode.tmpfiles node.toml"
    say "downloading exnode $VERSION ($target) from $releases"
    for f in SHA256SUMS SHA256SUMS.sigstore.json $files; do
        curl -fsSL --proto '=https' --proto-redir '=https' --retry 3 -o "$tmp/$f" \
            "$releases/download/$VERSION/$f" || die "could not download $f"
    done
    if command -v cosign >/dev/null 2>&1; then
        verify_signature "$tmp/SHA256SUMS" "$tmp/SHA256SUMS.sigstore.json"
    else
        unsigned "cosign is not installed" "$no_cosign_fix"
    fi
    verify_sums $files
    bin=$tmp/exnode-$target
fi

install -m 0755 "$bin" /usr/local/bin/.exnode.new
mv -f /usr/local/bin/.exnode.new /usr/local/bin/exnode   # atomic, even while it runs
mkdir -p /etc/sysusers.d /etc/tmpfiles.d
install -m 0644 "$tmp/exnode.sysusers" /etc/sysusers.d/exnode.conf
install -m 0644 "$tmp/exnode.tmpfiles" /etc/tmpfiles.d/exnode.conf
install -m 0644 "$tmp/exnode.service" /etc/systemd/system/exnode.service
systemd-sysusers /etc/sysusers.d/exnode.conf
systemd-tmpfiles --create /etc/tmpfiles.d/exnode.conf
if [ "$mode" = bundle ]; then
    # setup writes node.toml itself; the example explains every key
    install -m 0644 "$tmp/node.toml.example" /etc/exnode/node.toml.example
else
    [ -e /etc/exnode/node.toml ] || install -m 0640 -o root -g exnode "$tmp/node.toml" /etc/exnode/node.toml
fi

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
            warn "ollama.service sets $kv; exnode refuses to serve an Ollama that other"
            warn "machines can reach.  Set OLLAMA_HOST=127.0.0.1:11434." ;;
    esac
done
set +f

systemctl daemon-reload
systemctl enable exnode.service
if systemctl is-active --quiet exnode.service; then
    systemctl restart exnode.service
    say "exnode was running; restarted it on $shown"
fi

rm -rf "$tmp"
trap - EXIT
if [ -s /etc/exnode/node.toml ] && [ -e /var/lib/exnode/node.key ]; then
    cat <<EOF

exnode $shown is installed, and this node was already set up and enrolled.
  Change its settings:  sudo exnode setup
  Check it:             sudo -u exnode /usr/local/bin/exnode doctor --config /etc/exnode/node.toml
EOF
    exit 0
fi
if [ "$run_setup" -eq 1 ] && [ -t 0 ]; then
    say "exnode $shown is installed. Now the setup questions (run them again any time: sudo exnode setup)"
    exec /usr/local/bin/exnode setup
fi
cat <<EOF

exnode $shown is installed and enabled.  Next, answer the setup questions:
  sudo exnode setup
For a scripted install, setup takes flags instead (exnode setup --help), e.g.
  sudo exnode setup --yes --model qwen3:30b --token-stdin < token-file
EOF
