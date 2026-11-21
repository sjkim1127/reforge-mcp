# Changelog

All notable changes to Reforge-MCP (formerly Fossil-MCP) will be documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
Releases follow [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

### Added
- `generate_port_cases`: coverage-guided verification input generation over the original C (UBSan + SanitizerCoverage `trace-pc-guard,trace-cmp`, fork per input, hit-count buckets, comparison-operand dictionary, greedy set cover plus value-diverse samples). Undefined-behavior, crash, and timeout inputs are reported separately and excluded from cases. Results persist as `case_generations` job evidence.
- `verify_port` accepts typed fixtures (64-bit signed/unsigned integers and finite floats) and `use_generated_cases`; `Engine::verify_values` and `Engine::generate_cases` expose the same in the library.
- Clang include/macro preprocessing with original header source mapping, shared inline-function deduplication, and include-dependency hash invalidation.
- Pinned, unchanged musl `abs.c` fixture with upstream license/provenance and UB boundary checks.
- Independent `reforge-engine` with Clang AST C int32 analysis and deterministic Rust Cargo generation.
- Persistent porting jobs, dependency plans, agent candidate submission, hash invalidation, and append-only events.
- UBSan-backed original/Rust differential verification with bounded processes and retained build/case evidence.
- Real stdio MCP porting/restart smoke probe and compiler-backed regression tests.
- Rust-porting product design for C, C++, Java, Python, and Go with behavioral verification gates.
- `get_porting_capabilities` to distinguish implemented source lookup from planned porting features.
- Semantic search storage through local SQLite vector tables.
- SCIP index support when `index.scip` is present, with tree-sitter fallback.
- One-shot `analyze_feature` workflow for clone, index, and search.
- Structural migration analysis through `analyze_migration`.
- Local workspace vulnerability pattern scanning through `scan_vulnerabilities`.
- Transitive dependency indexing tools for Rust, Python, JavaScript/TypeScript, and C/C++ package caches.
- Incremental file indexing and workspace watcher tools for local workspaces.

### Changed
- Product, stdio server identity, executable, and release archives renamed to `reforge-mcp`; update MCP client command paths.
- Previous README preserved in `docs/fossil-reference.md`.
- Workspace crates renamed from `fossil-{core,repo,indexer,search,server}` to `reforge-{core,repo,indexer,search,server}`.
- Environment variables renamed to `REFORGE_LOG` and `REFORGE_CACHE_DIR`; `FOSSIL_LOG` / `FOSSIL_CACHE_DIR` remain as deprecated fallbacks with a startup warning.
- Default clone/index cache moved to `~/.reforge-mcp/cache`; an existing `~/.fossil-mcp/cache` keeps being used until the new directory exists.
- `REFORGE_DATA_DIR` resolution shared by porting jobs and campaigns; an empty value now falls back to the default directory.
- `clang.rs` translator split into focused `clang/` submodules (no behavior change).
- Verification drivers parse every argument at 64-bit width and narrow with C cast semantics; float parameters now receive exact decimal values instead of int32-only inputs.
- Default log filter now targets the `reforge_mcp` binary (`reforge_mcp=info,warn`); the previous `fossil_server` target matched no log events, so server info logs were silently dropped.
- README now documents the full MCP tool surface and current output contracts.
- CI product baseline now includes warning-free Clippy checks.
- Vulnerability scanning now uses named built-in heuristic patterns instead of mock CVE exact matching.
- Vulnerability scan results now expose `pattern_id` and `severity`, while keeping `cve_id` as a legacy alias.

### Fixed
- Float-returning entries print `nan` identically in the C and Rust drivers; previously C `nan` vs. Rust `NaN` made any NaN result a false mismatch.
- Removed dependency-indexer warnings that failed CI with `-D warnings`.
- Ignored macOS Finder metadata files.
- Honored `FOSSIL_CACHE_DIR` and create SQLite parent directories automatically.

---

## [0.1.0] - 2026-07-09

### Added
- **5 MCP tools** via rmcp 2.1 stdio transport:
  - `clone_reference` — shallow-clone a public git repo into `~/.fossil-mcp/cache/`
  - `index_repo` — parse symbols with tree-sitter (Rust, Python, TypeScript) and persist to per-repo SQLite
  - `locate_implementation` — fuzzy-search symbols by name + signature (nucleo-matcher / fzf algorithm)
  - `get_symbol_source` — read raw source for a file/line range
  - `list_indexed_repos` — list cached repos with indexing status
- **1-hop call graph** — `related_symbols` in search results (calls / called_by)
- **3-language support**: Rust (fn, struct, enum, trait, impl), Python (def, class, decorator), TypeScript/JS (function, class, interface, arrow fn)
- **`LanguageParser` trait** — pluggable parser registry for future language additions
- **`Searcher` trait** — pluggable search backend; `FuzzySearcher` is the MVP implementation
- Workspace of 5 crates: `fossil-{core,repo,indexer,search,server}`
- 17 unit tests across all crates

[Unreleased]: https://github.com/sjkim1127/reforge-mcp/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/sjkim1127/reforge-mcp/releases/tag/v0.1.0
