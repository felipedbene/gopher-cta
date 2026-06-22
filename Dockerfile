# Fetcher image: builds the Rust binary, ships a minimal runtime.
#
# Default (tls-rustls) backend — fine for x86_64/arm64 clusters. (For big-endian
# PowerPC, use scripts/build-ppc64.sh instead; ring has no big-endian support.)
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release   # release profile already strips (see Cargo.toml)

FROM debian:bookworm-slim
# CA roots for the outbound CTA HTTPS fetch. The recorded fixture is embedded in
# the binary (include_str!), so no data files are needed for offline mode.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/gopher-cta /usr/local/bin/gopher-cta
USER nobody:nogroup
# Writes the published tree to /srv (mount a volume there). Override args/env as
# needed: gopher-cta [--once] [--interval <secs>] [--out <dir>]; CTA_TRAIN_API_KEY.
ENTRYPOINT ["gopher-cta"]
CMD ["--interval", "30", "--out", "/srv"]
