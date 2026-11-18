.DEFAULT_GOAL := help
PROFILE ?= release
TARGET ?=
LINUX_TARGET ?= x86_64-unknown-linux-gnu
LINUX_PLATFORM ?= $(if $(findstring aarch64,$(LINUX_TARGET)),linux/arm64,linux/amd64)
WINDOWS_TARGET ?= x86_64-pc-windows-gnu
MACOS_TARGET ?= aarch64-apple-darwin
APPLE_SDK_DIR ?=
BUILD_ARGS = --profile $(PROFILE) $(if $(TARGET),--target $(TARGET),)
BUILD_JOBS ?= $(shell (nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 1) | awk '{ n = $$1 - 2; print (n > 1 ? n : 1) }')
DRY_RUN ?= 0
VERSION ?= $(shell awk -F ' = ' '$$1=="version" { gsub(/"/, "", $$2); print $$2; exit }' Cargo.toml)
DIST_DIR ?= dist/v$(VERSION)
GIT_HEAD ?= $(shell head=$$(git rev-parse --short=7 HEAD 2>/dev/null); if [ -n "$$head" ]; then if ! git diff --no-ext-diff --quiet 2>/dev/null || ! git diff --no-ext-diff --cached --quiet 2>/dev/null; then printf '%s-dirty' "$$head"; else printf '%s' "$$head"; fi; else echo unknown; fi)

FAST_RELEASE ?= 1

ifeq ($(FAST_RELEASE),1)
export CARGO_PROFILE_RELEASE_LTO := off
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS := 16
export CARGO_PROFILE_RELEASE_INCREMENTAL := true
export CARGO_PROFILE_RELEASE_OPT_LEVEL := 2
export CARGO_PROFILE_RELEASE_STRIP := none
export CARGO_PROFILE_RELEASE_DEBUG := 0
export CARGO_INCREMENTAL := 1
endif

.PHONY: help proto build package publish check test test-transports test-tui build-linux build-windows build-macos docker-linux docker-windows docker-macos docker-runtime clean clean-dist
help:
	@printf '%s\n' \
		'make build | package | check | test | test-transports | test-tui | proto' \
		'make build-linux | build-windows | build-macos' \
		'make docker-linux | docker-windows | docker-macos APPLE_SDK_DIR=/path/to/sdk-archive-directory' \
		'make docker-runtime' \
		'make publish [DRY_RUN=1]' \
		'make clean | clean-dist' \
		'Options: PROFILE=dev, TARGET=target-triple'
proto:
	cargo run --locked -q -p xtask -- proto
build:
	cargo run --locked -q -j $(BUILD_JOBS) -p xtask -- build $(BUILD_ARGS) --jobs $(BUILD_JOBS)
package:
	cargo run --locked -q -p xtask -- package $(BUILD_ARGS)
publish:
	cargo run --locked -q -p xtask -- publish --dist $(DIST_DIR) --docker-only $(if $(filter 1 true yes,$(DRY_RUN)),--dry-run,)
check:
	cargo run --locked -q -p xtask -- check
test:
	cargo run --locked -q -p xtask -- test
test-transports:
	cargo run --locked -q -p xtask -- transports $(BUILD_ARGS)
test-tui:
	cargo run --locked -q -p xtask -- tui $(BUILD_ARGS)
build-linux:
	cargo run --locked -q -p xtask -- package --profile $(PROFILE) --target $(LINUX_TARGET)
build-windows:
	cargo run --locked -q -p xtask -- package --profile $(PROFILE) --target $(WINDOWS_TARGET)
build-macos:
	cargo run --locked -q -p xtask -- package --profile $(PROFILE) --target $(MACOS_TARGET)
docker-linux:
	docker buildx build --platform $(LINUX_PLATFORM) --target artifacts --build-arg RUST_TARGET=$(LINUX_TARGET) --build-arg ANTICRUMB_GIT_HEAD=$(GIT_HEAD) --output type=local,dest=$(DIST_DIR)/docker-linux .
docker-windows:
	docker buildx build --platform linux/amd64 --target windows-artifacts --build-arg RUST_TARGET=$(WINDOWS_TARGET) --build-arg ANTICRUMB_GIT_HEAD=$(GIT_HEAD) --output type=local,dest=$(DIST_DIR)/docker-windows .
docker-macos:
	@test -n "$(APPLE_SDK_DIR)" && test -d "$(APPLE_SDK_DIR)" || { echo "Set APPLE_SDK_DIR to a directory containing your packaged Apple SDK" >&2; exit 1; }
	docker buildx build --platform linux/amd64 --target macos-artifacts --build-context apple-sdk="$(APPLE_SDK_DIR)" --build-arg RUST_TARGET=$(MACOS_TARGET) --build-arg ANTICRUMB_GIT_HEAD=$(GIT_HEAD) --output type=local,dest=$(DIST_DIR)/docker-macos .
docker-runtime:
	docker build --platform $(LINUX_PLATFORM) --target runtime --build-arg RUST_TARGET=$(LINUX_TARGET) --build-arg ANTICRUMB_GIT_HEAD=$(GIT_HEAD) -t anticrumb .
clean:
	cargo clean
clean-dist:
	rm -rf dist
