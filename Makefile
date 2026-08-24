.PHONY: build release debug codesign codesign-debug clean version e2e eval eval-quick eval-cred eval-list e2e eval eval-quick eval-list

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

version:
	@cargo run -p rustykrab-cli --quiet -- --version

# --- Evaluation harness ---
# scripts/e2e.sh boots a throwaway daemon and asserts over HTTP.
#
#   e2e         deterministic plumbing scenarios (scripted provider, no model)
#   eval        gemma4:26b behaviour scenarios (tools, compaction, memory)
#   eval-quick  the same, one repetition, skipping the slow scenarios
#
# Set ANTHROPIC_API_KEY to grade free-form answers with claude-sonnet-5;
# without it the model under test grades itself, and the report says so.

e2e:
	./scripts/e2e.sh $(ARGS)

eval:
	./scripts/e2e.sh --mode model --reps 3 $(ARGS)

eval-quick:
	./scripts/e2e.sh --mode model --reps 1 --quick $(ARGS)

eval-list:
	@./scripts/e2e.sh --list

# --- Evaluation harness ---
# Every eval runs through scripts/e2e.sh: one binary, one report, one exit
# code. See crates/rustykrab-e2e/README.md.
#
#   e2e         deterministic plumbing scenarios (scripted provider, no model)
#   eval        gemma4:26b behaviour scenarios (tools, compaction, memory)
#   eval-cred   the credential-ask measurement, on every surface
#
# Set ANTHROPIC_API_KEY to grade free-form answers with claude-sonnet-5;
# without it the model under test grades itself, and the report says so.

e2e:
	./scripts/e2e.sh $(ARGS)

eval:
	./scripts/e2e.sh --mode model --reps 3 $(ARGS)

eval-quick:
	./scripts/e2e.sh --mode model --reps 1 --quick $(ARGS)

eval-cred:
	./scripts/e2e.sh --mode credential --trials 5 $(ARGS)

# Reaches the real internet with real credentials, so it is never part of
# `all` and skips unless RK_LOGIN_* is set. See crates/rustykrab-e2e/README.md.
eval-login:
	./scripts/e2e.sh --mode login --trials 3 $(ARGS)

eval-list:
	@./scripts/e2e.sh --list
