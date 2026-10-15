# syntax=docker/dockerfile:1
FROM --platform=$BUILDPLATFORM ghcr.io/rust-cross/cargo-zigbuild:0.23.4 AS build
ARG TARGETARCH
ARG CARGO_ARGS=""
ARG VERSION=""
ENV COLLECTOR_VERSION=$VERSION
WORKDIR /src
COPY Cargo.toml Cargo.lock collector.example.toml ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    case "$TARGETARCH" in \
      amd64) t=x86_64-unknown-linux-musl ;; \
      arm64) t=aarch64-unknown-linux-musl; export JEMALLOC_SYS_WITH_LG_PAGE=16 ;; \
      *) echo "unsupported arch $TARGETARCH" >&2; exit 1 ;; \
    esac \
 && cargo zigbuild --release --locked --target "$t" $CARGO_ARGS \
 && cp "target/$t/release/collector" /collector

# glibc base so an injected nvidia-smi can run; the binary itself is static.
FROM gcr.io/distroless/base-debian12@sha256:fabbf1c0c357a3d42550111351daed089b20a2c954df13ee2fcff60602515e84
LABEL org.opencontainers.image.source="https://github.com/joeyyax/collector" \
      org.opencontainers.image.description="Host, container and GPU metrics plus container logs from one small binary" \
      org.opencontainers.image.licenses="MIT"
COPY --from=build /collector /usr/local/bin/collector
COPY deploy/collector.toml /etc/collector/collector.toml
ENV COLLECTOR_CONFIG=/etc/collector/collector.toml
EXPOSE 9273
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 CMD ["/usr/local/bin/collector", "health"]
ENTRYPOINT ["/usr/local/bin/collector"]
