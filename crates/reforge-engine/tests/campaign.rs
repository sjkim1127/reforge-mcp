use reforge_engine::campaign::{CampaignService, model::*};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

struct Setup {
    _temp: TempDir,
    data: PathBuf,
    source: PathBuf,
    rust: PathBuf,
    raw: PathBuf,
}
impl Setup {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let data = temp.path().join("data");
        let source = temp.path().join("source");
        let rust = temp.path().join("rust");
        let raw = temp.path().join("raw");
        for path in [&source, &rust, &raw] {
            std::fs::create_dir_all(path.join("target")).unwrap();
        }
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/migration");
        std::fs::copy(
            fixtures.join("strtol-source/probe.c"),
            source.join("probe.c"),
        )
        .unwrap();
        std::fs::write(source.join("pom.xml"),"<project><dependencies><dependency><artifactId>demo</artifactId></dependency></dependencies></project>\n").unwrap();
        std::fs::write(source.join("App.java"), "class App {}\n").unwrap();
        build(
            "clang",
            &[
                source.join("probe.c").to_str().unwrap(),
                "-o",
                source.join("probe").to_str().unwrap(),
            ],
        );
        std::fs::copy(
            fixtures.join("strtol-rust/src/main.rs"),
            rust.join("main.rs"),
        )
        .unwrap();
        build(
            "rustc",
            &[
                "--edition=2024",
                rust.join("main.rs").to_str().unwrap(),
                "-o",
                rust.join("target/probe").to_str().unwrap(),
            ],
        );
        std::fs::write(raw.join("main.rs"),"fn main(){let x=std::env::args().nth(1).unwrap();match x.parse::<i32>(){Ok(v)=>println!(\"{{\\\"value\\\":{v}}}\"),Err(_)=>println!(\"{{\\\"error\\\":\\\"invalid\\\"}}\")}}\n").unwrap();
        build(
            "rustc",
            &[
                "--edition=2024",
                raw.join("main.rs").to_str().unwrap(),
                "-o",
                raw.join("target/probe").to_str().unwrap(),
            ],
        );
        Self {
            _temp: temp,
            data,
            source,
            rust,
            raw,
        }
    }
    fn service(&self) -> CampaignService {
        CampaignService::new(&self.data).unwrap()
    }
    fn requirement(&self) -> RequirementSpec {
        RequirementSpec { ecosystem:"c".into(),package:"libc strtol usage".into(),version:"C11 API; ambient host runtime".into(),used_apis:vec!["strtol".into()],usage_refs:vec![UsageRef{file:"probe.c".into(),line_start:8,line_end:11}],unknown_usage:vec![],contracts:vec![Contract{id:"decimal_int32".into(),description:"whole-string decimal int32, leading ASCII whitespace/sign, distinct invalid/range errors".into(),mandatory:true}] }
    }
    fn candidate(&self, root: &Path) -> CandidateSpec {
        let version = String::from_utf8(
            Command::new("rustc")
                .arg("--version")
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        CandidateSpec {
            package: "Rust std + caller adapter".into(),
            version: version.trim().into(),
            strategy: Strategy::Adapter,
            source_path: root.display().to_string(),
            research_urls: vec![
                "https://doc.rust-lang.org/std/primitive.i32.html#method.from_str".into(),
            ],
            license: "MIT OR Apache-2.0 (agent assertion)".into(),
            target: "host native".into(),
            api_mapping: BTreeMap::from([(
                "strtol".into(),
                "str::parse::<i32> with lexical adapter".into(),
            )]),
            known_gaps: vec![],
            adapter_tasks: vec!["preserve whitespace and error precedence".into()],
            dependency_campaign_id: None,
        }
    }
    fn probes(&self, root: &Path) -> Vec<ContractProbe> {
        vec![ContractProbe {
            contract_id: "decimal_int32".into(),
            source: ProbeCommand {
                program: self
                    .source
                    .join("probe")
                    .canonicalize()
                    .unwrap()
                    .display()
                    .to_string(),
                args: vec![],
            },
            replacement: ProbeCommand {
                program: root
                    .join("target/probe")
                    .canonicalize()
                    .unwrap()
                    .display()
                    .to_string(),
                args: vec![],
            },
            cases: [
                "42",
                " +42",
                "\x0b-7",
                "12x",
                "2147483648",
                "",
                "-2147483648",
                "999999999999999999999x",
            ]
            .iter()
            .map(|s| vec![(*s).into()])
            .collect(),
        }]
    }
}
fn build(program: &str, args: &[&str]) {
    let output = Command::new(program).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn agent_candidate_mismatch_then_adapter_acceptance_survives_restart() {
    let setup = Setup::new();
    let service = setup.service();
    let campaign = service
        .inventory_project("native and Java inventory", &setup.source)
        .unwrap();
    assert_eq!(campaign.languages["c"], 1);
    assert_eq!(campaign.languages["java"], 1);
    assert!(campaign.manifests.contains(&"pom.xml".into()));
    let page = service.get_inventory(&campaign.campaign_id, 0, 1).unwrap();
    assert!(page["next_offset"].is_number());
    let context = service
        .get_source_context(&campaign.campaign_id, "pom.xml", 0, 12)
        .unwrap();
    assert_eq!(context["text"], "<project><de");
    let req = service
        .register_requirement(&campaign.campaign_id, setup.requirement())
        .unwrap();
    let tasks = service.next_tasks(&campaign.campaign_id, 0, 100).unwrap();
    assert_eq!(
        tasks["tasks"][0]["action"],
        "search_rust_candidates_or_port_dependency"
    );
    let raw = service
        .record_candidate(&req.requirement_id, setup.candidate(&setup.raw))
        .unwrap();
    let failed = service
        .verify(&raw.candidate_id, setup.probes(&setup.raw))
        .await
        .unwrap();
    assert_eq!(failed.status, "mismatch");
    assert!(failed.cases.iter().any(|c| !c.matched));
    let fixed = service
        .record_candidate(&req.requirement_id, setup.candidate(&setup.rust))
        .unwrap();
    assert_eq!(
        service
            .get_requirements(&campaign.campaign_id, 0, 100)
            .unwrap()["requirements"][0]["state"],
        "unresolved"
    );
    let evidence = service
        .verify(&fixed.candidate_id, setup.probes(&setup.rust))
        .await
        .unwrap();
    assert_eq!(evidence.status, "passed", "{:?}", evidence.last_error);
    assert_eq!(evidence.cases.len(), 8);
    let resumed = setup.service();
    let view = resumed
        .get_requirements(&campaign.campaign_id, 0, 100)
        .unwrap();
    assert_eq!(view["requirements"][0]["state"], "accepted_fixture_scope");
    assert_eq!(
        resumed.next_tasks(&campaign.campaign_id, 0, 100).unwrap()["tasks"][0]["action"],
        "continue_program_conversion_and_integration"
    );
    assert_eq!(
        resumed.report(&campaign.campaign_id).unwrap()["program_migration_completed"],
        false
    );
}

#[tokio::test]
async fn unknown_usage_missing_contracts_and_revision_invalidate_acceptance() {
    let setup = Setup::new();
    let service = setup.service();
    let campaign = service
        .inventory_project("contracts", &setup.source)
        .unwrap();
    let mut spec = setup.requirement();
    spec.unknown_usage.push("plugins".into());
    let req = service
        .register_requirement(&campaign.campaign_id, spec)
        .unwrap();
    let candidate = service
        .record_candidate(&req.requirement_id, setup.candidate(&setup.rust))
        .unwrap();
    assert!(
        service
            .verify(&candidate.candidate_id, setup.probes(&setup.rust))
            .await
            .is_err()
    );
    let revised = service
        .revise_requirement(&req.requirement_id, setup.requirement())
        .unwrap();
    assert_ne!(req.fingerprint, revised.fingerprint);
    assert!(
        service
            .verify(&candidate.candidate_id, vec![])
            .await
            .is_err()
    );
    assert_eq!(
        service
            .verify(&candidate.candidate_id, setup.probes(&setup.rust))
            .await
            .unwrap()
            .status,
        "passed"
    );
    let mut changed = setup.requirement();
    changed.contracts[0]
        .description
        .push_str("; revised oracle required");
    service
        .revise_requirement(&req.requirement_id, changed)
        .unwrap();
    assert_ne!(
        service
            .get_requirements(&campaign.campaign_id, 0, 100)
            .unwrap()["requirements"][0]["state"],
        "accepted_fixture_scope"
    );
    assert_eq!(
        service
            .verify(&candidate.candidate_id, setup.probes(&setup.rust))
            .await
            .unwrap()
            .status,
        "passed"
    );
}

#[tokio::test]
async fn fingerprints_cover_source_candidate_and_executed_artifact() {
    let setup = Setup::new();
    let service = setup.service();
    let campaign = service
        .inventory_project("freshness", &setup.source)
        .unwrap();
    let req = service
        .register_requirement(&campaign.campaign_id, setup.requirement())
        .unwrap();
    let candidate = service
        .record_candidate(&req.requirement_id, setup.candidate(&setup.rust))
        .unwrap();
    service
        .verify(&candidate.candidate_id, setup.probes(&setup.rust))
        .await
        .unwrap();
    std::fs::write(setup.rust.join("target/probe"), b"changed executable").unwrap();
    assert_ne!(
        service
            .get_requirements(&campaign.campaign_id, 0, 100)
            .unwrap()["requirements"][0]["state"],
        "accepted_fixture_scope"
    );
    std::fs::write(setup.rust.join("main.rs"), "fn main(){}\n").unwrap();
    assert!(
        service
            .verify(&candidate.candidate_id, setup.probes(&setup.rust))
            .await
            .is_err()
    );
    std::fs::write(setup.source.join("App.java"), "class App { int value; }\n").unwrap();
    assert_eq!(
        service.report(&campaign.campaign_id).unwrap()["state"],
        "stale"
    );
}

#[test]
fn child_campaign_links_guard_cycles_and_invalid_source_references() {
    let setup = Setup::new();
    let service = setup.service();
    let campaign = service
        .inventory_project("children", &setup.source)
        .unwrap();
    let req = service
        .register_requirement(&campaign.campaign_id, setup.requirement())
        .unwrap();
    assert!(
        service
            .create_dependency_campaign(&req.requirement_id, "cycle", &setup.source)
            .is_err()
    );
    let child = service
        .create_dependency_campaign(&req.requirement_id, "port libc usage", &setup.raw)
        .unwrap();
    assert_eq!(
        child.parent_requirement_id.as_deref(),
        Some(req.requirement_id.as_str())
    );
    let mut candidate = setup.candidate(&setup.rust);
    candidate.strategy = Strategy::PortDependency;
    candidate.dependency_campaign_id = Some(child.campaign_id);
    service
        .record_candidate(&req.requirement_id, candidate)
        .unwrap();
    let mut invalid = setup.requirement();
    invalid.usage_refs[0].line_end = 999;
    assert!(
        service
            .register_requirement(&campaign.campaign_id, invalid)
            .is_err()
    );
    assert!(
        service
            .get_source_context(&campaign.campaign_id, "../escape", 0, 1)
            .is_err()
    );
    assert!(
        service
            .get_inventory(&campaign.campaign_id, 0, 101)
            .is_err()
    );
}

#[tokio::test]
async fn temporary_ffi_and_metadata_never_become_native_program_completion() {
    let setup = Setup::new();
    let service = setup.service();
    let campaign = service.inventory_project("FFI", &setup.source).unwrap();
    let req = service
        .register_requirement(&campaign.campaign_id, setup.requirement())
        .unwrap();
    let mut spec = setup.candidate(&setup.rust);
    spec.known_gaps.push("unimplemented".into());
    let bad = service.record_candidate(&req.requirement_id, spec).unwrap();
    assert!(
        service
            .verify(&bad.candidate_id, setup.probes(&setup.rust))
            .await
            .is_err()
    );
    let mut spec = setup.candidate(&setup.rust);
    spec.strategy = Strategy::TemporaryFfi;
    let ffi = service.record_candidate(&req.requirement_id, spec).unwrap();
    service
        .verify(&ffi.candidate_id, setup.probes(&setup.rust))
        .await
        .unwrap();
    let view = service
        .get_requirements(&campaign.campaign_id, 0, 100)
        .unwrap();
    assert_eq!(view["requirements"][0]["state"], "verified_temporary_ffi");
    assert_eq!(
        view["requirements"][0]["candidates"][1]["native_implementation_claimed"],
        false
    );
    assert_eq!(
        service.report(&campaign.campaign_id).unwrap()["program_migration_completed"],
        false
    );
}

#[tokio::test]
async fn cancelled_verification_is_interrupted_and_releases_campaign_lock() {
    let setup = Setup::new();
    // Prepare the slow replacement before pinning its source. Cancellation must
    // leave durable evidence without accepting an unobserved result.
    std::fs::write(setup.rust.join("main.rs"), "fn main(){std::thread::sleep(std::time::Duration::from_secs(5));println!(\"{{\\\"value\\\":42}}\");}").unwrap();
    build(
        "rustc",
        &[
            setup.rust.join("main.rs").to_str().unwrap(),
            "-o",
            setup.rust.join("target/probe").to_str().unwrap(),
        ],
    );
    let service = setup.service();
    let campaign = service
        .inventory_project("cancelled", &setup.source)
        .unwrap();
    let req = service
        .register_requirement(&campaign.campaign_id, setup.requirement())
        .unwrap();
    let candidate = service
        .record_candidate(&req.requirement_id, setup.candidate(&setup.rust))
        .unwrap();
    assert_eq!(
        service.plan_adapter(&candidate.candidate_id).unwrap()["acceptance_granted"],
        false
    );
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        service.verify(&candidate.candidate_id, setup.probes(&setup.rust)),
    )
    .await;
    assert!(result.is_err());
    let view = setup
        .service()
        .get_requirements(&campaign.campaign_id, 0, 100)
        .unwrap();
    assert_eq!(view["requirements"][0]["state"], "unresolved");
    assert_eq!(
        view["requirements"][0]["candidates"][0]["attempts"][0]["status"],
        "interrupted"
    );
}

