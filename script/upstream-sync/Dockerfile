FROM node:24-bookworm-slim@sha256:2fe369e969550cde8e867afc3fe370b260140cab4a23d467074295b42163d553

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git unzip \
    && rm -rf /var/lib/apt/lists/*
ENV CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
RUN npm install --global dprint@0.57.4
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
