use reforge_engine::Engine;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

struct Workspace {
    _temp: TempDir,
    source: PathBuf,
    data: PathBuf,
}
impl Workspace {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("input");
        let data = temp.path().join("data");
        std::fs::create_dir(&source).unwrap();
        Self {
            _temp: temp,
            source,
            data,
        }
    }
    fn engine(&self) -> Engine {
        Engine::new(&self.data).unwrap()
    }
    fn write(&self, file: &str, source: &str) {
        std::fs::write(self.source.join(file), source).unwrap();
    }
    fn arithmetic(&self) {
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/porting/arithmetic");
        for file in ["math.c", "score.c"] {
            std::fs::copy(fixtures.join(file), self.source.join(file)).unwrap();
        }
    }
}

fn describe(job: &reforge_engine::model::Job) -> String {
    format!(
        "error={:?} attempts={:?}",
        job.last_error,
        job.verifications
            .iter()
            .map(|v| (
                &v.status,
                v.commands
                    .iter()
                    .filter(|c| !c.success())
                    .map(|c| (&c.program, c.exit_code, &c.stderr))
                    .collect::<Vec<_>>(),
                v.cases
                    .iter()
                    .filter(|c| !c.matched)
                    .map(|c| (
                        &c.arguments,
                        c.source.exit_code,
                        c.source.signal,
                        c.source.timed_out,
                        c.source.duration_ms,
                        &c.source.stdout,
                        &c.source.stderr,
                        c.rust.as_ref().map(|r| (
                            r.exit_code,
                            r.signal,
                            r.timed_out,
                            r.duration_ms,
                            &r.stdout,
                            &r.stderr
                        ))
                    ))
                    .collect::<Vec<_>>()
            ))
            .collect::<Vec<_>>()
    )
}

#[tokio::test]
async fn multi_file_port_compiles_matches_and_resumes() {
    let workspace = Workspace::new();
    workspace.arithmetic();
    let engine = workspace.engine();
    let analyzed = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(analyzed.state, "analyzed", "{:?}", analyzed.unsupported);
    assert_eq!(analyzed.functions.len(), 3);
    let plan = engine.plan(&analyzed.job_id).unwrap();
    let groups = plan["dependency_groups"].as_array().unwrap();
    assert_eq!(groups.last().unwrap(), &serde_json::json!(["score"]));
    let generated = engine.generate(&analyzed.job_id).unwrap();
    assert_eq!(generated.state, "generated");
    let reopened = workspace.engine();
    let verified = reopened
        .verify(
            &analyzed.job_id,
            "score",
            vec![
                vec![-200, 0],
                vec![-100, 1],
                vec![-7, 3],
                vec![0, 0],
                vec![8, -1],
                vec![200, 10],
            ],
        )
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
    let evidence = verified.verifications.last().unwrap();
    assert!(evidence.cases.iter().all(|case| case.matched));
    let outputs = evidence
        .cases
        .iter()
        .map(|case| case.source.stdout.trim())
        .collect::<Vec<_>>();
    assert_eq!(outputs, vec!["199", "201", "17", "-1", "15", "210"]);
    assert!(
        evidence
            .commands
            .iter()
            .any(|run| run.program == "cargo" && run.success())
    );
    assert!(
        evidence.commands.iter().any(|run| run.program == "clang"
            && run.args.iter().any(|arg| arg == "-fsanitize=undefined"))
    );
    assert_eq!(
        workspace.engine().report(&analyzed.job_id).unwrap().state,
        "completed"
    );
    assert_eq!(
        workspace.engine().list().unwrap()["jobs"][0]["job_id"],
        analyzed.job_id
    );
}

#[tokio::test]
async fn rejects_unsupported_semantics_and_unresolved_calls() {
    let sources = [
        "_Complex double f(_Complex double a) { return a; }",
        "int global=1; int f(int x) { return x+global; }",
        "int f(int x) { int value; return value+x; }",
        "int f(int x) { static int count=1; return x+count; }",
        "int f(int x) { loop_top: if (x > 0) { x--; goto loop_top; } return x; }",
        "int f(int x) { __asm__(\"nop\"); return x; }",
        "int vfn(int x, ...); int f(int x) { return vfn(x, 1); }",
    ];
    for source in sources {
        let workspace = Workspace::new();
        workspace.write("input.c", source);
        let engine = workspace.engine();
        let job = engine.analyze(&workspace.source).await.unwrap();
        assert_eq!(job.state, "blocked", "{source}: {:?}", job);
        assert!(!job.unsupported.is_empty());
        assert!(engine.plan(&job.job_id).is_err());
        assert!(engine.generate(&job.job_id).is_err());
    }
}

#[tokio::test]
async fn detects_wrong_candidate_then_allows_a_verified_new_attempt() {
    let workspace = Workspace::new();
    workspace.arithmetic();
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    engine.plan(&job.job_id).unwrap();
    engine
        .submit(&job.job_id, "pub fn rf_score(_x:i32,_bias:i32)->i32 { 0 }")
        .unwrap();
    let failed = engine
        .verify(&job.job_id, "score", vec![vec![8, 2]])
        .await
        .unwrap();
    assert_eq!(failed.state, "failed", "{}", describe(&failed));
    assert_eq!(failed.verifications[0].status, "mismatch");
    engine.generate(&job.job_id).unwrap();
    let passed = engine
        .verify(&job.job_id, "score", vec![vec![8, 2]])
        .await
        .unwrap();
    assert_eq!(
        passed.state, "completed",
        "error={:?} evidence={:?}",
        passed.last_error, passed.verifications
    );
    assert_eq!(passed.generation_attempt, 2);
    assert_eq!(passed.verifications.len(), 2);
    assert_ne!(
        passed.verifications[0].candidate_sha256,
        passed.verifications[1].candidate_sha256
    );
}

