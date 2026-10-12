# Migration campaigns: implemented contract

The campaign layer stores inventories, agent-authored dependency requirements,
Rust candidate trees, adapter plans, linked dependency campaigns, and executable
contract evidence in `REFORGE_DATA_DIR/porting.db`. It does not translate arbitrary
programs or automatically connect the scalar-C jobs to a whole-program build.

## Agent workflow

Call `inventory_project({"name":"demo","source_path":"/absolute/source"})`.
Use the returned `campaign_id` with `get_migration_inventory` and
`get_migration_source_context`. Pages take `offset` and `limit` (1–100 records);
context takes `file_path`, a byte `offset`, and `limit` (1–65536 bytes).
Source references must point to registered source/build files and valid line ranges.

Register actual usage with `register_dependency_requirements`:

```json
{
  "campaign_id": "<returned campaign_id>",
  "requirement": {
    "ecosystem": "c", "package": "libc strtol caller", "version": "C11 API; ambient runtime",
    "used_apis": ["strtol"],
    "usage_refs": [{"file": "probe.c", "line_start": 8, "line_end": 11}],
    "unknown_usage": [],
    "contracts": [{"id": "decimal_int32", "description": "whole decimal int32; leading ASCII whitespace/sign; invalid and overflow errors", "mandatory": true}]
  }
}
```

Agent searches sources/registries/docs using its own tools, then prepares a separate
Rust project. `record_dependency_candidate` takes `requirement_id` and `candidate`:
`package`, `version`, `strategy`, absolute `source_path`, HTTPS `research_urls`,
`license`, `target`, `api_mapping` (source API to Rust description), `known_gaps`,
`adapter_tasks`, optional `dependency_campaign_id`. Rust source is required.
Strategies: `direct_replace`, `adapter`, `compose_rust_libraries`, `port_dependency`,
`temporary_ffi`. Metadata and implementation origin are assertions, not authenticated
package discovery or proof that native Rust has replaced every original dependency.

`plan_dependency_adapter({"candidate_id":"..."})` persists a plan and reports
unmapped APIs, gaps, required contracts and next actions; it grants no acceptance.
If porting a dependency, first call `create_dependency_campaign` with
`requirement_id`, `name`, `source_path`. A `port_dependency` candidate must link that
child's ID. Ancestor source cycles and nesting above 32 are rejected. Child linkage
is planning infrastructure and is not automated translation or child-completion proof.

The client prepares/compiles executable probes or launchers inside their respective
project roots before pinning source/candidate trees. Call:

```json
{
  "candidate_id": "<returned candidate_id>",
  "probes": [{
    "contract_id": "decimal_int32",
    "source": {"program": "/absolute/source/probe", "args": []},
    "replacement": {"program": "/absolute/replacement/target/probe", "args": []},
    "cases": [["42"], [" +42"], ["2147483648"]]
  }]
}
```

`verify_dependency_replacement` appends each case's argv to both commands, executes
in each project's root, and compares one nonempty JSON object/array observation
(semantic JSON equality), stderr bytes as decoded text and successful exit status.
Probe observations are client-authored; fixtures cannot prove unobserved library or
whole-program behavior. JSON wrapping cannot establish full semantic equivalence.
Unknown usages, known gaps, missing API mappings, or untested mandatory contracts
block acceptance. Optional untested contracts are not implied verified.

Read `get_dependency_requirements`, `get_migration_campaign` and
`next_migration_tasks` after execution or server restart. Evidence has `passed`,
`mismatch`, `source_failed`, `failed`, `budget_exceeded`, `stale`, or `interrupted`
status. Effective candidate acceptance rechecks current source, replacement tree,
requirement revision, executable hashes and the latest attempt. Native strategy
assertions use `accepted_fixture_scope`; temporary FFI uses `verified_temporary_ffi`.
`revise_dependency_requirements` takes `requirement_id` and a complete `requirement`;
every revision invalidates prior acceptance, even if the old wording is restored.

There is no campaign-complete state: `program_migration_completed` remains false.
After dependency fixtures pass, the next action continues conversion/integration.

## Resource and environment limits

- Inventory: 100000 files, 4 GiB total, 512 MiB/file, a 120-second budget checked
  between entries and after file hashing; exhaustion errors
  instead of returning a complete inventory. Files are streamed for hashing but the
  bounded stamp list is currently held in memory; distributed graph processing is planned.
- Excludes `.git`, `target`, `node_modules`, `.gradle`, `.venv`, `__pycache__`.
  Symlinks are rejected. Excluded files are outside the tree fingerprint; executed
  artifacts are separately hashed before/after execution and when reading acceptance.
- Recognizes C/C++/Java/Python/Go/Rust filenames and common manifests, including
  Maven/Gradle/Cargo/Go/Python/CMake/Kbuild. No manifest package resolution or usage inference.
- At most 100 candidates per requirement, 32 attempts per candidate, 256 fixture
  cases per verification, 120 seconds total, 10 seconds/process, 8 MiB/output stream.
  Recent 10 attempts per candidate are returned; older evidence remains in SQLite/logs.
- Data directory must be outside source and replacement trees; those trees must
  not overlap. Operations use campaign file locks and transactional record/event writes.
- Execution uses argv without a shell, local host process limits and Unix process
  group cleanup. No OS sandbox, hermetic runtime, automatic compiler stage, network
  registry search, or ambient dynamic-library/JVM/OS fingerprint is provided.
- Source changes require a new inventory; changed replacement source requires a new
  candidate. Preparation/rebuilds can modify ignored artifacts and revoke prior evidence.

Run `python3 scripts/campaign_smoke.py` after building `reforge-mcp` to exercise the
actual stdio tools, whitespace/error mismatch, adapter acceptance, revision
invalidation and restart. See [fixture provenance](../fixtures/migration/strtol-rust/PROVENANCE.md).
