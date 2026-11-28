# Installing exnode as a provider

`exnode` is the node agent a provider runs next to their inference backend
(Ollama, or an OpenAI-compatible server such as llama.cpp's `llama-server` or
vLLM). It makes outbound connections to the hub and to the backend on
`127.0.0.1`, and to nothing else. It never listens on a port, never writes to
the model directory, and never logs prompts or outputs. Its private key stays
in `/var/lib/exnode/node.key` (mode 0600) and never leaves the machine.

## The three-step install

One bundle per processor type, the same for every accelerator (NVIDIA, AMD,
Tenstorrent: exnode never opens the GPU, the engine does). Its name carries no
version, so the `latest` link always fetches the newest release. Use
`aarch64` in place of `x86_64` on 64-bit ARM.

```sh
# 1. download and extract
curl -fsSLO https://github.com/holdfast-services/exnode/releases/latest/download/exnode-linux-x86_64.tar.gz && tar xzf exnode-linux-x86_64.tar.gz
# 2. install
sudo ./exnode/install.sh
# 3. answer the setup questions
```

Before step 2, have your engine serving a model on `127.0.0.1`, and install
[cosign](https://docs.sigstore.dev/cosign/system_config/installation/) (v3).
`install.sh` is POSIX sh; read it first if you like (`less exnode/install.sh`).
It copies the bundle to a private temp directory, checks the signature on the
bundle's `SHA256SUMS` (the signer must be this repository's release workflow
at that tag) and every file against `SHA256SUMS`, then does the manual steps
below. Without cosign it stops, unless you pass `--allow-unsigned`, in which
case only the checksums are checked. Checksums catch a corrupt download, not
a forged one.

When stdin is a terminal it then runs `exnode setup`; otherwise it prints that
command. On an upgrade of a node that is already set up and enrolled, it
restarts the agent on the new version and skips setup. `--no-setup` skips it
too.

### The setup questions

`sudo exnode setup` asks, in order, and can be run again any time to change an
answer (each question defaults to the current setting):

1. **Hub URL**, default `https://exchange.holdfast.services`. https only
   (plain http only for a hub on this machine). Setup checks that it answers
   and reads its model registry.
2. **Engine.** It looks for Ollama on `127.0.0.1:11434` and an
   OpenAI-compatible server (vLLM, llama.cpp, SGLang) on `127.0.0.1:8000` and
   `:8080`, or takes another loopback URL. It then runs the agent's safety
   checks (below) and stops, changing nothing, if they would refuse.
3. **Models to serve**, from the engine's list, with the ones the hub's
   registry lists (by name and hash) marked `*`. With an OpenAI-compatible
   server it also asks each model's weights hash (`sha256sum` of the file the
   server loaded): that server has no digest API.
4. **Context window** for each model, default the engine's own window when it
   says, else 8192 tokens.
5. **Level of service.** Open (runs on your own Linux, checked by hidden
   tests) is the one available now. Attested (Holdfast Node OS with
   TPM-measured boot) and Sealed (confidential-computing GPUs) are shown, and
   coming soon.
6. **Most jobs at once**, default 1, or 2 when `nvidia-smi` reports more than
   40 GB of GPU memory.
7. **Serving hours**, such as `18:00-08:00` in local time, or always. For a weekly
   schedule, a share of the machine or blackout dates, edit `node.toml` afterwards
   (see "When to serve, and how much" below); setup keeps a schedule it finds.

Then it writes `/etc/exnode/node.toml` (root:exnode, mode 0640, atomically).
If the file is already there it lists what would change and asks first. It
asks for the one-time enrollment token, with the input hidden; create it on
the exchange site's "Host a model" page, step 6. It enrolls exactly as
`exnode init --token -` does, as the `exnode` user, and the token is never
saved or shown. Last, it offers to start the service.

For a scripted install, `--yes` asks nothing and takes the answers from flags,
the current config and the defaults:

```sh
sudo exnode setup --yes --hub https://exchange.holdfast.services \
    --backend-url http://127.0.0.1:11434 --model qwen3:30b:32768 \
    --max-concurrency 1 --serve-hours 18:00-08:00 --token-stdin < token-file
```