#[tokio::test]
async fn undefined_source_arithmetic_never_counts_as_parity() {
    let workspace = Workspace::new();
    workspace.write("input.c", "int add(int x,int y) { return x+y; }");
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(&job.job_id, "add", vec![vec![i32::MAX, 1]])
        .await
        .unwrap();
    assert_eq!(verified.state, "blocked", "{:?}", verified.last_error);
    assert_eq!(verified.verifications[0].status, "source_failed");
    assert!(!verified.verifications[0].cases[0].source.success());
    assert!(verified.verifications[0].cases[0].rust.is_none());
}

#[tokio::test]
async fn invalidation_catches_input_and_candidate_changes() {
    let workspace = Workspace::new();
    workspace.write("input.c", "int f(int x) { return x+1; }");
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    engine.plan(&job.job_id).unwrap();
    let generated = engine.generate(&job.job_id).unwrap();
    let passed = engine
        .verify(&job.job_id, "f", vec![vec![1]])
        .await
        .unwrap();
    assert_eq!(
        passed.state, "completed",
        "error={:?} evidence={:?}",
        passed.last_error, passed.verifications
    );
    std::fs::write(
        Path::new(&generated.artifact_path).join("candidates/1/src/lib.rs"),
        "pub fn rf_f(x:i32)->i32{x}",
    )
    .unwrap();
    assert_eq!(engine.report(&job.job_id).unwrap().state, "stale");
    assert!(
        engine
            .verify(&job.job_id, "f", vec![vec![1]])
            .await
            .is_err()
    );
    engine
        .submit(&job.job_id, "pub fn rf_f(x:i32)->i32{x+1}")
        .unwrap();
    assert_eq!(
        engine
            .verify(&job.job_id, "f", vec![vec![1]])
            .await
            .unwrap()
            .state,
        "completed"
    );
    workspace.write("input.c", "int f(int x) { return x+2; }");
    assert_eq!(
        workspace.engine().report(&job.job_id).unwrap().state,
        "stale"
    );
    assert!(
        engine
            .verify(&job.job_id, "f", vec![vec![1]])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn compile_failure_and_timeout_preserve_diagnostics() {
    let workspace = Workspace::new();
    workspace.write("input.c", "int f(int x) { return x; }");
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    engine.plan(&job.job_id).unwrap();
    engine.submit(&job.job_id, "this is not Rust").unwrap();
    let failed = engine
        .verify(&job.job_id, "f", vec![vec![1]])
        .await
        .unwrap();
    assert_eq!(failed.state, "failed", "{}", describe(&failed));
    assert!(
        failed.verifications[0]
            .commands
            .iter()
            .any(|run| run.program == "cargo" && !run.success() && !run.stderr.is_empty())
    );
    engine
        .submit(
            &job.job_id,
            "pub fn rf_f(_:i32)->i32{loop{std::hint::spin_loop();}}",
        )
        .unwrap();
    let timeout = engine
        .verify(&job.job_id, "f", vec![vec![1]])
        .await
        .unwrap();
    assert_eq!(timeout.state, "failed", "{}", describe(&timeout));
    assert!(
        timeout.verifications.last().unwrap().cases[0]
            .rust
            .as_ref()
            .unwrap()
            .timed_out
    );
}

#[tokio::test]
async fn short_circuit_recursion_and_variable_shadowing_match() {
    let workspace = Workspace::new();
    workspace.write("input.c", "int factorial(int x) { if(x<=1) return 1; return x*factorial(x-1); }\nint f(int x) { int v=2; if(x>0) { int v=3; x=x+v; } if(x==0 || (10/x)>0) return factorial(v)+x; return x; }");
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    let plan = engine.plan(&job.job_id).unwrap();
    assert_eq!(
        plan["dependency_groups"][0],
        serde_json::json!(["factorial"])
    );
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(&job.job_id, "f", vec![vec![0], vec![1], vec![-2], vec![3]])
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
}

#[tokio::test]
async fn invalid_requests_and_path_traversal_do_not_create_evidence() {
    let workspace = Workspace::new();
    workspace.write("input.c", "int f(int x) { return x; }");
    let engine = workspace.engine();
    assert!(engine.report("../../escape").is_err());
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert!(engine.generate(&job.job_id).is_err());
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    assert!(engine.verify(&job.job_id, "f", vec![]).await.is_err());
    assert!(engine.verify(&job.job_id, "f", vec![vec![]]).await.is_err());
    assert!(
        engine
            .verify(&job.job_id, "unknown", vec![vec![1]])
            .await
            .is_err()
    );
    assert!(engine.report(&job.job_id).unwrap().verifications.is_empty());
}

#[tokio::test]
async fn cancelled_verification_releases_lock_and_can_resume() {
    let workspace = Workspace::new();
    workspace.write("input.c", "int f(int x) { return x; }");
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    engine.plan(&job.job_id).unwrap();
    engine
        .submit(
            &job.job_id,
            "pub fn rf_f(_:i32)->i32{loop{std::hint::spin_loop();}}",
        )
        .unwrap();
    let runner = workspace.engine();
    let id = job.job_id.clone();
    let task = tokio::spawn(async move { runner.verify(&id, "f", vec![vec![1]]).await });
    let log = Path::new(&job.artifact_path).join("verification/1/logs/case-0-rust.stdout");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !log.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "verification never reached the runtime case"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    assert!(
        engine.report(&job.job_id).is_err(),
        "running job must be locked"
    );
    task.abort();
    let _ = task.await;
    let resumed = workspace.engine().report(&job.job_id).unwrap();
    assert_eq!(resumed.state, "interrupted");
    assert_eq!(resumed.verifications[0].status, "interrupted");
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(&job.job_id, "f", vec![vec![1]])
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
    assert_eq!(verified.verifications.len(), 2);
    assert_eq!(verified.verifications[0].status, "interrupted");
    assert_eq!(verified.verifications[1].attempt, 2);
}

#[tokio::test]
async fn signed_division_remainder_bits_and_zero_argument_entries_match() {
    let workspace = Workspace::new();
    workspace.write("input.c","int f(int x,int y) { if(y==0) return x; int v=x/y+x%y; return (v^~y)&127; }\nint constant(void) { return 42; }");
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let values = engine
        .verify(
            &job.job_id,
            "f",
            vec![
                vec![-7, 3],
                vec![7, -3],
                vec![-7, -3],
                vec![i32::MIN, 3],
                vec![9, 0],
            ],
        )
        .await
        .unwrap();
    assert_eq!(values.state, "completed", "{}", describe(&values));
    let constant = engine
        .verify(&job.job_id, "constant", vec![vec![]])
        .await
        .unwrap();
    assert_eq!(constant.state, "completed", "{}", describe(&constant));
    assert_eq!(
        constant.verifications.last().unwrap().cases[0]
            .source
            .stdout,
        "42\n"
    );
}

#[tokio::test]
async fn headers_macros_and_shared_inline_functions_keep_correct_provenance() {
    let workspace = Workspace::new();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/porting/headers");
    for file in ["api.h", "scale.c", "evaluate.c"] {
        std::fs::copy(fixtures.join(file), workspace.source.join(file)).unwrap();
    }
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    assert_eq!(job.functions.len(), 3);
    let header = job.functions.iter().find(|f| f.name == "bounded").unwrap();
    assert_eq!(header.source_file, "api.h");
    assert_eq!(header.line_start, 6);
    assert_eq!(header.line_end, 10);
    assert!(job.header_dependencies.iter().any(|d| d.kind == "external"));
    assert!(
        job.header_dependencies
            .iter()
            .any(|d| d.path.ends_with("api.h"))
    );
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let result = engine
        .verify(
            &job.job_id,
            "evaluate",
            vec![
                vec![-100],
                vec![-40],
                vec![0],
                vec![17],
                vec![40],
                vec![100],
            ],
        )
        .await
        .unwrap();
    assert_eq!(result.state, "completed", "{}", describe(&result));
    assert_eq!(
        result.verifications[0]
            .cases
            .iter()
            .map(|c| c.source.stdout.trim())
            .collect::<Vec<_>>(),
        vec!["-160", "-160", "0", "68", "160", "160"]
    );
    workspace.write("api.h", "#define SCALE 5\n");
    assert_eq!(engine.report(&job.job_id).unwrap().state, "stale");
}

#[tokio::test]
async fn pinned_real_musl_abs_compiles_matches_and_rejects_undefined_boundary() {
    let workspace = Workspace::new();
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/porting/musl-abs/abs.c");
    std::fs::copy(fixture, workspace.source.join("abs.c")).unwrap();
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    assert_eq!(job.functions.len(), 1);
    assert_eq!(job.functions[0].source_file, "abs.c");
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let good = engine
        .verify(
            &job.job_id,
            "abs",
            vec![
                vec![0],
                vec![1],
                vec![-1],
                vec![-65536],
                vec![i32::MAX],
                vec![i32::MIN + 1],
            ],
        )
        .await
        .unwrap();
    assert_eq!(good.state, "completed", "{}", describe(&good));
    assert_eq!(
        good.verifications[0]
            .cases
            .iter()
            .map(|c| c.source.stdout.trim())
            .collect::<Vec<_>>(),
        vec!["0", "1", "1", "65536", "2147483647", "2147483647"]
    );
    let ub = engine
        .verify(&job.job_id, "abs", vec![vec![i32::MIN]])
        .await
        .unwrap();
    assert_eq!(ub.state, "blocked", "{}", describe(&ub));
    assert_eq!(ub.verifications.last().unwrap().status, "source_failed");
}

#[tokio::test]
async fn external_header_drift_invalidates_a_verified_job() {
    let workspace = Workspace::new();
    let external = workspace._temp.path().join("external.h");
    std::fs::write(&external, "#define FACTOR 2\n").unwrap();
    workspace.write(
        "input.c",
        &format!(
            "#include \"{}\"\nint f(int x) {{ return x*FACTOR; }}",
            external.display()
        ),
    );
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    assert!(job.header_dependencies.iter().any(|d| d.path
        == external.canonicalize().unwrap().display().to_string()
        && d.kind == "external"));
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    assert_eq!(
        engine
            .verify(&job.job_id, "f", vec![vec![7]])
            .await
            .unwrap()
            .state,
        "completed"
    );
    std::fs::write(external, "#define FACTOR 3\n").unwrap();
    assert_eq!(engine.report(&job.job_id).unwrap().state, "stale");
    let changed = engine.analyze(&workspace.source).await.unwrap();
    assert_ne!(job.source_snapshot_sha256, changed.source_snapshot_sha256);
}

#[tokio::test]
async fn escaped_header_paths_and_missing_final_newline_preserve_source_bytes() {
    let workspace = Workspace::new();
    workspace.write("api #$.h", "#define FACTOR 4\n");
    let contents = "#include \"api #$.h\"\nint f(int x) { return x*FACTOR; }";
    workspace.write("input.c", contents);
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    let snapshot = Path::new(&job.artifact_path).join("source/input.c");
    assert_eq!(std::fs::read_to_string(snapshot).unwrap(), contents);
    assert!(
        job.header_dependencies
            .iter()
            .any(|d| d.path.ends_with("api #$.h"))
    );
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let result = engine
        .verify(&job.job_id, "f", vec![vec![-3], vec![0], vec![5]])
        .await
        .unwrap();
    assert_eq!(result.state, "completed", "{}", describe(&result));
}

#[tokio::test]
async fn loops_for_while_do_break_continue_and_compound_assignments_match() {
    let workspace = Workspace::new();
    workspace.write(
        "loops.c",
        r#"
int sum_loops(int n, int limit) {
    int acc = 0;
    for (int i = 0; i < n; i++) {
        if (i == 2) continue;
        if (i >= limit) break;
        acc += (i << 1) + 1;
    }
    int w = n;
    while (w > 0) {
        w -= 2;
        if (w == 1) continue;
        acc ^= (w & 7);
    }
    int d = 0;
    do {
        d++;
        if (d == 2) continue;
        acc += d;
    } while (d < 4);
    int z = 16;
    z >>= 2;
    return acc + z;
}
"#,
    );
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(
            &job.job_id,
            "sum_loops",
            vec![vec![0, 5], vec![1, 5], vec![6, 4], vec![8, 10]],
        )
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
}

#[tokio::test]
async fn structs_nested_fields_and_pointer_mutations_match() {
    let workspace = Workspace::new();
    workspace.write(
        "types.h",
        r#"
struct Inner {
    int x;
    int y;
};
typedef struct {
    struct Inner pos;
    int weight;
} Item;
"#,
    );
    workspace.write(
        "ops.c",
        r#"
#include "types.h"

void bump_pair(int *a, int *b) {
    int tmp = *a;
    *a = *b + 1;
    *b = tmp + 2;
}

void scale_item(Item *item, const int *factor) {
    if (!item || !factor) return;
    item->pos.x *= *factor;
    item->pos.y += item->weight;
}

int evaluate_item(int *io_val, int factor) {
    Item it = { .pos = { *io_val, 3 }, .weight = 5 };
    Item zeroed = { 0 };
    int other = zeroed.weight + 4;
    bump_pair(io_val, &other);
    scale_item(&it, &factor);
    int *p = &it.pos.x;
    int *q = &it.pos.y;
    p[0] += *q + other;
    return p[0] + (int)(p - p);
}
"#,
    );
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    assert_eq!(job.structs.len(), 2);
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(
            &job.job_id,
            "evaluate_item",
            vec![vec![2, 3], vec![-4, 5], vec![10, 0]],
        )
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
    assert!(
        verified.verifications[0].cases[0]
            .source
            .stdout
            .contains("p0=5")
    );
    let void_verified = engine
        .verify(&job.job_id, "bump_pair", vec![vec![3, 7], vec![0, -1]])
        .await
        .unwrap();
    assert_eq!(
        void_verified.state,
        "completed",
        "{}",
        describe(&void_verified)
    );
    assert_eq!(
        void_verified.verifications.last().unwrap().cases[0]
            .source
            .stdout
            .trim(),
        "p0=8 p1=5"
    );
}

#[tokio::test]
async fn external_function_calls_and_c_boundary_files_compile_and_match() {
    let workspace = Workspace::new();
    std::fs::create_dir(workspace.source.join("ffi")).unwrap();
    workspace.write(
        "ffi/provider.c",
        r#"
static int hidden_bias = 10;
int c_boundary_compute(int x, int *out) {
    *out = x * 3 + hidden_bias;
    return *out + 1;
}
"#,
    );
    workspace.write(
        "caller.c",
        r#"
#include <stdlib.h>
int c_boundary_compute(int x, int *out);
int run_with_external(int x) {
    int out = 0;
    int res = c_boundary_compute(abs(x), &out);
    return res + out;
}
"#,
    );
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    assert_eq!(job.c_boundary_files, vec!["ffi/provider.c"]);
    assert!(
        job.external_functions
            .iter()
            .any(|e| e.name == "c_boundary_compute")
    );
    assert!(job.external_functions.iter().any(|e| e.name == "abs"));
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(
            &job.job_id,
            "run_with_external",
            vec![vec![-5], vec![0], vec![4]],
        )
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
}

#[tokio::test]
async fn large_multi_file_project_and_compile_commands_database_work() {
    let workspace = Workspace::new();
    let mut entries = Vec::new();
    let mut header = String::new();
    let mut main_body =
        String::from("#include \"units.h\"\nint sum_40(int x) {\n    int total = x;\n");
    for i in 0..40 {
        let fname = format!("unit_{i}.c");
        header.push_str(&format!("int unit_{i}(int x);\n"));
        workspace.write(
            &fname,
            &format!("int unit_{i}(int x) {{ return x + UNIT_BIAS + {i}; }}\n"),
        );
        main_body.push_str(&format!("    total += unit_{i}(x);\n"));
        entries.push(serde_json::json!({
            "directory": workspace.source.display().to_string(),
            "file": workspace.source.join(&fname).display().to_string(),
            "arguments": ["clang", "-std=c11", "-DUNIT_BIAS=3", "-c", &fname]
        }));
    }
    main_body.push_str("    return total;\n}\n");
    workspace.write("units.h", &header);
    workspace.write("driver_lib.c", &main_body);
    entries.push(serde_json::json!({
        "directory": workspace.source.display().to_string(),
        "file": "driver_lib.c",
        "arguments": ["clang", "-std=c11", "-c", "driver_lib.c"]
    }));
    workspace.write(
        "compile_commands.json",
        &serde_json::to_string_pretty(&entries).unwrap(),
    );

    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    assert!(job.compilation_database);
    assert_eq!(job.functions.len(), 41);
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(&job.job_id, "sum_40", vec![vec![0], vec![2], vec![-5]])
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
}

#[tokio::test]
async fn fixed_width_unsigned_enums_and_switch_fallthrough_match() {
    let workspace = Workspace::new();
    workspace.write(
        "types_switch.c",
        r#"
#include <stdint.h>
#include <stdbool.h>

enum Mode {
    MODE_ADD = 1,
    MODE_FALLTHROUGH = 2,
    MODE_MASK = 3
};

static uint32_t mix_u32(uint32_t a, uint8_t b, bool flag) {
    uint64_t wide = (uint64_t)a + (uint64_t)b;
    if (flag) {
        wide += 0xffffffffu;
    }
    return (uint32_t)(wide ^ (wide >> 4));
}

int eval_modes(unsigned int raw, int steps) {
    int64_t acc = 0;
    for (int i = 0; i < steps; i++) {
        enum Mode m = MODE_ADD;
        if ((i & 3) == 1) m = MODE_FALLTHROUGH;
        else if ((i & 3) == 2) m = MODE_MASK;
        switch (m) {
            case MODE_ADD:
                acc += (int64_t)mix_u32(raw + (uint32_t)i, (uint8_t)(i & 255), i == 0);
                break;
            case MODE_FALLTHROUGH:
                acc += 10;
                /* intentional fallthrough */
            case MODE_MASK:
                acc ^= 0x55;
                if (i == 2) continue;
                acc += 1;
                break;
            default:
                break;
        }
    }
    return (int)(acc & 0x7fffffff);
}
"#,
    );
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(
            &job.job_id,
            "eval_modes",
            vec![vec![0, 0], vec![7, 5], vec![-1, 8], vec![123456, 4]],
        )
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
}

#[tokio::test]
async fn arrays_sizeof_and_function_pointer_ops_tables_match() {
    let workspace = Workspace::new();
    workspace.write(
        "ops_arrays.c",
        r#"
#include <stddef.h>
#include <stdint.h>

typedef int (*bin_op_t)(int, int);

struct Ops {
    bin_op_t apply;
    int weights[4];
};

int op_add(int a, int b) { return a + b; }
int op_sub(int a, int b) { return a - b; }

static int dot_slice(const int *p, int count) {
    int sum = 0;
    for (int i = 0; i < count; i++) {
        sum += p[i] * (i + 1);
    }
    return sum;
}

int run_dispatch(int x, int use_sub) {
    int local[4] = { 10, 20 };
    local[2] = x;
    local[3] = (int)sizeof(local) + (int)sizeof(struct Ops);

    struct Ops ops = { op_add, { 1, 2, 3, 4 } };
    if (use_sub) {
        ops.apply = op_sub;
    }
    bin_op_t null_fn = NULL;
    int bonus = (null_fn == NULL && ops.apply != NULL) ? 5 : 0;
    if (null_fn) {
        bonus = -999;
    }

    int d1 = dot_slice(local, 4);
    int d2 = dot_slice(ops.weights, 4);
    return ops.apply(d1, d2) + bonus;
}
"#,
    );
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(
            &job.job_id,
            "run_dispatch",
            vec![vec![0, 0], vec![7, 0], vec![-3, 1], vec![42, 1]],
        )
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
}

#[tokio::test]
async fn kbuild_and_kconfig_filter_units_and_define_config_macros() {
    let workspace = Workspace::new();
    workspace.write(
        ".config",
        "CONFIG_FAST_PATH=y\n# CONFIG_LEGACY_BUG is not set\nCONFIG_SCALE=4\n",
    );
    workspace.write(
        "Kbuild",
        "ccflags-y += -DEXTRA_OFFSET=7\nobj-y += core.o\nobj-$(CONFIG_FAST_PATH) += fast.o\nobj-$(CONFIG_LEGACY_BUG) += broken_legacy.o\n",
    );
    workspace.write("fast.h", "int fast_calc(int x);\n");
    workspace.write(
        "fast.c",
        r#"
#include "fast.h"
int fast_calc(int x) {
#ifdef CONFIG_FAST_PATH
    return x * CONFIG_SCALE + EXTRA_OFFSET;
#else
    return 0;
#endif
}
"#,
    );
    workspace.write(
        "core.c",
        r#"
#include "fast.h"
int entry_kbuild(int x) {
    return fast_calc(x) + 1;
}
"#,
    );
    // Intentionally invalid C that must be skipped because CONFIG_LEGACY_BUG is not set.
    workspace.write(
        "broken_legacy.c",
        "int global_bad = 1; int broken_fn(int x) { return x + global_bad; }\n",
    );

    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    assert_eq!(job.inactive_source_files, vec!["broken_legacy.c"]);
    assert_eq!(
        job.kconfig.get("CONFIG_FAST_PATH").map(String::as_str),
        Some("y")
    );
    assert_eq!(
        job.kconfig.get("CONFIG_LEGACY_BUG").map(String::as_str),
        Some("n")
    );
    assert_eq!(
        job.kconfig.get("CONFIG_SCALE").map(String::as_str),
        Some("4")
    );
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(
            &job.job_id,
            "entry_kbuild",
            vec![vec![0], vec![3], vec![-2]],
        )
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));

    // Changing .config must invalidate the verified job.
    workspace.write(
        ".config",
        "CONFIG_FAST_PATH=y\n# CONFIG_LEGACY_BUG is not set\nCONFIG_SCALE=5\n",
    );
    assert_eq!(engine.report(&job.job_id).unwrap().state, "stale");
}

