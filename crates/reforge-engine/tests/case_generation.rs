use reforge_engine::model::{FixtureValue, Job};
use reforge_engine::{CaseGenerationOptions, Engine};
use std::collections::BTreeSet;
use std::path::PathBuf;
use tempfile::TempDir;

struct Workspace {
    _temp: TempDir,
    source: PathBuf,
    data: PathBuf,
}

impl Workspace {
    fn with(file: &str, source: &str) -> Self {
        let temp = TempDir::new().unwrap();
        let input = temp.path().join("input");
        let data = temp.path().join("data");
        std::fs::create_dir(&input).unwrap();
        std::fs::write(input.join(file), source).unwrap();
        Self {
            _temp: temp,
            source: input,
            data,
        }
    }

    async fn generated_job(&self) -> (Engine, Job) {
        let engine = Engine::new(&self.data).unwrap();
        let job = engine.analyze(&self.source).await.unwrap();
        assert_eq!(job.state, "analyzed", "{:?}", job.unsupported);
        engine.plan(&job.job_id).unwrap();
        engine.generate(&job.job_id).unwrap();
        (engine, job)
    }
}

fn options() -> CaseGenerationOptions {
    CaseGenerationOptions {
        max_executions: 5_000,
        time_budget_ms: 20_000,
        ..CaseGenerationOptions::default()
    }
}

fn outputs(job: &Job) -> BTreeSet<String> {
    job.verifications
        .last()
        .unwrap()
        .cases
        .iter()
        .map(|case| case.source.stdout.trim().to_string())
        .collect()
}

#[tokio::test]
async fn generated_cases_reach_magic_constants_and_verify() {
    let workspace = Workspace::with(
        "classify.c",
        "int classify(int x, int y) {\n    if (x == 4242) return 1;\n    if (x > 1000 && y < -5) return 2;\n    if (y == 77) return 3;\n    return 0;\n}\n",
    );
    let (engine, job) = workspace.generated_job().await;
    let generation = engine
        .generate_cases(&job.job_id, "classify", options())
        .await
        .unwrap();
    assert_eq!(generation.status, "completed", "{:?}", generation.error);
    assert!(!generation.cases.is_empty());
    assert!(generation.undefined_behavior.is_empty());
    assert!(generation.stats["covered_edges"] > 0);

    let cases = engine.generated_cases(&job.job_id, "classify").unwrap();
    assert_eq!(cases, generation.cases);
    let verified = engine
        .verify_values(&job.job_id, "classify", cases)
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{:?}", verified.last_error);
    let seen = outputs(&verified);
    for expected in ["0", "1", "2", "3"] {
        assert!(
            seen.contains(expected),
            "missing branch {expected}: {seen:?}"
        );
    }

    let report = engine.report(&job.job_id).unwrap();
    assert_eq!(report.case_generations.len(), 1);
}

#[tokio::test]
async fn source_undefined_behavior_is_reported_and_excluded() {
    let workspace = Workspace::with(
        "bump.c",
        "int bump(int x) {\n    if (x > 100) return x + 1;\n    return -x;\n}\n",
    );
    let (engine, job) = workspace.generated_job().await;
    let generation = engine
        .generate_cases(&job.job_id, "bump", options())
        .await
        .unwrap();
    assert_eq!(generation.status, "completed", "{:?}", generation.error);
    assert!(
        generation.stats["undefined_behavior"] > 0,
        "{:?}",
        generation.stats
    );
    let ub: Vec<_> = generation
        .undefined_behavior
        .iter()
        .map(|case| case[0])
        .collect();
    assert!(
        ub.contains(&FixtureValue::Int(i32::MAX.into()))
            || ub.contains(&FixtureValue::Int(i32::MIN.into())),
        "{ub:?}"
    );
    for case in &generation.cases {
        assert_ne!(case[0], FixtureValue::Int(i32::MAX.into()));
        assert_ne!(case[0], FixtureValue::Int(i32::MIN.into()));
    }
    let verified = engine
        .verify_values(&job.job_id, "bump", generation.cases.clone())
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{:?}", verified.last_error);
}

#[tokio::test]
async fn typed_fixtures_cover_wide_integers_and_floats() {
    let workspace = Workspace::with(
        "typed.c",
        "unsigned long long mix(unsigned long long a, long long b) { return a ^ (unsigned long long)b; }\n\
         double scale(double x, float y) { return x * 0.5 + y; }\n",
    );
    let (engine, job) = workspace.generated_job().await;
    let verified = engine
        .verify_values(
            &job.job_id,
            "mix",
            vec![
                vec![FixtureValue::from(u64::MAX), FixtureValue::Int(i64::MIN)],
                vec![FixtureValue::Int(1 << 40), FixtureValue::Int(-1)],
            ],
        )
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{:?}", verified.last_error);
    assert_eq!(
        verified.verifications[0].cases[0].source.stdout.trim(),
        (u64::MAX ^ (i64::MIN as u64)).to_string()
    );

    let verified = engine
        .verify_values(
            &job.job_id,
            "scale",
            vec![
                vec![FixtureValue::Float(0.1), FixtureValue::Float(2.5)],
                vec![FixtureValue::Float(-1e300), FixtureValue::Int(3)],
            ],
        )
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{:?}", verified.last_error);
    assert_eq!(
        verified.verifications[1].cases[0].source.stdout.trim(),
        "2.550000"
    );

    let rejected = engine
        .verify_values(
            &job.job_id,
            "mix",
            vec![vec![FixtureValue::Float(1.5), FixtureValue::Int(0)]],
        )
        .await;
    assert!(
        rejected
            .unwrap_err()
            .to_string()
            .contains("requires an integer")
    );

    let generation = engine
        .generate_cases(&job.job_id, "scale", options())
        .await
        .unwrap();
    assert_eq!(generation.status, "completed", "{:?}", generation.error);
    assert!(
        generation
            .cases
            .iter()
            .flatten()
            .any(|v| matches!(v, FixtureValue::Float(f) if f.fract() != 0.0))
    );
    let verified = engine
        .verify_values(&job.job_id, "scale", generation.cases)
        .await
        .unwrap();
    assert_eq!(verified.state, "completed", "{:?}", verified.last_error);
}

#[tokio::test]
async fn generation_rejects_bad_options_and_unknown_entries() {
    let workspace = Workspace::with("one.c", "int one(int x) { return x; }\n");
    let (engine, job) = workspace.generated_job().await;
    let bad = CaseGenerationOptions {
        max_cases: 0,
        ..options()
    };
    assert!(
        engine
            .generate_cases(&job.job_id, "one", bad)
            .await
            .is_err()
    );
    assert!(
        engine
            .generate_cases(&job.job_id, "missing", options())
            .await
            .is_err()
    );
    assert!(engine.generated_cases(&job.job_id, "one").is_err());
    assert!(
        engine
            .report(&job.job_id)
            .unwrap()
            .case_generations
            .is_empty()
    );
}