`--model` is `NAME`, `NAME:CONTEXT`, or either followed by `@sha256:<hex>`
(the weights hash, needed for an OpenAI-compatible server). A trailing
`:digits` is read as the context window, so a model whose name ends in
`:digits` needs the window after it (`gemma:7:8192`).

### When to serve, and how much

You decide how much of the machine the exchange gets, and when. Every limit is
set in `node.toml` on the machine; the exchange is told what the node is doing
and is never in charge of it.

`max_concurrency` is what the machine can run at full capacity. A weekly
schedule gives each window a share of that:

```toml
timezone = "America/Chicago"      # a datacenter server is usually on UTC

[[schedule]]                      # nights on weekdays, in full
days  = ["weekdays"]
hours = "18:00-08:00"
share = 100

[[schedule]]                      # weekends, at half
days  = ["weekends"]
hours = "all day"
share = 50

[blackout]                        # nothing on these dates
dates = ["2026-12-24..2027-01-02"]
```

Rules are read in order and the first that covers a moment decides it; no match
means off, and no rules means on, in full, all the time. A window may cross
midnight, and its morning belongs to the day it started on. A share is a
percentage of `max_concurrency`, rounded up, and a machine that serves at all
serves at least one job. `serve_hours = "18:00-08:00"` is the one-line form (one
window, every day, in full); use it or `[[schedule]]`, not both.

`exnode schedule` prints the next seven days as you wrote them, and what the node
is doing now. `exnode doctor` shows the same. To stop at once, run
`exnode pause` (or `exnode pause --for 2h`) and `exnode resume` to end it; the
running agent notices within seconds. A job already running finishes, and a job
poll that started just before a window closed can still bring one job, up to
`poll_secs` later.

Outside its schedule the node keeps heartbeating, so it stays registered, and its
heartbeat says it is off duty. The exchange does not count an off-duty node as
supply, so a buyer is told at once when no node is serving and when the next
window opens, instead of a job waiting for a node that is not there. The exchange
shows that a node is off duty and when it says it returns; it does not publish
the schedule itself.

## Just the script: `install.sh` downloads the rest

Do **not** pipe the installer into a shell (`curl … | sh`). A piped script
runs before you have seen it, and a dropped connection can run half of it.
Download it, read it, then run it:

```sh
curl -fsSLO https://github.com/holdfast-services/exnode/releases/download/vX.Y.Z/install.sh
less install.sh
sudo sh install.sh                 # --version vX.Y.Z to pick another release
```

Run this way (no `exnode` binary beside it), the script needs `curl`. It
downloads the binary for this CPU, the packaging files, `SHA256SUMS` and its
Sigstore bundle, checks them as above, installs, and then runs setup the same
way. It also installs a template `/etc/exnode/node.toml`, which setup offers to
replace.

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
Each provider bundle (`exnode-linux-<arch>.tar.gz`) holds its own
`SHA256SUMS` and `SHA256SUMS.sigstore.json`, checked the same way from inside
the extracted `exnode/` directory.
From a checkout, `source scripts/release-check.fish` and then
`ex_release_check holdfast-services/exnode vX.Y.Z` does all of this in a temp
directory, and `ex_release_rebuild holdfast-services/exnode vX.Y.Z` rebuilds
both binaries from the tagged source and checks they match the signed
checksums. `packaging/bundle.sh` lays out the same bundle from a local build (its
header shows how), for testing; install an unsigned one with `--allow-unsigned`.

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

**4. Configure.** Run `sudo exnode setup` (it also does steps 5 and 6), or
set `hub_url` and your `[[models]]` in `/etc/exnode/node.toml` by hand (every
key is explained in `node.example.toml`). Bind the
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

To upgrade, extract a newer bundle and run its `install.sh` (or run a newer
standalone `install.sh`). It replaces the binary and the unit, keeps
`node.toml`, the key and your drop-ins, and restarts the agent if it was
running. To remove exnode:

```sh
sudo systemctl disable --now exnode
sudo rm /usr/local/bin/exnode /etc/systemd/system/exnode.service \
        /etc/sysusers.d/exnode.conf /etc/tmpfiles.d/exnode.conf
sudo rm -r /etc/systemd/system/exnode.service.d /etc/exnode /var/lib/exnode   # the key too: the node's identity is gone
sudo userdel exnode
```