#[tokio::test]
async fn unions_bitfields_static_const_strings_and_forward_goto_cleanup_match() {
    let workspace = Workspace::new();
    workspace.write(
        "lowlevel.c",
        r#"
#include <stdint.h>

typedef union {
    int32_t s;
    uint32_t u;
} Word;

struct Flags {
    unsigned int mode : 3;
    int delta : 4;
};

static const int K_LUT[4] = { 3, 7, 11, 19 };
static const int K_BIAS = 5;

static int tag_score(const char *tag) {
    return (int)tag[0] + (int)tag[1];
}

int eval_lowlevel(int x, int step) {
    Word w = { x };
    struct Flags fl = { (unsigned int)x, step };
    fl.mode += 1;
    fl.delta -= 2;
    int acc = (int)(w.u & 0xffu) + (int)fl.mode + fl.delta + K_LUT[fl.mode & 3] + K_BIAS;
    if (x < 0) {
        acc += tag_score("NG");
        goto err_stage2;
    }
    int bonus = x * 2;
    if (step > 5) {
        acc += bonus + tag_score("HI");
        goto err_stage1;
    }
    return acc + bonus + tag_score("OK");
err_stage1:
    acc += 100;
err_stage2:
    return acc - 10;
}
"#,
    );
    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    assert!(!job.constants.is_empty());
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(
            &job.job_id,
            "eval_lowlevel",
            vec![
                vec![0, 0],
                vec![9, 3],
                vec![4, 8],
                vec![-3, -4],
                vec![15, 7],
            ],
        )
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
}

