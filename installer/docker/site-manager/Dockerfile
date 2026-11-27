# IDN Site Manager (IDN Media's: spec/extensions/idn-node.md §N.11, §N.12) as a container,
# built from the sources: the site authority, its discovery server and its web panel.
# From the repository's root:
#   docker build -f installer/docker/site-manager/Dockerfile -t idn-site-manager .
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
    cargo build --locked --release ${CARGO_BUILD_JOBS:+--jobs $CARGO_BUILD_JOBS} -p idn-site-manager --bin idn-site-manager \
 && cp target/release/idn-site-manager /usr/local/bin/

FROM debian:12-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data idn \
 && mkdir -p /data && chown idn /data
COPY --from=build /usr/local/bin/idn-site-manager /usr/local/bin/idn-site-manager
USER idn
VOLUME ["/data"]
# The web panel (HTTPS), the discovery server (machines and NDI-style clients).
# The panel, the Discovery Server, Voice (the panel's port + 1) and its calls, MQTT when
# the site's broker is on (F1-7).
EXPOSE 29202/tcp 5959/tcp 29203/tcp 29203/udp 29207/udp 1883/tcp
STOPSIGNAL SIGTERM
# IDN_SITE_PASSWORD: the panel's site password (at least 6 characters), set at each start.
ENTRYPOINT ["/usr/local/bin/idn-site-manager", "--trust-dir", "/data"]
