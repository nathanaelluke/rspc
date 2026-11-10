PREFIX ?= $(HOME)/.local
CARGO ?= cargo
CARGO_BUILD_JOBS ?= 1

.PHONY: build check install
build:
	$(CARGO) build --release --locked -j$(CARGO_BUILD_JOBS) -p rspc -p rspc-ui

check: build
	$(CARGO) fmt --all -- --check
	$(CARGO) test --locked -j$(CARGO_BUILD_JOBS) --workspace -- --test-threads=1
	$(CARGO) clippy --locked -j$(CARGO_BUILD_JOBS) --workspace --all-targets -- -D warnings

install:
	./scripts/install.sh --prefix "$(PREFIX)"