#[tokio::test]
async fn recursive_subdir_kbuild_and_bidirectional_c_rust_abi_calls_match() {
    let workspace = Workspace::new();
    std::fs::create_dir(workspace.source.join("sub")).unwrap();
    std::fs::create_dir(workspace.source.join("disabled_sub")).unwrap();
    std::fs::create_dir(workspace.source.join("ffi")).unwrap();

    workspace.write(".config", "# CONFIG_DISABLED_SUB is not set\n");
    workspace.write(
        "Kbuild",
        "obj-y += core.o sub/\nobj-$(CONFIG_DISABLED_SUB) += disabled_sub/\n",
    );
    workspace.write(
        "sub/Kbuild",
        "ccflags-y += -DSUB_FACTOR=3\nobj-y += sub_math.o\n",
    );
    workspace.write(
        "sub/sub_math.c",
        "int sub_step(int x) { return x * SUB_FACTOR + 1; }\n",
    );
    workspace.write(
        "disabled_sub/bad.c",
        "int bad_global = 9; int bad_fn(int x) { return x + bad_global; }\n",
    );
    workspace.write(
        "ffi/bridge.c",
        "extern int rf_abi_sub_step(int x);\nint c_roundtrip(int x) { return rf_abi_sub_step(x + 2) * 2; }\n",
    );
    workspace.write(
        "core.c",
        "int c_roundtrip(int x);\nint entry_bidir(int x) { return c_roundtrip(x) + 5; }\n",
    );

    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    assert_eq!(job.inactive_source_files, vec!["disabled_sub/bad.c"]);
    assert_eq!(job.c_boundary_files, vec!["ffi/bridge.c"]);
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(&job.job_id, "entry_bidir", vec![vec![-4], vec![0], vec![7]])
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
}

