# Neolink Docker image build scripts
# Copyright (c) 2020 George Hilliard,
#                    Andrew King,
#                    Miroslav Šedivý
# SPDX-License-Identifier: AGPL-3.0-only

FROM docker.io/rust:slim-bookworm AS build
ARG TARGETPLATFORM

ENV DEBIAN_FRONTEND=noninteractive
WORKDIR /usr/local/src/neolink
COPY . /usr/local/src/neolink

# Build the main program or copy from artifact
#
# We prefer copying from artifact to reduce
# build time on the github runners
#
# Because of this though, during normal
# github runner ops we are not testing the
# docker to see if it will build from scratch
# so if it is failing please make a PR
#
# `JEMALLOC_SYS_WITH_LG_PAGE=16` on the from-scratch branch is not optional.
# jemalloc's page size must be at least the running kernel's, and it is chosen
# at compile time. Building arm64 under buildx/QEMU on a 4K-page host makes
# jemalloc pick LG_PAGE=12, and that binary aborts at startup on a 16K-page
# arm64 kernel -- a Raspberry Pi 5. The CI cross and native jobs already export
# this (build.yml); the artifact branch below inherits their binary, but the
# from-scratch branch builds its own and needs it too.
#
# hadolint ignore=DL3008
RUN  echo "TARGETPLATFORM: ${TARGETPLATFORM}"; \
  if [ -f "${TARGETPLATFORM}/neolink" ]; then \
    echo "Restoring from artifact"; \
    mkdir -p /usr/local/src/neolink/target/release/; \
    cp "${TARGETPLATFORM}/neolink" "/usr/local/src/neolink/target/release/neolink"; \
  else \
    echo "Building from scratch"; \
    apt-get update && \
        apt-get upgrade -y && \
        apt-get install -y --no-install-recommends \
          build-essential \
          openssl \
          libssl-dev \
          ca-certificates \
          libgstrtspserver-1.0-dev \
          libgstreamer1.0-dev \
          protobuf-compiler \
          libglib2.0-dev && \
        apt-get clean -y && rm -rf /var/lib/apt/lists/* ; \
    JEMALLOC_SYS_WITH_LG_PAGE=16 cargo build --release; \
  fi

# Create the release container. Match the base OS used to build
FROM debian:bookworm-slim
ARG TARGETPLATFORM
ARG REPO
ARG VERSION
ARG OWNER

LABEL description="An image for the neolink program which is a reolink camera to rtsp translator"
LABEL repository="$REPO"
LABEL version="$VERSION"
LABEL maintainer="$OWNER"

# hadolint ignore=DL3008
RUN apt-get update && \
    apt-get upgrade -y && \
    apt-get install -y --no-install-recommends \
        openssl \
        dnsutils \
        iputils-ping \
        ca-certificates \
        libgstrtspserver-1.0-0 \
        libgstreamer1.0-0 \
        gstreamer1.0-tools \
        gstreamer1.0-x \
        gstreamer1.0-plugins-base \
        gstreamer1.0-plugins-good \
        gstreamer1.0-plugins-bad \
        gstreamer1.0-libav && \
    apt-get clean -y && rm -rf /var/lib/apt/lists/*

COPY --from=build \
  /usr/local/src/neolink/target/release/neolink \
  /usr/local/bin/neolink
COPY docker/entrypoint.sh /entrypoint.sh

RUN gst-inspect-1.0; \
    chmod +x "/usr/local/bin/neolink" && \
    "/usr/local/bin/neolink" --version && \
    mkdir -m 0700 /root/.config/

ENV NEO_LINK_MODE="rtsp" NEO_LINK_PORT=8554

CMD /usr/local/bin/neolink "${NEO_LINK_MODE}" --config /etc/neolink.toml
ENTRYPOINT ["/entrypoint.sh"]
EXPOSE ${NEO_LINK_PORT}

