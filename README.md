<p align="center">
  <img src="docs/assets/logo.svg" width="140" height="140" alt="Reforge-MCP monochrome logo">
</p>

<h1 align="center">Reforge-MCP</h1>

<p align="center">
  <em>Agent-oriented MCP infrastructure for verified migration of C/system software and its dependency ecosystem to Rust.</em>
</p>

<p align="center">
  <strong>English</strong> · <a href="docs/README_KO.md">한국어</a>
</p>

Reforge-MCP (formerly Fossil-MCP) targets automated porting of C, C++, Java, Python, and Go programs—alongside their dependency graphs—to Rust. Representative benchmark targets include kernel subsystems, general-purpose C libraries, Ghidra, and Spring Boot without imposing architectural upper bounds on project size.

---

## Product Direction

Reforge-MCP unifies source translation and library/framework replacement inside a single persistent migration graph. An AI agent analyzes actual API usage and behavioral contracts, chooses an appropriate dependency strategy (direct Rust crate replacement, typed adapter, multi-crate composition, recursive sub-campaign porting, or temporary FFI), and validates behavior against real execution fixtures.

End-to-end migration of entire ecosystems is the long-term product objective. What is executable today is the **typed C-to-Rust backend** (supporting both Linux kernel-scale `no_std`/Kbuild and general-purpose user-space C) paired with the **persistent campaign & dependency contract verification lifecycle**. See [Ecosystem Migration Design](docs/ecosystem-migration-design.md) and [Kernel-Scale Roadmap](docs/kernel-scale-roadmap.md) for architecture details.

```text
Source Project ─▶ Clang AST Analysis ─▶ Dependency Plan ─▶ Rust Generation / Agent Candidate
                                              ▲                        │
                                      Diagnostic Feedback ◀─── Differential Verification
                                                                       │
                                                       Persistent Evidence & Fail-Closed Report
```

---

## Current Implementation Status

| Capability | Status |
|---|---|
| **C Analysis & Source Snapshots** | Clang JSON AST, `compile_commands.json`, `CMakeLists.txt` / `Makefile` / recursive `Kbuild` & `.config` collection, automatic `.h` subdirectory `-I` discovery, header/source SHA-256 tracking, fail-closed rejection of unsupported constructs |
| **Planning & Context Inspection** | SCC dependency-ordered translation units, recursive function groups, pointer ownership/mutability inference (`pointer_contracts`), struct/union/constant/extern symbol maps, Kconfig flags, and line-level provenance |
| **Deterministic Rust Generation** | Signed/unsigned/fixed-width integers, `float`/`double`, enums, `void`, `#[repr(C)]` structs/unions, bitfields, fixed & string-padded char arrays, raw pointers with runtime null checks (`rf_ptr`), `Option<extern "C" fn>` function pointers, `rf_safe_*` safe reference wrappers, `static const` tables, loops, `switch` fallthrough, forward `goto` cleanup labels, definite-init locals, `malloc`/libc/compiler builtins, `rf_abi_*` exports, and kernel `#![no_std]` / `Kbuild` / `rf_port_bindings.h` output |
| **Agent Candidate Path** | `submit_rust_candidate` persists agent-authored `src/lib.rs` attempts under content hashing and runs the exact same verification gates |
| **Differential Verification** | C source compiled with `-fsanitize=undefined` and `-lm`, offline dependency-free Cargo build, `#![no_std]` `rlib` build check (`compile-kernel-nostd`), C11 header syntax check (`check-kernel-bindings-header`), bidirectional C↔Rust FFI object linking, and stdout/stderr/exit-code parity comparison over typed fixtures (64-bit signed/unsigned integers, finite `float`/`double`) |
| **Verification Input Generation** | `generate_port_cases` compiles the original C with UBSan + SanitizerCoverage (`trace-pc-guard,trace-cmp`) and runs a fork-per-input, coverage-guided search: edge hit-count buckets select the corpus, comparison operands feed a value dictionary, and a greedy set cover plus up to 16 value-diverse clean runs become the cases. Inputs that hit undefined behavior, crashes, or timeouts are reported separately and never handed to `verify_port` (Unix hosts) |
| **Persistence & Tamper Invalidation** | SQLite job/event store, attempt history across restarts, and automatic `stale` invalidation if source, headers, `.config`, manifests, or candidates change |
| **Campaign Inventory** | File hashing, build/manifest discovery, paginated inventory inspection, and source context lookup |
| **Dependency Migration Lifecycle** | Agent-authored dependency contracts & candidates, adapter planning, child sub-campaigns, and JSON execution differential verification |
| **Code Search & General Roadmaps** | Preserved (`clone_reference`, `index_repo`, `locate_implementation`, `get_symbol_source`, SQLite roadmaps) |
| **C++ / Java / Python / Go, Joern CPG, Autonomous Repair** | Planned |

