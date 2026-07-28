# syntax=docker/dockerfile:1.7
# Aster multi-stage Docker build for the two production binaries.
#
# `aster_brokerd` is the authority side: it exclusively owns Postgres,
# policy, launch-token, and capsule-seal credentials. It is a long-lived
# service listening on a Unix-domain socket.
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
FROM rust:1.94-slim-bookworm@sha256:cf9dd0ec73e75f827fe59123fff9dc65af1a1c8363c3c31ee8d7f8ad0b6a5fb2 AS builder

# v8 = "0.106" links a prebuilt libv8.a. Fetch it explicitly with retries
# and verify the uncompressed archive hash before build.rs consumes it.
# pkg-config/libssl-dev cover transitive native dependencies.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        ca-certificates \
        curl \
        gzip \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy the full workspace. BuildKit cache mounts below preserve downloaded
# crates, rusty_v8, and release objects across source-only changes.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY proto ./proto

# Build once for both runtime images. The target directory is a cache mount,
# so copy final artifacts to a normal layer before the RUN ends. The V8
# archive cache is versioned separately: Cargo.lock pins the crate while the
# digest below pins the native binary that crate otherwise downloads at build.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=aster-rusty-v8-v0.106.0,target=/usr/local/cargo/.rusty_v8,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    set -eu; \
    archive=/usr/local/cargo/.rusty_v8/https___github_com_denoland_rusty_v8_releases_download_v0_106_0_librusty_v8_release_x86_64_unknown_linux_gnu_a_gz; \
    expected=7fb57ad05ebd5ec461830837e0af53156aee7a556ebb1ecc2a92c6e6f955ece7; \
    actual=; \
    if [ -s "$archive" ]; then \
        actual="$(gzip -dc "$archive" | sha256sum | cut -d ' ' -f1)"; \
    fi; \
    if [ "$actual" != "$expected" ]; then \
        rm -f "$archive" "$archive.tmp"; \
        curl --fail --location --show-error \
            --retry 5 --retry-all-errors --retry-delay 2 --retry-max-time 900 \
            --connect-timeout 20 --max-time 600 \
            --output "$archive.tmp" \
            https://github.com/denoland/rusty_v8/releases/download/v0.106.0/librusty_v8_release_x86_64-unknown-linux-gnu.a.gz; \
        actual="$(gzip -dc "$archive.tmp" | sha256sum | cut -d ' ' -f1)"; \
        if [ "$actual" != "$expected" ]; then \
            rm -f "$archive.tmp"; \
            echo "rusty_v8 archive digest mismatch: $actual" >&2; \
            exit 1; \
        fi; \
        mv "$archive.tmp" "$archive"; \
    fi; \
    CARGO_NET_RETRY=10 CARGO_HTTP_TIMEOUT=120 cargo build --release --locked \
        --bin aster_brokerd \
        --bin aster_launch_token \
        --bin aster_v8cell; \
    mkdir -p /out; \
    cp target/release/aster_brokerd \
        target/release/aster_launch_token \
        target/release/aster_v8cell \
        /out/

# ---------------------------------------------------------------------------
# Stage 2a — Runtime image for aster_brokerd.
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818 AS runtime-broker

# Non-root by default. UID 65532 is the conventional "nobody" range
# Distroless uses; we mirror it so volume permissions are predictable
# when Synapse mounts the broker socket from a sibling container.
RUN groupadd --gid 65532 --system aster \
    && useradd --uid 65532 --gid aster --system --no-create-home \
        --shell /sbin/nologin aster \
    && mkdir -p /run/aster \
    && chown aster:aster /run/aster

COPY --from=builder /out/aster_brokerd /usr/local/bin/aster_brokerd
COPY --from=builder /out/aster_launch_token /usr/local/bin/aster_launch_token
RUN chmod 0755 /usr/local/bin/aster_brokerd /usr/local/bin/aster_launch_token

USER aster:aster

# Default socket path lives under /run/aster, which the broker creates
# on bind. Operators override via ASTER_BROKER_SOCK at run time.
ENV ASTER_BROKER_SOCK=/run/aster/broker.sock

ENTRYPOINT ["/usr/local/bin/aster_brokerd"]

# ---------------------------------------------------------------------------
# Stage 2b — Runtime image for aster_v8cell.
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818 AS runtime-v8cell

RUN groupadd --gid 65532 --system aster \
    && useradd --uid 65532 --gid aster --system --no-create-home \
        --shell /sbin/nologin aster \
    && mkdir -p /run/aster /tenant \
    && chown aster:aster /run/aster /tenant

COPY --from=builder /out/aster_v8cell /usr/local/bin/aster_v8cell
RUN chmod 0755 /usr/local/bin/aster_v8cell

USER aster:aster

# Conventional defaults. Do not set ASTER_JS here: Synapse invokes cells
# through ASTER_JS_INLINE, and an image-level ASTER_JS default makes the
# binary reject the mutually-exclusive sources. File-based smokes pass
# ASTER_JS explicitly.
ENV ASTER_BROKER_SOCK=/run/aster/broker.sock

ENTRYPOINT ["/usr/local/bin/aster_v8cell"]
