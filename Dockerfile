# Builds CIWS from source. The published image, ghcr.io/warcon-app/ciws, is made by
# .github/release.Dockerfile from the release's own binaries.

# ---- build ----
FROM rust:1-bookworm AS builder
WORKDIR /app

# Dependencies first, so a change to the source does not rebuild them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

# ---- run ----
# The binary, the C runtime and CA certificates; no shell, no package manager.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /app/target/release/ciws /usr/local/bin/ciws
# Mount the folder that holds ciws.toml here. Nothing else is kept: no volume needed.
WORKDIR /config
ENV CIWS_CONFIG=/config/ciws.toml \
    CIWS_ADMIN_LISTEN=0.0.0.0:7781
EXPOSE 7780 7781
ENTRYPOINT ["/usr/local/bin/ciws"]
CMD ["run"]
