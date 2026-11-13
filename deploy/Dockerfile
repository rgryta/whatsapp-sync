# Built from the repo root:
#   docker build -f deploy/Dockerfile -t whatsapp-sync:latest .
#
# One image, two binaries: the services differ only by `command`. Both are Rust, so the
# runtime stage carries nothing but the two binaries.

# The latest stable Rust, matching the manifest's rust-version. Bump both together.
FROM rust:1.100-slim-trixie@sha256:e9d95b74296f7126a83ae3d041aeaba1ff4c478f13abc5cc034fcf7c131c3d4a AS rust
WORKDIR /build
RUN rustup target add x86_64-unknown-linux-musl \
    && apt-get update -qq \
    && apt-get install -y -qq musl-tools \
    && rm -rf /var/lib/apt/lists/*
# `db-migrations/` is here because `crates/core/src/db.rs` embeds the migrations with
# `include_str!`, so the build needs them even though the binary then carries its own copy.
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY src/ ./src/
COPY db-migrations/ ./db-migrations/
# `tests/` is excluded by .dockerignore and not needed: `--bins` does not build it.
# `--workspace --bins`, the same line `.github/workflows/ci.yml` runs: `--bin` resolves only
# inside the default-run package, so naming `whatsapp-client` that way fails.
RUN cargo build --release --workspace --bins --target x86_64-unknown-linux-musl

# Nothing but the binaries. No libc, no shell, no certificate store, and each absence is
# fine on purpose:
#
#   * TLS trust is bundled: WhatsApp's websocket (`tokio-tungstenite`), the media CDN and
#     whisper (`reqwest`) all use `webpki-roots`. `ci.yml` fails the build if a release
#     binary ever reaches for a system store.
#   * Every time is UTC; nothing reads a timezone database.
#   * Nothing writes to a temporary directory. DNS resolves from the /etc/resolv.conf
#     Docker mounts in, which musl reads directly.
FROM scratch
# Root-owned and merely executable, so neither service can rewrite its own binary.
COPY --from=rust /build/target/x86_64-unknown-linux-musl/release/whatsapp-mcp /usr/local/bin/whatsapp-mcp
COPY --from=rust /build/target/x86_64-unknown-linux-musl/release/whatsapp-client /usr/local/bin/whatsapp-client
# Numeric: there is no /etc/passwd here. 65532 is distroless' `nonroot`.
USER 65532:65532
EXPOSE 8000
# No shell, so `whatsapp-mcp --health-check` is the only probe; deploy/compose.yaml runs it.
#
# GIT_SHA LAST, on purpose: above the COPYs it would enter their layer cache key.
ARG GIT_SHA=unknown
ENV WHATSAPP_GIT_SHA=$GIT_SHA
# The client service overrides this with `whatsapp-client`.
CMD ["whatsapp-mcp"]