### Supported C Language Scope

- **Types**: signed/unsigned/`_Bool`/fixed-width integers (`int8_t`–`int64_t`, `uint8_t`–`uint64_t`, `u8`–`u64`, `s8`–`s64`, `size_t`, `ptrdiff_t`, `intptr_t`/`uintptr_t`), `float`/`double`, `enum`, `void`, `struct`/`union`/`typedef`, bitfields, fixed-size arrays, string-padded `char` arrays, raw pointers (`int *`, `const int *`, `struct *`, `union *`, `void *`, nested pointers, pointer↔integer casts), and C ABI function pointers (`Option<extern "C" fn(...)>`).
- **Control Flow & Semantics**: checked signed vs. wrapping unsigned integer arithmetic, IEEE-754 floating-point arithmetic/casts/comparisons, shifts (`<<`, `>>`), bitwise/logical operators, compound assignments (`+=`, `-=`, `*=` including mixed int/float), prefix/postfix `++`/`--`, `if`/`else`, `switch`/`case`/`default` with fallthrough, `for`/`while`/`do-while` with `break`/`continue`, forward `goto` error-cleanup labels, definite-initialization tracking for locals (`int x;` allowed when assigned or address-taken before read), `sizeof`/`alignof`, and file-scope `static const` tables and string literals.
- **Stdlib, Builtins & FFI**: `<stdlib.h>` heap APIs (`malloc`, `calloc`, `realloc`, `free`, `qsort`), `<string.h>`/`<math.h>`, GCC/Clang builtins (`__builtin_add/sub/mul_overflow` & typed variants, `__builtin_bswap*`, `clz*`, `ctz*`, `popcount*`, `ffs*`, `parity*`, `rotateleft*`/`rotateright*`, `__builtin_expect`, `__builtin_constant_p`, `__builtin___memcpy/memmove/memset/strcpy/strncpy_chk`, `__builtin_isnan`/`isfinite`/`isinf`/`signbit*`/`fabs*`/`inf*`/`nan*`), external C prototypes (`unsafe extern "C"`), C boundary files (`ffi/`, `external/`, `*.ffi.c`, `*.external.c`), bidirectional C ABI exports (`rf_abi_*`), and inferred safe Rust wrappers (`rf_safe_*`) when pointer parameters satisfy single-element borrow semantics.
- **Kernel & General Build Contexts**: `compile_commands.json`, `CMakeLists.txt` (`add_compile_definitions`, `add_definitions`, `include_directories`, `add_subdirectory`), `Makefile` (`CFLAGS`, `CPPFLAGS`, `SUBDIRS`), recursive `Kbuild` & `.config` (`CONFIG_*`, `ccflags-y`, `obj-y`/`obj-m`/`obj-n`), automatic header directory `-I` discovery, include guards, macros, and shared header `inline` functions—up to **4,096 files** and **64 MiB** total source.
- **Kernel Output Adapter**: deterministic generation emits `kbuild/rf_kernel_port.rs` (`#![no_std]`), `kbuild/Kbuild`, and `kbuild/rf_port_bindings.h`, verified during `verify_port`.
- **Fail-Closed Exclusions**: mutable global variables, `static` locals, reads of uninitialized locals, backward/cross-scope `goto`, inline assembly, variadic functions, `main`, and `_Complex`.

