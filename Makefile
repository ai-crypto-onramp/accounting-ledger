.PHONY: build test cover-check run docker-build docker-run clean

build:
	cargo build --release

test:
	cargo llvm-cov --codecov --output-path codecov.json

cover-check:
	@cargo llvm-cov --summary-only 2>/dev/null | awk '/^TOTAL/ {gsub(/%/,"",$$10); if($$10+0 < 80) {print "Coverage " $$10 "% below 80% threshold"; exit 1} else {print "Coverage " $$10 "% OK"}}'

run:
	cargo run --release

docker-build:
	docker build -t ai-crypto-onramp/accounting-ledger .

docker-run:
	docker run --rm -p 8080:8080 ai-crypto-onramp/accounting-ledger

clean:
	cargo clean
	rm -f codecov.json
