use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceFile {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderDependency {
    pub path: String,
    pub sha256: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructField {
    pub name: String,
    pub c_type: String,
    pub rust_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructDef {
    pub name: String,
    pub rust_name: String,
    pub fields: Vec<StructField>,
    pub source_file: String,
    pub line_start: usize,
    pub line_end: usize,
    pub rust: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalFunction {
    pub name: String,
    pub parameter_types: Vec<String>,
    pub return_type: String,
    pub rust: String,
}

fn default_int_type() -> String {
    "int".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    pub parameters: Vec<String>,
    #[serde(default)]
    pub parameter_types: Vec<String>,
    #[serde(default = "default_int_type")]
    pub return_type: String,
    pub source_file: String,
    pub line_start: usize,
    pub line_end: usize,
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub external_dependencies: Vec<String>,
    #[serde(default)]
    pub is_static: bool,
    pub rust: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub file: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandRun {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub output_limit_exceeded: bool,
    pub duration_ms: u64,
    pub stdout_path: String,
    pub stderr_path: String,
    pub stdout: String,
    pub stderr: String,
}
impl CommandRun {
    pub fn compact(&mut self) {
        for value in [&mut self.stdout, &mut self.stderr] {
            if value.len() > 16384 {
                let mut end = 16384;
                while !value.is_char_boundary(end) {
                    end -= 1;
                }
                value.truncate(end);
                value.push_str("\n[excerpt; full output is in log artifact]");
            }
        }
    }
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.output_limit_exceeded
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseResult {
    pub arguments: Vec<i32>,
    pub source: CommandRun,
    pub rust: Option<CommandRun>,
    pub matched: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verification {
    pub attempt: usize,
    pub candidate_sha256: String,
    pub source_snapshot_sha256: String,
    pub entry_function: String,
    pub fixtures: Vec<Vec<i32>>,
    pub status: String,
    pub commands: Vec<CommandRun>,
    pub cases: Vec<CaseResult>,
    pub limitation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub schema_version: u32,
    pub job_id: String,
    pub source_path: String,
    pub artifact_path: String,
    pub state: String,
    pub source_snapshot_sha256: String,
    pub source_files: Vec<SourceFile>,
    #[serde(default)]
    pub header_dependencies: Vec<HeaderDependency>,
    #[serde(default)]
    pub structs: Vec<StructDef>,
    #[serde(default)]
    pub constants: Vec<String>,
    pub functions: Vec<Function>,
    #[serde(default)]
    pub external_functions: Vec<ExternalFunction>,
    #[serde(default)]
    pub c_boundary_files: Vec<String>,
    #[serde(default)]
    pub compilation_database: bool,
    #[serde(default)]
    pub kconfig: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub inactive_source_files: Vec<String>,
    #[serde(default)]
    pub kernel_profile: Option<String>,
    #[serde(default)]
    pub kbuild_artifacts: Vec<SourceFile>,
    pub unsupported: Vec<Diagnostic>,
    pub analysis_commands: Vec<CommandRun>,
    pub analysis_complete: bool,
    pub generation_attempt: usize,
    pub candidate_sha256: Option<String>,
    pub verifications: Vec<Verification>,
    pub last_error: Option<String>,
}
