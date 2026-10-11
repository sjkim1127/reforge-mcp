use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceFile {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    pub parameters: Vec<String>,
    pub source_file: String,
    pub line_start: usize,
    pub line_end: usize,
    pub dependencies: Vec<String>,
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
    pub functions: Vec<Function>,
    pub unsupported: Vec<Diagnostic>,
    pub analysis_commands: Vec<CommandRun>,
    pub analysis_complete: bool,
    pub generation_attempt: usize,
    pub candidate_sha256: Option<String>,
    pub verifications: Vec<Verification>,
    pub last_error: Option<String>,
}
