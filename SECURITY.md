# Security policy

## Reporting a vulnerability

Email **security@holdfast.services**, or use **Report a vulnerability** on this repository's
Security tab. Include the version you ran (`exnode --version`), the steps
that show the problem, and what an attacker could do with it.

Please do not open a public issue or pull request for a vulnerability. Keep the details private
until a fixed release is out, so that providers can upgrade first.

## What is in scope

- The `exnode` agent, including its safety checks and its handling of jobs, keys and the
  enrollment token.
- The systemd unit and its sandbox.
- `install.sh`, and the release signing, checksums and build provenance.

Problems with the exchange's hub can be reported to the same address.

## Supported versions

Only the latest release receives fixes. To upgrade, run the newer release's `install.sh`. It
keeps your configuration and your node's key.
