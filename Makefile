SHELL := /bin/bash
.DEFAULT_GOAL := help
.NOTPARALLEL:

MISE_RUST := $(shell sed -n 's/^rust *= *"\(.*\)"/\1/p' mise.toml)
TOOLCHAIN_RUST := $(shell sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)
CARGO_RUST := $(shell sed -n 's/^rust-version *= *"\(.*\)"/\1/p' Cargo.toml)

.PHONY: help build test run toolchain lint format precommit clean

help: ## Show this help
	@grep -E '^[a-z]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  make %-10s %s\n", $$1, $$2}'

build: ## Build al into target/debug/
	cargo build

test: ## Run the tests
	cargo test

run: ## Run al with ARGS, e.g. `make run ARGS="status --json"`
	cargo run --quiet -- $(ARGS)

toolchain: ## Check that mise.toml, rust-toolchain.toml and Cargo.toml use the same Rust
	@for pin in "mise.toml rust:$(MISE_RUST)" "rust-toolchain.toml channel:$(TOOLCHAIN_RUST)" \
	  "Cargo.toml rust-version:$(CARGO_RUST)"; do \
	  if [[ "$$pin" == *: ]]; then echo "Could not read the Rust version from $${pin%:}" >&2; exit 1; fi; \
	done
	@if [[ "$(MISE_RUST)" != "$(TOOLCHAIN_RUST)" || "$(MISE_RUST)" != "$(CARGO_RUST)" ]]; then \
	  echo "Rust versions differ: mise.toml rust '$(MISE_RUST)'," \
	    "rust-toolchain.toml channel '$(TOOLCHAIN_RUST)', Cargo.toml rust-version '$(CARGO_RUST)'" >&2; \
	  exit 1; \
	fi

lint: toolchain ## Check the Rust versions, formatting with rustfmt, and lints with clippy
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

format: ## Reformat Rust sources in place with rustfmt
	cargo fmt

precommit: lint test ## Quality gate: lint, then test

clean: ## Remove target/
	cargo clean
