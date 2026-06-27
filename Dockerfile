# Fetcher image: builds the Rust binary, ships a minimal runtime.
#
# Default (tls-rustls) backend — fine for x86_64/arm64 clusters. (For big-endian
# PowerPC, use scripts/build-ppc64.sh instead; ring has no big-endian support.)
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release   # release profile already strips (see Cargo.toml)

# Source tarball served over gopher (/src.tar.gz): the whole repo minus build
# artifacts and -- crucially -- the MaxMind GeoLite2 .mmdb, which is NOT
# redistributable (it's gitignored and only lives locally). The build FAILS
# loudly if any .mmdb ever slips into the archive.
RUN tar czf /src.tar.gz \
      --exclude='./target' --exclude='./target-ppc64' --exclude='./dist' \
      --exclude='./public' --exclude='./.git' \
      --exclude='*.mmdb' --exclude='*.log' --exclude='./.env' --exclude='*.ppm' \
      -C /src . \
 && if tar tzf /src.tar.gz | grep -qiE '\.mmdb'; then \
        echo 'ERROR: .mmdb leaked into src.tar.gz' >&2; exit 1; fi \
 && echo "src.tar.gz built ($(du -h /src.tar.gz | cut -f1)), no .mmdb"

FROM debian:bookworm-slim
# CA roots for the outbound CTA HTTPS fetch. The recorded fixture is embedded in
# the binary (include_str!), so no data files are needed for offline mode.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/gopher-cta /usr/local/bin/gopher-cta
# The source tarball served at the /src.tar.gz selector (excludes the .mmdb).
COPY --from=build /src.tar.gz /usr/local/share/gopher-cta/src.tar.gz
USER nobody:nogroup
# Writes the published tree to /srv (mount a volume there). Override args/env as
# needed: gopher-cta [--once] [--interval <secs>] [--out <dir>]; CTA_TRAIN_API_KEY.
ENTRYPOINT ["gopher-cta"]
CMD ["--interval", "30", "--out", "/srv"]
