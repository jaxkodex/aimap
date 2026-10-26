# syntax=docker/dockerfile:1
# The service image: a fully static musl binary. rust/.cargo/config.toml pins
# the x86_64-unknown-linux-musl target and rustup's own rust-lld with the
# self-contained CRT, so no cross toolchain is needed — and the dependency
# tree is pure Rust, so no C compiler for the target either. TLS roots are
# compiled in (webpki-roots), so the runtime image needs no ca-certificates.
#
# Lives at the repo root because Railway builds this path for the service;
# the legacy Python image is kept as Dockerfile.python. Build context is the
# repository root: docker build -t aimap .
FROM rust:1-slim AS build
RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /app/rust
COPY rust/Cargo.toml rust/Cargo.lock ./
COPY rust/.cargo/ .cargo/
COPY rust/src/ src/
COPY rust/migrations/ migrations/
RUN cargo build --release --bin aimap

FROM alpine:3.20
# Same system user as the Python image; busybox sh stays for `railway ssh`.
RUN adduser -S -D -u 10001 -h /app aimap
COPY --from=build --chown=aimap:aimap /app/rust/target/x86_64-unknown-linux-musl/release/aimap /usr/local/bin/aimap
WORKDIR /app
USER aimap
ENTRYPOINT ["aimap"]
CMD ["all"]
