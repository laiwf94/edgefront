.PHONY: check fmt lint test cov audit fuzz tsan difftest difftest-pacer e2e-steer

# A crate with parsers keeps its cargo-fuzz targets in crates/<crate>/fuzz:
# a crate outside the workspace, with its own Cargo.lock.
FUZZ_DIRS := crates/edgefront-cache/fuzz crates/edgefront-core/fuzz
FUZZ_SECS ?= 30

# The full gate: format, lint with warnings as errors, tests with line coverage.
check: fmt lint cov

fmt:
	cargo fmt --all --check
	for d in $(FUZZ_DIRS); do cargo fmt --check --manifest-path $$d/Cargo.toml || exit 1; done

lint:
	cargo clippy --workspace --all-targets -- -D warnings
	for d in $(FUZZ_DIRS); do cargo clippy --manifest-path $$d/Cargo.toml --all-targets -- -D warnings || exit 1; done

test:
	cargo test --workspace

cov:
	cargo llvm-cov --workspace --summary-only --fail-under-lines 90

audit:
	cargo deny check
	cargo audit
	for d in $(FUZZ_DIRS); do cargo audit --file $$d/Cargo.lock || exit 1; done

# Runs every target of every fuzz crate for FUZZ_SECS seconds. Needs a
# nightly toolchain and cargo-fuzz.
fuzz:
	for d in $(FUZZ_DIRS); do \
		for t in $$(cargo +nightly fuzz list --fuzz-dir $$d); do \
			cargo +nightly fuzz run --fuzz-dir $$d $$t -- -max_total_time=$(FUZZ_SECS) || exit 1; \
		done; \
	done

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

# Diff edgefront-limiter against libpacer: 100 generated scripts of 10,000
# operations each through legacy/pacer/tests/diffdrive.c (built by
# edgefront-difftest's build script) and the Rust crate. Needs a C compiler.
# The seed comes from the clock and is printed; a divergence prints the
# command that replays it.
difftest-pacer:
	cargo run --release --locked -p edgefront-difftest -- pacer $(DIFFTEST_ARGS)
