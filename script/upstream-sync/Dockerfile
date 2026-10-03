FROM node:24-bookworm-slim@sha256:2fe369e969550cde8e867afc3fe370b260140cab4a23d467074295b42163d553

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git unzip \
    && rm -rf /var/lib/apt/lists/*
RUN git config --system advice.detachedHead false
ARG MERGIRAF_VERSION=0.20.0
ARG MERGIRAF_SHA256=fc77469be936b02ab9021b984a3aebc9250a21ae3d264f2ef2198eb8b31b2a7b
RUN curl --proto '=https' --tlsv1.2 -sSfL --output /tmp/mergiraf.tar.gz \
        "https://codeberg.org/mergiraf/mergiraf/releases/download/v${MERGIRAF_VERSION}/mergiraf_x86_64-unknown-linux-musl.tar.gz" \
    && echo "${MERGIRAF_SHA256}  /tmp/mergiraf.tar.gz" | sha256sum --check --quiet \
    && tar -xzf /tmp/mergiraf.tar.gz -C /usr/local/bin mergiraf \
    && rm /tmp/mergiraf.tar.gz
ENV CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
RUN npm install --global dprint@0.58.0
ENV PATH="/opt/npm/bin:/opt/cargo/bin:${PATH}" \
    HOME=/tmp \
    DPRINT_CACHE_DIR=/opt/dprint-cache \
    NPM_CONFIG_CACHE=/opt/npm-cache \
    NPM_CONFIG_PREFIX=/opt/npm

# Match prepare.sh so configDir and cacheKeyFiles resolve against the checkout.
WORKDIR /tmp/work
COPY rust-toolchain.toml ./
RUN rustup show active-toolchain >/dev/null \
    && chmod -R a+rwX /opt/cargo /opt/rustup
COPY .dprint.jsonc tombi.toml rustfmt.toml ./
COPY .cargo/config.toml .cargo/config.toml
RUN dprint output-resolved-config >/dev/null \
    && printf 'value = 1\n' | dprint fmt --stdin warmup.toml >/dev/null \
    && printf 'fn main() {}\n' | dprint fmt --stdin warmup.rs >/dev/null \
    && chmod -R a+rwX /opt/dprint-cache /opt/npm-cache /opt/npm
WORKDIR /
RUN rm -rf /tmp/work
