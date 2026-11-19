//! A persistent, bounded C-to-Rust vertical slice, independent of code-search infrastructure.
pub mod campaign;
mod clang;
mod dependencies;
mod kbuild;
pub mod model;
mod process;
mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use model::{
    CaseResult, Diagnostic, ExternalFunction, Function, HeaderDependency, Job, SourceFile,
    StructDef, Verification,
};
use store::Store;

const MAX_FILES: usize = 4096;
const OPERATION_SECONDS: u64 = 120;
const CASE_SECONDS: u64 = 10;
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CANDIDATE_BYTES: usize = 16 * 1024 * 1024;
const MAX_HEADER_DEPENDENCIES: usize = 4096;
const MANIFEST: &str = "[package]\nname = \"reforge-port\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[lib]\npath = \"src/lib.rs\"\n";
const LIMITATION: &str = "Fixture-level evidence for C library functions (int32, void, structs, pointers, loops, external calls); not proof of whole-program equivalence. Local host execution with time/output limits, not a security sandbox.";

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
        ensure!(
            files.iter().any(|f| f.ends_with(".c")),
            "no C translation units found"
        );
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
            structs: Vec::new(),
            constants: Vec::new(),
            functions: Vec::new(),
            external_functions: Vec::new(),
            c_boundary_files: Vec::new(),
            compilation_database: files.iter().any(|f| f == "compile_commands.json"),
            kconfig: BTreeMap::new(),
            inactive_source_files: Vec::new(),
            kernel_profile: None,
            kbuild_artifacts: Vec::new(),
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
            ensure!(bytes <= MAX_SOURCE_BYTES, "source exceeds 64 MiB limit");
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
        let snapshot = dir.join("source");
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
        let kb = kbuild::load(&snapshot)?;
        job.kconfig = kb.kconfig.clone();
        job.inactive_source_files = kb.inactive_units.clone();
        let compile_flags = load_compile_commands(&snapshot, Path::new(&job.source_path))?;
        let mut prototypes: BTreeMap<String, ExternalFunction> = BTreeMap::new();
        for (i, file) in job.source_files.iter().enumerate() {
            if !file.path.ends_with(".c") {
                continue;
            }
            if job.inactive_source_files.contains(&file.path) {
                continue;
            }
            if is_c_boundary_file(&file.path) {
                job.c_boundary_files.push(file.path.clone());
                continue;
            }
            let path = snapshot.join(&file.path);
            let dependency_path = logs.join(format!("ast-{i}.d"));
            let mut args = vec![];
            let extra_flags = compile_flags
                .get(&file.path)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let kb_flags = kb.cflags_for(&file.path);
            if !extra_flags.iter().any(|f| f.starts_with("-std="))
                && !kb_flags.iter().any(|f| f.starts_with("-std="))
            {
                args.push("-std=c11".into());
            }
            args.extend([
                "-fno-builtin".into(),
                "-I".into(),
                snapshot.display().to_string(),
            ]);
            args.extend(kb_flags);
            args.extend(extra_flags.iter().cloned());
            args.extend([
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
            ]);
            let mut run =
                process::run("clang", &args, &dir, &logs, &format!("ast-{i}"), 30).await?;
            let translated = if run.success() {
                let ast = serde_json::from_str(&run.stdout)?;
                clang::translate(&ast, &path, &snapshot)
            } else {
                Err(anyhow::anyhow!("Clang analysis failed: {}", run.stderr))
            };
            match translated {
                Ok(output) => {
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
                            job.header_dependencies.len() < MAX_HEADER_DEPENDENCIES,
                            "include dependency count exceeds {MAX_HEADER_DEPENDENCIES}"
                        );
                        ensure!(
                            std::fs::metadata(&path)?.len() <= 4 * 1024 * 1024,
                            "include dependency exceeds 4 MiB"
                        );
                        job.header_dependencies.push(HeaderDependency {
                            path: name,
                            sha256: hash(&std::fs::read(&path)?),
                            kind: if path.starts_with(&snapshot) {
                                "project"
                            } else {
                                "external"
                            }
                            .into(),
                        });
                    }
                    for sdef in output.structs {
                        if let Some(existing) =
                            job.structs.iter().find(|other| other.name == sdef.name)
                        {
                            if existing.fields != sdef.fields {
                                job.unsupported.push(Diagnostic {
                                    file: sdef.source_file.clone(),
                                    message: format!("conflicting struct definition {}", sdef.name),
                                });
                            }
                            continue;
                        }
                        job.structs.push(sdef);
                    }
                    for citem in output.constants {
                        if !job.constants.contains(&citem) {
                            job.constants.push(citem);
                        }
                    }
                    for proto in output.prototypes {
                        if let Some(existing) = prototypes.get(&proto.name) {
                            if existing.parameter_types != proto.parameter_types
                                || existing.return_type != proto.return_type
                            {
                                job.unsupported.push(Diagnostic {
                                    file: file.path.clone(),
                                    message: format!(
                                        "conflicting prototype for external function {}",
                                        proto.name
                                    ),
                                });
                            }
                        } else {
                            prototypes.insert(proto.name.clone(), proto);
                        }
                    }
                    for function in output.functions {
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
        let mut used_externals: BTreeMap<String, ExternalFunction> = BTreeMap::new();
        for function in &mut job.functions {
            let mut internal = Vec::new();
            let mut external = Vec::new();
            for dependency in &function.dependencies {
                if names.contains(dependency) {
                    internal.push(dependency.clone());
                } else if let Some(proto) = prototypes.get(dependency) {
                    external.push(dependency.clone());
                    used_externals
                        .entry(dependency.clone())
                        .or_insert_with(|| proto.clone());
                } else {
                    job.unsupported.push(Diagnostic {
                        file: function.source_file.clone(),
                        message: format!("unresolved or external call: {dependency}"),
                    });
                }
            }
            function.dependencies = internal;
            function.external_dependencies = external;
        }
        job.external_functions = used_externals.into_values().collect();
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
        Ok(json!({
            "job_id": id,
            "state": job.state,
            "dependency_groups": groups,
            "structs": job.structs.iter().map(|s| &s.name).collect::<Vec<_>>(),
            "external_functions": job.external_functions.iter().map(|e| &e.name).collect::<Vec<_>>(),
            "c_boundary_files": job.c_boundary_files,
            "compilation_database": job.compilation_database,
            "kconfig": job.kconfig,
            "inactive_source_files": job.inactive_source_files,
            "kernel_profile": job.kernel_profile,
            "kbuild_artifacts": job.kbuild_artifacts,
            "verification_contract": {
                "parameter_type": "integer | float | double | *mut i32",
                "return_type": "integer | float | double | ()",
                "minimum_cases": 1,
                "maximum_cases": 256,
                "source_undefined_behavior": "blocks_acceptance",
                "comparison": ["stdout", "stderr", "exit_code"],
                "scope": LIMITATION
            }
        }))
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
        Ok(json!({
            "job_id": id,
            "function": function,
            "structs": job.structs,
            "constants": job.constants,
            "external_functions": job.external_functions,
            "c_boundary_files": job.c_boundary_files,
            "kconfig": job.kconfig,
            "inactive_source_files": job.inactive_source_files,
            "kernel_profile": job.kernel_profile,
            "kbuild_artifacts": job.kbuild_artifacts,
            "translation_unit_source": source,
            "source_sha256": job.source_snapshot_sha256,
            "unsupported": job.unsupported,
            "semantics": "fixed-width/unsigned/bool/platform integers, float/double, enums, void, repr(C) structs/unions, bitfields, fixed-size arrays, raw pointers (*mut T with null check, ptr-int casts), function pointers (Option<extern \"C\" fn>), string literals, static const globals, loops (for/while/do-while/break/continue), switch/case/default, forward goto cleanup labels, sizeof/alignof, libc/heap/builtins (malloc/free/memcpy/memset/memmove/qsort/__builtin_*), C ABI exports (rf_abi_*), compile_commands.json, CMake/Makefile and recursive Kbuild/Kconfig (.config); checked signed vs wrapping unsigned arithmetic; include dependencies hashed"
        }))
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
        let mut rust = String::from(
            "#![allow(unused_mut, unused_parens, unreachable_code, non_camel_case_types, non_snake_case, dead_code, unused_unsafe, unused_labels, improper_ctypes, improper_ctypes_definitions)]\n#[inline]\nfn rf_ptr<T>(ptr: *mut T) -> *mut T {\n    assert!(!ptr.is_null(), \"null pointer dereference\");\n    ptr\n}\n",
        );
        for sdef in &job.structs {
            rust.push_str(&sdef.rust);
        }
        for citem in &job.constants {
            rust.push_str(citem);
        }
        for ext in &job.external_functions {
            rust.push_str(&ext.rust);
        }
        for function in &job.functions {
            rust.push_str(&function.rust);
        }
        self.write_candidate(&mut job, &rust, true)?;
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
            "candidate must be 1 byte to 16 MiB"
        );
        self.write_candidate(&mut job, rust, false)?;
        Ok(job)
    }
    fn write_candidate(&self, job: &mut Job, rust: &str, emit_kbuild: bool) -> Result<()> {
        ensure!(
            rust.len() <= MAX_CANDIDATE_BYTES,
            "candidate exceeds 16 MiB limit"
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
        if emit_kbuild {
            let kbuild_dir = dir.join("kbuild");
            std::fs::create_dir_all(&kbuild_dir)?;
            let kbuild_src = render_kbuild_file(job);
            let nostd_src = format!("#![no_std]\n{rust}");
            let header_src = render_bindings_header(job);
            std::fs::write(kbuild_dir.join("Kbuild"), kbuild_src.as_bytes())?;
            std::fs::write(kbuild_dir.join("rf_kernel_port.rs"), nostd_src.as_bytes())?;
            std::fs::write(kbuild_dir.join("rf_port_bindings.h"), header_src.as_bytes())?;
            job.kernel_profile = Some("no_std_kbuild".into());
            job.kbuild_artifacts = vec![
                SourceFile {
                    path: "kbuild/Kbuild".into(),
                    sha256: hash(kbuild_src.as_bytes()),
                },
                SourceFile {
                    path: "kbuild/rf_kernel_port.rs".into(),
                    sha256: hash(nostd_src.as_bytes()),
                },
                SourceFile {
                    path: "kbuild/rf_port_bindings.h".into(),
                    sha256: hash(header_src.as_bytes()),
                },
            ];
        } else {
            job.kernel_profile = None;
            job.kbuild_artifacts.clear();
        }
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
            .context("unknown entry function")?
            .clone();
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
        let param_types: Vec<String> = if function.parameter_types.is_empty() {
            vec!["int".into(); function.parameters.len()]
        } else {
            function.parameter_types.clone()
        };
        for pty in &param_types {
            ensure!(
                is_supported_entry_int(pty).is_some()
                    || is_supported_entry_float(pty).is_some()
                    || matches!(pty.as_str(), "int *" | "const int *"),
                "verify_port entry parameter type {pty:?} requires a scalar number or int* wrapper entry"
            );
        }
        let return_type = if function.return_type.is_empty() {
            "int".to_owned()
        } else {
            function.return_type.clone()
        };
        ensure!(
            return_type == "void"
                || is_supported_entry_int(&return_type).is_some()
                || is_supported_entry_float(&return_type).is_some(),
            "verify_port entry return type {return_type:?} requires a scalar number or void wrapper entry"
        );
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
            self.verify_inner(&job, entry, &param_types, &return_type, &mut evidence),
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
        param_types: &[String],
        return_type: &str,
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
        let count = param_types.len();
        let args_types = if count == 0 {
            "void".to_string()
        } else {
            param_types.join(", ")
        };
        let has_ptr_param = param_types
            .iter()
            .any(|t| matches!(t.as_str(), "int *" | "const int *"));
        let source_args = param_types
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if matches!(t.as_str(), "int *" | "const int *") {
                    format!("&values[{i}]")
                } else if t == "int" {
                    format!("values[{i}]")
                } else if t == "_Bool" {
                    format!("(_Bool)(values[{i}] != 0)")
                } else {
                    format!("({t})values[{i}]")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let c_body = if return_type == "void" {
            let mut body = format!("{entry}({source_args}); int rf_printed = 0;");
            for (i, t) in param_types.iter().enumerate() {
                if matches!(t.as_str(), "int *" | "const int *") {
                    body.push_str(&format!(
                        " if (values[{i}] != before[{i}]) {{ printf(\"%sp{i}=%d\", rf_printed ? \" \" : \"\", values[{i}]); rf_printed = 1; }}"
                    ));
                }
            }
            body.push_str(" if (!rf_printed) printf(\"void\"); printf(\"\\n\");");
            body
        } else if is_supported_entry_float(return_type).is_some() {
            let mut body = format!(
                "{return_type} rf_ret = {entry}({source_args}); printf(\"%.6f\", (double)rf_ret);"
            );
            if has_ptr_param {
                for (i, t) in param_types.iter().enumerate() {
                    if matches!(t.as_str(), "int *" | "const int *") {
                        body.push_str(&format!(
                            " if (values[{i}] != before[{i}]) printf(\" p{i}=%d\", values[{i}]);"
                        ));
                    }
                }
            }
            body.push_str(" printf(\"\\n\");");
            body
        } else {
            let (_, ret_unsigned) = is_supported_entry_int(return_type)
                .context("unsupported return type in verify_inner")?;
            let call_and_print = if return_type == "int" {
                format!("int rf_ret = {entry}({source_args}); printf(\"%d\", rf_ret);")
            } else if ret_unsigned {
                format!(
                    "{return_type} rf_ret = {entry}({source_args}); printf(\"%llu\", (unsigned long long)rf_ret);"
                )
            } else {
                format!(
                    "{return_type} rf_ret = {entry}({source_args}); printf(\"%lld\", (long long)rf_ret);"
                )
            };
            let mut body = call_and_print;
            if has_ptr_param {
                for (i, t) in param_types.iter().enumerate() {
                    if matches!(t.as_str(), "int *" | "const int *") {
                        body.push_str(&format!(
                            " if (values[{i}] != before[{i}]) printf(\" p{i}=%d\", values[{i}]);"
                        ));
                    }
                }
            }
            body.push_str(" printf(\"\\n\");");
            body
        };
        let abi_shims = if job.c_boundary_files.is_empty() {
            String::new()
        } else {
            let mut shims = String::new();
            for func in &job.functions {
                if func.is_static || func.name == entry {
                    continue;
                }
                let ret_ok = func.return_type == "void"
                    || is_supported_entry_int(&func.return_type).is_some();
                let params_ok = func.parameter_types.iter().all(|p| {
                    is_supported_entry_int(p).is_some()
                        || matches!(p.as_str(), "int *" | "const int *")
                });
                if !ret_ok || !params_ok {
                    continue;
                }
                let plist = if func.parameter_types.is_empty() {
                    "void".to_string()
                } else {
                    func.parameter_types
                        .iter()
                        .enumerate()
                        .map(|(i, p)| format!("{p} v{i}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let alist = (0..func.parameter_types.len())
                    .map(|i| format!("v{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let call = if func.return_type == "void" {
                    format!("{}({alist});", func.name)
                } else {
                    format!("return {}({alist});", func.name)
                };
                shims.push_str(&format!(
                    "{} {}({plist});\n{} rf_abi_{}({plist}) {{ {call} }}\n",
                    func.return_type, func.name, func.return_type, func.name
                ));
            }
            shims
        };
        let source_harness = format!(
            "#include <stdio.h>\n#include <stdlib.h>\n#include <limits.h>\n_Static_assert(INT_MAX == 2147483647 && INT_MIN == (-2147483647 - 1), \"requires int32\");\n{abi_shims}{return_type} {entry}({args_types});\nint main(int argc, char **argv) {{ if(argc != {}) return 2; int values[{}]; int before[{}]; for(int i=0;i<{};i++) {{ values[i]=(int)strtol(argv[i+1],0,10); before[i]=values[i]; }} {c_body} return 0; }}\n",
            count + 1,
            count.max(1),
            count.max(1),
            count
        );
        std::fs::write(dir.join("driver.c"), source_harness)?;

        let snapshot = self.store.directory(&job.job_id)?.join("source");
        let kb = kbuild::load(&snapshot)?;
        let compile_flags = load_compile_commands(&snapshot, Path::new(&job.source_path))?;
        let original = dir.join(executable("original"));

        if compile_flags.is_empty() && kb.cflags.is_empty() && kb.dir_cflags.is_empty() {
            let mut args = vec![
                "-std=c11".into(),
                "-O0".into(),
                "-fno-builtin".into(),
                "-I".into(),
                snapshot.display().to_string(),
                "-fsanitize=undefined".into(),
                "-fno-sanitize-recover=all".into(),
            ];
            args.extend(
                job.source_files
                    .iter()
                    .filter(|f| {
                        f.path.ends_with(".c") && !job.inactive_source_files.contains(&f.path)
                    })
                    .map(|f| snapshot.join(&f.path).display().to_string()),
            );
            args.extend([
                "driver.c".into(),
                "-lm".into(),
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
        } else {
            let mut objects = Vec::new();
            for (idx, file) in job
                .source_files
                .iter()
                .filter(|f| f.path.ends_with(".c") && !job.inactive_source_files.contains(&f.path))
                .enumerate()
            {
                let obj = dir.join(format!("source-{idx}.o"));
                let mut args = vec![];
                let extra = compile_flags
                    .get(&file.path)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let kb_flags = kb.cflags_for(&file.path);
                if !extra.iter().any(|f| f.starts_with("-std="))
                    && !kb_flags.iter().any(|f| f.starts_with("-std="))
                {
                    args.push("-std=c11".into());
                }
                args.extend([
                    "-c".into(),
                    "-O0".into(),
                    "-fno-builtin".into(),
                    "-I".into(),
                    snapshot.display().to_string(),
                    "-fsanitize=undefined".into(),
                    "-fno-sanitize-recover=all".into(),
                ]);
                args.extend(kb_flags);
                args.extend(extra.iter().cloned());
                args.extend([
                    snapshot.join(&file.path).display().to_string(),
                    "-o".into(),
                    obj.display().to_string(),
                ]);
                let run = process::run(
                    "clang",
                    &args,
                    &dir,
                    &logs,
                    &format!("compile-source-{idx}"),
                    30,
                )
                .await?;
                let ok = run.success();
                evidence.commands.push(run);
                if !ok {
                    evidence.status = "source_failed".into();
                    bail!("original C build failed on {}", file.path);
                }
                objects.push(obj.display().to_string());
            }
            let mut link_args = vec![
                "-std=c11".into(),
                "-O0".into(),
                "-fsanitize=undefined".into(),
                "-fno-sanitize-recover=all".into(),
                "driver.c".into(),
            ];
            link_args.extend(objects);
            link_args.extend(["-lm".into(), "-o".into(), original.display().to_string()]);
            let run = process::run("clang", &link_args, &dir, &logs, "compile-source", 30).await?;
            let ok = run.success();
            evidence.commands.push(run);
            if !ok {
                evidence.status = "source_failed".into();
                bail!("original C link failed");
            }
        }

        // Compile any C boundary files without UBSan for linking into the Rust driver.
        let mut boundary_link_args = Vec::new();
        for (idx, rel_path) in job.c_boundary_files.iter().enumerate() {
            let obj = dir.join(format!("boundary-{idx}.o"));
            let mut args = vec![];
            let extra = compile_flags
                .get(rel_path)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let kb_flags = kb.cflags_for(rel_path);
            if !extra.iter().any(|f| f.starts_with("-std="))
                && !kb_flags.iter().any(|f| f.starts_with("-std="))
            {
                args.push("-std=c11".into());
            }
            args.extend([
                "-c".into(),
                "-O0".into(),
                "-fno-builtin".into(),
                "-I".into(),
                snapshot.display().to_string(),
            ]);
            args.extend(kb_flags);
            args.extend(extra.iter().cloned());
            args.extend([
                snapshot.join(rel_path).display().to_string(),
                "-o".into(),
                obj.display().to_string(),
            ]);
            let run = process::run(
                "clang",
                &args,
                &dir,
                &logs,
                &format!("compile-boundary-{idx}"),
                30,
            )
            .await?;
            let ok = run.success();
            evidence.commands.push(run);
            ensure!(ok, "C boundary build failed on {rel_path}");
            boundary_link_args.push(format!("-Clink-arg={}", obj.display()));
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
        let nostd_root = candidate.join("kbuild/rf_kernel_port.rs");
        if nostd_root.exists() {
            let nostd_rlib = dir.join("libreforge_kernel_nostd.rlib");
            let run = process::run(
                "rustc",
                &[
                    "--edition=2024".into(),
                    "--crate-type=rlib".into(),
                    "-C".into(),
                    "overflow-checks=yes".into(),
                    "--crate-name".into(),
                    "reforge_kernel_port".into(),
                    nostd_root.display().to_string(),
                    "-o".into(),
                    nostd_rlib.display().to_string(),
                ],
                &dir,
                &logs,
                "compile-kernel-nostd",
                30,
            )
            .await?;
            let ok = run.success();
            evidence.commands.push(run);
            ensure!(ok, "Kernel no_std rlib build failed");
        }
        let bindings_hdr = candidate.join("kbuild/rf_port_bindings.h");
        if bindings_hdr.exists() {
            let run = process::run(
                "clang",
                &[
                    "-std=c11".into(),
                    "-fsyntax-only".into(),
                    "-x".into(),
                    "c-header".into(),
                    bindings_hdr.display().to_string(),
                ],
                &dir,
                &logs,
                "check-kernel-bindings-header",
                30,
            )
            .await?;
            let ok = run.success();
            evidence.commands.push(run);
            ensure!(ok, "Kernel C bindings header check failed");
        }
        let include_path =
            serde_json::to_string(&candidate.join("src/lib.rs").display().to_string())?;
        let rust_args = param_types
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if matches!(t.as_str(), "int *" | "const int *") {
                    format!("unsafe {{ values.as_mut_ptr().add({i}) }}")
                } else if t == "int" {
                    format!("values[{i}]")
                } else if t == "_Bool" {
                    format!("u8::from(values[{i}] != 0)")
                } else if let Some(fty) = is_supported_entry_float(t) {
                    format!("(values[{i}] as {fty})")
                } else {
                    let (rty, _) = is_supported_entry_int(t).unwrap();
                    format!("(values[{i}] as {rty})")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let rust_body = if return_type == "void" {
            let mut body = format!("port::rf_{entry}({rust_args}); let mut rf_printed = false;");
            for (i, t) in param_types.iter().enumerate() {
                if matches!(t.as_str(), "int *" | "const int *") {
                    body.push_str(&format!(
                        " if values[{i}] != before[{i}] {{ print!(\"{{}}p{i}={{}}\", if rf_printed {{ \" \" }} else {{ \"\" }}, values[{i}]); rf_printed = true; }}"
                    ));
                }
            }
            body.push_str(" if !rf_printed { print!(\"void\"); } println!();");
            body
        } else if is_supported_entry_float(return_type).is_some() {
            let mut body = format!(
                "let rf_ret = port::rf_{entry}({rust_args}); print!(\"{{:.6}}\", rf_ret as f64);"
            );
            if has_ptr_param {
                for (i, t) in param_types.iter().enumerate() {
                    if matches!(t.as_str(), "int *" | "const int *") {
                        body.push_str(&format!(
                            " if values[{i}] != before[{i}] {{ print!(\" p{i}={{}}\", values[{i}]); }}"
                        ));
                    }
                }
            }
            body.push_str(" println!();");
            body
        } else if has_ptr_param {
            let mut body =
                format!("let rf_ret = port::rf_{entry}({rust_args}); print!(\"{{rf_ret}}\");");
            for (i, t) in param_types.iter().enumerate() {
                if matches!(t.as_str(), "int *" | "const int *") {
                    body.push_str(&format!(
                        " if values[{i}] != before[{i}] {{ print!(\" p{i}={{}}\", values[{i}]); }}"
                    ));
                }
            }
            body.push_str(" println!();");
            body
        } else {
            format!("println!(\"{{}}\", port::rf_{entry}({rust_args}));")
        };
        let driver = format!(
            "#[path={include_path}] mod port;\nfn main() {{ let mut values: Vec<i32> = std::env::args().skip(1).map(|v|v.parse().unwrap()).collect(); assert_eq!(values.len(),{count}); let before = values.clone(); {rust_body} }}\n"
        );
        std::fs::write(dir.join("driver.rs"), driver)?;
        let rust_bin = dir.join(executable("rust-port"));
        let mut rustc_args = vec![
            "--edition=2024".into(),
            "-C".into(),
            "overflow-checks=yes".into(),
            "driver.rs".into(),
            "-o".into(),
            rust_bin.display().to_string(),
        ];
        rustc_args.extend(boundary_link_args);
        let run =
            process::run("rustc", &rustc_args, &dir, &logs, "compile-rust-driver", 30).await?;
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
                    || relative == Path::new("src/lib.rs")
                    || relative == Path::new("kbuild/Kbuild")
                    || relative == Path::new("kbuild/rf_kernel_port.rs")
                    || relative == Path::new("kbuild/rf_port_bindings.h"),
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
    let mut hasher = Sha256::new();
    hasher.update(MANIFEST.as_bytes());
    hasher.update(&source);
    for rel in [
        "kbuild/Kbuild",
        "kbuild/rf_kernel_port.rs",
        "kbuild/rf_port_bindings.h",
    ] {
        let p = dir.join(rel);
        if p.exists() {
            let bytes = std::fs::read(p)?;
            hasher.update(rel.as_bytes());
            hasher.update(bytes);
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

fn record_key(sdef: &StructDef) -> (&'static str, &str) {
    if sdef.rust_name.starts_with("RfUnion_") {
        (
            "union",
            sdef.name.strip_prefix("union:").unwrap_or(&sdef.name),
        )
    } else {
        ("struct", &sdef.name)
    }
}

fn extract_record_value_dep(c_type: &str) -> Option<(&'static str, &str)> {
    let mut s = c_type.trim();
    while s.ends_with(']') {
        let open = s.rfind('[')?;
        s = s[..open].trim_end();
    }
    if s.contains('*') || s.contains('(') {
        return None;
    }
    if let Some(tag) = s.strip_prefix("struct ") {
        return Some(("struct", tag.trim()));
    }
    if let Some(tag) = s.strip_prefix("union ") {
        return Some(("union", tag.trim()));
    }
    None
}

fn ordered_records(structs: &[StructDef]) -> Vec<&StructDef> {
    let mut by_key = BTreeMap::new();
    for sdef in structs {
        by_key.insert(record_key(sdef), sdef);
    }
    let mut visited = BTreeSet::new();
    let mut visiting = BTreeSet::new();
    let mut out = Vec::with_capacity(structs.len());
    fn visit<'a>(
        key: (&'static str, &'a str),
        by_key: &BTreeMap<(&'static str, &'a str), &'a StructDef>,
        visited: &mut BTreeSet<(&'static str, &'a str)>,
        visiting: &mut BTreeSet<(&'static str, &'a str)>,
        out: &mut Vec<&'a StructDef>,
    ) {
        if visited.contains(&key) || !visiting.insert(key) {
            return;
        }
        if let Some(&sdef) = by_key.get(&key) {
            for field in &sdef.fields {
                if let Some(dep) = extract_record_value_dep(&field.c_type)
                    && by_key.contains_key(&dep)
                {
                    visit(dep, by_key, visited, visiting, out);
                }
            }
            out.push(sdef);
        }
        visiting.remove(&key);
        visited.insert(key);
    }
    for sdef in structs {
        visit(
            record_key(sdef),
            &by_key,
            &mut visited,
            &mut visiting,
            &mut out,
        );
    }
    out
}

fn format_c_decl(c_type: &str, ident: &str) -> String {
    if let Some((prefix, rest)) = c_type.split_once("(*)") {
        if rest.starts_with('(') {
            let mut depth = 0usize;
            let mut close_idx = None;
            for (idx, ch) in rest.char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            close_idx = Some(idx);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(end) = close_idx {
                let params_part = &rest[..=end];
                let trailing = rest[end + 1..].trim();
                if !trailing.is_empty() && trailing.starts_with('[') {
                    return format!("{prefix}(*{ident}{trailing}){params_part}");
                }
            }
        }
        return format!("{prefix}(*{ident}){rest}");
    }
    if let Some(idx) = c_type.find('[') {
        let base = c_type[..idx].trim_end();
        let dims = &c_type[idx..];
        if dims.trim_end().ends_with('*')
            && let Some(star) = dims.rfind('*')
        {
            return format!("{base} (*{ident}){}", dims[..star].trim_end());
        }
        return format!("{base} {ident}{dims}");
    }
    format!("{c_type} {ident}")
}

fn render_kbuild_file(job: &Job) -> String {
    let mut out = String::from("# Generated by Reforge-MCP kernel no_std profile\n");
    for (k, v) in &job.kconfig {
        out.push_str(&format!("# Kconfig: {k}={v}\n"));
    }
    out.push_str("obj-y += rf_kernel_port.o\n");
    out
}

fn render_bindings_header(job: &Job) -> String {
    let mut out = String::from(
        "#ifndef REFORGE_PORT_BINDINGS_H\n#define REFORGE_PORT_BINDINGS_H\n\n#include <stdbool.h>\n#include <stddef.h>\n#include <stdint.h>\n\n",
    );
    let records = ordered_records(&job.structs);
    for sdef in &records {
        let (kind, tag) = record_key(sdef);
        out.push_str(&format!("{kind} {tag};\n"));
    }
    if !records.is_empty() {
        out.push('\n');
    }
    for sdef in &records {
        let (kind, tag) = record_key(sdef);
        out.push_str(&format!("{kind} {tag} {{\n"));
        for field in &sdef.fields {
            out.push_str(&format!(
                "    {};\n",
                format_c_decl(&field.c_type, &field.name)
            ));
        }
        out.push_str("};\n\n");
    }
    for function in &job.functions {
        if function.is_static {
            continue;
        }
        let params = if function.parameters.is_empty() {
            "void".to_string()
        } else if function.parameter_types.len() == function.parameters.len() {
            function
                .parameter_types
                .iter()
                .zip(&function.parameters)
                .map(|(pty, pname)| format_c_decl(pty, pname))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            function
                .parameters
                .iter()
                .map(|pname| format!("int {pname}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let ret = if function.return_type.is_empty() {
            "int"
        } else {
            function.return_type.as_str()
        };
        out.push_str(&format!(
            "{};\n",
            format_c_decl(ret, &format!("rf_abi_{}({params})", function.name))
        ));
    }
    out.push_str("\n#endif /* REFORGE_PORT_BINDINGS_H */\n");
    out
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
fn is_supported_entry_float(ty: &str) -> Option<&'static str> {
    match ty {
        "float" => Some("f32"),
        "double" => Some("f64"),
        _ => None,
    }
}

fn is_supported_entry_int(ty: &str) -> Option<(&'static str, bool)> {
    Some(match ty {
        "_Bool" => ("u8", true),
        "char" => {
            let unsigned = cfg!(all(
                target_os = "linux",
                any(target_arch = "aarch64", target_arch = "arm")
            ));
            (if unsigned { "u8" } else { "i8" }, unsigned)
        }
        "signed char" => ("i8", false),
        "unsigned char" => ("u8", true),
        "short" => ("i16", false),
        "unsigned short" => ("u16", true),
        "int" => ("i32", false),
        "unsigned int" => ("u32", true),
        "long" | "long long" => ("i64", false),
        "unsigned long" | "unsigned long long" => ("u64", true),
        _ => return None,
    })
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
        let is_c_or_h = matches!(
            entry.path().extension().and_then(|s| s.to_str()),
            Some("c" | "h")
        );
        let file_name = entry.path().file_name().and_then(|s| s.to_str());
        let is_root_meta = matches!(file_name, Some("compile_commands.json" | ".config"))
            && entry.path().parent() == Some(root);
        let is_sub_kbuild = matches!(
            file_name,
            Some("Kbuild" | "Kconfig" | "Makefile" | "CMakeLists.txt")
        );
        if !is_c_or_h && !is_root_meta && !is_sub_kbuild {
            continue;
        }
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
            ensure!(
                files.len() <= MAX_FILES,
                "source exceeds {MAX_FILES} file limit"
            );
        }
    }
    files.sort();
    Ok(files)
}

fn is_c_boundary_file(path: &str) -> bool {
    let in_boundary_dir = Path::new(path)
        .components()
        .any(|c| matches!(c.as_os_str().to_str(), Some("ffi" | "external")));
    in_boundary_dir || path.ends_with(".ffi.c") || path.ends_with(".external.c")
}

#[derive(Deserialize)]
struct CompileCommandEntry {
    #[serde(default)]
    directory: String,
    file: String,
    #[serde(default)]
    arguments: Vec<String>,
    #[serde(default)]
    command: String,
}

fn canonicalize_soft(path: &Path) -> PathBuf {
    if let Ok(canon) = path.canonicalize() {
        return canon;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(canon_parent) = parent.canonicalize()
    {
        return canon_parent.join(name);
    }
    path.to_path_buf()
}

fn rewrite_compile_path(raw: &str, snapshot: &Path, original_root: &Path) -> String {
    let p = Path::new(raw);
    if p.is_relative() {
        return snapshot.join(p).display().to_string();
    }
    let canon = canonicalize_soft(p);
    if let Ok(rel) = canon.strip_prefix(original_root) {
        return snapshot.join(rel).display().to_string();
    }
    raw.to_owned()
}

fn load_compile_commands(
    snapshot: &Path,
    original_root: &Path,
) -> Result<BTreeMap<String, Vec<String>>> {
    let db_path = snapshot.join("compile_commands.json");
    if !db_path.exists() {
        return Ok(BTreeMap::new());
    }
    let entries: Vec<CompileCommandEntry> =
        serde_json::from_str(&std::fs::read_to_string(&db_path)?)
            .context("invalid compile_commands.json")?;
    let mut map = BTreeMap::new();
    for entry in entries {
        let tokens: Vec<String> = if !entry.arguments.is_empty() {
            entry.arguments
        } else {
            entry
                .command
                .split_whitespace()
                .map(str::to_owned)
                .collect()
        };
        if tokens.is_empty() {
            continue;
        }
        let file_path = Path::new(&entry.file);
        let rel = if file_path.is_absolute() {
            let canon = canonicalize_soft(file_path);
            if let Ok(r) = canon.strip_prefix(original_root) {
                r.to_path_buf()
            } else {
                file_path.to_path_buf()
            }
        } else if !entry.directory.is_empty() {
            let joined = canonicalize_soft(&Path::new(&entry.directory).join(file_path));
            if let Ok(r) = joined.strip_prefix(original_root) {
                r.to_path_buf()
            } else {
                file_path.to_path_buf()
            }
        } else {
            file_path.to_path_buf()
        };
        let Some(rel_str) = rel.to_str() else {
            continue;
        };
        let mut flags = Vec::new();
        let mut idx = 1; // skip compiler binary
        while idx < tokens.len() {
            let tok = &tokens[idx];
            if matches!(tok.as_str(), "-I" | "-isystem" | "-D" | "-U" | "-include")
                && idx + 1 < tokens.len()
            {
                let val = &tokens[idx + 1];
                flags.push(tok.clone());
                if matches!(tok.as_str(), "-I" | "-isystem" | "-include") {
                    flags.push(rewrite_compile_path(val, snapshot, original_root));
                } else {
                    flags.push(val.clone());
                }
                idx += 2;
                continue;
            }
            if let Some(rest) = tok.strip_prefix("-I")
                && !rest.is_empty()
            {
                flags.push(format!(
                    "-I{}",
                    rewrite_compile_path(rest, snapshot, original_root)
                ));
            } else if tok.starts_with("-std=")
                || tok.starts_with("-D")
                || tok.starts_with("-U")
                || (tok.starts_with("-W") && !tok.starts_with("-Wl,"))
                || (tok.starts_with("-f")
                    && !tok.starts_with("-fplugin")
                    && !tok.starts_with("-fsanitize"))
            {
                flags.push(tok.clone());
            }
            idx += 1;
        }
        map.insert(rel_str.to_owned(), flags);
    }
    Ok(map)
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
    let internal_names: BTreeSet<&str> = functions.iter().map(|f| f.name.as_str()).collect();
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
                        .all(|dep| {
                            !internal_names.contains(dep.as_str())
                                || group.contains(dep)
                                || done.contains(dep)
                        })
                })
            })
            .expect("SCC condensation is acyclic");
        let group = groups.remove(ready);
        done.extend(group.iter().cloned());
        ordered.push(group);
    }
    ordered
}
