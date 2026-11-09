.PHONY: check fmt lint test cov audit fuzz tsan tsan-crates difftest difftest-balancer difftest-pacer difftest-pacer-config e2e-steer

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

# Line coverage, held to the floor for every crate on its own sources and
# for the workspace as a whole.
COVERAGE_FLOOR ?= 90
cov:
	scripts/coverage.sh $(COVERAGE_FLOOR)

# The supply-chain gate: cargo deny's policy on the workspace and on each fuzz
# crate's own lockfile, RustSec on every lockfile, and the direct dependency
# budget (exact pins, at most DIRECT_DEPS runtime crates).
DIRECT_DEPS ?= 14
audit:
	cargo deny check
	for d in $(FUZZ_DIRS); do cargo deny --manifest-path $$d/Cargo.toml check || exit 1; done
	cargo audit
	for d in $(FUZZ_DIRS); do cargo audit --file $$d/Cargo.lock || exit 1; done
	scripts/direct-deps.sh $(DIRECT_DEPS)

# Runs every target of every fuzz crate for FUZZ_SECS seconds. Needs a
# nightly toolchain and cargo-fuzz.
fuzz:
	for d in $(FUZZ_DIRS); do \
		for t in $$(cargo +nightly fuzz list --fuzz-dir $$d); do \
			cargo +nightly fuzz run --fuzz-dir $$d $$t -- -max_total_time=$(FUZZ_SECS) || exit 1; \
		done; \
	done

# ThreadSanitizer, one crate at a time: each crate's unit tests plus the
# integration tests listed for it below, the ones that run servers, clients
# and background tasks concurrently. `make tsan` runs every crate in
# TSAN_CRATES and `make tsan-<crate>` runs one. Needs nightly with rust-src;
# TSAN_TARGET is the host triple.
TSAN_TARGET ?= $(shell rustc -vV | sed -n 's/^host: //p')
TSAN_CARGO ?= cargo +nightly
TSAN_CRATES := cachet edgefront-cache edgefront-core edgefront-limiter edgefront-proxy edgefront-difftest
TSAN_TESTS_cachet := proxy revalidation failure cluster
TSAN_TESTS_edgefront-core := listener admin client
TSAN_TESTS_edgefront-proxy := server

define tsan_crate
.PHONY: tsan-$(1)
tsan-$(1):
	RUSTFLAGS=-Zsanitizer=thread TSAN_OPTIONS=halt_on_error=1:suppressions=$(CURDIR)/tsan-suppressions.txt \
		$(TSAN_CARGO) test -Zbuild-std --target $(TSAN_TARGET) --target-dir target/tsan \
		-p $(1) --lib $(addprefix --test ,$(TSAN_TESTS_$(1)))
endef
$(foreach c,$(TSAN_CRATES),$(eval $(call tsan_crate,$(c))))

tsan: $(addprefix tsan-,$(TSAN_CRATES))

# The list CI builds its per-crate ThreadSanitizer matrix from.
tsan-crates:
	@echo $(TSAN_CRATES)

# Diff the Rust port against steer: 10,000 generated configs and 100,000
# generated requests through legacy/steer/cmd/diffdrive and the Rust crates.
# Needs Go. The seed comes from the clock and is printed; replay a run with
# DIFFTEST_ARGS="--seed N".
DIFFTEST_ARGS ?=
difftest:
	cd legacy/steer && go build -o $(CURDIR)/target/diffdrive ./cmd/diffdrive
	cargo run --release --locked -p edgefront-difftest -- steer $(DIFFTEST_ARGS) -- target/diffdrive

# Diff edgefront-proxy's balancer against steer's, pick for pick: 1,000
# seeded scenarios of 10,000 requests each, with health changes, ejections,
# clock moves and latency samples between them, through the same driver.
# Needs Go. The seed comes from the clock and is printed; a divergence
# prints the command that replays it.
difftest-balancer:
	cd legacy/steer && go build -o $(CURDIR)/target/diffdrive ./cmd/diffdrive
	cargo run --release --locked -p edgefront-difftest -- balancer $(DIFFTEST_ARGS) -- target/diffdrive

# Run the end-to-end HTTP/1.1 cases against a live steer as well as the
# edgefront binary, and the end-to-end balancing run (20,000 requests per
# policy over three priority levels) against steer's balancer through
# diffdrive, and check steer still matches both recordings. Needs Go.
# E2E_BLESS=1 rewrites the recordings from steer.
e2e-steer:
	cd legacy/steer && go build -o $(CURDIR)/target/steer ./cmd/steer
	cd legacy/steer && go build -o $(CURDIR)/target/diffdrive ./cmd/diffdrive
	STEER_BIN=$(CURDIR)/target/steer cargo test --locked -p edgefront --test e2e_port_http1
	STEER_DIFFDRIVE=$(CURDIR)/target/diffdrive cargo test --locked -p edgefront --test e2e_port_balancer

# Diff edgefront-limiter against libpacer: 100 generated scripts of 10,000
# operations each through legacy/pacer/tests/diffdrive.c (built by
# edgefront-difftest's build script) and the Rust crate. Needs a C compiler.
# The seed comes from the clock and is printed; a divergence prints the
# command that replays it.
difftest-pacer:
	cargo run --release --locked -p edgefront-difftest -- pacer $(DIFFTEST_ARGS)

# Diff pacerd's config reader against edgefront-limiter's: every file under
# legacy/pacer's tests, examples and fuzz/corpus, every string literal in its
# tests, and 2,000 seeded mutants (more with DIFFTEST_ARGS="--mutants N"),
# through `diffdrive config` and the Rust parser.
difftest-pacer-config:
	cargo run --release --locked -p edgefront-difftest -- pacer-config $(DIFFTEST_ARGS)
