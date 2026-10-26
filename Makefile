.PHONY: check fmt lint test cov audit

# The full gate: format, lint with warnings as errors, tests with line coverage.
check: fmt lint cov

fmt:
	cargo fmt --check

lint:
	cargo clippy --all-targets -- -D warnings

test:
	cargo test

cov:
	cargo llvm-cov --summary-only --fail-under-lines 90

audit:
	cargo deny check
	cargo audit