> **Verification Policy**: `"completed"` means the selected entry function passed all submitted fixtures against the pinned source and latest candidate without undefined behavior. It does not claim whole-program equivalence over untested inputs or execution contexts. Source undefined behavior (caught via UBSan), build failures, or timeouts block completion.

---

## C-to-Rust Porting Workflow

1. `analyze_porting_project({"source_path": "/absolute/path/to/c-project"})`
2. `create_porting_plan({"job_id": "..."})`
3. `generate_rust({"job_id": "..."})` for deterministic translation, or inspect `get_porting_context` and submit agent code via `submit_rust_candidate`
4. `verify_port({"job_id": "...", "entry_function": "score", "cases": [[-7, 3], [0, 0], [200, 10]]})`, or generate inputs first with `generate_port_cases({"job_id": "...", "entry_function": "score"})` and pass `"use_generated_cases": true`
5. `get_porting_report({"job_id": "..."})` for verification status, build/execution logs, `pointer_contracts`, and `kbuild/` artifacts

Across server restarts, use `list_porting_jobs({})` to resume or inspect existing jobs. Submitting a new candidate creates a new numbered attempt while preserving historical verification records.

| Tool | Purpose |
|---|---|
| `get_porting_capabilities` | Inspect supported C scope, required tools, and execution limits |
| `analyze_porting_project` | Snapshot project files and run Clang AST + build-context analysis |
| `create_porting_plan` | Produce SCC dependency ordering and verification contract |
| `get_porting_context` | Inspect per-function source, types, pointer contracts, and Rust correspondence |
| `generate_rust` | Emit deterministic Cargo (`src/lib.rs`) + `no_std` Kbuild artifacts (`kbuild/*`) |
| `submit_rust_candidate` | Persist agent-authored `src/lib.rs` as a new candidate attempt |
| `generate_port_cases` | Coverage-guided search over the original C for verification inputs; reports UB/crash/timeout inputs separately |
| `verify_port` | Compile/run C (UBSan) vs. Rust (`cargo`, `no_std`, header check) differential harness over supplied or generated cases |
| `get_porting_report` | Retrieve current job state, evidence, commands, and tamper/staleness checks |
| `list_porting_jobs` | List the most recent 100 persisted porting jobs |

Verification executes on the local host without OS container isolation, enforcing direct command invocation (no shell), process-group cleanup on Unix, and bounded budgets: max 32 generation attempts/job, max 64 verifications/job, max 64 input generations/job (1–256 cases, ≤500,000 executions, ≤60 s search, 1 s per execution), 1–256 cases/run, 120 s overall operation timeout, 10 s per-case timeout, and 8 MiB per-stream output cap.

---

## Migration Campaigns & Dependency Lifecycle

1. Register a repository with `inventory_project` and inspect paginated files/usage via `get_migration_inventory` and `get_migration_source_context`.
2. Record dependency versions, consumed APIs, line ranges, unknown usage, and mandatory behavioral contracts via `register_dependency_requirements`.
3. Submit agent research & implementation metadata with `record_dependency_candidate`, store integration notes with `plan_dependency_adapter`, or link recursive sub-campaigns via `create_dependency_campaign`.
4. Prepare source/candidate executables inside each project and run `verify_dependency_replacement` to diff non-empty JSON stdout, stderr, and exit codes across required contracts.
5. Track next actions with `get_dependency_requirements`, `get_migration_campaign`, and `next_migration_tasks` (`revise_dependency_requirements` invalidates prior acceptance).

`accepted_fixture_scope` reflects passing the submitted contract fixtures (temporary FFI replacements are tracked separately as `verified_temporary_ffi`), while `program_migration_completed` remains `false` until full program integration is verified. See [Migration Campaigns Guide](docs/migration-campaigns.md).

