VERSION := $(shell grep '^version =' Cargo.toml | head -n1 | sed 's/version = "\(.*\)"/\1/')
TAG := v$(VERSION)

.PHONY: help build test test-release check bench-build stress clean version release

help:
	@printf '%s\n' \
		'Available targets:' \
		'  make build         Build the library' \
		'  make test          Run the test suite' \
		'  make test-release  Run tests in release mode' \
		'  make check         Run the main verification set' \
		'  make bench-build   Build benches without running them' \
		'  make stress        Run the stress example in release mode' \
		'  make version       Print the release version from Cargo.toml' \
		'  make release       Validate, commit, and tag the current release' \
		'  make clean         Remove Cargo build artifacts'

build:
	cargo build

test:
	cargo test

test-release:
	cargo test --release

bench-build:
	cargo build --release --benches

stress:
	cargo run --release --example stress

check: test test-release
	cargo build --release --example stress --benches

version:
	@printf '%s\n' '$(TAG)'

release: check
	git add CHANGELOG.md Makefile README.md
	@git diff --cached --quiet && { echo "No release changes staged"; exit 1; } || true
	git commit -m "release: $(TAG)"
	git tag -a $(TAG) -m "Release $(TAG)"

clean:
	cargo clean
