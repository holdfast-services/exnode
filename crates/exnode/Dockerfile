# ── build stage ───────────────────────────────────────────────────────────────
FROM rust:1.97-bookworm AS builder

WORKDIR /src
COPY . .
# --locked: build exactly what Cargo.lock pins, as the release workflow does
RUN cargo build --release --locked -p exnode

# ── runtime stage ─────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/exnode /usr/local/bin/exnode

RUN useradd --system --uid 10001 exnode
USER exnode

# Config and key are mounted at runtime
VOLUME ["/config"]

ENV RUST_LOG=exnode=info

# The safety checks see this container's network namespace: run the backend
# beside exnode on 127.0.0.1 (or share the host's network) and publish no
# backend port.  Check with: docker run … exnode doctor --config /config/node.toml
# A refusal exits 78.  Docker cannot exempt one exit status from a restart
# policy, so under on-failure it retries; each start refuses again until fixed.
ENTRYPOINT ["exnode"]
CMD ["run", "--config", "/config/node.toml"]