#[tokio::test]
async fn every_mandatory_contract_needs_cases_and_empty_source_observations_fail() {
    let setup = Setup::new();
    std::fs::write(
        setup.source.join("empty.c"),
        "#include <stdio.h>\nint main(void){puts(\"{}\");return 0;}\n",
    )
    .unwrap();
    build(
        "clang",
        &[
            setup.source.join("empty.c").to_str().unwrap(),
            "-o",
            setup.source.join("empty").to_str().unwrap(),
        ],
    );
    let service = setup.service();
    let campaign = service
        .inventory_project("coverage", &setup.source)
        .unwrap();
    let mut spec = setup.requirement();
    spec.contracts.push(Contract {
        id: "errors".into(),
        description: "error outcomes".into(),
        mandatory: true,
    });
    let req = service
        .register_requirement(&campaign.campaign_id, spec)
        .unwrap();
    let candidate = service
        .record_candidate(&req.requirement_id, setup.candidate(&setup.rust))
        .unwrap();
    let mut probes = setup.probes(&setup.rust);
    assert!(
        service
            .verify(&candidate.candidate_id, probes.clone())
            .await
            .is_err()
    );
    let mut errors = probes[0].clone();
    errors.contract_id = "errors".into();
    probes.push(errors);
    assert_eq!(
        service
            .verify(&candidate.candidate_id, probes.clone())
            .await
            .unwrap()
            .status,
        "passed"
    );
    probes[0].source.program = setup.source.join("empty").display().to_string();
    let evidence = service
        .verify(&candidate.candidate_id, probes)
        .await
        .unwrap();
    assert_eq!(evidence.status, "source_failed");
    assert!(evidence.cases[0].replacement.is_none());
    assert_eq!(
        service
            .get_requirements(&campaign.campaign_id, 0, 100)
            .unwrap()["requirements"][0]["state"],
        "unresolved"
    );
}
