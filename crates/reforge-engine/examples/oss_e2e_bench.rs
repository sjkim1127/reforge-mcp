use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use reforge_engine::TranslateOptions;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Manifest {
    projects: Vec<ProjectJob>,
}

#[derive(Deserialize)]
struct ProjectJob {
    name: String,
    root: String,
    out_dir: String,
    crate_type: String,
    units: Vec<UnitJob>,
}

#[derive(Deserialize)]
struct UnitJob {
    file: String,
    args: Vec<String>,
}

#[derive(Serialize)]
struct UnitStatus {
    file: String,
    stage: String,
    functions: usize,
    error: Option<String>,
}

#[derive(Serialize)]
struct ProjectReport {
    name: String,
    stage: String,
    units_total: usize,
    units_ok: usize,
    functions: usize,
    artifact: Option<String>,
    rust_source: Option<String>,
    error: Option<String>,
    units: Vec<UnitStatus>,
}

#[derive(Serialize)]
struct BenchReport {
    projects: Vec<ProjectReport>,
}

fn sanitize_crate_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() || out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("rf_{out}")
    } else {
        out
    }
}

fn run_project(job: &ProjectJob) -> Result<ProjectReport> {
    let out_dir = PathBuf::from(&job.out_dir);
    std::fs::create_dir_all(&out_dir)?;

    let mut unit_statuses = Vec::with_capacity(job.units.len());
    let mut modules_src = String::from(
        "#![allow(dead_code, non_snake_case, non_camel_case_types, non_upper_case_globals, unused_imports)]\n",
    );
    let mut total_functions = 0usize;
    let mut units_ok = 0usize;
    let mut main_entry: Option<(usize, usize)> = None;
    let mut first_error: Option<String> = None;

    for (idx, unit) in job.units.iter().enumerate() {
        let clang_out = Command::new("clang").args(&unit.args).output()?;
        if !clang_out.status.success() {
            let stderr = String::from_utf8_lossy(&clang_out.stderr);
            let msg = stderr
                .lines()
                .find(|l| l.contains("error:"))
                .or_else(|| stderr.lines().next())
                .unwrap_or("clang failed")
                .trim()
                .to_owned();
            if first_error.is_none() {
                first_error = Some(format!("{}: {msg}", unit.file));
            }
            unit_statuses.push(UnitStatus {
                file: unit.file.clone(),
                stage: "clang_failed".into(),
                functions: 0,
                error: Some(msg),
            });
            continue;
        }

        let ast: serde_json::Value = match serde_json::from_slice(&clang_out.stdout) {
            Ok(v) => v,
            Err(e) => {
                let msg = format!("invalid clang AST JSON: {e}");
                if first_error.is_none() {
                    first_error = Some(format!("{}: {msg}", unit.file));
                }
                unit_statuses.push(UnitStatus {
                    file: unit.file.clone(),
                    stage: "translate_failed".into(),
                    functions: 0,
                    error: Some(msg),
                });
                continue;
            }
        };

        let translated = match reforge_engine::translate_ast_with_options(
            &ast,
            Path::new(&unit.file),
            Path::new(&job.root),
            TranslateOptions::drop_in_c_abi(),
        ) {
            Ok(u) => u,
            Err(e) => {
                let msg = e.to_string();
                if first_error.is_none() {
                    first_error = Some(format!("{}: {msg}", unit.file));
                }
                unit_statuses.push(UnitStatus {
                    file: unit.file.clone(),
                    stage: "translate_failed".into(),
                    functions: 0,
                    error: Some(msg),
                });
                continue;
            }
        };

        units_ok += 1;
        total_functions += translated.functions.len();
        if main_entry.is_none()
            && let Some(main_fn) = translated.functions.iter().find(|f| f.name == "main")
        {
            main_entry = Some((idx, main_fn.parameter_types.len()));
        }
        let tu_rust = reforge_engine::render_rust_module(
            &translated.structs,
            &translated.constants,
            &translated.prototypes,
            &translated.functions,
        );
        modules_src.push_str(&format!("pub mod tu_{idx} {{\n{tu_rust}\n}}\n"));
        unit_statuses.push(UnitStatus {
            file: unit.file.clone(),
            stage: "ok".into(),
            functions: translated.functions.len(),
            error: None,
        });
    }

    if units_ok != job.units.len() {
        return Ok(ProjectReport {
            name: job.name.clone(),
            stage: "translate_failed".into(),
            units_total: job.units.len(),
            units_ok,
            functions: total_functions,
            artifact: None,
            rust_source: None,
            error: first_error,
            units: unit_statuses,
        });
    }

    if job.crate_type == "bin" {
        let Some((tu_idx, argc_kind)) = main_entry else {
            return Ok(ProjectReport {
                name: job.name.clone(),
                stage: "rustc_failed".into(),
                units_total: job.units.len(),
                units_ok,
                functions: total_functions,
                artifact: None,
                rust_source: None,
                error: Some("crate_type=bin requested but no main() function found".into()),
                units: unit_statuses,
            });
        };
        if argc_kind == 0 {
            modules_src.push_str(&format!(
                "fn main() {{\n    std::process::exit(tu_{tu_idx}::rf_main());\n}}\n"
            ));
        } else {
            modules_src.push_str(&format!(
                "fn main() {{\n    let c_args: Vec<std::ffi::CString> = std::env::args_os()\n        .map(|s| std::ffi::CString::new(s.to_string_lossy().as_bytes()).unwrap_or_default())\n        .collect();\n    let mut ptrs: Vec<*mut i8> = c_args.iter().map(|s| s.as_ptr() as *mut i8).collect();\n    ptrs.push(core::ptr::null_mut());\n    let argc = (ptrs.len() - 1) as i32;\n    let code = tu_{tu_idx}::rf_main(argc, ptrs.as_mut_ptr());\n    std::process::exit(code);\n}}\n"
            ));
        }
    }

    let sanitized = sanitize_crate_name(&job.name);
    let rs_path = out_dir.join(format!("{sanitized}.rs"));
    std::fs::write(&rs_path, &modules_src)?;

    let artifact_path = if job.crate_type == "bin" {
        out_dir.join(format!("{sanitized}_rs"))
    } else {
        out_dir.join(format!("lib{sanitized}_rs.a"))
    };

    let rustc_out = Command::new("rustc")
        .args([
            "--edition=2024",
            "-O",
            "--crate-type",
            &job.crate_type,
            "--crate-name",
            &format!("{sanitized}_rs"),
            &rs_path.display().to_string(),
            "-o",
            &artifact_path.display().to_string(),
        ])
        .output()?;

    if rustc_out.status.success() {
        Ok(ProjectReport {
            name: job.name.clone(),
            stage: "rustc_ok".into(),
            units_total: job.units.len(),
            units_ok,
            functions: total_functions,
            artifact: Some(artifact_path.display().to_string()),
            rust_source: Some(rs_path.display().to_string()),
            error: None,
            units: unit_statuses,
        })
    } else {
        let stderr = String::from_utf8_lossy(&rustc_out.stderr);
        let msg = stderr.lines().take(15).collect::<Vec<_>>().join("\n");
        Ok(ProjectReport {
            name: job.name.clone(),
            stage: "rustc_failed".into(),
            units_total: job.units.len(),
            units_ok,
            functions: total_functions,
            artifact: None,
            rust_source: Some(rs_path.display().to_string()),
            error: Some(msg),
            units: unit_statuses,
        })
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let manifest_path = args
        .windows(2)
        .find(|w| w[0] == "--manifest")
        .map(|w| PathBuf::from(&w[1]))
        .context("usage: oss_e2e_bench --manifest <path> [--output <path>]")?;
    let output_path = args
        .windows(2)
        .find(|w| w[0] == "--output")
        .map(|w| PathBuf::from(&w[1]));

    let manifest: Manifest = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path).context("failed to read manifest")?,
    )
    .context("invalid manifest JSON")?;

    let mut reports = Vec::with_capacity(manifest.projects.len());
    for proj in &manifest.projects {
        reports.push(run_project(proj)?);
    }

    let json = serde_json::to_string_pretty(&BenchReport { projects: reports })?;
    if let Some(out) = output_path {
        std::fs::write(out, json)?;
    } else {
        println!("{json}");
    }
    Ok(())
}
