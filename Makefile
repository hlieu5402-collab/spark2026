.PHONY: ci-lints ci-zc-asm ci-no-std-alloc ci-doc-warning ci-bench-smoke

ci-lints:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo run --quiet --package spark-deprecation-lint

ci-zc-asm:
	cargo build --workspace

ci-no-std-alloc:
	cargo build --workspace --no-default-features --features alloc

ci-doc-warning:
	cargo doc --workspace

ci-bench-smoke:
	cargo bench --workspace -- --quick
