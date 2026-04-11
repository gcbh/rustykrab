# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
