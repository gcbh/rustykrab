.PHONY: build release debug codesign codesign-debug bundle bundle-debug clean version

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

# Bundle + sign into a .app. Required for Data Protection Keychain access:
# a bare signed binary falls back to the legacy keychain and prompts on every
# credential read. Use these for any build you intend to run.
bundle:
	cargo build --release -p rustykrab-cli
	./scripts/bundle.sh --release

bundle-debug:
	cargo build -p rustykrab-cli
	./scripts/bundle.sh --debug

clean:
	cargo clean

version:
	@cargo run -p rustykrab-cli --quiet -- --version
