.PHONY: build test run docker-build docker-run clean

build:
	cargo build --release

test:
	cargo llvm-cov --codecov --output-path codecov.json

run:
	cargo run --release

docker-build:
	docker build -t ai-crypto-onramp/accounting-ledger .

docker-run:
	docker run --rm -p 8080:8080 ai-crypto-onramp/accounting-ledger

clean:
	cargo clean
	rm -f codecov.json
