# Builder stage — uses the full Rust toolchain
FROM rust:1.94-alpine AS builder

# musl-dev needed for static linking, and openssl headers for reqwest
RUN apk add --no-cache musl-dev

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

# Build a fully static binary. The release profile in Cargo.toml
# enables LTO and size optimisation for a small final image.
RUN cargo build --release

# Runtime stage — just the binary on a minimal base
FROM alpine

RUN apk --no-cache add ca-certificates

COPY --from=builder /src/target/release/cap-link-notifier /usr/local/bin/cap-link-notifier

ENTRYPOINT ["cap-link-notifier"]
