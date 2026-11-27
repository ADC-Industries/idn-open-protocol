# IDN Site Manager | Industrial (spec/extensions/idn-industrial.md I.12) as a container,
# built from the sources. From the repository's root:
#   docker build -f installer/docker/site-manager-industrial/Dockerfile -t idn-site-manager-industrial .
FROM debian:12 AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl build-essential pkg-config \
 && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.99.1
WORKDIR /src
COPY . .
ARG IDN_BUILD=docker
# On a small machine: --build-arg CARGO_BUILD_JOBS=2
ARG CARGO_BUILD_JOBS
RUN --mount=type=cache,target=/opt/cargo/registry --mount=type=cache,target=/src/target \
    IDN_BUILD="$IDN_BUILD" IDN_BUILD_DATE="$(date '+%Y-%m-%d %H:%M')" \
    cargo build --locked --release ${CARGO_BUILD_JOBS:+--jobs $CARGO_BUILD_JOBS} -p idn-industrial --bin idn-site-manager-industrial \
 && cp target/release/idn-site-manager-industrial /usr/local/bin/

FROM debian:12-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data idn \
 && mkdir -p /data && chown idn /data
COPY --from=build /usr/local/bin/idn-site-manager-industrial /usr/local/bin/idn-site-manager-industrial
USER idn
ENV IDN_DATA=/data IDN_HTTP=0.0.0.0:8080 IDN_QUIC=0.0.0.0:29206 IDN_MQTT=0.0.0.0:1883
VOLUME ["/data"]
# HTTP (web UI, API, the WebSocket link at /idn/link, MQTT over WebSocket at /mqtt);
# QUIC links; MQTT.
# Web UI, QUIC, MQTT and Voice's calls (WebRTC; IDN_VOICE_PUBLIC when forwarded from
# outside; F8-8); Voice for handsets over HTTPS and over IDN's link (QUIC, the same
# number on UDP).
EXPOSE 8080/tcp 29206/udp 1883/tcp 29207/udp 29203/tcp 29203/udp
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD ["/usr/local/bin/idn-site-manager-industrial", "healthcheck"]
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/idn-site-manager-industrial"]
