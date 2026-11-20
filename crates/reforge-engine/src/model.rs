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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointerParamContract {
    pub param_index: usize,
    pub name: String,
    pub c_type: String,
    pub access: String,
    pub kind: String,
    pub safe_rust_type: String,
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
    #[serde(default)]
    pub pointer_contracts: Vec<PointerParamContract>,
    #[serde(default)]
    pub safe_rust_wrapper: Option<String>,
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

/// One entry-function argument in a verification fixture.
///
/// Serialized as a plain JSON number, so fixtures written as `[[1, 2]]` (including
/// pre-existing int32 jobs) keep deserializing. Integers are converted to the
/// parameter type with C cast semantics (two's complement wrapping); floats must
/// be finite and are only accepted for `float`/`double` parameters.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FixtureValue {
    Int(i64),
    UInt(u64),
    Float(f64),
}

impl FixtureValue {
    /// Canonical form for an unsigned value: `Int` when it fits in `i64`.
    pub fn from_u64(value: u64) -> Self {
        i64::try_from(value).map_or(Self::UInt(value), Self::Int)
    }

    /// Exact decimal spelling passed on the command line to both drivers.
    pub fn to_arg(self) -> String {
        match self {
            Self::Int(v) => v.to_string(),
            Self::UInt(v) => v.to_string(),
            // `Debug` is the shortest round-trip spelling (`0.1`, `1e300`, `-0.0`),
            // accepted by both C `strtod` and Rust `str::parse::<f64>`.
            Self::Float(v) => format!("{v:?}"),
        }
    }

    pub fn is_integer(self) -> bool {
        !matches!(self, Self::Float(_))
    }
}

impl From<i32> for FixtureValue {
    fn from(value: i32) -> Self {
        Self::Int(value.into())
    }
}

impl From<i64> for FixtureValue {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<u64> for FixtureValue {
    fn from(value: u64) -> Self {
        Self::from_u64(value)
    }
}

impl From<f64> for FixtureValue {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

impl std::fmt::Display for FixtureValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_arg())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseResult {
    pub arguments: Vec<FixtureValue>,
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
    pub fixtures: Vec<Vec<FixtureValue>>,
    pub status: String,
    pub commands: Vec<CommandRun>,
    pub cases: Vec<CaseResult>,
    pub limitation: String,
}

/// A generated input whose source execution exited or crashed (not UBSan).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedFailure {
    pub arguments: Vec<FixtureValue>,
    /// `exit:<code>` or `signal:<number>`.
    pub outcome: String,
}

/// Evidence for one coverage-guided input generation run against the source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseGeneration {
    pub attempt: usize,
    pub entry_function: String,
    pub source_snapshot_sha256: String,
    /// `running`, `completed`, `failed`, `budget_exceeded`, `stale`, or `interrupted`.
    pub status: String,
    pub strategy: String,
    pub seed: u64,
    pub max_cases: usize,
    pub max_executions: u64,
    pub time_budget_ms: u64,
    /// Driver counters: executions, corpus, instrumented_edges, covered_edges, features,
    /// selected_features, coverage_cases, diversity_cases, undefined_behavior,
    /// source_failures, timeouts, elapsed_ms.
    pub stats: std::collections::BTreeMap<String, u64>,
    /// Inputs that ran cleanly on the source: a greedy cover of every observed feature,
    /// followed by up to 16 value-diverse clean runs.
    pub cases: Vec<Vec<FixtureValue>>,
    /// Sample inputs where the source hit UBSan-detected undefined behavior.
    pub undefined_behavior: Vec<Vec<FixtureValue>>,
    pub source_failures: Vec<GeneratedFailure>,
    pub timeouts: Vec<Vec<FixtureValue>>,
    pub commands: Vec<CommandRun>,
    pub error: Option<String>,
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
    #[serde(default)]
    pub case_generations: Vec<CaseGeneration>,
    pub last_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::FixtureValue;

    #[test]
    fn fixture_values_keep_reading_int32_json_and_round_trip_wide_numbers() {
        let values: Vec<FixtureValue> =
            serde_json::from_str("[1, -7, 18446744073709551615, -2.5, 3.0]").unwrap();
        assert_eq!(
            values,
            vec![
                FixtureValue::Int(1),
                FixtureValue::Int(-7),
                FixtureValue::UInt(u64::MAX),
                FixtureValue::Float(-2.5),
                FixtureValue::Float(3.0),
            ]
        );
        let json = serde_json::to_string(&values).unwrap();
        assert_eq!(json, "[1,-7,18446744073709551615,-2.5,3.0]");
        assert_eq!(FixtureValue::from(5_u64), FixtureValue::Int(5));
    }

    #[test]
    fn fixture_arguments_use_exact_round_trip_spellings() {
        for value in [0.1, 1e300, -0.0, 5e-324, f64::MAX] {
            let arg = FixtureValue::Float(value).to_arg();
            assert_eq!(
                arg.parse::<f64>().unwrap().to_bits(),
                value.to_bits(),
                "{arg}"
            );
        }
        assert_eq!(FixtureValue::Int(i64::MIN).to_arg(), "-9223372036854775808");
        assert_eq!(
            FixtureValue::UInt(u64::MAX).to_arg(),
            "18446744073709551615"
        );
    }
}
