.PHONY: build release debug codesign codesign-debug clean

# Default: release build + codesign
build: release

release:
	cargo build --release -p rustykrab-cli
	@if [ "$$(uname)" = "Darwin" ]; then \
		./scripts/codesign.sh --release; \
	fi

debug:
	cargo build -p rustykrab-cli
	@if [ "$$(uname)" = "Darwin" ]; then \
		./scripts/codesign.sh; \
	fi

codesign:
	./scripts/codesign.sh --release

codesign-debug:
	./scripts/codesign.sh

clean:
	cargo clean
