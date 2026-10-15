# Builds run in containers; only Docker (with buildx) is needed on the host.
VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null | sed 's/^v//' || echo dev)
IMAGE ?= ghcr.io/joeyyax/collector
PLATFORMS ?= linux/amd64,linux/arm64
ZIGBUILD ?= ghcr.io/rust-cross/cargo-zigbuild:0.23.4
BUILDER ?= collector-builder

RUN = docker run --rm -v "$(CURDIR)":/src -w /src -v collector-cargo:/usr/local/cargo/registry \
	-e CARGO_TARGET_DIR=/src/target/docker -e COLLECTOR_VERSION=$(VERSION) $(ZIGBUILD)

.PHONY: all test lint binaries image image-push checksums clean version

all: test binaries

version:
	@echo $(VERSION)

test:
	$(RUN) cargo test --locked

lint:
	$(RUN) sh -c 'rustup component add clippy rustfmt >/dev/null 2>&1; cargo clippy --locked --all-targets -- -D warnings'

# Static musl binaries in dist/.
binaries:
	mkdir -p dist
	$(RUN) cargo zigbuild --release --locked --target x86_64-unknown-linux-musl
	$(RUN) sh -c 'JEMALLOC_SYS_WITH_LG_PAGE=16 cargo zigbuild --release --locked --target aarch64-unknown-linux-musl'
	cp target/docker/x86_64-unknown-linux-musl/release/collector dist/collector-$(VERSION)-linux-amd64
	cp target/docker/aarch64-unknown-linux-musl/release/collector dist/collector-$(VERSION)-linux-arm64
	$(MAKE) checksums

checksums:
	cd dist && shasum -a 256 collector-$(VERSION)-linux-* > collector-$(VERSION)-SHA256SUMS

$(BUILDER):
	docker buildx inspect $(BUILDER) >/dev/null 2>&1 || docker buildx create --name $(BUILDER) --driver docker-container >/dev/null

# Multi-arch image as an OCI archive in dist/; nothing is pushed.
image: $(BUILDER)
	mkdir -p dist
	docker buildx build --builder $(BUILDER) --platform $(PLATFORMS) --build-arg VERSION=$(VERSION) \
		-t $(IMAGE):$(VERSION) --output type=oci,dest=dist/collector-$(VERSION).oci.tar .

image-push: $(BUILDER)
	docker buildx build --builder $(BUILDER) --platform $(PLATFORMS) --build-arg VERSION=$(VERSION) \
		-t $(IMAGE):$(VERSION) -t $(IMAGE):latest --push .

clean:
	rm -rf dist target
