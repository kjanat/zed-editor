FROM node:24-bookworm-slim@sha256:2fe369e969550cde8e867afc3fe370b260140cab4a23d467074295b42163d553

ENV CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git unzip \
    && rm -rf /var/lib/apt/lists/* \
    && npm install --global dprint@0.57.4 \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
ENV PATH="/tmp/npm/bin:/opt/cargo/bin:${PATH}" \
    HOME=/tmp \
    CARGO_HOME=/tmp/cargo \
    RUSTUP_HOME=/tmp/rustup \
    NPM_CONFIG_PREFIX=/tmp/npm
