# Building and Installation

## Runtime Requirements

The browser host needs Chrome/Chromium and a signed-in provider account. The workspace machine needs [ripgrep](https://github.com/BurntSushi/ripgrep#installation) and Bash (Unix) or PowerShell (Windows) when shell tools are enabled.

## Install a Release

See [Install](../README.md#install) for the Linux/macOS and Windows installation scripts and update instructions. Continue with [Quick Start](usage.md#quick-start).

## Build from Source

Install Rust 1.97, `protoc`, and a C/C++ compiler: GCC/Clang on Linux, Xcode Command Line Tools on macOS, or Visual Studio C++ Build Tools on Windows.

```sh
git clone https://github.com/ivere27/anticrumb.git
cd anticrumb
make build
```

Without Make, run `cargo run --locked -q -p xtask -- build --profile release`.

The executable is `target/<target-triple>/release/anticrumb` (`anticrumb.exe` on Windows); add its directory to `PATH`. Direct `cargo build --locked --release --bin anticrumb` instead uses `target/release/` unless a target override is configured. `CARGO_TARGET_DIR` changes the base directory.

## Development and Packaging

```sh
make build PROFILE=dev         # Debug executable
make package                  # Native archive in dist/
make proto                    # Regenerate RPC bindings
```

Packages contain one executable with SQLite and all provider resources bundled. Local source and Docker builds default to the `dev` build channel (`X.Y.Z-dev`), while official GitHub Actions builds produce `stable` (`X.Y.Z`) and `nightly` (`X.Y.Z-nightly`) releases. `make clean` removes Cargo output, not user state or packages.

See [Testing](testing.md), [daemon debugging](usage.md#runtime-health-and-debugging), and [Releasing](releasing.md). Command definitions: [Makefile](../Makefile) and [xtask](../xtask/src/build.rs).

## Cross Compilation and Docker Builds

Cross compilation needs `rustup target add TARGET` plus the target's C compiler/linker and SDK. `make build-windows` defaults to MinGW x64; use MSVC for native Windows. `make build-macos` defaults to Apple Silicon; set `MACOS_TARGET=x86_64-apple-darwin` for Intel.

Docker exports packages under `dist/docker-<platform>/`:

```sh
make docker-linux
make docker-windows
make docker-macos APPLE_SDK_DIR=/path/to/sdk-archive-directory
```

Windows uses MinGW. For Linux ARM64, set `LINUX_TARGET=aarch64-unknown-linux-gnu`. macOS requires BuildKit named contexts and an Apple SDK archive (11+) packaged using [OSXCross's instructions](https://github.com/tpoechtrager/osxcross/blob/00bbf6b090a1f5fd8710adfe5d7da56da8282f96/README.SDK.md); the project does not supply the SDK. Packages are unsigned. Build stages and pinned toolchains are in [Dockerfile](../Dockerfile).

## Runtime Container

```sh
make docker-runtime
docker run --rm -it \
  -v /path/to/project:/workspace \
  -v anticrumb-state:/home/agent/.anticrumb \
  anticrumb --connect https://192.0.2.10:50051 \
    --token TOKEN_PRINTED_BY_DAEMON -C /workspace
```

Use your host's address and token from [remote TLS setup](rpc.md#network-grpc--tls-setup). The state volume retains trust and connection settings. The image includes Bash/rg, runs workspace tools as UID 10001, and connects to Chrome elsewhere; Chrome is not bundled. Make workspace mounts writable by that user, or use `--user` with a writable state directory.
