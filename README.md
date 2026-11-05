# exnode

`exnode` is the node agent for the Holdfast Exchange. A provider runs it next to their own
inference server: Ollama, or an OpenAI-compatible server such as llama.cpp's `llama-server` or
vLLM. The agent takes inference jobs from the exchange's hub, runs them on the local server,
streams the output back, and signs a receipt for each finished job.

This repository holds the agent and its packaging. It is public so that a provider can read
exactly what runs on their machine, and can check that a release was built from this source.
The hub is operated by Holdfast Services LLC and is not part of this repository.

## What the agent does, and what it never does

- It makes outbound connections to the hub and to the inference server on `127.0.0.1`, and to
  nothing else. It never listens on a port.
- It never writes to the model directory and never logs prompts or outputs.
- Its private key stays in `/var/lib/exnode/node.key` (mode 0600) and never leaves the machine.
- Before it takes jobs, and every ten minutes while serving, it checks its surroundings. It
  refuses to serve when the inference server has a known vulnerability or is reachable from the
  network. The list of checks is in `crates/exnode/src/advisories.rs`.
- The systemd unit runs it as an unprivileged user with no capabilities, on a read-only
  filesystem, with the usual model directories hidden from it.

[packaging/README.md](packaging/README.md) explains each of these in detail.

## Installing

Releases provide static Linux binaries for `x86_64` and `aarch64`, with an `install.sh` that
installs the binary, the system user and the systemd unit. Download the script from the latest
release, read it, then run it. Do not pipe it into a shell:

```sh
curl -fsSLO https://github.com/holdfast-services/exnode/releases/download/vX.Y.Z/install.sh
less install.sh
sudo sh install.sh
```

Enrolling a node takes a one-time token from the exchange console. The full steps, including a
manual install without the script, are in [packaging/README.md](packaging/README.md).

## Checking a release

This repository's release workflow builds every release and signs its `SHA256SUMS` with
Sigstore. The checksums cover every other file, and each binary also carries a build-provenance
attestation that names the commit it was built from. To check all three from a checkout:

```fish
source scripts/release-check.fish
ex_release_check holdfast-services/exnode vX.Y.Z
```

The same checks with plain `cosign`, `sha256sum` and `gh` are in
[packaging/README.md](packaging/README.md).

## Building from source

The agent needs Rust 1.97 or newer.

```sh
cargo build --release --locked -p exnode     # target/release/exnode
cargo test --workspace --locked
```

| Path | What it is |
|---|---|
| `crates/exnode` | The agent |
| `crates/ex-common` | Node identity (Ed25519) and the job types the agent and the hub share |
| `packaging/` | The systemd unit, sysusers and tmpfiles entries, and the default `node.toml` |
| `scripts/install.sh` | The installer each release ships |
| `node.example.toml` | Every configuration key, explained |

## Contributing

Issues are welcome. This repository is published from the exchange's main source tree, so an
accepted pull request is applied there and appears here in the next update, with you credited
as co-author.

## Security

Please report vulnerabilities by email, not in a public issue. See [SECURITY.md](SECURITY.md).

## License

Copyright 2026 Holdfast Services LLC. Licensed under the Apache License, Version 2.0. See
[LICENSE](LICENSE).
