#!/bin/sh
# bundle.sh: lay out and pack an exnode provider bundle.  The release workflow
# and `ex_bundle` in scripts/dev.fish both use it, so a local bundle is laid
# out exactly like a released one.  POSIX sh because the release runner has
# no fish.
#
#   sh packaging/bundle.sh stage <dir> <exnode binary> <install.sh> <notices file>
#       makes <dir>/exnode/ with the binary, install.sh, the systemd, sysusers
#       and tmpfiles files, node.toml.example, README.txt, the notices and a
#       SHA256SUMS covering every one of them
#   (the release workflow signs <dir>/exnode/SHA256SUMS here, writing
#    SHA256SUMS.sigstore.json beside it)
#   sh packaging/bundle.sh pack <dir> <out.tar.gz>
#       packs <dir>/exnode/ reproducibly: sorted, owned by root, fixed mtime
#       (SOURCE_DATE_EPOCH, else 0), gzip without a name or time
#
# Extracting the tarball gives ./exnode/, and the provider runs
# sudo ./exnode/install.sh from there.
set -eu

die() { echo "bundle.sh: $*" >&2; exit 1; }
here=$(cd "$(dirname "$0")" && pwd -P)

case ${1:-} in
    stage)
        [ $# -eq 5 ] || die "usage: bundle.sh stage <dir> <exnode binary> <install.sh> <notices file>"
        dir=$2/exnode bin=$3 inst=$4 notices=$5
        [ -f "$bin" ] && [ -f "$inst" ] && [ -f "$notices" ] || die "missing an input file"
        [ ! -e "$dir" ] || die "$dir already exists"
        mkdir -p "$dir"
        install -m 0755 "$bin"                             "$dir/exnode"
        install -m 0755 "$inst"                            "$dir/install.sh"
        install -m 0644 "$here/systemd/exnode.service"     "$dir/exnode.service"
        install -m 0644 "$here/sysusers.d/exnode.conf"     "$dir/exnode.sysusers"
        install -m 0644 "$here/tmpfiles.d/exnode.conf"     "$dir/exnode.tmpfiles"
        install -m 0644 "$here/node.toml"                  "$dir/node.toml.example"
        install -m 0644 "$here/bundle-README.txt"          "$dir/README.txt"
        install -m 0644 "$notices"                         "$dir/THIRD-PARTY-NOTICES.txt"
        (cd "$dir" && sha256sum exnode install.sh exnode.service exnode.sysusers exnode.tmpfiles \
            node.toml.example README.txt THIRD-PARTY-NOTICES.txt > SHA256SUMS)
        ;;
    pack)
        [ $# -eq 3 ] || die "usage: bundle.sh pack <dir> <out.tar.gz>"
        [ -f "$2/exnode/SHA256SUMS" ] || die "$2/exnode is not a staged bundle"
        (cd "$2/exnode" && sha256sum --quiet --strict -c SHA256SUMS) || die "$2/exnode does not match its SHA256SUMS"
        out=$3
        case $out in /*) ;; *) out=$(pwd -P)/$out ;; esac
        tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@${SOURCE_DATE_EPOCH:-0}" \
            --format=gnu -C "$2" -cf - exnode | gzip -n -9 > "$out.part"
        mv -f "$out.part" "$out"
        ;;
    *)
        sed -n '2,/^set -eu$/{/^set -eu$/d;s/^# \{0,1\}//;p;}' "$0" >&2
        exit 2 ;;
esac