#[tokio::test]
async fn kernel_nostd_profile_and_kbuild_bindings_compile_and_verify() {
    let workspace = Workspace::new();
    workspace.write(".config", "CONFIG_KERNEL_PORT=y\n");
    workspace.write(
        "Kbuild",
        "ccflags-y += -DKERNEL_BIAS=9\nobj-$(CONFIG_KERNEL_PORT) += drv.o\n",
    );
    workspace.write(
        "drv.c",
        r#"
#include <stdint.h>

union Payload {
    int32_t s;
    uint32_t u;
};

struct Packet {
    int32_t tag;
    union Payload data;
    int32_t vec[2];
};

static int mix_tag(int32_t t) {
    return t ^ KERNEL_BIAS;
}

int process_packet(int32_t raw) {
    struct Packet p = { mix_tag(raw), { .s = raw + 3 }, { 1, 2 } };
    return p.tag + (int)(p.data.u & 0x1fu) + p.vec[0] + p.vec[1];
}
"#,
    );

    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    engine.plan(&job.job_id).unwrap();
    let generated = engine.generate(&job.job_id).unwrap();
    assert_eq!(generated.kernel_profile.as_deref(), Some("no_std_kbuild"));
    assert_eq!(generated.kbuild_artifacts.len(), 3);

    let attempt_dir = Path::new(&generated.artifact_path)
        .join("candidates")
        .join(generated.generation_attempt.to_string());
    let nostd_src = std::fs::read_to_string(attempt_dir.join("kbuild/rf_kernel_port.rs")).unwrap();
    let kbuild_src = std::fs::read_to_string(attempt_dir.join("kbuild/Kbuild")).unwrap();
    let bindings_hdr =
        std::fs::read_to_string(attempt_dir.join("kbuild/rf_port_bindings.h")).unwrap();
    assert!(nostd_src.starts_with("#![no_std]\n"));
    assert!(kbuild_src.contains("CONFIG_KERNEL_PORT=y"));
    assert!(kbuild_src.contains("obj-y += rf_kernel_port.o"));
    assert!(bindings_hdr.contains("union Payload;"));
    assert!(bindings_hdr.contains("struct Packet;"));
    assert!(bindings_hdr.contains("union Payload data;"));
    assert!(bindings_hdr.contains("int vec[2];"));
    assert!(bindings_hdr.contains("int rf_abi_process_packet(int raw);"));
    assert!(!bindings_hdr.contains("rf_abi_mix_tag"));

    let verified = engine
        .verify(
            &job.job_id,
            "process_packet",
            vec![vec![-5], vec![0], vec![17]],
        )
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
    let cmds = &verified.verifications[0].commands;
    assert!(cmds.iter().any(|c| {
        c.success()
            && c.program == "rustc"
            && c.args.iter().any(|a| a.contains("rf_kernel_port.rs"))
    }));
    assert!(cmds.iter().any(|c| {
        c.success()
            && c.program == "clang"
            && c.args.iter().any(|a| a.contains("rf_port_bindings.h"))
    }));

    // Tampering with kbuild output must mark the verified job stale.
    std::fs::write(
        attempt_dir.join("kbuild/rf_kernel_port.rs"),
        format!("{nostd_src}\n// tampered\n"),
    )
    .unwrap();
    assert_eq!(engine.report(&job.job_id).unwrap().state, "stale");
}

