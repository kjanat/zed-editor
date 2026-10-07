FROM ghcr.io/jdx/mise:2026.10.3-debian@sha256:58c4c847f5518a9a87a9a582886a485443dbca5eb024426577f58d586a0990c6

SHELL ["/bin/bash", "-o", "pipefail", "-c"]
RUN apt-get update && apt-get install -y --no-install-recommends unzip \
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
ARG RUSTUP_VERSION=1.29.1
ARG RUSTUP_INIT_SHA256=dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71
RUN curl --proto '=https' --tlsv1.2 -sSfL --output /tmp/rustup-init \
        "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init" \
    && echo "${RUSTUP_INIT_SHA256}  /tmp/rustup-init" | sha256sum --check --quiet \
    && chmod +x /tmp/rustup-init \
    && /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain none \
    && rm /tmp/rustup-init

# Mise shims read the mise config of the candidate checkout.
WORKDIR /opt/sync-tools
COPY .mise.toml mise.lock ./
RUN mise trust .mise.toml \
    && mise install --locked dprint node tombi \
    && ln -s "$(mise which dprint)" "$(mise which node)" "$(mise which tombi)" /usr/local/bin/
ENV PATH="/opt/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
    HOME=/tmp \
    DPRINT_CACHE_DIR=/opt/dprint-cache

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
    && chmod -R a+rwX /opt/dprint-cache
WORKDIR /
RUN rm -rf /tmp/work
