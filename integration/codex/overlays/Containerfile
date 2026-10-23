# Ubuntu 26.04 verification image for Nyacode.
# Docker and Podman both build this file; it uses no BuildKit-only syntax.
#
#   docker build -f Containerfile -t nyacode-verify:rust-1.95 .
#   podman build -f Containerfile -t nyacode-verify:rust-1.95 .
#
# scripts/verify-container.sh is the supported entrypoint. The repository is
# bind-mounted at runtime. This image does not copy it, and it does not expect
# the host home directory to be mounted.

FROM ubuntu:26.04

ARG RUST_VERSION=1.95.0
ARG NEXTEST_VERSION=0.9.148
ARG SCCACHE_VERSION=0.18.0

ENV DEBIAN_FRONTEND=noninteractive \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    HOME=/home/verifier \
    RUST_MIN_STACK=67108864 \
    CARGO_INCREMENTAL=0 \
    CARGO_NET_GIT_FETCH_WITH_CLI=true \
    CARGO_TERM_COLOR=always

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bubblewrap \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        curl \
        git \
        gstreamer1.0-plugins-base \
        gstreamer1.0-plugins-good \
        gstreamer1.0-tools \
        libasound2-dev \
        libcap-dev \
        libclang-dev \
        libssl-dev \
        libgstreamer1.0-dev \
        libgstreamer-plugins-base1.0-dev \
        libopus-dev \
        util-linux \
        perl \
        pkg-config \
        protobuf-compiler \
        python3 \
        ripgrep \
        xz-utils \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /home/verifier /var/cache/sccache /var/cache/nyacode-target \
        /var/cache/nyacode-v8 /usr/local/cargo/registry /usr/local/cargo/git /usr/local/rustup \
    && chmod 0777 /home/verifier /var/cache/sccache /var/cache/nyacode-target \
        /var/cache/nyacode-v8 /usr/local/cargo /usr/local/cargo/registry /usr/local/cargo/git

# Install the toolchain under /usr/local and symlink it into /usr/local/bin so
# cargo is on PATH for login shells, non-login shells, and stripped PATH values.
RUN curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal \
            --default-toolchain "${RUST_VERSION}" \
            --component rustfmt,clippy,rust-src \
    && ln -sfn /usr/local/cargo/bin/cargo /usr/local/bin/cargo \
    && ln -sfn /usr/local/cargo/bin/cargo-clippy /usr/local/bin/cargo-clippy \
    && ln -sfn /usr/local/cargo/bin/cargo-fmt /usr/local/bin/cargo-fmt \
    && ln -sfn /usr/local/cargo/bin/clippy-driver /usr/local/bin/clippy-driver \
    && ln -sfn /usr/local/cargo/bin/rustc /usr/local/bin/rustc \
    && ln -sfn /usr/local/cargo/bin/rustdoc /usr/local/bin/rustdoc \
    && ln -sfn /usr/local/cargo/bin/rustfmt /usr/local/bin/rustfmt \
    && ln -sfn /usr/local/cargo/bin/rustup /usr/local/bin/rustup \
    && rustc --version \
    && rustc --version | grep -F "rustc ${RUST_VERSION}" \
    && command -v cargo \
    && command -v rustc \
    && cargo --version

# Verify the exact release bytes for both supported Linux host architectures.
RUN set -eu; \
    case "$(uname -m)" in \
        x86_64) nextest_sha=223ba936714cc861a9cdedad5a8eb664dfdd69d6fbdf6cea6aa8184f523290dc; \
                sccache_sha=45f1447fbe231e3037bde351ef70677dd212216c8d62ae7ca409fecc4d6acc89 ;; \
        aarch64) nextest_sha=81469eb963ef883fd590e60189e77d6aba3d91b418591d3391f50fe3e9480d2b; \
                 sccache_sha=2b3284d5da3b46a47dc4229e75bb7b88ac4aa99c8d754fb7d2f84997e5a4354a ;; \
        *) echo "Unsupported verification host architecture" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-${NEXTEST_VERSION}/cargo-nextest-${NEXTEST_VERSION}-$(uname -m)-unknown-linux-gnu.tar.gz" -o /tmp/nextest.tar.gz; \
    echo "${nextest_sha}  /tmp/nextest.tar.gz" | sha256sum --check; \
    tar -xzf /tmp/nextest.tar.gz -C /usr/local/cargo/bin; \
    ln -sfn /usr/local/cargo/bin/cargo-nextest /usr/local/bin/cargo-nextest; \
    cargo nextest --version; \
    curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-$(uname -m)-unknown-linux-musl.tar.gz" -o /tmp/sccache.tar.gz; \
    echo "${sccache_sha}  /tmp/sccache.tar.gz" | sha256sum --check; \
    tar -xzf /tmp/sccache.tar.gz --strip-components=1 -C /usr/local/bin "sccache-v${SCCACHE_VERSION}-$(uname -m)-unknown-linux-musl/sccache"; \
    sccache --version; \
    pkg-config --atleast-version=1.28 gstreamer-1.0 gstreamer-app-1.0 gstreamer-audio-1.0; \
    for element in appsrc appsink audioconvert audioresample capsfilter opusdec rtpopusdepay rtpjitterbuffer; do \
        gst-inspect-1.0 "$element" >/dev/null; \
    done; \
    rm /tmp/nextest.tar.gz /tmp/sccache.tar.gz

WORKDIR /src
# The verifier script is the command. A bare `docker run` still has cargo on PATH.
CMD ["/bin/bash"]