#[tokio::test]
async fn floating_point_math_structs_and_compound_assignments_match() {
    let workspace = Workspace::new();
    workspace.write(
        "geom.c",
        r#"
#include <math.h>

struct Vec2D {
    double x;
    double y;
    float weight;
};

double eval_vec(int n, float scale) {
    struct Vec2D v = { (double)n + 1.5, 2.0, scale * 0.5f };
    v.x += n;
    v.weight++;
    int bonus = 10;
    bonus += 2.75;
    double norm = sqrt(v.x * v.x + v.y * v.y);
    double mag = fabs((double)v.weight);
    if (v.weight) {
        norm += mag + (double)(int)(norm * 0.25) + (double)bonus;
    }
    return norm;
}
"#,
    );

    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(
            &job.job_id,
            "eval_vec",
            vec![vec![0, 2], vec![3, 4], vec![-5, -6]],
        )
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
}

#[tokio::test]
async fn general_purpose_stdlib_heap_string_qsort_and_cmake_context_match() {
    let workspace = Workspace::new();
    std::fs::create_dir(workspace.source.join("inc")).unwrap();
    std::fs::create_dir(workspace.source.join("sub")).unwrap();

    workspace.write(
        "CMakeLists.txt",
        "cmake_minimum_required(VERSION 3.16)\nproject(app C)\nadd_compile_definitions(CMAKE_SCALE=2)\ninclude_directories(inc)\nadd_subdirectory(sub)\n",
    );
    workspace.write("inc/defs.h", "int sub_adjust(int v);\n");
    workspace.write("sub/Makefile", "CFLAGS += -DSUB_BIAS=5\n");
    workspace.write(
        "sub/helper.c",
        "#include \"defs.h\"\nint sub_adjust(int v) { return v * CMAKE_SCALE + SUB_BIAS; }\n",
    );
    workspace.write(
        "app.c",
        r#"
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include "defs.h"

static int cmp_desc(const void *a, const void *b) {
    int va = *(const int *)a;
    int vb = *(const int *)b;
    return (vb > va) - (vb < va);
}

int run_userspace_lib(int n, int seed) {
    int count = n;
    if (__builtin_expect(count < 4, 0)) count = 4;
    if (count > 16) count = 16;

    int *buf = (int *)calloc((size_t)count, sizeof(int));
    if (!buf) return -1;
    buf = (int *)realloc(buf, (size_t)count * sizeof(int));
    if (!buf) return -2;

    int *mirror = (int *)malloc((size_t)count * sizeof(int));
    if (!mirror) {
        free(buf);
        return -3;
    }
    memset(mirror, 0, (size_t)count * sizeof(int));

    for (int i = 0; i < count; i++) {
        buf[i] = sub_adjust(seed + i * 3);
    }
    memcpy(mirror, buf, (size_t)count * sizeof(int));
    memmove(mirror + 1, mirror, (size_t)(count - 1) * sizeof(int));
    qsort(buf, (size_t)count, sizeof(int), cmp_desc);

    char label[8] = {0};
    memcpy(label, "rf-ok", 6);
    int slen = (int)strlen(label);
    int scmp = strcmp(label, "rf-ok") == 0 ? 10 : -10;
    int mcmp = memcmp(buf, buf, sizeof(int)) == 0 ? 1 : 0;

    uintptr_t paddr = (uintptr_t)buf;
    int low_bits = (int)(paddr & 0x3u);
    void *sentinel = (void *)(uintptr_t)0x10;
    int sentinel_ok = sentinel != NULL ? 1 : 0;

    uint32_t u = (uint32_t)(seed == 0 ? 0x12345678u : (uint32_t)seed);
    int bits = __builtin_popcount(u) + __builtin_clz(u) + __builtin_ctz(u);
    uint32_t swapped = __builtin_bswap32(u);

    int total = buf[0] + mirror[1] + slen + scmp + mcmp + low_bits + sentinel_ok + bits + (int)(swapped & 0xffu);
    free(mirror);
    free(buf);
    return total;
}
"#,
    );

    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    assert!(job.external_functions.iter().any(|e| e.name == "malloc"));
    assert!(job.external_functions.iter().any(|e| e.name == "qsort"));
    assert!(job.external_functions.iter().any(|e| e.name == "free"));
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(
            &job.job_id,
            "run_userspace_lib",
            vec![vec![2, 0], vec![6, 7], vec![10, -13]],
        )
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
}

