exnode provider bundle
======================

exnode is the Holdfast Exchange node agent. It runs next to your inference
engine (Ollama, vLLM, llama.cpp's llama-server or another OpenAI-compatible
server) and serves jobs from the exchange. It only makes outbound connections,
to the hub and to the engine on 127.0.0.1. It never listens on a port, never
writes to your model directory and never logs prompts or outputs.

Install
-------

You already did step 1 (download and extract). From the directory that holds
this exnode/ folder:

  2. sudo ./exnode/install.sh
  3. Answer the setup questions.

Before step 2, have your engine serving a model on 127.0.0.1, and install
cosign (https://docs.sigstore.dev/cosign/system_config/installation/) so the
installer can check the bundle's signature. Without cosign it stops, unless
you pass --allow-unsigned, which checks the checksums only.

The installer checks SHA256SUMS's signature and every file against it, copies
exnode to /usr/local/bin, creates the unprivileged exnode user, installs the
sandboxed systemd service, and starts "exnode setup". Setup asks for the hub,
finds your engine, lists its models, and asks for the context window, the
level of service, how many jobs at once and your serving hours. Then it asks
for a one-time enrollment token (the exchange site, "Host a model" page,
step 6), enrolls the node and starts it.

To change anything later: sudo exnode setup
To check the node:        sudo -u exnode exnode doctor --config /etc/exnode/node.toml
To follow it:             journalctl -u exnode -f

What is here
------------

  exnode                    the agent (static Linux binary)
  install.sh                the installer (POSIX sh; read it first)
  exnode.service            the systemd unit
  exnode.sysusers           the exnode system user
  exnode.tmpfiles           /etc/exnode and /var/lib/exnode
  node.toml.example         every config key, explained
  THIRD-PARTY-NOTICES.txt   licenses of the open-source code inside exnode
  SHA256SUMS                checksums of all of the above
  SHA256SUMS.sigstore.json  the release workflow's signature over SHA256SUMS

The full guide, with the manual install and what the agent refuses, is
packaging/README.md in the source repository.
