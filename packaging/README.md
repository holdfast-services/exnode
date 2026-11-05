# Installing exnode as a provider

`exnode` is the node agent a provider runs next to their inference backend
(Ollama, or an OpenAI-compatible server such as llama.cpp's `llama-server` or
vLLM). It makes outbound connections to the hub and to the backend on
`127.0.0.1`, and to nothing else. It never listens on a port, never writes to
the model directory, and never logs prompts or outputs. Its private key stays
in `/var/lib/exnode/node.key` (mode 0600) and never leaves the machine.

## The short way: `install.sh`

Do **not** pipe the installer into a shell (`curl … | sh`). A piped script
runs before you have seen it, and a dropped connection can run half of it.
Download it, read it, then run it:

```sh
curl -fsSLO https://github.com/holdfast-services/exnode/releases/download/vX.Y.Z/install.sh
less install.sh
sudo sh install.sh                 # --version vX.Y.Z to pick another release
```

The script needs `curl` and systemd. It checks the release's signature with
[cosign](https://docs.sigstore.dev/cosign/system_config/installation/) (v3).
Without cosign it stops, unless you pass `--allow-unsigned`, in which case only
the checksums are checked. Checksums catch a corrupt download, not a forged one.
It then does the manual steps below and prints the rest.

## The manual way

**1. Download** the binary for your CPU (`x86_64` or `aarch64`), the packaging
files, `SHA256SUMS` and its Sigstore bundle from the release page:

```sh
tag=vX.Y.Z repo=holdfast-services/exnode arch=x86_64
base=https://github.com/$repo/releases/download/$tag
for f in exnode-$arch-unknown-linux-musl exnode.service exnode.sysusers exnode.tmpfiles \
         node.toml SHA256SUMS SHA256SUMS.sigstore.json; do
    curl -fsSLO --proto '=https' "$base/$f"
done
```

**2. Verify.** The signature proves that this repository's release workflow
built and signed `SHA256SUMS` at that tag. The checksums then cover every
other file:

```sh
cosign verify-blob --bundle SHA256SUMS.sigstore.json \
  --certificate-identity "https://github.com/$repo/.github/workflows/release.yml@refs/tags/$tag" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
gh attestation verify exnode-$arch-unknown-linux-musl --repo "$repo"   # build provenance, optional
```

Each file also has its own `.sigstore.json` bundle, if you only want one.
From a checkout, `source scripts/release-check.fish` and then `ex_release_check holdfast-services/exnode vX.Y.Z`
does all of this in a temp directory.

**3. Install** the binary, the system user, the directories and the unit:

```sh
sudo install -m 0755 exnode-$arch-unknown-linux-musl /usr/local/bin/exnode
sudo install -m 0644 exnode.sysusers /etc/sysusers.d/exnode.conf
sudo install -m 0644 exnode.tmpfiles /etc/tmpfiles.d/exnode.conf
sudo install -m 0644 exnode.service  /etc/systemd/system/exnode.service
sudo systemd-sysusers /etc/sysusers.d/exnode.conf      # user and group "exnode", no login
sudo systemd-tmpfiles --create /etc/tmpfiles.d/exnode.conf
sudo install -m 0640 -o root -g exnode node.toml /etc/exnode/node.toml
sudo systemctl daemon-reload && sudo systemctl enable exnode
```

**4. Configure.** Set `hub_url` and your `[[models]]` in
`/etc/exnode/node.toml` (every key is explained in `node.example.toml`). Bind the
backend to loopback. For Ollama, put `Environment=OLLAMA_HOST=127.0.0.1:11434`
in its unit; for `llama-server`, use `--host 127.0.0.1`. A backend API key
goes in `/etc/exnode/exnode.env` (root, mode 0600), as the variable that
`backend_api_key_env` names.

**5. Enroll** with the one-time `qd_reg_…` token from the console. This
creates the key and registers the node. The token is never written to disk.
`--token qd_reg_…` works, but it shows the token in the process list and your
shell history. `--token -` reads it from stdin instead:

```sh
read -rs TOKEN    # bash or zsh; paste the token, then Enter
printf '%s\n' "$TOKEN" | sudo -u exnode /usr/local/bin/exnode init --config /etc/exnode/node.toml --token -
unset TOKEN
```

The agent runs the safety checks below before it enrolls. A refused node
never registers.

**6. Check, then start:**

```sh
sudo -u exnode /usr/local/bin/exnode doctor --config /etc/exnode/node.toml
sudo systemctl start exnode && journalctl -u exnode -f
```

## What the agent refuses

At startup, and every ten minutes while serving, the agent checks its
surroundings. A failing check stops it taking jobs, with a log line starting
`refusing to serve:`. It resumes on its own once the check passes. At startup a
refusal exits with status 78, which the unit does not restart; fix the cause
and run `systemctl restart exnode`.

| Check | Refuses | Warns |
|---|---|---|
| Ollama (`/api/version`) | below 0.17.1 (CVE-2026-7482), ≤ 0.12.3 (CVE-2025-63389), or no readable version | |
| llama.cpp server (`/props` build) | below b8146 (CVE-2025-53630, CVE-2026-27940), below b7824 (CVE-2026-33298) | build number unreadable |
| other OpenAI-compatible server | | version is never checked |
| NVIDIA Container Toolkit (`nvidia-ctk`) | below 1.17.8 (CVE-2025-23266, CVE-2025-23267) | below 1.19.1 (CVE-2026-24260; NVIDIA has not confirmed a fixed release) |
| exposure | backend URL off loopback, `OLLAMA_HOST` open to the network, backend port listening beyond loopback | |
| model directories | | readable from inside the service |

The table lives in `crates/exnode/src/advisories.rs` and nowhere else. To
change it, add a row there.

## The sandbox

`exnode.service` runs as the unprivileged `exnode` user with no capabilities.
The whole filesystem is read-only to it except `/var/lib/exnode`. Homes are
hidden. `/usr/share/ollama`, `/var/lib/ollama`, `/var/lib/docker`,
`/var/lib/containers`, `/opt/models`, `/srv/models` and `/var/lib/models` are
inaccessible, so the agent sees models only through the backend's API.
`install.sh` also hides Ollama's `OLLAMA_MODELS` directory if one is set. Hide
other directories with `systemctl edit exnode`:

```ini
[Service]
InaccessiblePaths=-/data/models
```

`PrivateDevices=yes` stays on, even on GPU machines. The backend opens the
GPU in its own service; exnode only speaks HTTP to it.
`RestrictAddressFamilies=AF_INET AF_INET6` allows outbound TCP and DNS only.
The system call filter is `@system-service` without the privileged and
resource groups. `systemd-analyze security exnode.service` rates the unit
about 1.2 ("OK").

## Upgrading and removing

To upgrade, run a newer `install.sh`. It replaces the binary and the unit,
keeps `node.toml`, the key and your drop-ins, and restarts the agent if it was
running. To remove exnode:

```sh
sudo systemctl disable --now exnode
sudo rm /usr/local/bin/exnode /etc/systemd/system/exnode.service \
        /etc/sysusers.d/exnode.conf /etc/tmpfiles.d/exnode.conf
sudo rm -r /etc/systemd/system/exnode.service.d /etc/exnode /var/lib/exnode   # the key too: the node's identity is gone
sudo userdel exnode
```