#[tokio::test]
async fn c2rust_builtins_overflow_string_padding_and_deferred_init_match() {
    let workspace = Workspace::new();
    workspace.write(
        "c2rust_features.c",
        r#"
#include <stdint.h>
#include <stdbool.h>

static const char K_HDR[8] = "HDR";

int eval_builtins(int a, int b) {
    int sum;
    int diff;
    int prod;
    bool ovf_add = __builtin_add_overflow(a, b, &sum);
    bool ovf_sub = __builtin_sub_overflow(a, b, &diff);
    bool ovf_mul = __builtin_mul_overflow(a, b, &prod);

    int delayed;
    if ((a & 1) == 0) {
        delayed = 11;
    } else {
        delayed = 23;
    }

    char padded[12] = "tag";
    char braced[8] = { "ok" };
    char exact[4] = "ABCD";

    uint32_t u = (uint32_t)a;
    uint32_t r = __builtin_rotateleft32(u, 5) ^ __builtin_rotateright32(u, 3);
    int ffs_val = __builtin_ffs(a);
    int parity_val = __builtin_parity(u);
    int const_p = __builtin_constant_p(a);

    double d = (double)a * 0.5;
    int f_checks = __builtin_isnan(d) + __builtin_isfinite(d) + __builtin_signbit(d);

    int acc = delayed
        + (ovf_add ? 1000 : (sum & 0xffff))
        + (ovf_sub ? 2000 : (diff & 0xffff))
        + (ovf_mul ? 3000 : (prod & 0xffff))
        + (int)padded[0] + (int)padded[11]
        + (int)braced[0] + (int)braced[7]
        + (int)exact[0] + (int)exact[3]
        + (int)K_HDR[0] + (int)K_HDR[7]
        + (int)(r & 0xffffu) + ffs_val + parity_val + const_p + f_checks;
    return acc;
}
"#,
    );

    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(
            &job.job_id,
            "eval_builtins",
            vec![
                vec![0, 5],
                vec![7, 13],
                vec![2147483640, 100],
                vec![-2147483640, -100],
                vec![100000, 100000],
            ],
        )
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
}

