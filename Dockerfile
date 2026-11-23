# syntax=docker/dockerfile:1
FROM --platform=$BUILDPLATFORM rust:1.100.0-slim-bookworm AS build

ARG BUILDARCH
ARG ZIG_VERSION=0.17.1
ARG ZIG_SHA256_AMD64=3581863e3cbcfd0fce0efc682ce7e9e93da96668a977791d356a26d2a7e9b17a
ARG ZIG_SHA256_ARM64=a1546d5646eb184437918d6635ca6b762a0e889eac09cef720ee7a1ed3fe7795
ARG CARGO_ZIGBUILD_VERSION=0.24.1

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl xz-utils \
    && rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    case "$BUILDARCH" in \
      amd64) zig_arch=x86_64; zig_sha="$ZIG_SHA256_AMD64" ;; \
      arm64) zig_arch=aarch64; zig_sha="$ZIG_SHA256_ARM64" ;; \
      *) echo "unsupported build arch: $BUILDARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/zig.tar.xz "https://ziglang.org/download/${ZIG_VERSION}/zig-${zig_arch}-linux-${ZIG_VERSION}.tar.xz"; \
    echo "${zig_sha}  /tmp/zig.tar.xz" | sha256sum -c -; \
    mkdir /opt/zig; \
    tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1; \
    rm /tmp/zig.tar.xz; \
    ln -s /opt/zig/zig /usr/local/bin/zig

RUN cargo install cargo-zigbuild --version "${CARGO_ZIGBUILD_VERSION}" --locked

WORKDIR /src
COPY rust-toolchain.toml ./
RUN rustup target add \
      x86_64-unknown-linux-musl \
      arm-unknown-linux-musleabihf \
      armv7-unknown-linux-musleabihf \
      aarch64-unknown-linux-musl

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo fetch --locked

ENV CARGO_TARGET_DIR=/target

ARG TARGETPLATFORM
RUN --mount=type=cache,id=target-${TARGETPLATFORM},target=/target \
    set -eu; \
    case "$TARGETPLATFORM" in \
      linux/amd64) rust_target=x86_64-unknown-linux-musl ;; \
      linux/arm/v6) rust_target=arm-unknown-linux-musleabihf ;; \
      linux/arm/v7) rust_target=armv7-unknown-linux-musleabihf ;; \
      linux/arm64|linux/arm64/v8) rust_target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported platform: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac; \
    cargo zigbuild --release --frozen --target "$rust_target"; \
    install -D "/target/${rust_target}/release/registry-proxy" /out/registry-proxy

FROM scratch
COPY --from=build /out/registry-proxy /registry-proxy
USER 65532:65532
EXPOSE 3000
ENTRYPOINT ["/registry-proxy"]
