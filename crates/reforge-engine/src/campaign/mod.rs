//! Agent-authored dependency requirements with server-executed, fixture-scoped evidence.
mod inventory;
pub mod model;
mod store;

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use model::*;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use store::Ledger;

const SCOPE: &str = "Agent-authored API usage and JSON observation fixtures only. Research metadata is not authenticated by the server. Not proof of complete API equivalence or whole-program migration. Local execution limits are not an OS sandbox.";

pub struct CampaignService {
    ledger: Ledger,
}
impl CampaignService {
    pub fn new(root: &Path) -> Result<Self> {
        Ok(Self {
            ledger: Ledger::new(root)?,
        })
    }
    pub fn from_env() -> Result<Self> {
        let root = std::env::var_os("REFORGE_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::cache_dir()
                    .unwrap_or_else(std::env::temp_dir)
                    .join("reforge-mcp")
            });
        Self::new(&root)
    }
    pub fn inventory_project(&self, name: &str, path: &Path) -> Result<Campaign> {
        self.create(name, path, None)
    }
    fn create(&self, name: &str, path: &Path, parent: Option<&str>) -> Result<Campaign> {
        ensure!(
            !name.trim().is_empty() && name.len() <= 512,
            "campaign name must be 1-512 bytes"
        );
        let root = path.canonicalize()?;
        ensure!(
            !self.ledger.root.starts_with(&root),
            "data directory must be outside the inventoried project"
        );
        if let Some(parent) = parent {
            let requirement: Requirement = self.ledger.load(parent, "requirement")?;
            let mut campaign: Campaign = self.ledger.load(&requirement.campaign_id, "campaign")?;
            for depth in 0..32 {
                ensure!(
                    Path::new(&campaign.source_path) != root,
                    "recursive source project cycle"
                );
                let Some(next) = &campaign.parent_requirement_id else {
                    break;
                };
                ensure!(depth < 31, "dependency campaign depth exceeds 32");
                let req: Requirement = self.ledger.load(next, "requirement")?;
                campaign = self.ledger.load(&req.campaign_id, "campaign")?;
            }
        }
        let files = inventory::scan(&root)?;
        let mut languages = BTreeMap::new();
        let mut manifests = Vec::new();
        for file in &files {
            *languages.entry(file.language.clone()).or_insert(0) += 1;
            if file.manifest {
                manifests.push(file.path.clone());
            }
        }
        let campaign=Campaign {schema_version:1,campaign_id:new_id("migration"),name:name.into(),parent_requirement_id:parent.map(str::to_owned),source_path:root.display().to_string(),source_fingerprint:inventory::fingerprint(&files)?,file_count:files.len(),languages,manifests,limitations:vec!["Manifest discovery and file hashing only; no automatic Maven/Gradle/package resolution or source API-use analysis".into(),"Ignores .git, target, node_modules, .gradle, .venv and __pycache__; excluded files are not part of the source fingerprint".into(),"Inventory budgets: 100000 files, 4 GiB total, 512 MiB/file, 120 seconds. Budget exhaustion is an error, not a complete inventory".into()]};
        std::fs::create_dir_all(self.directory(&campaign.campaign_id)?)?;
        self.ledger.save(
            &campaign.campaign_id,
            &campaign.campaign_id,
            parent.unwrap_or(""),
            "campaign",
            &campaign,
            Some(&files),
        )?;
        Ok(campaign)
    }
    pub fn create_dependency_campaign(
        &self,
        requirement_id: &str,
        name: &str,
        path: &Path,
    ) -> Result<Campaign> {
        let requirement: Requirement = self.ledger.load(requirement_id, "requirement")?;
        let _lock = self.lock(&requirement.campaign_id)?;
        let campaign: Campaign = self.ledger.load(&requirement.campaign_id, "campaign")?;
        self.fresh_campaign(&campaign)?;
        self.create(name, path, Some(requirement_id))
    }
    pub fn get_inventory(&self, id: &str, offset: usize, limit: usize) -> Result<Value> {
        page(limit)?;
        let _lock = self.lock(id)?;
        let campaign: Campaign = self.ledger.load(id, "campaign")?;
        let fresh = self.fresh_campaign(&campaign);
        let files = self.ledger.files(id, offset, limit)?;
        Ok(
            json!({"campaign":campaign,"state":if fresh.is_ok(){"inventory_ready"}else{"stale"},"freshness_error":fresh.err().map(|e|e.to_string()),"files":files,"offset":offset,"total":campaign.file_count,"next_offset":if offset+files.len()<campaign.file_count{Some(offset+files.len())}else{None},"program_migration_completed":false}),
        )
    }
    pub fn get_source_context(
        &self,
        id: &str,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Value> {
        ensure!(
            limit > 0 && limit <= 65536,
            "context limit must be 1-65536 bytes"
        );
        let _lock = self.lock(id)?;
        let campaign: Campaign = self.ledger.load(id, "campaign")?;
        self.fresh_campaign(&campaign)?;
        let file = self.ledger.file(id, path)?;
        ensure!(
            offset as u64 <= file.bytes,
            "context offset exceeds file size"
        );
        use std::io::{Read, Seek};
        let mut input = std::fs::File::open(Path::new(&campaign.source_path).join(&file.path))?;
        input.seek(std::io::SeekFrom::Start(offset as u64))?;
        let mut bytes = Vec::new();
        input.take(limit as u64).read_to_end(&mut bytes)?;
        Ok(
            json!({"file":file,"offset":offset,"text":String::from_utf8_lossy(&bytes),"encoding":"UTF-8 lossy byte slice","next_offset":if offset+bytes.len()<file.bytes as usize{Some(offset+bytes.len())}else{None}}),
        )
    }
    pub fn register_requirement(&self, id: &str, spec: RequirementSpec) -> Result<Requirement> {
        let _lock = self.lock(id)?;
        let campaign: Campaign = self.ledger.load(id, "campaign")?;
        self.fresh_campaign(&campaign)?;
        self.validate_requirement(id, &spec)?;
        let requirement = Requirement {
            requirement_id: new_id("requirement"),
            campaign_id: id.into(),
            revision: 1,
            fingerprint: digest(
                &json!({"source":campaign.source_fingerprint,"revision":1,"spec":spec}),
            )?,
            spec,
        };
        self.ledger.save(
            &requirement.requirement_id,
            id,
            id,
            "requirement",
            &requirement,
            None,
        )?;
        Ok(requirement)
    }
    pub fn revise_requirement(&self, id: &str, spec: RequirementSpec) -> Result<Requirement> {
        let locator: Requirement = self.ledger.load(id, "requirement")?;
        let _lock = self.lock(&locator.campaign_id)?;
        let mut requirement: Requirement = self.ledger.load(id, "requirement")?;
        let campaign: Campaign = self.ledger.load(&requirement.campaign_id, "campaign")?;
        self.fresh_campaign(&campaign)?;
        self.validate_requirement(&requirement.campaign_id, &spec)?;
        requirement.revision += 1;
        requirement.fingerprint = digest(
            &json!({"source":campaign.source_fingerprint,"revision":requirement.revision,"spec":spec}),
        )?;
        requirement.spec = spec;
        self.ledger.save(
            id,
            &requirement.campaign_id,
            &requirement.campaign_id,
            "requirement",
            &requirement,
            None,
        )?;
        Ok(requirement)
    }
    fn validate_requirement(&self, campaign: &str, spec: &RequirementSpec) -> Result<()> {
        bounded(spec)?;
        ensure!(
            !spec.ecosystem.trim().is_empty()
                && !spec.package.trim().is_empty()
                && !spec.version.trim().is_empty(),
            "source ecosystem/package/version required"
        );
        unique(&spec.used_apis)?;
        ensure!(
            !spec.usage_refs.is_empty(),
            "source usage references required"
        );
        let mut ids = BTreeSet::new();
        ensure!(
            spec.contracts.iter().any(|c| c.mandatory),
            "at least one mandatory contract required"
        );
        for contract in &spec.contracts {
            ensure!(
                !contract.id.trim().is_empty()
                    && !contract.description.trim().is_empty()
                    && ids.insert(&contract.id),
                "contracts must have unique nonempty ids/descriptions"
            );
        }
        let project: Campaign = self.ledger.load(campaign, "campaign")?;
        for reference in &spec.usage_refs {
            let file = self.ledger.file(campaign, &reference.file)?;
            ensure!(
                file.language != "other" || file.manifest,
                "usage reference must point to source or a build manifest"
            );
            ensure!(
                reference.line_start > 0 && reference.line_end >= reference.line_start,
                "invalid source line range"
            );
            let count = line_count(&Path::new(&project.source_path).join(&file.path))?;
            ensure!(
                reference.line_end <= count,
                "source reference line exceeds file"
            );
        }
        Ok(())
    }
    pub fn record_candidate(&self, requirement_id: &str, spec: CandidateSpec) -> Result<Candidate> {
        bounded(&spec)?;
        let requirement: Requirement = self.ledger.load(requirement_id, "requirement")?;
        let _lock = self.lock(&requirement.campaign_id)?;
        let campaign: Campaign = self.ledger.load(&requirement.campaign_id, "campaign")?;
        self.fresh_campaign(&campaign)?;
        ensure!(
            self.ledger.count(requirement_id, "candidate")? < 100,
            "candidate budget (100 per requirement) reached"
        );
        ensure!(
            !spec.package.trim().is_empty()
                && !spec.version.trim().is_empty()
                && !spec.license.trim().is_empty()
                && !spec.target.trim().is_empty(),
            "candidate package/version/license/target required"
        );
        ensure!(
            !spec.research_urls.is_empty()
                && spec.research_urls.iter().all(|u| u.starts_with("https://")),
            "at least one HTTPS research URL required; URLs are recorded, not fetched"
        );
        let path = Path::new(&spec.source_path).canonicalize()?;
        let original = Path::new(&campaign.source_path);
        ensure!(
            !path.starts_with(original) && !original.starts_with(&path),
            "replacement project must be separate from source project"
        );
        ensure!(
            !self.ledger.root.starts_with(&path),
            "data directory must be outside replacement project"
        );
        if spec.strategy == Strategy::PortDependency {
            let child: Campaign = self.ledger.load(
                spec.dependency_campaign_id
                    .as_deref()
                    .context("port_dependency requires child campaign")?,
                "campaign",
            )?;
            ensure!(
                child.parent_requirement_id.as_deref() == Some(requirement_id),
                "child campaign belongs to another requirement"
            );
        }
        let files = inventory::scan(&path)?;
        ensure!(
            files.iter().any(|f| f.language == "rust"),
            "replacement inventory must contain Rust source"
        );
        let mut spec = spec;
        spec.source_path = path.display().to_string();
        let candidate = Candidate {
            candidate_id: new_id("candidate"),
            requirement_id: requirement_id.into(),
            campaign_id: requirement.campaign_id.clone(),
            source_fingerprint: inventory::fingerprint(&files)?,
            file_count: files.len(),
            spec,
        };
        self.ledger.save(
            &candidate.candidate_id,
            &candidate.campaign_id,
            requirement_id,
            "candidate",
            &candidate,
            Some(&files),
        )?;
        Ok(candidate)
    }
    pub fn plan_adapter(&self, id: &str) -> Result<Value> {
        let candidate: Candidate = self.ledger.load(id, "candidate")?;
        let _lock = self.lock(&candidate.campaign_id)?;
        let campaign: Campaign = self.ledger.load(&candidate.campaign_id, "campaign")?;
        let requirement: Requirement =
            self.ledger.load(&candidate.requirement_id, "requirement")?;
        self.fresh_campaign(&campaign)?;
        self.fresh_candidate(&candidate)?;
        let unmapped: Vec<_> = requirement
            .spec
            .used_apis
            .iter()
            .filter(|api| {
                !candidate
                    .spec
                    .api_mapping
                    .get(*api)
                    .is_some_and(|v| !v.trim().is_empty())
            })
            .collect();
        let plan = json!({"candidate_id": id, "requirement_fingerprint": requirement.fingerprint,
            "strategy": candidate.spec.strategy, "api_mapping": candidate.spec.api_mapping,
            "unmapped_apis": unmapped, "known_gaps": candidate.spec.known_gaps,
            "adapter_tasks": candidate.spec.adapter_tasks, "unknown_usage": requirement.spec.unknown_usage,
            "required_contracts": requirement.spec.contracts, "source_usage_refs": requirement.spec.usage_refs,
            "next_actions": ["Inspect candidate source and implement/review adapter", "Record a new candidate if source or gap metadata changes", "Prepare project-local source/replacement probe executables", "Submit fixtures for every mandatory contract to verify_dependency_replacement"],
            "state": "plan_ready", "acceptance_granted": false, "scope": SCOPE});
        let plan_id = new_id("adapter-plan");
        self.ledger.save(
            &plan_id,
            &candidate.campaign_id,
            id,
            "adapter_plan",
            &plan,
            None,
        )?;
        Ok(plan)
    }
    pub fn get_requirements(&self, id: &str, offset: usize, limit: usize) -> Result<Value> {
        page(limit)?;
        let _lock = self.lock(id)?;
        let campaign: Campaign = self.ledger.load(id, "campaign")?;
        self.fresh_campaign(&campaign)?;
        let requirements: Vec<Requirement> = self.ledger.list(id, "requirement", offset, limit)?;
        let mut entries = Vec::new();
        for requirement in requirements {
            entries.push(self.requirement_view(&requirement)?);
        }
        let total = self.ledger.count(id, "requirement")?;
        Ok(
            json!({"campaign_id":id,"requirements":entries,"offset":offset,"total":total,"next_offset":if offset+entries.len()<total{Some(offset+entries.len())}else{None},"scope":SCOPE}),
        )
    }
    fn requirement_view(&self, requirement: &Requirement) -> Result<Value> {
        let candidates: Vec<Candidate> =
            self.ledger
                .list(&requirement.requirement_id, "candidate", 0, 100)?;
        let total = self
            .ledger
            .count(&requirement.requirement_id, "candidate")?;
        let mut views = Vec::new();
        let mut state = "unresolved";
        for candidate in candidates {
            let count = self.ledger.count(&candidate.candidate_id, "attempt")?;
            let mut attempts: Vec<DependencyAttempt> = self.ledger.list(
                &candidate.candidate_id,
                "attempt",
                count.saturating_sub(10),
                10,
            )?;
            for attempt in &mut attempts {
                if attempt.status == "running" {
                    attempt.status = "interrupted".into();
                }
            }
            let current = self.fresh_candidate(&candidate).is_ok();
            let accepted = attempts.last().is_some_and(|a| {
                a.status == "passed"
                    && a.requirement_fingerprint == requirement.fingerprint
                    && current
                    && a.programs.iter().all(|p| {
                        inventory::file_hash(Path::new(&p.path)).is_ok_and(|h| h == p.sha256)
                    })
            });
            if accepted {
                if candidate.spec.strategy != Strategy::TemporaryFfi {
                    state = "accepted_fixture_scope";
                } else if state != "accepted_fixture_scope" {
                    state = "verified_temporary_ffi";
                }
            }
            views.push(json!({"candidate":candidate,"state":if !current{"stale"}else if accepted && candidate.spec.strategy==Strategy::TemporaryFfi{"verified_temporary_ffi"}else if accepted{"accepted_fixture_scope"}else{"proposed_or_unverified"},"attempts":attempts,"total_attempts":count,"native_implementation_claimed":accepted && candidate.spec.strategy!=Strategy::TemporaryFfi}));
        }
        let children: Vec<Campaign> =
            self.ledger
                .list(&requirement.requirement_id, "campaign", 0, 100)?;
        Ok(
            json!({"requirement":requirement,"state":state,"candidates":views,"candidate_total":total,"candidate_page_truncated":total>100,"dependency_campaigns":children,"scope":SCOPE}),
        )
    }
    pub fn next_tasks(&self, id: &str, offset: usize, limit: usize) -> Result<Value> {
        let requirements = self.get_requirements(id, offset, limit)?;
        let mut actions = Vec::new();
        for entry in requirements["requirements"]
            .as_array()
            .context("missing requirements")?
        {
            if entry["state"] == "accepted_fixture_scope" {
                continue;
            }
            let req = &entry["requirement"];
            let action = if !req["spec"]["unknown_usage"].as_array().unwrap().is_empty() {
                "close_usage_gaps"
            } else if entry["candidate_total"] == 0 {
                "search_rust_candidates_or_port_dependency"
            } else {
                "implement_adapter_or_verify_contracts"
            };
            actions.push(json!({"requirement_id":req["requirement_id"],"action":action,"search_queries":[format!("Rust alternative {} {}",req["spec"]["package"].as_str().unwrap_or(""),req["spec"]["used_apis"].to_string())],"dependency_campaigns":entry["dependency_campaigns"]}));
        }
        if actions.is_empty() && requirements["next_offset"].is_null() {
            actions.push(json!({"campaign_id":id,"action": if requirements["total"] == 0 {"inspect_inventory_and_register_dependency_requirements"} else {"continue_program_conversion_and_integration"},"scope":"Dependency-page completion is not whole-program completion"}));
        }
        Ok(
            json!({"campaign_id":id,"tasks":actions,"next_offset":requirements["next_offset"],"program_migration_completed":false}),
        )
    }
    pub fn report(&self, id: &str) -> Result<Value> {
        let _lock = self.lock(id)?;
        let campaign: Campaign = self.ledger.load(id, "campaign")?;
        let fresh = self.fresh_campaign(&campaign);
        Ok(
            json!({"campaign":campaign,"state":if fresh.is_ok(){"planning"}else{"stale"},"freshness_error":fresh.err().map(|e|e.to_string()),"requirement_count":self.ledger.count(id,"requirement")?,"event_count":self.ledger.event_count(id)?,"program_migration_completed":false,"scope":SCOPE,"next_step":"get_dependency_requirements or next_migration_tasks with pagination"}),
        )
    }
    pub async fn verify(
        &self,
        candidate_id: &str,
        probes: Vec<ContractProbe>,
    ) -> Result<DependencyAttempt> {
        bounded(&probes)?;
        let candidate: Candidate = self.ledger.load(candidate_id, "candidate")?;
        let _lock = self.lock(&candidate.campaign_id)?;
        let campaign: Campaign = self.ledger.load(&candidate.campaign_id, "campaign")?;
        let requirement: Requirement =
            self.ledger.load(&candidate.requirement_id, "requirement")?;
        self.fresh_campaign(&campaign)?;
        self.fresh_candidate(&candidate)?;
        ensure!(
            requirement.spec.unknown_usage.is_empty(),
            "close unknown source usage with revise_dependency_requirements before acceptance"
        );
        ensure!(
            candidate.spec.known_gaps.is_empty(),
            "candidate has known gaps; propose a corrected candidate/adapter"
        );
        ensure!(
            requirement.spec.used_apis.iter().all(|api| candidate
                .spec
                .api_mapping
                .get(api)
                .is_some_and(|target| !target.trim().is_empty())),
            "candidate API mapping is incomplete"
        );
        ensure!(
            self.ledger.count(candidate_id, "attempt")? < 32,
            "candidate verification attempt limit (32) reached"
        );
        let mut ids = BTreeSet::new();
        let mut cases = 0;
        for probe in &probes {
            ensure!(
                ids.insert(probe.contract_id.clone())
                    && requirement
                        .spec
                        .contracts
                        .iter()
                        .any(|c| c.id == probe.contract_id),
                "duplicate or unknown contract probe"
            );
            ensure!(!probe.cases.is_empty(), "contract probe needs fixtures");
            cases += probe.cases.len();
        }
        ensure!(
            cases > 0
                && cases <= 256
                && requirement
                    .spec
                    .contracts
                    .iter()
                    .filter(|c| c.mandatory)
                    .all(|c| ids.contains(&c.id)),
            "every mandatory contract needs fixtures; maximum 256 cases"
        );
        let mut programs = Vec::new();
        for probe in &probes {
            for command in [&probe.source, &probe.replacement] {
                let path = Path::new(&command.program);
                ensure!(
                    path.is_absolute() && path.is_file(),
                    "probe executable must be an existing absolute file path"
                );
                let path = path.canonicalize()?;
                let project_root = if std::ptr::eq(command, &probe.source) {
                    Path::new(&campaign.source_path)
                } else {
                    Path::new(&candidate.spec.source_path)
                };
                ensure!(
                    path.starts_with(project_root),
                    "probe executable/launcher must be inside its pinned project root"
                );
                programs.push(ProgramStamp {
                    path: path.display().to_string(),
                    sha256: inventory::file_hash(&path)?,
                });
            }
        }
        let mut attempt = DependencyAttempt {
            attempt_id: new_id("evidence"),
            candidate_id: candidate_id.into(),
            requirement_id: requirement.requirement_id.clone(),
            campaign_id: campaign.campaign_id.clone(),
            status: "running".into(),
            source_fingerprint: campaign.source_fingerprint.clone(),
            candidate_fingerprint: candidate.source_fingerprint.clone(),
            requirement_fingerprint: requirement.fingerprint.clone(),
            probes,
            programs,
            cases: Vec::new(),
            last_error: None,
            scope: SCOPE.into(),
        };
        self.save_attempt(&attempt)?;
        let run = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            self.run_probes(&campaign, &candidate, &mut attempt),
        )
        .await;
        match run {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                attempt.status = "failed".into();
                attempt.last_error = Some(format!("{error:#}"));
            }
            Err(_) => {
                attempt.status = "budget_exceeded".into();
                attempt.last_error = Some("120 second verification budget exceeded".into());
            }
        }
        if self.fresh_campaign(&campaign).is_err()
            || self.fresh_candidate(&candidate).is_err()
            || attempt
                .programs
                .iter()
                .any(|p| inventory::file_hash(Path::new(&p.path)).map_or(true, |h| h != p.sha256))
        {
            attempt.status = "stale".into();
            attempt.last_error =
                Some("source, replacement or probe executable changed during verification".into());
        }
        self.save_attempt(&attempt)?;
        Ok(attempt)
    }
    async fn run_probes(
        &self,
        campaign: &Campaign,
        candidate: &Candidate,
        attempt: &mut DependencyAttempt,
    ) -> Result<()> {
        let logs = self
            .directory(&campaign.campaign_id)?
            .join("evidence")
            .join(&attempt.attempt_id);
        let mut index = 0;
        for probe in &attempt.probes {
            for arguments in &probe.cases {
                let mut args = probe.source.args.clone();
                args.extend(arguments.clone());
                let mut source = crate::process::run(
                    &probe.source.program,
                    &args,
                    Path::new(&campaign.source_path),
                    &logs,
                    &format!("{index}-source"),
                    10,
                )
                .await?;
                let source_json = if source.success() {
                    serde_json::from_str::<Value>(&source.stdout)
                        .ok()
                        .filter(|v| {
                            v.as_object().is_some_and(|o| !o.is_empty())
                                || v.as_array().is_some_and(|a| !a.is_empty())
                        })
                } else {
                    None
                };
                if source_json.is_none() {
                    source.compact();
                    attempt.cases.push(ProbeCase {
                        contract_id: probe.contract_id.clone(),
                        arguments: arguments.clone(),
                        source,
                        replacement: None,
                        matched: false,
                    });
                    attempt.status = "source_failed".into();
                    attempt.last_error=Some("source probe must exit successfully and emit one JSON object/array observation".into());
                    return Ok(());
                }
                let mut args = probe.replacement.args.clone();
                args.extend(arguments.clone());
                let mut replacement = crate::process::run(
                    &probe.replacement.program,
                    &args,
                    Path::new(&candidate.spec.source_path),
                    &logs,
                    &format!("{index}-replacement"),
                    10,
                )
                .await?;
                let replacement_json = serde_json::from_str::<Value>(&replacement.stdout).ok();
                let matched = replacement.success()
                    && source_json == replacement_json
                    && source.stderr == replacement.stderr
                    && source.exit_code == replacement.exit_code;
                source.compact();
                replacement.compact();
                attempt.cases.push(ProbeCase {
                    contract_id: probe.contract_id.clone(),
                    arguments: arguments.clone(),
                    source,
                    replacement: Some(replacement),
                    matched,
                });
                index += 1;
            }
        }
        attempt.status = if attempt.cases.iter().all(|c| c.matched) {
            "passed"
        } else {
            "mismatch"
        }
        .into();
        Ok(())
    }
    fn save_attempt(&self, attempt: &DependencyAttempt) -> Result<()> {
        self.ledger.save(
            &attempt.attempt_id,
            &attempt.campaign_id,
            &attempt.candidate_id,
            "attempt",
            attempt,
            None,
        )
    }
    fn fresh_campaign(&self, campaign: &Campaign) -> Result<()> {
        ensure!(
            inventory::fingerprint(&inventory::scan(Path::new(&campaign.source_path))?)?
                == campaign.source_fingerprint,
            "source inventory changed; create a new campaign"
        );
        Ok(())
    }
    fn fresh_candidate(&self, candidate: &Candidate) -> Result<()> {
        ensure!(
            inventory::fingerprint(&inventory::scan(Path::new(&candidate.spec.source_path))?)?
                == candidate.source_fingerprint,
            "replacement project changed; record a new candidate"
        );
        Ok(())
    }
    fn directory(&self, id: &str) -> Result<PathBuf> {
        ensure!(
            !id.is_empty()
                && id.len() < 128
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid campaign id"
        );
        Ok(self.ledger.root.join("campaigns").join(id))
    }
    fn lock(&self, id: &str) -> Result<CampaignLock> {
        let dir = self.directory(id)?;
        ensure!(dir.is_dir(), "campaign not found");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("campaign.lock"))?;
        file.try_lock_exclusive()
            .context("another campaign operation is running")?;
        Ok(CampaignLock(file))
    }
}
struct CampaignLock(std::fs::File);
impl Drop for CampaignLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}
fn page(limit: usize) -> Result<()> {
    ensure!(limit > 0 && limit <= 100, "page limit must be 1-100");
    Ok(())
}
fn bounded<T: Serialize>(value: &T) -> Result<()> {
    ensure!(
        serde_json::to_vec(value)?.len() <= 1024 * 1024,
        "request exceeds 1 MiB"
    );
    Ok(())
}
fn unique(values: &[String]) -> Result<()> {
    ensure!(
        !values.is_empty()
            && values.len() <= 1000
            && values.iter().all(|v| !v.trim().is_empty())
            && values.iter().collect::<BTreeSet<_>>().len() == values.len(),
        "used APIs must be unique nonempty strings (1-1000)"
    );
    Ok(())
}
fn digest<T: Serialize>(value: &T) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(value)?)))
}
fn new_id(prefix: &str) -> String {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}-{:x}-{:x}-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    )
}
fn line_count(path: &Path) -> Result<usize> {
    use std::io::Read;
    let mut input = std::fs::File::open(path)?;
    let mut buffer = [0u8; 65536];
    let mut count = 0;
    let mut last = None;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        count += buffer[..read].iter().filter(|b| **b == b'\n').count();
        last = Some(buffer[read - 1]);
    }
    Ok(count + usize::from(last.is_some_and(|b| b != b'\n')))
}
