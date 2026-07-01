# Build stage
FROM rust:trixie AS builder

# Deno runs the ./x build orchestrator (sets up the static-lib sysroot)
COPY --from=denoland/deno:bin /deno /usr/local/bin/deno

WORKDIR /usr/src/app

# clang/clang++ are required: cg3-rs hardcodes them (with ThinLTO) for its cmake
# and cc builds. lld is the matching LLVM linker (see linker wrapper below).
RUN apt-get update && apt-get install -y \
    cmake \
    build-essential \
    clang \
    lld \
    bison \
    flex \
    pkg-config \
    libboost-dev \
    libsqlite3-dev \
    && rm -rf /var/lib/apt/lists/*

# cg3's static libs contain clang ThinLTO bitcode. rustc would otherwise link
# with its bundled rust-lld (LLVM version mismatch); force clang + system lld.
# CARGO_TARGET_*_LINKER is honoured even though ./x sets its own RUSTFLAGS.
RUN printf '#!/bin/sh\nexec clang -fuse-ld=lld "$@"\n' > /usr/local/bin/clang-lld \
    && chmod +x /usr/local/bin/clang-lld
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=/usr/local/bin/clang-lld

# Copy the build orchestrator and sources
COPY build ./build
COPY x build.rs Cargo.toml Cargo.lock index.html ./
COPY src ./src

# Build release binary via ./x: downloads the static-lib sysroot (provides static
# ICU via CG3_SYSROOT), then builds and strips the binary. An explicit --target
# also avoids ./x's target-cpu=native (only set for native/no-target builds).
RUN ./x build --target x86_64-unknown-linux-gnu

# Runtime stage
FROM debian:trixie-slim

# Install required packages
RUN dpkg --add-architecture amd64 && \
    apt-get update && apt-get install -y \
    wget \
    gnupg2 \
    ca-certificates \
    lsb-release \
    libicu76 \
    && rm -rf /var/lib/apt/lists/*

# Copy the binary from builder stage
COPY --from=builder /usr/src/app/target/x86_64-unknown-linux-gnu/release/divvun-worker-grammar /usr/local/bin/divvun-worker-grammar

# vislcg3's constraint-grammar engine recurses deeply. divvun_runtime runs each
# cg3/hfst stage on a raw std::thread (no explicit stack size), which defaults to
# 2 MiB — not enough for real grammars, so processing overflows the stack and the
# process aborts with SIGSEGV. std::thread honours RUST_MIN_STACK for threads
# spawned without an explicit size, so raise the default for every such thread.
ENV RUST_MIN_STACK=67108864

# Create non-root user
RUN useradd -r -u 1000 grammar

# Create data directory and set permissions
RUN mkdir -p /data && chown grammar:grammar /data

USER grammar

EXPOSE 4000

# Health check for Kubernetes and Docker
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:4000/health || exit 1

ENTRYPOINT ["/usr/local/bin/divvun-worker-grammar"]
