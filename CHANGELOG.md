# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.4.2] - 2026-04-16

- fix(providers): raise Ollama HTTP timeout to 15 min and skip retry on timeout (#373)

## [2.4.1] - 2026-04-16

- feat: enable thinking mode by default for Ollama models (#372)

## [2.4.0] - 2026-04-16

- feat: add task queue with dedup and bounded concurrency for cron jobs (#370)

## [2.3.6] - 2026-04-16

- fix: grant net_discovery capability to Telegram chat sessions (#371)

## [2.3.5] - 2026-04-16

- fix: prevent Ollama 500s from context window overload (#368)

## [2.3.4] - 2026-04-16

- Fix rustls-webpki vulnerabilities RUSTSEC-2026-0098 and RUSTSEC-2026-0099 (#369)

## [2.3.3] - 2026-04-15

- Fix stop reason fallthrough causing misleading re-prompts and infinite loops (#367)

## [2.3.2] - 2026-04-15

- Expose Telegram chat ID and thread ID to the agent (#364)

## [2.3.1] - 2026-04-15

- fix: handle stream read errors gracefully instead of crashing (#366)

## [2.3.0] - 2026-04-15

- feat: add network device discovery mechanisms (Part 1) (#365)

## [2.2.6] - 2026-04-15

- Fix agent visibility of Telegram chat IDs (#363)

## [2.2.5] - 2026-04-14

- Debug Telegram chat IDs and fix silent bare @mention drops (#362)

## [2.2.4] - 2026-04-14

- debug: add raw JSON logging for Telegram updates to diagnose chat ID issues (#361)

## [2.2.3] - 2026-04-12

- fix: add missing sandbox requirements for net_discovery and nodes tools (#360)

## [2.2.2] - 2026-04-12

- debug: dump raw LLM response when message text is empty (#359)

## [2.2.1] - 2026-04-12

- fix: require explicit EndTurn stop reason before exiting agent loop (#358)

## [2.2.0] - 2026-04-12

- feat: add net_discovery tool for network discovery (#357)

## [2.1.0] - 2026-04-12

- fix: replace hardcoded sandbox allowlist with Tool::sandbox_requirements() (#356)

## [2.0.4] - 2026-04-12

- fix: add net_scan, net_admin, net_audit to runner sandbox policy allowlist (#354)

## [2.0.3] - 2026-04-12

- fix: Data Protection Keychain via provisioning profile (#353)

## [2.0.2] - 2026-04-12

- refactor: simplify system prompt, remove orchestration pipeline, clean up agent loop (#351)

## [2.0.1] - 2026-04-12

- Improve agent persistence: raise iteration limits, add context flush, strengthen prompts (#350)

## [2.0.0] - 2026-04-12

- refactor: replace RLM text markers with REPL-style tool-based context exploration (#256)

## [1.4.12] - 2026-04-12

- [feat] register Gmail credentials in secret registry (#346)

## [1.4.11] - 2026-04-12

- [fix] read tool returns NotFound for missing files (#344)

## [1.4.10] - 2026-04-12

- Add central secret registry and cross-platform OS keychain support (#343)

## [1.4.9] - 2026-04-12

- Fix orchestration pipeline timeout by adding deadline awareness and missing timeouts (#342)

## [1.4.8] - 2026-04-12

- Add tool availability checking to prevent unavailable tools from being used (#341)

## [1.4.7] - 2026-04-11

- Fix credential_read tool to return actual credential values (#340)

## [1.4.6] - 2026-04-11

- feat: increase Telegram heartbeat timeout from 5 to 30 minutes (#329)

## [1.4.5] - 2026-04-11

- fix: upgrade croner v2→v3 and improve cron tool error messages (#262) (#264)

## [1.4.4] - 2026-04-11

- fix: make account/service required in credential tool schemas (#263) (#265)

## [1.4.3] - 2026-04-11

- fix: harden release script against special characters in PR titles (#261)

## [1.4.2] - 2026-04-11

- fix: enforce account and service as required params for keychain source in credential_read schema (#259)

## [1.4.1] - 2026-04-11

- Fix net_scan timeout: skip non-retryable errors and add deadline awareness (#258)

## [1.4.0] - 2026-04-11

- Add central secret registry and cross-platform OS keychain support (#255)

## [1.3.3] - 2026-04-11

- Drop Intel macOS build, fix release workflow (#257)

## [1.3.2] - 2026-04-11

- Fix macOS release builds: pin runners to correct architectures, add manual dispatch (#254)

## [1.3.1] - 2026-04-11

- Show Telegram conversations in web UI, persist chat mapping (#253)

## [1.3.0] - 2026-04-11

- Add network reconnaissance tools and skill (net_scan, net_admin, net_audit) (#252)

## [1.2.0] - 2026-04-11

- Add Obsidian vault integration with automatic Notion document sync (#246)

## [1.1.4] - 2026-04-11

- Add Telegram forum topic (thread) support (#251)

## [1.1.3] - 2026-04-11

- Add task scheduling with SQLite job store and executor loop (#247)

## [1.1.2] - 2026-04-11

- Fix semver release action: use GitHub App token to push to protected main (#249)

### Added
- Semantic versioning with git metadata embedded at build time
- `--version` / `-V` CLI flag prints version, git hash, and build date
- Version banner logged at startup
- CHANGELOG.md following Keep a Changelog format
- Release script (`scripts/release.sh`) for automated version bumps
- GitHub Actions release workflow for tag-triggered builds

## [0.1.0] - 2026-04-11

### Added
- Initial release
- Multi-crate workspace: core, store, gateway, agent, providers, tools, channels, skills, cli, memory
- Anthropic Claude and Ollama model providers
- Axum-based HTTP gateway with authentication
- SQLite-based persistent storage with encrypted credentials
- Communication channels: WebChat, Telegram, Signal, Video
- Hybrid memory system with fastembed embeddings, BM25, temporal, and knowledge graph retrieval
- Built-in tools: browser automation, email, Notion, file operations
- Composable skill system with YAML definitions
- OS-level sandbox and code execution isolation
- macOS Keychain integration for credential management
- CI pipeline: check, clippy, test, fmt, security audit
