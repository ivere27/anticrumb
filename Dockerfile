# syntax=docker/dockerfile:1
ARG RUST_IMAGE=rust:1.97-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97
FROM ${RUST_IMAGE} AS build-base
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /src

# Native Linux builds. Use --platform linux/arm64 and RUST_TARGET=aarch64-unknown-linux-gnu for ARM.
FROM build-base AS linux-build
ARG RUST_TARGET=x86_64-unknown-linux-gnu
RUN rustup target add ${RUST_TARGET}
COPY . .
ARG ANTICRUMB_GIT_HEAD=unknown
ENV ANTICRUMB_GIT_HEAD=${ANTICRUMB_GIT_HEAD}
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/usr/local/cargo/git --mount=type=cache,target=/src/target cargo run --locked -q -p xtask -- package --target ${RUST_TARGET}
RUN mkdir /out && cp dist/anticrumb-${RUST_TARGET}/anticrumb dist/anticrumb-${RUST_TARGET}/LICENSE /out/
FROM scratch AS artifacts
COPY --from=linux-build /src/dist/ /

FROM build-base AS windows-build
ARG RUST_TARGET=x86_64-pc-windows-gnu
RUN test "${RUST_TARGET}" = x86_64-pc-windows-gnu && apt-get update && apt-get install -y --no-install-recommends gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64 && rm -rf /var/lib/apt/lists/*
RUN rustup target add ${RUST_TARGET}
COPY . .
ARG ANTICRUMB_GIT_HEAD=unknown
ENV ANTICRUMB_GIT_HEAD=${ANTICRUMB_GIT_HEAD}
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/usr/local/cargo/git --mount=type=cache,target=/src/target cargo run --locked -q -p xtask -- package --target ${RUST_TARGET}
FROM scratch AS windows-artifacts
COPY --from=windows-build /src/dist/ /

# Optional Linux -> macOS build. The Apple SDK must be supplied by the operator.
# No SDK is downloaded or redistributed by this Dockerfile.
FROM build-base AS macos-build
ARG OSXCROSS_COMMIT=00bbf6b090a1f5fd8710adfe5d7da56da8282f96
ARG RUST_TARGET=aarch64-apple-darwin
RUN apt-get update && apt-get install -y --no-install-recommends clang cmake libssl-dev libxml2-dev xz-utils bzip2 patch && rm -rf /var/lib/apt/lists/*
RUN git init /opt/osxcross && git -C /opt/osxcross remote add origin https://github.com/tpoechtrager/osxcross.git && git -C /opt/osxcross fetch --depth 1 origin ${OSXCROSS_COMMIT} && git -C /opt/osxcross checkout --detach FETCH_HEAD
COPY --from=apple-sdk / /opt/osxcross/tarballs/
RUN cd /opt/osxcross && UNATTENDED=1 BUILD_FLAVOR=stable ENABLE_ARCHS="arm64 x86_64" OSX_VERSION_MIN=11.0 ./build.sh
ENV PATH="/opt/osxcross/target/bin:${PATH}" MACOSX_DEPLOYMENT_TARGET=11.0
RUN rustup target add ${RUST_TARGET}
COPY . .
ARG ANTICRUMB_GIT_HEAD=unknown
ENV ANTICRUMB_GIT_HEAD=${ANTICRUMB_GIT_HEAD}
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/usr/local/cargo/git --mount=type=cache,target=/src/target cargo run --locked -q -p xtask -- macos-cross ${RUST_TARGET}
FROM scratch AS macos-artifacts
COPY --from=macos-build /src/dist/ /

# TUI/provider container connecting to a daemon on another host. Chrome is not bundled.
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates bash ripgrep && rm -rf /var/lib/apt/lists/* && useradd --create-home --uid 10001 agent
COPY --from=linux-build /out/anticrumb /usr/local/bin/
COPY --from=linux-build /out/LICENSE /usr/share/licenses/anticrumb/LICENSE
USER agent
WORKDIR /home/agent
ENTRYPOINT ["anticrumb"]
