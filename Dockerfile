# EmpireBT, serving the web UI. See DOCKER.md.
#
#   docker build -t empirebt .
#
# Two stages: Rust builds the binary on Debian; the image you run is
# distroless (glibc, CA certificates, a non-root user, and our binary;
# no shell or package manager).
#
# The release workflow builds the binaries natively for each architecture
# first and passes --build-arg BINARIES=prebuilt, so making the arm64
# image needs no emulated compiling.
ARG BINARIES=build

# ---- build -------------------------------------------------------------------
FROM rust:1-bookworm AS build
WORKDIR /src

# Downloaded crates and build output are cached between builds.
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p empirebt-console \
 && install -D target/release/empirebt /out/usr/local/bin/empirebt

# Where settings, the session, and downloads go: writable by whichever user
# the container runs as (see `user:` in docker-compose.yml).
RUN mkdir -p /out/data /out/downloads && chmod 0777 /out/data /out/downloads

# ---- prebuilt: binaries from the release workflow, in dist/<arch>/ ----------
FROM busybox:1.37 AS prebuilt
ARG TARGETARCH
COPY dist/${TARGETARCH}/empirebt /out/usr/local/bin/empirebt
RUN chmod 0755 /out/usr/local/bin/empirebt \
 && mkdir -p /out/data /out/downloads && chmod 0777 /out/data /out/downloads

FROM ${BINARIES} AS binaries

# ---- run ---------------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot
LABEL org.opencontainers.image.title="EmpireBT" \
      org.opencontainers.image.licenses="GPL-3.0-or-later" \
      org.opencontainers.image.authors="ThroneEmpire" \
      org.opencontainers.image.source="https://github.com/ThroneEmpire/EmpireBT"

COPY --from=binaries /out/ /

ENV HOME=/data \
    EMPIREBT_DATA_DIR=/data \
    EMPIREBT_STORAGE_DOWNLOAD_DIR=/downloads \
    EMPIREBT_WEB_ADDRESS=0.0.0.0

VOLUME ["/data", "/downloads"]

# 8080: the web UI. 6881: peers, over TCP and UDP (uTP and the DHT).
EXPOSE 8080 6881 6881/udp

# Healthy once the web UI accepts connections.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s \
    CMD ["/usr/local/bin/empirebt", "health"]

ENTRYPOINT ["/usr/local/bin/empirebt"]
CMD ["serve"]
