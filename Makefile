.PHONY: build release test fmt lint check run mock loadtest docker clean

build:
	cargo build -j 2

release:
	cargo build -j 2 --release

test:
	cargo test -j 2

fmt:
	cargo fmt --all

lint:
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings

check: lint test

run:
	cargo run --release --bin gateway -- serve

mock:
	cargo run --release --bin mock-downstream

# Needs a running gateway. Set LOAD_API_KEY and LOAD_SECRET first.
loadtest:
	cargo run --release --bin loadtest -- --requests 5000 --concurrency 32

docker:
	docker build -t webhook-connector-gateway .

clean:
	cargo clean