---

## Getting Started

Building `reforge-mcp` requires **Rust 1.88+**. Running C analysis and differential verification requires `clang` (with JSON AST dump and UBSan), `rustc`, and `cargo` on `PATH`.

```bash
git clone https://github.com/sjkim1127/reforge-mcp
cd reforge-mcp
cargo build --release --locked --bin reforge-mcp
```

Add the MCP server to your client config:

```json
{
  "mcpServers": {
    "reforge-mcp": {
      "command": "/absolute/path/to/reforge-mcp/target/release/reforge-mcp"
    }
  }
}
```

State is stored under `REFORGE_DATA_DIR` (defaulting to the OS cache directory under `reforge-mcp`): SQLite `porting.db`, pinned snapshots, generated `candidates/<attempt>/` (`Cargo.toml`, `src/lib.rs`, `kbuild/*`), and verification logs in `verification/<attempt>/logs/`.

---

## Reproducible MCP Smoke Demos

```bash
cargo build --locked --bin reforge-mcp
python3 scripts/porting_smoke.py
python3 scripts/campaign_smoke.py
```

- `porting_smoke.py`: drives the stdio MCP server over arithmetic fixtures (6 cases), server restart/resume, global-state rejection, header/macro fixtures (6 cases), and pinned [musl `abs.c`](fixtures/porting/musl-abs/PROVENANCE.md) (6 defined cases passing + `INT_MIN` UB blocked), writing artifacts under `.reforge-demo/`.
- `campaign_smoke.py`: verifies dependency contract matching between C `strtol` and a Rust parser candidate (8 cases), mismatch rejection, revision invalidation, and restart persistence into `.reforge-demo/campaign-report.json`.

### 1,000+ C Unit Clang Benchmark

Run the multi-threaded Clang AST + deterministic translator + `rustc #![no_std]` corpus profiler in either `strict` (default fail-closed profile) or `general` (`TranslateOptions::general_c()`) mode:

```bash
python3 scripts/clang_benchmark.py                    # strict profile (default)
python3 scripts/clang_benchmark.py --profile general  # general-purpose C profile
```

By default `scripts/clang_benchmark.py` discovers real C translation units across `fixtures/porting/` and any locally checked-out `vendor/` reference corpora (`c2rust`, `crown`, `c2saferrust`/Laertes, `musl`, `zopfli`), falling back to synthetic C units when `--min-units 1000` is not yet reached, and writes `.reforge-demo/clang-benchmark-report.json`.

Measured across **1,690 real C translation units** (`1,383` musl, `191` Crown, `100` C2Rust unit tests, `10` Laertes, `5` fixtures, `1` Zopfli):

| Stage | Strict Profile | General Profile (`--profile general`) |
|---|---|---|
| Clang AST Parsed (`-ast-dump=json`) | 1,668 / 1,690 (98.7%) | 1,668 / 1,690 (98.7%) |
| Translated Units (non-empty) | **504** (+ 20 empty) | **689** (+ 26 empty) |
| `rustc --edition=2024` `#![no_std]` Compiled | **504 (100.0% of translated)** | **689 (100.0% of translated)** |
| Translated Functions (`rf_safe_*` wrappers) | **2,334 (467)** | **3,992 (964)** |

Under `--profile general` (including 10/10 Laertes, 72/89 C2Rust unit tests, 104/181 Crown, and 498/1,383 musl), remaining rejections are primarily multi-variable declaration lists / unsupported attributes, `GCCAsmStmt` inline assembly, `__builtin_va_list`, and external incomplete system structs (`struct stat`, `z_t`). Strict mode continues to reject mutable `static`/global state, uninitialized scalars, variadic calls, `main`, and arbitrary CFG `goto`/Duff's device by design.

### Real Open-Source E2E Porting & Native Test Suite Benchmark

