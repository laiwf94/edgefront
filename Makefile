.PHONY: check fmt lint test cov audit fuzz tsan tsan-crates difftest difftest-balancer difftest-resilience difftest-pacer difftest-pacer-config difftest-l4 difftest-cache e2e-steer

# A crate with parsers keeps its cargo-fuzz targets in crates/<crate>/fuzz:
# a crate outside the workspace, with its own Cargo.lock.
FUZZ_DIRS := crates/edgefront-cache/fuzz crates/edgefront-core/fuzz crates/edgefront-limiter/fuzz
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
DIRECT_DEPS ?= 17
audit:
	cargo deny check
	for d in $(FUZZ_DIRS); do cargo deny --manifest-path $$d/Cargo.toml check || exit 1; done
	cargo audit
	for d in $(FUZZ_DIRS); do cargo audit --file $$d/Cargo.lock || exit 1; done
	scripts/direct-deps.sh $(DIRECT_DEPS)

# Runs every target of every fuzz crate for FUZZ_SECS seconds, with the
# target's dictionary (crates/<crate>/fuzz/<target>.dict) when it has one.
# An input that takes longer than FUZZ_TIMEOUT seconds fails the run, so a
# parser that loops is caught like a crash. Needs a nightly toolchain and
# cargo-fuzz.
FUZZ_TIMEOUT ?= 10
fuzz:
	for d in $(FUZZ_DIRS); do \
		for t in $$(cargo +nightly fuzz list --fuzz-dir $$d); do \
			dict=; if [ -f $$d/$$t.dict ]; then dict=-dict=$$d/$$t.dict; fi; \
			cargo +nightly fuzz run --fuzz-dir $$d $$t -- -max_total_time=$(FUZZ_SECS) -timeout=$(FUZZ_TIMEOUT) $$dict || exit 1; \
		done; \
	done

# ThreadSanitizer, one crate at a time: each crate's unit tests plus the
# integration tests listed for it below, the ones that run servers, clients
# and background tasks concurrently. `make tsan` runs every crate in
# TSAN_CRATES and `make tsan-<crate>` runs one. Needs nightly with rust-src;
# TSAN_TARGET is the host triple.
TSAN_TARGET ?= $(shell rustc -vV | sed -n 's/^host: //p')
TSAN_CARGO ?= cargo +nightly
TSAN_CRATES := cachet edgefront-cache edgefront-core edgefront-limiter edgefront-proxy edgefront-difftest edgefront-bench
TSAN_TESTS_cachet := proxy revalidation failure cluster
TSAN_TESTS_edgefront-core := listener admin client l4 tls ocsp
TSAN_TESTS_edgefront-cache := layer proxy_layer
TSAN_TESTS_edgefront-proxy := server health fakeupstream outlier filters retry resilience operate lifecycle

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

# Diff edgefront-proxy's outlier detection and retry decisions against
# steer's: 5,000 seeded outlier traces of 2,000 operations and 5,000 retry
# traces of 1,000 requests (every hundredth of 100,000), through the same
# driver. Every ejection, multiplier, retry decision, backoff and budget
# balance must match, and every budget decision must agree with an
# independent ledger. Needs Go. The seed comes from the clock and is
# printed; a divergence prints the command that replays it.
difftest-resilience:
	cd legacy/steer && go build -o $(CURDIR)/target/diffdrive ./cmd/diffdrive
	cargo run --release --locked -p edgefront-difftest -- resilience $(DIFFTEST_ARGS) -- target/diffdrive

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

# Run pacer's integration scenarios (legacy/pacer/tests/it_*.c), written as
# the traffic scripts in tests/pacer_scripts, against legacy pacerd and
# against edgefront limits, and diff their outcomes and counters step by
# step. pacerd needs Linux; `make test` runs the scripts against edgefront
# alone.
PACER_RELEASE := $(CURDIR)/legacy/pacer/build/release
difftest-l4:
	$(MAKE) -C legacy/pacer VARIANT=release build/release/pacerd build/release/pacerctl
	PACERD_BIN=$(PACER_RELEASE)/pacerd PACERCTL_BIN=$(PACER_RELEASE)/pacerctl \
		cargo test --locked -p edgefront --test pacer_scripts -- --nocapture

# Diff the cache node against cachet 0.1: replay 50,000 generated requests
# (or DIFFTEST_ARGS="--log FILE", a recorded log) against cachet as it was
# before the workspace split and against the edgefront binary, on one
# logical clock, and diff status, headers but Date, body SHA-256 and origin
# fetches. cachet 0.1 is the last commit before the split (the repository
# has no release tags). It is fetched when a shallow clone lacks it,
# exported once into a directory named after it and built there. Needs
# libfaketime (apt install libfaketime). The seed comes from the clock and
# is printed; replay a run with DIFFTEST_ARGS="--seed N".
CACHET_0_1_REV := ed77c3212eb61fdfb442fae654b382df53a798f1
CACHET_0_1_DIR := target/cachet-0.1-$(CACHET_0_1_REV)
difftest-cache:
	git cat-file -e $(CACHET_0_1_REV)^{commit} 2>/dev/null || git fetch --quiet --depth=1 origin $(CACHET_0_1_REV)
	test -f $(CACHET_0_1_DIR)/Cargo.lock || { mkdir -p $(CACHET_0_1_DIR) && git archive $(CACHET_0_1_REV) | tar -x -C $(CACHET_0_1_DIR); }
	cd $(CACHET_0_1_DIR) && cargo build --release --locked --bin cachet
	cargo build --release --locked -p edgefront -p edgefront-difftest
	target/release/edgefront-difftest cache $(DIFFTEST_ARGS) -- $(CACHET_0_1_DIR)/target/release/cachet target/release/edgefront
