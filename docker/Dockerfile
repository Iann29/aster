# Aster v0.3 — multi-stage Docker build for the two production binaries.
#
# `aster_brokerd` is the read-authority side: holds the seal key and the
# (eventually real) database handle. Long-lived service, listens on a
# Unix-domain socket.
#
# `aster_v8cell` is the execution side: receives a sealed capsule, runs
# tenant JS in V8, and never touches a database directly. Short-lived
# (one invocation per process) by design.
#
# Both images come from the same workspace build to keep the linker
# state consistent and to share Cargo's compile cache across stages.
# Pick the one you want with `--target=runtime-broker` or
# `--target=runtime-v8cell`. CI does both and tags them
# `aster-brokerd:<tag>` / `aster-v8cell:<tag>` respectively.

# ---------------------------------------------------------------------------
# Stage 1 — Builder. Compiles every workspace binary in release mode.
# ---------------------------------------------------------------------------
FROM rust:1.94-slim-bookworm AS builder

# v8 = "0.106" downloads a prebuilt libv8.a during build.rs; the only
# C/C++ glue we still need is pkg-config + libssl-dev for crates that
# pull TLS in transitively. ca-certificates lets cargo resolve the
# crates.io HTTPS endpoints. Everything else is wiped to keep the
# layer slim.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy workspace manifests + source. Order matters for layer caching:
# manifests first so a code-only edit reuses the dependency layer.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY proto ./proto

# Build both binaries in one cargo invocation so they share the dep
# graph compile. --locked enforces the committed Cargo.lock; in CI
# this is a guard, locally it just matches the bench numbers we ship.
RUN cargo build --release --locked \
        --bin aster_brokerd \
        --bin aster_v8cell

# ---------------------------------------------------------------------------
# Stage 2a — Runtime image for aster_brokerd.
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime-broker

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root by default. UID 65532 is the conventional "nobody" range
# Distroless uses; we mirror it so volume permissions are predictable
# when Synapse mounts the broker socket from a sibling container.
RUN groupadd --gid 65532 --system aster \
    && useradd --uid 65532 --gid aster --system --no-create-home \
        --shell /sbin/nologin aster \
    && mkdir -p /run/aster \
    && chown aster:aster /run/aster

COPY --from=builder /build/target/release/aster_brokerd /usr/local/bin/aster_brokerd
RUN chmod 0755 /usr/local/bin/aster_brokerd

USER aster:aster

# Default socket path lives under /run/aster, which the broker creates
# on bind. Operators override via ASTER_BROKER_SOCK at run time.
ENV ASTER_BROKER_SOCK=/run/aster/broker.sock

ENTRYPOINT ["/usr/local/bin/aster_brokerd"]

# ---------------------------------------------------------------------------
# Stage 2b — Runtime image for aster_v8cell.
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime-v8cell

# libstdc++6 is already in bookworm-slim; libv8 is statically linked
# into the binary by the v8 crate, so nothing extra to install for
# JavaScript execution. ca-certificates stays in case future tenant
# code wants TLS via a host-mediated egress broker.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --gid 65532 --system aster \
    && useradd --uid 65532 --gid aster --system --no-create-home \
        --shell /sbin/nologin aster \
    && mkdir -p /run/aster /tenant \
    && chown aster:aster /run/aster /tenant

COPY --from=builder /build/target/release/aster_v8cell /usr/local/bin/aster_v8cell
RUN chmod 0755 /usr/local/bin/aster_v8cell

USER aster:aster

# Conventional defaults — operators override via env.
ENV ASTER_BROKER_SOCK=/run/aster/broker.sock
ENV ASTER_JS=/tenant/main.js

ENTRYPOINT ["/usr/local/bin/aster_v8cell"]
