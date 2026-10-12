//! A persistent, bounded C-to-Rust vertical slice, independent of code-search infrastructure.
mod clang;
mod dependencies;
pub mod model;
mod process;
mod store;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use model::{CaseResult, Diagnostic, Function, HeaderDependency, Job, SourceFile, Verification};
use store::Store;

const MAX_FILES: usize = 32;
const OPERATION_SECONDS: u64 = 120;
const CASE_SECONDS: u64 = 10;
const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_CANDIDATE_BYTES: usize = 1024 * 1024;
const MANIFEST: &str = "[package]\nname = \"reforge-port\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[lib]\npath = \"src/lib.rs\"\n";
const LIMITATION: &str = "Fixture-level evidence for int32 library functions only; not proof of whole-program equivalence. Local host execution with time/output limits, not a security sandbox.";

pub struct Engine {
    store: Store,
}
impl Engine {
    pub fn new(root: &Path) -> Result<Self> {
        Ok(Self {
            store: Store::new(root)?,
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
    pub fn list(&self) -> Result<serde_json::Value> {
        Ok(
            json!({"jobs":self.store.list()?.iter().map(|job|json!({"job_id":job.job_id,"state":job.state,"source_path":job.source_path,"generation_attempt":job.generation_attempt})).collect::<Vec<_>>() }),
        )
    }
    pub fn report(&self, id: &str) -> Result<Job> {
        let _lock = self.store.lock(id)?;
        let mut job = self.load_locked_job(id)?;
        if let Err(error) = self.fresh(&job) {
            job.state = "stale".into();
            job.last_error = Some(error.to_string());
        }
        Ok(job)
    }
    fn load_locked_job(&self, id: &str) -> Result<Job> {
        let mut job = self.store.load(id)?;
        // All callers own the advisory lock: no live operation can still own this state.
        if job.state == "verifying" || job.state == "analyzing" {
            job.state = "interrupted".into();
            job.last_error = Some("Previous operation was interrupted; reanalyze an incomplete analysis or rerun verification.".into());
            if let Some(evidence) = job.verifications.last_mut()
                && evidence.status == "running"
            {
                evidence.status = "interrupted".into();
            }
        }
        Ok(job)
    }
    pub async fn analyze(&self, path: &Path) -> Result<Job> {
        let source = path
            .canonicalize()
            .context("source directory does not exist")?;
        ensure!(source.is_dir(), "source_path must be a directory");
        ensure!(
            !self.store.root().starts_with(&source),
            "engine data directory must be outside the source project"
        );
        let files = source_files(&source)?;
        ensure!(!files.is_empty(), "no C translation units found");
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = format!(
            "port-{:x}-{:x}-{:x}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let dir = self.store.directory(&id)?;
        let snapshot = dir.join("source");
        std::fs::create_dir_all(&snapshot)?;
        let mut job = Job {
            schema_version: 2,
            job_id: id,
            source_path: source.display().to_string(),
            artifact_path: dir.display().to_string(),
            state: "analyzing".into(),
            source_snapshot_sha256: String::new(),
            source_files: Vec::new(),
            header_dependencies: Vec::new(),
            functions: Vec::new(),
            unsupported: Vec::new(),
            analysis_commands: Vec::new(),
            analysis_complete: false,
            generation_attempt: 0,
            candidate_sha256: None,
            verifications: Vec::new(),
            last_error: None,
        };
        let mut bytes = 0;
        for file in files {
            let contents = std::fs::read(source.join(&file))?;
            bytes += contents.len();
            ensure!(bytes <= MAX_SOURCE_BYTES, "source exceeds 256 KiB limit");
            let target = snapshot.join(&file);
            std::fs::create_dir_all(target.parent().context("missing parent")?)?;
            std::fs::write(target, &contents)?;
            job.source_files.push(SourceFile {
                path: file,
                sha256: hash(&contents),
            });
        }
        job.source_snapshot_sha256 = hash(&serde_json::to_vec(&job.source_files)?);
        let _lock = self.store.lock(&job.job_id)?;
        self.store.save(&job)?;
        let result = match tokio::time::timeout(
            std::time::Duration::from_secs(OPERATION_SECONDS),
            self.analyze_inner(&mut job),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "analysis exceeded the 120 second operation budget"
            )),
        };
        if let Err(error) = result {
            job.state = "failed".into();
            job.last_error = Some(format!("{error:#}"));
        }
        self.store.save(&job)?;
        Ok(job)
    }
    async fn analyze_inner(&self, job: &mut Job) -> Result<()> {
        let dir = self.store.directory(&job.job_id)?;
        let logs = dir.join("analysis");
        let version = process::run(
            "clang",
            &["--version".into()],
            &dir,
            &logs,
            "clang-version",
            15,
        )
        .await?;
        ensure!(version.success(), "clang version probe failed");
        job.analysis_commands.push(version);
        for (i, file) in job.source_files.iter().enumerate() {
            let path = dir.join("source").join(&file.path);
            if file.path.ends_with(".h") {
                continue;
            }
            let dependency_path = logs.join(format!("ast-{i}.d"));
            let args = vec![
                "-std=c11".into(),
                "-fno-builtin".into(),
                "-I".into(),
                dir.join("source").display().to_string(),
                "-MD".into(),
                "-MF".into(),
                dependency_path.display().to_string(),
                "-MT".into(),
                "reforge".into(),
                "-pedantic-errors".into(),
                "-Werror".into(),
                "-Wno-newline-eof".into(),
                "-Xclang".into(),
                "-ast-dump=json".into(),
                "-fsyntax-only".into(),
                path.display().to_string(),
            ];
            let mut run =
                process::run("clang", &args, &dir, &logs, &format!("ast-{i}"), 30).await?;
            let translated = if run.success() {
                let ast = serde_json::from_str(&run.stdout)?;
                clang::translate(&ast, &path, &dir.join("source"))
            } else {
                Err(anyhow::anyhow!("Clang analysis failed: {}", run.stderr))
            };
            match translated {
                Ok(functions) => {
                    let parsed = dependencies::parse(&std::fs::read_to_string(&dependency_path)?)?;
                    for dependency in parsed {
                        let path = if dependency.is_absolute() {
                            dependency
                        } else {
                            dir.join(dependency)
                        }
                        .canonicalize()?;
                        let name = path.display().to_string();
                        if job.header_dependencies.iter().any(|d| d.path == name) {
                            continue;
                        }
                        ensure!(
                            job.header_dependencies.len() < 512,
                            "include dependency count exceeds 512"
                        );
                        ensure!(
                            std::fs::metadata(&path)?.len() <= 4 * 1024 * 1024,
                            "include dependency exceeds 4 MiB"
                        );
                        job.header_dependencies.push(HeaderDependency {
                            path: name,
                            sha256: hash(&std::fs::read(&path)?),
                            kind: if path.starts_with(dir.join("source")) {
                                "project"
                            } else {
                                "external"
                            }
                            .into(),
                        });
                    }
                    for function in functions {
                        if job.functions.iter().any(|other| {
                            other.name == function.name
                                && other.source_file == function.source_file
                                && other.line_start == function.line_start
                                && other.line_end == function.line_end
                                && other.rust == function.rust
                        }) {
                            continue;
                        }
                        job.functions.push(function);
                    }
                }
                Err(error) => job.unsupported.push(Diagnostic {
                    file: file.path.clone(),
                    message: error.to_string(),
                }),
            }
            run.compact();
            job.analysis_commands.push(run);
        }
        if job.functions.is_empty() {
            job.unsupported.push(Diagnostic {
                file: String::new(),
                message: "no supported function definitions found".into(),
            });
        }
        let mut names = BTreeSet::new();
        for function in &job.functions {
            if !names.insert(function.name.clone()) {
                job.unsupported.push(Diagnostic {
                    file: function.source_file.clone(),
                    message: format!(
                        "duplicate definition {} (including file-local functions)",
                        function.name
                    ),
                });
            }
        }
        for function in &job.functions {
            for dependency in &function.dependencies {
                if !names.contains(dependency) {
                    job.unsupported.push(Diagnostic {
                        file: function.source_file.clone(),
                        message: format!("unresolved or external call: {dependency}"),
                    });
                }
            }
        }
        let mut external_headers: Vec<_> = job
            .header_dependencies
            .iter()
            .filter(|d| d.kind == "external")
            .collect();
        external_headers.sort_by(|a, b| a.path.cmp(&b.path));
        job.source_snapshot_sha256 = hash(&serde_json::to_vec(
            &json!({"project_files":job.source_files,"external_headers":external_headers}),
        )?);
        job.analysis_complete = true;
        job.state = if job.unsupported.is_empty() {
            "analyzed"
        } else {
            "blocked"
        }
        .into();
        Ok(())
    }
    pub fn plan(&self, id: &str) -> Result<serde_json::Value> {
        let _lock = self.store.lock(id)?;
        let mut job = self.load_locked_job(id)?;
        self.fresh(&job)?;
        supported(&job)?;
        // Groups are SCCs; recursive components can be generated together.
        let groups = dependency_groups(&job.functions);
        ensure!(
            job.generation_attempt == 0,
            "plan is already materialized; inspect the existing report"
        );
        job.state = "planned".into();
        self.store.save(&job)?;
        Ok(
            json!({"job_id":id,"state":job.state,"dependency_groups":groups,"verification_contract":{"parameter_type":"i32","return_type":"i32","minimum_cases":1,"maximum_cases":256,"source_undefined_behavior":"blocks_acceptance","comparison":["stdout","stderr","exit_code"],"scope":LIMITATION}}),
        )
    }
    pub fn context(&self, id: &str, name: &str) -> Result<serde_json::Value> {
        let _lock = self.store.lock(id)?;
        let job = self.load_locked_job(id)?;
        self.fresh(&job)?;
        let function = job
            .functions
            .iter()
            .find(|f| f.name == name)
            .context("unknown function")?;
        let source = std::fs::read_to_string(
            self.store
                .directory(id)?
                .join("source")
                .join(&function.source_file),
        )?;
        Ok(
            json!({"job_id":id,"function":function,"translation_unit_source":source,"source_sha256":job.source_snapshot_sha256,"unsupported":job.unsupported,"semantics":"32-bit signed int; checked arithmetic; no external calls, global state, pointers, or loops; macros expanded by Clang; include dependencies are hashed"}),
        )
    }
    pub fn generate(&self, id: &str) -> Result<Job> {
        let _lock = self.store.lock(id)?;
        let mut job = self.load_locked_job(id)?;
        self.fresh_source(&job)?;
        supported(&job)?;
        ensure!(
            job.state != "analyzed",
            "create_porting_plan must be called before generation"
        );
        let mut rust = String::from("#![allow(unused_mut, unused_parens, unreachable_code)]\n");
        for function in &job.functions {
            rust.push_str(&function.rust);
        }
        self.write_candidate(&mut job, &rust)?;
        Ok(job)
    }
    pub fn submit(&self, id: &str, rust: &str) -> Result<Job> {
        let _lock = self.store.lock(id)?;
        let mut job = self.load_locked_job(id)?;
        self.fresh_source(&job)?;
        supported(&job)?;
        ensure!(
            job.state != "analyzed",
            "create_porting_plan must be called before submission"
        );
        ensure!(
            !rust.trim().is_empty() && rust.len() <= MAX_CANDIDATE_BYTES,
            "candidate must be 1 byte to 1 MiB"
        );
        self.write_candidate(&mut job, rust)?;
        Ok(job)
    }
    fn write_candidate(&self, job: &mut Job, rust: &str) -> Result<()> {
        ensure!(
            rust.len() <= MAX_CANDIDATE_BYTES,
            "candidate exceeds 1 MiB limit"
        );
        ensure!(
            job.generation_attempt < 32,
            "generation attempt limit (32) reached"
        );
        job.generation_attempt += 1;
        let dir = candidate_dir(job);
        std::fs::create_dir_all(dir.join("src"))?;
        std::fs::write(dir.join("Cargo.toml"), MANIFEST)?;
        std::fs::write(dir.join("src/lib.rs"), rust)?;
        job.candidate_sha256 = Some(candidate_hash(&dir)?);
        job.state = "generated".into();
        job.last_error = None;
        self.store.save(job)
    }
    fn fresh_source(&self, job: &Job) -> Result<()> {
        for dependency in &job.header_dependencies {
            ensure!(
                hash(&std::fs::read(&dependency.path)?) == dependency.sha256,
                "include dependency changed: {}; reanalyze project",
                dependency.path
            );
        }
        let original = Path::new(&job.source_path);
        let files = source_files(original)?;
        ensure!(
            files
                == job
                    .source_files
                    .iter()
                    .map(|f| f.path.clone())
                    .collect::<Vec<_>>(),
            "source file inventory changed; reanalyze project"
        );
        let snapshot = self.store.directory(&job.job_id)?.join("source");
        for file in &job.source_files {
            ensure!(
                hash(&std::fs::read(original.join(&file.path))?) == file.sha256,
                "source changed: {}; reanalyze project",
                file.path
            );
            ensure!(
                hash(&std::fs::read(snapshot.join(&file.path))?) == file.sha256,
                "stored snapshot changed: {}; reanalyze project",
                file.path
            );
        }
        Ok(())
    }
    fn fresh(&self, job: &Job) -> Result<()> {
        self.fresh_source(job)?;
        if let Some(expected) = &job.candidate_sha256 {
            ensure!(
                &candidate_hash(&candidate_dir(job))? == expected,
                "candidate changed outside submit_rust_candidate; submit a new candidate"
            );
        }
        Ok(())
    }
    pub async fn verify(&self, id: &str, entry: &str, fixtures: Vec<Vec<i32>>) -> Result<Job> {
        let _lock = self.store.lock(id)?;
        let mut job = self.load_locked_job(id)?;
        self.fresh(&job)?;
        supported(&job)?;
        ensure!(
            job.verifications.len() < 64,
            "verification attempt limit (64) reached"
        );
        ensure!(
            job.candidate_sha256.is_some(),
            "generate or submit a Rust candidate first"
        );
        let function = job
            .functions
            .iter()
            .find(|f| f.name == entry)
            .context("unknown entry function")?;
        ensure!(
            !fixtures.is_empty() && fixtures.len() <= 256,
            "provide between 1 and 256 cases"
        );
        ensure!(
            fixtures
                .iter()
                .all(|args| args.len() == function.parameters.len()),
            "fixture argument count does not match entry signature"
        );
        let count = function.parameters.len();
        job.state = "verifying".into();
        job.last_error = None;
        let mut evidence = Verification {
            attempt: job.verifications.len() + 1,
            candidate_sha256: job.candidate_sha256.clone().unwrap_or_default(),
            source_snapshot_sha256: job.source_snapshot_sha256.clone(),
            entry_function: entry.into(),
            fixtures,
            status: "running".into(),
            commands: Vec::new(),
            cases: Vec::new(),
            limitation: LIMITATION.into(),
        };
        job.verifications.push(evidence.clone());
        self.store.save(&job)?;
        evidence.status = "failed".into();
        let result = match tokio::time::timeout(
            std::time::Duration::from_secs(OPERATION_SECONDS),
            self.verify_inner(&job, entry, count, &mut evidence),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                evidence.status = "budget_exceeded".into();
                Err(anyhow::anyhow!(
                    "verification exceeded the 120 second operation budget"
                ))
            }
        };
        if let Err(error) = result {
            job.last_error = Some(format!("{error:#}"));
        }
        if self.fresh(&job).is_err() {
            evidence.status = "stale".into();
            job.last_error = Some("source or candidate changed during verification".into());
        }
        job.state = if evidence.status == "passed" {
            "completed"
        } else if evidence.status == "source_failed" || evidence.status == "stale" {
            "blocked"
        } else {
            "failed"
        }
        .into();
        *job.verifications
            .last_mut()
            .context("missing verification attempt")? = evidence;
        self.store.save(&job)?;
        Ok(job)
    }
    async fn verify_inner(
        &self,
        job: &Job,
        entry: &str,
        count: usize,
        evidence: &mut Verification,
    ) -> Result<()> {
        let dir = self
            .store
            .directory(&job.job_id)?
            .join("verification")
            .join(evidence.attempt.to_string());
        std::fs::create_dir_all(&dir)?;
        let logs = dir.join("logs");
        for program in ["clang", "rustc", "cargo"] {
            let run = process::run(
                program,
                &["--version".into()],
                &dir,
                &logs,
                &format!("{program}-version"),
                15,
            )
            .await?;
            let ok = run.success();
            evidence.commands.push(run);
            ensure!(ok, "{program} version probe failed");
        }
        let args_types = if count == 0 {
            "void".to_string()
        } else {
            vec!["int"; count].join(",")
        };
        let source_args = (0..count)
            .map(|i| format!("values[{i}]"))
            .collect::<Vec<_>>()
            .join(",");
        let source_harness = format!(
            "#include <stdio.h>\n#include <stdlib.h>\n#include <limits.h>\n_Static_assert(INT_MAX == 2147483647 && INT_MIN == (-2147483647 - 1), \"requires int32\");\nint {entry}({args_types});\nint main(int argc, char **argv) {{ if(argc != {}) return 2; int values[{}]; for(int i=0;i<{};i++) values[i]=(int)strtol(argv[i+1],0,10); printf(\"%d\\n\",{entry}({source_args})); return 0; }}\n",
            count + 1,
            count.max(1),
            count
        );
        std::fs::write(dir.join("driver.c"), source_harness)?;
        let original = dir.join(executable("original"));
        let mut args = vec![
            "-std=c11".into(),
            "-O0".into(),
            "-fno-builtin".into(),
            "-I".into(),
            self.store
                .directory(&job.job_id)?
                .join("source")
                .display()
                .to_string(),
            "-fsanitize=undefined".into(),
            "-fno-sanitize-recover=all".into(),
        ];
        let snapshot = self.store.directory(&job.job_id)?.join("source");
        args.extend(
            job.source_files
                .iter()
                .filter(|f| f.path.ends_with(".c"))
                .map(|f| snapshot.join(&f.path).display().to_string()),
        );
        args.extend([
            "driver.c".into(),
            "-o".into(),
            original.display().to_string(),
        ]);
        let run = process::run("clang", &args, &dir, &logs, "compile-source", 30).await?;
        let ok = run.success();
        evidence.commands.push(run);
        if !ok {
            evidence.status = "source_failed".into();
            bail!("original C build failed");
        }
        // Build the fixed, dependency-free Cargo project offline; no user manifests/build scripts.
        let candidate = candidate_dir(job);
        let run = process::run(
            "cargo",
            &[
                "build".into(),
                "--offline".into(),
                "--manifest-path".into(),
                candidate.join("Cargo.toml").display().to_string(),
                "--target-dir".into(),
                dir.join("cargo-target").display().to_string(),
            ],
            &dir,
            &logs,
            "compile-cargo",
            30,
        )
        .await?;
        let ok = run.success();
        evidence.commands.push(run);
        ensure!(ok, "Rust Cargo build failed");
        let include_path =
            serde_json::to_string(&candidate.join("src/lib.rs").display().to_string())?;
        let rust_args = (0..count)
            .map(|i| format!("values[{i}]"))
            .collect::<Vec<_>>()
            .join(",");
        let driver = format!(
            "#[path={include_path}] mod port;\nfn main() {{ let values: Vec<i32> = std::env::args().skip(1).map(|v|v.parse().unwrap()).collect(); assert_eq!(values.len(),{count}); println!(\"{{}}\",port::rf_{entry}({rust_args})); }}\n"
        );
        std::fs::write(dir.join("driver.rs"), driver)?;
        let rust_bin = dir.join(executable("rust-port"));
        let run = process::run(
            "rustc",
            &[
                "--edition=2024".into(),
                "-C".into(),
                "overflow-checks=yes".into(),
                "driver.rs".into(),
                "-o".into(),
                rust_bin.display().to_string(),
            ],
            &dir,
            &logs,
            "compile-rust-driver",
            30,
        )
        .await?;
        let ok = run.success();
        evidence.commands.push(run);
        ensure!(ok, "Rust harness build failed");
        for (i, case) in evidence.fixtures.iter().enumerate() {
            let args = case.iter().map(ToString::to_string).collect::<Vec<_>>();
            let mut source = process::run(
                &original.display().to_string(),
                &args,
                &dir,
                &logs,
                &format!("case-{i}-source"),
                CASE_SECONDS,
            )
            .await?;
            if !source.success() {
                source.compact();
                evidence.cases.push(CaseResult {
                    arguments: case.clone(),
                    source,
                    rust: None,
                    matched: false,
                });
                evidence.status = "source_failed".into();
                bail!("source case {i} failed or has undefined behavior");
            }
            let mut rust = process::run(
                &rust_bin.display().to_string(),
                &args,
                &dir,
                &logs,
                &format!("case-{i}-rust"),
                CASE_SECONDS,
            )
            .await?;
            let matched = rust.success()
                && source.exit_code == rust.exit_code
                && source.stdout == rust.stdout
                && source.stderr == rust.stderr;
            source.compact();
            rust.compact();
            evidence.cases.push(CaseResult {
                arguments: case.clone(),
                source,
                rust: Some(rust),
                matched,
            });
        }
        evidence.status = if evidence.cases.iter().all(|case| case.matched) {
            "passed"
        } else {
            "mismatch"
        }
        .into();
        Ok(())
    }
}

fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn candidate_dir(job: &Job) -> PathBuf {
    Path::new(&job.artifact_path)
        .join("candidates")
        .join(job.generation_attempt.to_string())
}
fn candidate_hash(dir: &Path) -> Result<String> {
    ensure!(
        std::fs::read_to_string(dir.join("Cargo.toml"))? == MANIFEST,
        "candidate manifest was modified"
    );
    ensure!(
        !dir.join("build.rs").exists(),
        "candidate build scripts are not allowed"
    );
    let files = WalkDir::new(dir)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for file in files {
        ensure!(
            !file.file_type().is_symlink(),
            "candidate symlinks are not allowed"
        );
        if file.file_type().is_file() {
            let relative = file.path().strip_prefix(dir)?;
            ensure!(
                relative == Path::new("Cargo.toml")
                    || relative == Path::new("Cargo.lock")
                    || relative == Path::new("src/lib.rs"),
                "unexpected candidate file {}",
                relative.display()
            );
        }
    }
    let source = std::fs::read(dir.join("src/lib.rs"))?;
    ensure!(
        source.len() <= MAX_CANDIDATE_BYTES,
        "candidate exceeds size limit"
    );
    Ok(hash(&[MANIFEST.as_bytes(), source.as_slice()].concat()))
}
fn supported(job: &Job) -> Result<()> {
    ensure!(
        job.unsupported.is_empty() && !job.functions.is_empty(),
        "analysis contains unsupported features; inspect get_porting_report"
    );
    ensure!(
        job.analysis_complete,
        "analysis was interrupted or failed; reanalyze project"
    );
    Ok(())
}
fn source_files(root: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some(".git" | "target")))
    {
        let entry = entry?;
        ensure!(
            !entry.file_type().is_symlink(),
            "source symlinks unsupported"
        );
        if !matches!(
            entry.path().extension().and_then(|s| s.to_str()),
            Some("c" | "h")
        ) {
            continue;
        }
        ensure!(
            !entry.file_type().is_symlink(),
            "source symlink unsupported"
        );
        if entry.file_type().is_file() {
            ensure!(
                entry.metadata()?.len() <= MAX_SOURCE_BYTES as u64,
                "source file exceeds size limit"
            );
            files.push(
                entry
                    .path()
                    .strip_prefix(root)?
                    .to_str()
                    .context("non-UTF8 source path")?
                    .to_owned(),
            );
            ensure!(files.len() <= MAX_FILES, "source exceeds 32 file limit");
        }
    }
    files.sort();
    Ok(files)
}
fn executable(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

/// Transitive reachability yields SCC groups, then schedules dependency-ready groups.
fn dependency_groups(functions: &[Function]) -> Vec<Vec<String>> {
    fn visit(name: &str, functions: &[Function], seen: &mut BTreeSet<String>) {
        if !seen.insert(name.into()) {
            return;
        }
        if let Some(function) = functions.iter().find(|f| f.name == name) {
            for dependency in &function.dependencies {
                visit(dependency, functions, seen);
            }
        }
    }
    let reach: Vec<_> = functions
        .iter()
        .map(|f| {
            let mut seen = BTreeSet::new();
            visit(&f.name, functions, &mut seen);
            seen
        })
        .collect();
    let mut grouped = BTreeSet::new();
    let mut groups = Vec::new();
    for (i, function) in functions.iter().enumerate() {
        if grouped.contains(&function.name) {
            continue;
        }
        let group: Vec<_> = functions
            .iter()
            .enumerate()
            .filter(|(j, other)| {
                reach[i].contains(&other.name) && reach[*j].contains(&function.name)
            })
            .map(|(_, f)| f.name.clone())
            .collect();
        grouped.extend(group.iter().cloned());
        groups.push(group);
    }
    let mut done = BTreeSet::new();
    let mut ordered = Vec::new();
    while !groups.is_empty() {
        let ready = groups
            .iter()
            .position(|group| {
                group.iter().all(|name| {
                    functions
                        .iter()
                        .find(|f| &f.name == name)
                        .unwrap()
                        .dependencies
                        .iter()
                        .all(|dep| group.contains(dep) || done.contains(dep))
                })
            })
            .expect("SCC condensation is acyclic");
        let group = groups.remove(ready);
        done.extend(group.iter().cloned());
        ordered.push(group);
    }
    ordered
}