#[tokio::test]
async fn crown_pointer_contracts_and_safe_rust_wrappers_compile_and_verify() {
    let workspace = Workspace::new();
    std::fs::create_dir(workspace.source.join("sub_inc")).unwrap();
    workspace.write("sub_inc/ops.h", "struct Node { int val; int weight; };\n");
    workspace.write(
        "ptr_ops.c",
        r#"
#include "ops.h"

void scale_node(struct Node *n, const int *bias) {
    if (!n || !bias) return;
    n->val = n->val * n->weight + *bias;
}

int sum_slice(const int *arr, int len) {
    int s = 0;
    for (int i = 0; i < len; i++) {
        s += arr[i];
    }
    return s;
}

int run_contracts(int init_val, int bias) {
    struct Node node = { init_val, 3 };
    scale_node(&node, &bias);
    int items[3] = { node.val, node.weight, bias };
    return sum_slice(items, 3);
}
"#,
    );

    let engine = workspace.engine();
    let job = engine.analyze(&workspace.source).await.unwrap();
    assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);

    let scale_fn = job
        .functions
        .iter()
        .find(|f| f.name == "scale_node")
        .unwrap();
    assert_eq!(scale_fn.pointer_contracts.len(), 2);
    assert_eq!(scale_fn.pointer_contracts[0].access, "read_write");
    assert_eq!(scale_fn.pointer_contracts[0].kind, "ref");
    assert_eq!(
        scale_fn.pointer_contracts[0].safe_rust_type,
        "&mut RfStruct_Node"
    );
    assert_eq!(scale_fn.pointer_contracts[1].access, "read_only");
    assert_eq!(scale_fn.pointer_contracts[1].kind, "ref");
    assert_eq!(scale_fn.pointer_contracts[1].safe_rust_type, "&i32");
    let wrapper = scale_fn.safe_rust_wrapper.as_deref().unwrap();
    assert!(wrapper.contains("pub fn rf_safe_scale_node"));

    let sum_fn = job
        .functions
        .iter()
        .find(|f| f.name == "sum_slice")
        .unwrap();
    assert_eq!(sum_fn.pointer_contracts.len(), 1);
    assert_eq!(sum_fn.pointer_contracts[0].access, "read_only");
    assert_eq!(sum_fn.pointer_contracts[0].kind, "slice_or_offset");
    assert!(sum_fn.safe_rust_wrapper.is_none());

    engine.plan(&job.job_id).unwrap();
    engine.generate(&job.job_id).unwrap();
    let verified = engine
        .verify(
            &job.job_id,
            "run_contracts",
            vec![vec![-4, 2], vec![0, 5], vec![9, -3]],
        )
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{}", describe(&verified));
}
