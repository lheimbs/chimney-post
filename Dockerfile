# syntax=docker/dockerfile:1
#
# Chiseled-distroless image: chimney-post's binary has no dynamic dependencies
# beyond libc/libgcc (no OpenSSL -- rustls-tls; no libsqlite3 -- rusqlite
# "bundled" statically links it). The runtime stage below is `FROM scratch`
# plus only the glibc/CA-cert slices Canonical's `chisel` cuts from real
# Ubuntu 24.04 packages -- no shell, no package manager, nothing else.
#
# Built natively per architecture (see .github/workflows/release.yml and
# ci.yml) rather than cross-compiled, so no TARGETARCH/--target plumbing is
# needed here: `cargo build --release` already targets the host the builder
# stage is running on.

FROM rust:1.93-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# Slices matched against `ldd target/release/chimney-post`:
#   libc6_libs      -> ld-linux*.so, libc.so.*, libm.so.*
#   libgcc-s1_libs  -> libgcc_s.so.*
#   ca-certificates_data -> /etc/ssl/certs/ca-certificates.crt, read at
#     runtime by reqwest's rustls-tls-native-roots feature when connecting to
#     the configured Matrix homeserver
#   base-files_base -> minimal /etc, /tmp, ownership base the other slices and
#     the runtime directories below build on
FROM ubuntu:24.04 AS chisel
ARG CHISEL_VERSION=v1.5.0
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
RUN ARCH=$(dpkg --print-architecture) && \
    curl -fsSL "https://github.com/canonical/chisel/releases/download/${CHISEL_VERSION}/chisel_${CHISEL_VERSION}_linux_${ARCH}.tar.gz" \
      | tar -xz -C /usr/local/bin chisel
RUN mkdir -p /rootfs && chisel cut --release ubuntu-24.04 --root /rootfs \
      libc6_libs \
      libgcc-s1_libs \
      ca-certificates_data \
      base-files_base
# The service's persistent state (SQLite outbox + Matrix E2EE store) and its
# config directory, pre-created and owned by the non-root UID the final stage
# runs as -- `FROM scratch` has no shell to mkdir/chown at runtime.
RUN mkdir -p /rootfs/etc/chimney-post /rootfs/var/lib/chimney-post && \
    chown -R 65532:65532 /rootfs/etc/chimney-post /rootfs/var/lib/chimney-post

FROM scratch
COPY --from=chisel /rootfs /
COPY --from=builder /build/target/release/chimney-post /usr/local/bin/chimney-post

USER 65532:65532
ENV CHIMNEY_CONFIG=/etc/chimney-post/config.toml
EXPOSE 2525
VOLUME ["/var/lib/chimney-post"]
ENTRYPOINT ["/usr/local/bin/chimney-post"]