Translate whole real open-source C libraries with `TranslateOptions::drop_in_c_abi()`, compile each multi-TU project into a native Rust `.a` static library (or pure Rust binary) with `rustc --edition=2024 -O`, link unmodified upstream C/C++ test suites directly against the Rust output, and verify behavior parity against C baselines:

```bash
python3 scripts/oss_e2e_benchmark.py
```

Measured across **10 real open-source C projects** (**28 C translation units**, **557 translated functions**):

| Project | TUs | Functions | Output | Native Upstream Suite Verification |
|---|---:|---:|---|---|
| `ht` | 1/1 | 10 | `libht_rs.a` | Unmodified `samples/demo.c` linked & matched C stdout (exit 0) |
| `quadtree` | 4/4 | 24 | `libquadtree_rs.a` | Unmodified `test.c` linked (`-lm`) & matched C stdout (exit 0) |
| `urlparser` | 1/1 | 21 | `urlparser_rs` (bin) | Pure Rust binary matches C baseline (`c_exit == rs_exit == -11`, `stdout_match: True`; upstream `url.h:191` `malloc(1)` heap overflow traps identically in C and Rust) |
| `rgba` | 1/1 | 13 | `librgba_rs.a` | Unmodified `test.c` linked & matched C stdout (exit 0) |
| `libcsv` | 1/1 | 23 | `libcsv_rs.a` | Unmodified `test_csv.c` linked & matched (`All tests passed`, exit 0) |
| `lodepng` | 1/1 | 235 | `liblodepng_rs.a` | `example_encode.c` + `example_decode.c` linked; 10,622-byte PNG byte-identical to C baseline (exit 0) |
| `genann` | 1/1 | 15 | `libgenann_rs.a` | Unmodified `c/test.c` linked (`-lm`) (`ALL TESTS PASSED (521556/521556)`, exit 0) |
| `bzip2` | 7/7 | 64 | `libbzip2_rs.a` | Unmodified `bzip2.c` CLI linked; 6/6 upstream `sample1/2/3` compress & decompress `cmp` checks passed |
| `zlib` (`Z_SOLO`) | 9/9 | 99 | `libzlib_rs.a` | Unmodified `test/example.c` (`-DZ_SOLO`) linked without `-lz` & matched C stdout (exit 0) |
| `json.h` | 1/1 | 53 | `libjson_h_rs.a` | Strong `T _json_parse` linked via whole-archive override; `508/508` C/C++ JSONTestSuite tests passed (exit 0) |

---

## Compatibility, Documentation & Development

- **Rebrand compatibility**: the binary is `reforge-mcp` (formerly `fossil-mcp`; update MCP `command` paths) and workspace crates are `reforge-{core,repo,indexer,search,server,engine}`.
  - Environment variables: `REFORGE_LOG` (tracing filter, e.g. `REFORGE_LOG=reforge_mcp=debug,warn`), `REFORGE_CACHE_DIR` (clone/index cache), and `REFORGE_DATA_DIR` (porting jobs & campaigns). The legacy `FOSSIL_LOG` / `FOSSIL_CACHE_DIR` are still honored when the new name is unset, and the server logs a deprecation warning.
  - Clone/index cache defaults to `~/.reforge-mcp/cache`. If only the pre-rebrand `~/.fossil-mcp/cache` exists it keeps being used (its `global.db` stores absolute clone paths), so existing indexes survive; delete it and re-index to switch.
  - Vulnerability `pattern_id` values keep their `FOSSIL-*` prefix as stable result identifiers.
- **Design docs**:
  - [Ecosystem Migration Design](docs/ecosystem-migration-design.md)
  - [Kernel-Scale Roadmap](docs/kernel-scale-roadmap.md)
  - [Rust Porting Engine Design](docs/rust-porting-design.md)
  - [Migration Campaigns Contract](docs/migration-campaigns.md)
  - [Legacy Tool Reference](docs/fossil-reference.md)
  - [Release Process](RELEASING.md)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

Apache-2.0 licensed. See [LICENSE](LICENSE).
