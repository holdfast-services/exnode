#!/usr/bin/env fish
# release-check.fish: verify a published exnode release before installing it.
#
#   source scripts/release-check.fish
#   ex_release_check holdfast-services/exnode vX.Y.Z
#
# Downloads the release into a temporary directory and checks, in order: that SHA256SUMS
# was signed by the repository's release workflow at that tag (Sigstore cosign), that every
# file matches its checksum, and, when gh is installed, the binaries' build provenance.
# Needs curl, cosign (v3) and sha256sum; gh is optional.

function ex_release_check -a repo tag
    if not string match -qr '^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$' -- "$repo"
        or not string match -qr '^v[0-9][A-Za-z0-9._-]*$' -- "$tag"
        echo "usage: ex_release_check <owner/repo> <vX.Y.Z>" >&2
        return 1
    end
    for cmd in curl cosign sha256sum
        if not command -q $cmd
            echo "$cmd is required" >&2
            return 1
        end
    end
    set -l dir (mktemp -d)
    set -l base https://github.com/$repo/releases/download/$tag
    set -l get curl -fsSL --proto =https --proto-redir =https
    echo "→ $base into $dir"
    for f in SHA256SUMS SHA256SUMS.sigstore.json
        $get -o $dir/$f $base/$f; or begin; echo "could not download $f" >&2; return 1; end
    end
    cosign verify-blob --bundle $dir/SHA256SUMS.sigstore.json \
        --certificate-identity https://github.com/$repo/.github/workflows/release.yml@refs/tags/$tag \
        --certificate-oidc-issuer https://token.actions.githubusercontent.com $dir/SHA256SUMS
    or begin; echo "SHA256SUMS is not signed by $repo's release workflow at $tag" >&2; return 1; end
    for f in (string replace -r '^[0-9a-f]{64} [ *]' '' < $dir/SHA256SUMS)
        if not string match -qr '^[A-Za-z0-9._-]+$' -- $f
            echo "odd file name in SHA256SUMS: '$f'" >&2
            return 1
        end
        $get -o $dir/$f $base/$f; or begin; echo "could not download $f" >&2; return 1; end
    end
    pushd $dir
    sha256sum --strict -c SHA256SUMS
    set -l sums $status
    popd
    test $sums -eq 0; or begin; echo "checksum mismatch" >&2; return 1; end
    set -l checked "signature and checksums"
    if command -q gh
        for bin in $dir/exnode-*-linux-musl
            gh attestation verify $bin --repo $repo; or return 1
        end
        set checked "signature, checksums and provenance"
    else
        echo "gh is not installed: build provenance not checked"
    end
    echo "✓ $repo $tag: $checked check out ($dir)"
end

# ex_release_rebuild: rebuild a release from source and check it is byte-identical.
#
#   source scripts/release-check.fish
#   ex_release_rebuild holdfast-services/exnode vX.Y.Z
#
# Clones the tag into a temporary directory, builds both binaries with
# scripts/build-release.sh (the release workflow's own build script, with its
# pinned toolchain and zig), and compares them with the release's SHA256SUMS,
# after checking that SHA256SUMS was signed by the release workflow at that tag.
# Needs git, curl, rustup, python3, cosign (v3) and sha256sum. Takes a few minutes.

function ex_release_rebuild -a repo tag
    if not string match -qr '^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$' -- "$repo"
        or not string match -qr '^v[0-9][A-Za-z0-9._-]*$' -- "$tag"
        echo "usage: ex_release_rebuild <owner/repo> <vX.Y.Z>" >&2
        return 1
    end
    for cmd in git curl rustup python3 cosign sha256sum
        if not command -q $cmd
            echo "$cmd is required" >&2
            return 1
        end
    end
    set -l dir (mktemp -d)
    set -l base https://github.com/$repo/releases/download/$tag
    set -l get curl -fsSL --proto =https --proto-redir =https
    for f in SHA256SUMS SHA256SUMS.sigstore.json
        $get -o $dir/$f $base/$f; or begin; echo "could not download $f" >&2; return 1; end
    end
    cosign verify-blob --bundle $dir/SHA256SUMS.sigstore.json \
        --certificate-identity https://github.com/$repo/.github/workflows/release.yml@refs/tags/$tag \
        --certificate-oidc-issuer https://token.actions.githubusercontent.com $dir/SHA256SUMS
    or begin; echo "SHA256SUMS is not signed by $repo's release workflow at $tag" >&2; return 1; end

    git clone --quiet --depth 1 --branch $tag https://github.com/$repo $dir/src
    or begin; echo "could not clone $repo at $tag" >&2; return 1; end
    python3 -m venv $dir/zig
    and $dir/zig/bin/pip install --quiet --no-deps --require-hashes --only-binary :all: \
        -r $dir/src/scripts/zig-requirements.txt
    or begin; echo "could not install the pinned zig and cargo-zigbuild" >&2; return 1; end

    set -l failed 0
    for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl
        pushd $dir/src
        rustup toolchain install >/dev/null 2>&1
        and env PATH=(string join : $dir/zig/bin $PATH) sh scripts/build-release.sh $target $dir/out
        set -l built $status
        popd
        test $built -eq 0; or begin; echo "build failed for $target" >&2; return 1; end
        set -l want (string match -r "^[0-9a-f]{64}(?= [ *]exnode-$target\$)" < $dir/SHA256SUMS)
        set -l got (sha256sum $dir/out/exnode-$target | string split -f1 ' ')
        if test -n "$want" -a "$want" = "$got"
            echo "✓ exnode-$target rebuilds byte-identical ($got)"
        else
            echo "✗ exnode-$target: release has '$want', rebuild has $got" >&2
            set failed 1
        end
    end
    test $failed -eq 0; or return 1
    echo "✓ $repo $tag: both binaries rebuild from source to the signed checksums ($dir)"
end
