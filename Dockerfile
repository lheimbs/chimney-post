# syntax=docker/dockerfile:1
#
# Distroless runtime: chimney-post's binary has no dynamic dependencies
# beyond libc/libgcc (no OpenSSL -- rustls-tls; no libsqlite3 -- rusqlite
# "bundled" statically links it), and reqwest's rustls-tls-native-roots
# feature reads /etc/ssl/certs/ca-certificates.crt at runtime when connecting
# to the configured Matrix homeserver. gcr.io/distroless/cc-debian12 ships
# exactly that (glibc, libgcc, CA certs) and nothing else -- no shell, no
# package manager -- and its `:nonroot` tag already runs as a fixed
# non-root UID (65532), so it needs no extra OS layer of our own.
#
# Both stages are Debian bookworm-based (rust:1.93-bookworm here,
# distroless/cc-debian12 below), so the builder's glibc floor (2.36) matches
# what the runtime image ships -- no cross-distro ABI mismatch to worry
# about.
#
# Built natively per architecture (see .github/workflows/release.yml and
# ci.yml) rather than cross-compiled, so no TARGETARCH/--target plumbing is
# needed here: `cargo build --release` already targets the host the builder
# stage is running on.

# Base images are pinned by digest, not tag, for the same reason release.yml
# refuses a build cache: this image is cosign-signed and carries a SLSA
# attestation, so a retagged or compromised upstream would otherwise yield a
# compromised binary with a perfectly valid signature. Both digests are
# multi-arch indexes, so each native runner still resolves its own
# architecture. Update them deliberately, the way Action pins are updated.
FROM rust:1.93-bookworm@sha256:7c4ae649a84014c467d79319bbf17ce2632ae8b8be123ac2fb2ea5be46823f31 AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked
# The service's persistent state (SQLite outbox + Matrix E2EE store) and its
# config directory, pre-created here and owned by the non-root UID the final
# stage runs as -- distroless has no shell to mkdir/chown at runtime.
RUN mkdir -p /rootfs/etc/chimney-post /rootfs/var/lib/chimney-post

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=builder --chown=65532:65532 /rootfs/etc/chimney-post /etc/chimney-post
COPY --from=builder --chown=65532:65532 /rootfs/var/lib/chimney-post /var/lib/chimney-post
COPY --from=builder /build/target/release/chimney-post /usr/local/bin/chimney-post

USER 65532:65532
ENV CHIMNEY_CONFIG=/etc/chimney-post/config.toml
EXPOSE 2525
VOLUME ["/var/lib/chimney-post"]
ENTRYPOINT ["/usr/local/bin/chimney-post"]
