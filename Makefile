.PHONY: check fmt lint test cov audit fuzz tsan

FUZZ_TARGETS := cache_control entity_tags range http_date disk_record purge_messages
FUZZ_SECS ?= 30

# The full gate: format, lint with warnings as errors, tests with line coverage.
check: fmt lint cov

fmt:
	cargo fmt --all --check
	cargo fmt --check --manifest-path fuzz/Cargo.toml

lint:
	cargo clippy --workspace --all-targets -- -D warnings
	cargo clippy --manifest-path fuzz/Cargo.toml --all-targets -- -D warnings

test:
	cargo test --workspace

cov:
	cargo llvm-cov --workspace --summary-only --fail-under-lines 90

audit:
	cargo deny check
	cargo audit
	cargo audit --file fuzz/Cargo.lock

# Needs a nightly toolchain and cargo-fuzz.
fuzz:
	for t in $(FUZZ_TARGETS); do cargo +nightly fuzz run $$t -- -max_total_time=$(FUZZ_SECS) || exit 1; done

# ThreadSanitizer over the concurrency-heavy tests. Needs nightly with
# rust-src; set TSAN_TARGET to the host triple.
TSAN_TARGET ?= $(shell rustc -vV | sed -n 's/^host: //p')
tsan:
	RUSTFLAGS=-Zsanitizer=thread TSAN_OPTIONS=halt_on_error=1:suppressions=$(CURDIR)/tsan-suppressions.txt cargo +nightly test -Zbuild-std \
		--target $(TSAN_TARGET) --target-dir target/tsan \
		--lib --test proxy --test revalidation --test failure --test cluster
