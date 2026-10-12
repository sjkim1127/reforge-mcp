use crate::model::CommandRun;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileStamp {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub language: String,
    pub manifest: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Campaign {
    pub schema_version: u32,
    pub campaign_id: String,
    pub name: String,
    pub parent_requirement_id: Option<String>,
    pub source_path: String,
    pub source_fingerprint: String,
    pub file_count: usize,
    pub languages: BTreeMap<String, usize>,
    pub manifests: Vec<String>,
    pub limitations: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Contract {
    pub id: String,
    pub description: String,
    pub mandatory: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UsageRef {
    pub file: String,
    pub line_start: usize,
    pub line_end: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RequirementSpec {
    pub ecosystem: String,
    pub package: String,
    pub version: String,
    pub used_apis: Vec<String>,
    pub usage_refs: Vec<UsageRef>,
    pub unknown_usage: Vec<String>,
    pub contracts: Vec<Contract>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirement {
    pub requirement_id: String,
    pub campaign_id: String,
    pub revision: usize,
    pub fingerprint: String,
    pub spec: RequirementSpec,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    DirectReplace,
    Adapter,
    ComposeRustLibraries,
    PortDependency,
    TemporaryFfi,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CandidateSpec {
    pub package: String,
    pub version: String,
    pub strategy: Strategy,
    pub source_path: String,
    pub research_urls: Vec<String>,
    pub license: String,
    pub target: String,
    pub api_mapping: BTreeMap<String, String>,
    pub known_gaps: Vec<String>,
    pub adapter_tasks: Vec<String>,
    pub dependency_campaign_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub candidate_id: String,
    pub requirement_id: String,
    pub campaign_id: String,
    pub source_fingerprint: String,
    pub spec: CandidateSpec,
    pub file_count: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProbeCommand {
    pub program: String,
    pub args: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContractProbe {
    pub contract_id: String,
    pub source: ProbeCommand,
    pub replacement: ProbeCommand,
    pub cases: Vec<Vec<String>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgramStamp {
    pub path: String,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeCase {
    pub contract_id: String,
    pub arguments: Vec<String>,
    pub source: CommandRun,
    pub replacement: Option<CommandRun>,
    pub matched: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyAttempt {
    pub attempt_id: String,
    pub candidate_id: String,
    pub requirement_id: String,
    pub campaign_id: String,
    pub status: String,
    pub source_fingerprint: String,
    pub candidate_fingerprint: String,
    pub requirement_fingerprint: String,
    pub probes: Vec<ContractProbe>,
    pub programs: Vec<ProgramStamp>,
    pub cases: Vec<ProbeCase>,
    pub last_error: Option<String>,
    pub scope: String,
}
