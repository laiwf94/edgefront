.PHONY: check fmt lint test cov audit fuzz

FUZZ_TARGETS := cache_control entity_tags range http_date disk_record purge_messages
FUZZ_SECS ?= 30

# The full gate: format, lint with warnings as errors, tests with line coverage.
check: fmt lint cov

fmt:
	cargo fmt --check
	cargo fmt --check --manifest-path fuzz/Cargo.toml

lint:
	cargo clippy --all-targets -- -D warnings
	cargo clippy --manifest-path fuzz/Cargo.toml --all-targets -- -D warnings

test:
	cargo test

cov:
	cargo llvm-cov --summary-only --fail-under-lines 90

audit:
	cargo deny check
	cargo audit

# Needs a nightly toolchain and cargo-fuzz.
fuzz:
	for t in $(FUZZ_TARGETS); do cargo +nightly fuzz run $$t -- -max_total_time=$(FUZZ_SECS) || exit 1; done
