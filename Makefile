.PHONY: check fmt lint test cov audit fuzz tsan difftest e2e-steer

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

# Diff the Rust port against steer: 10,000 generated configs and 100,000
# generated requests through legacy/steer/cmd/diffdrive and the Rust crates.
# Needs Go. The seed comes from the clock and is printed; replay a run with
# DIFFTEST_ARGS="--seed N".
DIFFTEST_ARGS ?=
difftest:
	cd legacy/steer && go build -o $(CURDIR)/target/diffdrive ./cmd/diffdrive
	cargo run --release --locked -p edgefront-difftest -- steer $(DIFFTEST_ARGS) -- target/diffdrive

# Run the end-to-end HTTP/1.1 cases against a live steer as well as the
# edgefront binary, and check steer still matches the recorded transcript.
# Needs Go. E2E_BLESS=1 rewrites the recording from steer.
e2e-steer:
	cd legacy/steer && go build -o $(CURDIR)/target/steer ./cmd/steer
	STEER_BIN=$(CURDIR)/target/steer cargo test --locked -p edgefront --test e2e_port_http1
