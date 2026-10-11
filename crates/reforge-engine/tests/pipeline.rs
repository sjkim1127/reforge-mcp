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
async fn rejects_unsupported_semantics_and_external_calls() {
    let sources = [
        "int f(int *p) { return *p; }",
        "unsigned f(unsigned a) { return a+1; }",
        "int global=1; int f(int x) { return x+global; }",
        "int f(int x) { while(x>0) x=x-1; return x; }",
        "#define ONE 1\nint f(int x) { return x+ONE; }",
        "int external(int); int f(int x) { return external(x); }",
        "int f(int x) { int value; return value+x; }",
        "int f(int x) { static int count=1; return x+count; }",
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
