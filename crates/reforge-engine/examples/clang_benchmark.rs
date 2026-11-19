use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Manifest {
    jobs: Vec<BenchJob>,
    #[serde(default)]
    workers: Option<usize>,
    #[serde(default = "default_true")]
    compile_rustc: bool,
    #[serde(default = "default_profile")]
    profile: String,
    out_dir: String,
}

fn default_true() -> bool {
    true
}

fn default_profile() -> String {
    "strict".into()
}

#[derive(Deserialize)]
struct BenchJob {
    id: String,
    corpus: String,
    root: String,
    file: String,
    args: Vec<String>,
}

#[derive(Serialize)]
struct UnitResult {
    id: String,
    corpus: String,
    stage: String,
    functions: usize,
    safe_wrappers: usize,
    error: Option<String>,
}

#[derive(Serialize)]
struct BlockerBucket {
    category: String,
    count: usize,
    sample_id: String,
    sample_error: String,
}

#[derive(Serialize)]
struct BenchSummary {
    profile: String,
    total_units: usize,
    clang_ok: usize,
    translated_ok: usize,
    translated_empty: usize,
    rustc_nostd_ok: usize,
    total_functions: usize,
    total_safe_wrappers: usize,
    top_translate_blockers: Vec<BlockerBucket>,
    top_rustc_blockers: Vec<BlockerBucket>,
    top_clang_failures: Vec<BlockerBucket>,
    results: Vec<UnitResult>,
}

fn normalize_blocker(err: &str) -> String {
    let first = err.lines().next().unwrap_or(err).trim();
    if let Some(idx) = first.find(" unsupported") {
        let _ = idx;
    }
    let tokens: Vec<&str> = first.split_whitespace().collect();
    if let Some(pos) = tokens.iter().position(|t| {
        *t == "expression"
            || *t == "castKind"
            || *t == "statement"
            || *t == "declaration"
            || *t == "top-level"
            || *t == "builtin"
    }) && pos + 1 < tokens.len()
        && first.starts_with("unsupported ")
    {
        return format!("{} {} {}", tokens[0], tokens[1], tokens[2]);
    }
    if first.starts_with("unsupported type ") {
        return format!(
            "unsupported type {}",
            first.strip_prefix("unsupported type ").unwrap_or("")
        );
    }
    if first.starts_with("unsupported ") {
        return tokens.iter().take(4).copied().collect::<Vec<_>>().join(" ");
    }
    if first.starts_with("unresolved or external call:") {
        return "unresolved or external call".into();
    }
    if first.starts_with("unknown struct ") {
        return "unknown struct definition".into();
    }
    if first.starts_with("mutable global variable ") {
        return "mutable global variable".into();
    }
    if first.starts_with("call to ") {
        return tokens.iter().skip(3).copied().collect::<Vec<_>>().join(" ");
    }
    if first.len() > 72 {
        let mut end = 72;
        while !first.is_char_boundary(end) {
            end -= 1;
        }
        return first[..end].to_string();
    }
    first.to_string()
}

fn record_bucket(
    map: &mut BTreeMap<String, (usize, String, String)>,
    category: String,
    id: &str,
    err: &str,
) {
    map.entry(category)
        .and_modify(|e| e.0 += 1)
        .or_insert_with(|| {
            (
                1,
                id.to_owned(),
                err.lines().next().unwrap_or(err).to_owned(),
            )
        });
}

fn sort_buckets(
    map: BTreeMap<String, (usize, String, String)>,
    limit: usize,
) -> Vec<BlockerBucket> {
    let mut items: Vec<BlockerBucket> = map
        .into_iter()
        .map(
            |(category, (count, sample_id, sample_error))| BlockerBucket {
                category,
                count,
                sample_id,
                sample_error,
            },
        )
        .collect();
    items.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.category.cmp(&b.category))
    });
    items.truncate(limit);
    items
}

fn run_one(
    idx: usize,
    job: &BenchJob,
    out_dir: &Path,
    compile_rustc: bool,
    options: reforge_engine::TranslateOptions,
) -> UnitResult {
    let clang_out = match Command::new("clang").args(&job.args).output() {
        Ok(o) => o,
        Err(e) => {
            return UnitResult {
                id: job.id.clone(),
                corpus: job.corpus.clone(),
                stage: "clang_failed".into(),
                functions: 0,
                safe_wrappers: 0,
                error: Some(format!("failed to invoke clang: {e}")),
            };
        }
    };
    if !clang_out.status.success() {
        let stderr = String::from_utf8_lossy(&clang_out.stderr);
        let msg = stderr
            .lines()
            .find(|l| l.contains("error:"))
            .or_else(|| stderr.lines().next())
            .unwrap_or("clang failed")
            .trim()
            .to_owned();
        return UnitResult {
            id: job.id.clone(),
            corpus: job.corpus.clone(),
            stage: "clang_failed".into(),
            functions: 0,
            safe_wrappers: 0,
            error: Some(msg),
        };
    }

    let ast: serde_json::Value = match serde_json::from_slice(&clang_out.stdout) {
        Ok(v) => v,
        Err(e) => {
            return UnitResult {
                id: job.id.clone(),
                corpus: job.corpus.clone(),
                stage: "translate_failed".into(),
                functions: 0,
                safe_wrappers: 0,
                error: Some(format!("invalid clang AST JSON: {e}")),
            };
        }
    };

    let unit = match reforge_engine::translate_ast_with_options(
        &ast,
        Path::new(&job.file),
        Path::new(&job.root),
        options,
    ) {
        Ok(u) => u,
        Err(e) => {
            return UnitResult {
                id: job.id.clone(),
                corpus: job.corpus.clone(),
                stage: "translate_failed".into(),
                functions: 0,
                safe_wrappers: 0,
                error: Some(e.to_string()),
            };
        }
    };

    let fn_count = unit.functions.len();
    let safe_count = unit
        .functions
        .iter()
        .filter(|f| f.safe_rust_wrapper.is_some())
        .count();

    if !compile_rustc {
        return UnitResult {
            id: job.id.clone(),
            corpus: job.corpus.clone(),
            stage: if fn_count == 0 {
                "translated_empty"
            } else {
                "translated_ok"
            }
            .into(),
            functions: fn_count,
            safe_wrappers: safe_count,
            error: None,
        };
    }

    let rust_body = reforge_engine::render_rust_module(
        &unit.structs,
        &unit.constants,
        &unit.prototypes,
        &unit.functions,
    );
    let rs_path = out_dir.join(format!("unit_{idx}.rs"));
    let rlib_path = out_dir.join(format!("libunit_{idx}.rlib"));
    let nostd_src = format!("#![no_std]\n{rust_body}");
    if let Err(e) = std::fs::write(&rs_path, nostd_src) {
        return UnitResult {
            id: job.id.clone(),
            corpus: job.corpus.clone(),
            stage: "rustc_failed".into(),
            functions: fn_count,
            safe_wrappers: safe_count,
            error: Some(format!("write failed: {e}")),
        };
    }

    let rustc_out = Command::new("rustc")
        .args([
            "--edition=2024",
            "--crate-type=rlib",
            "--crate-name",
            &format!("rf_bench_{idx}"),
            &rs_path.display().to_string(),
            "-o",
            &rlib_path.display().to_string(),
        ])
        .output();
    let _ = std::fs::remove_file(&rs_path);
    let _ = std::fs::remove_file(&rlib_path);

    match rustc_out {
        Ok(o) if o.status.success() => UnitResult {
            id: job.id.clone(),
            corpus: job.corpus.clone(),
            stage: if fn_count == 0 {
                "translated_empty"
            } else {
                "rustc_nostd_ok"
            }
            .into(),
            functions: fn_count,
            safe_wrappers: safe_count,
            error: None,
        },
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let msg = stderr
                .lines()
                .find(|l| l.starts_with("error[") || l.starts_with("error:"))
                .or_else(|| stderr.lines().next())
                .unwrap_or("rustc failed")
                .trim()
                .to_owned();
            UnitResult {
                id: job.id.clone(),
                corpus: job.corpus.clone(),
                stage: "rustc_failed".into(),
                functions: fn_count,
                safe_wrappers: safe_count,
                error: Some(msg),
            }
        }
        Err(e) => UnitResult {
            id: job.id.clone(),
            corpus: job.corpus.clone(),
            stage: "rustc_failed".into(),
            functions: fn_count,
            safe_wrappers: safe_count,
            error: Some(format!("failed to invoke rustc: {e}")),
        },
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let manifest_path = args
        .windows(2)
        .find(|w| w[0] == "--manifest")
        .map(|w| PathBuf::from(&w[1]))
        .context("usage: clang_benchmark --manifest <path> [--output <path>]")?;
    let output_path = args
        .windows(2)
        .find(|w| w[0] == "--output")
        .map(|w| PathBuf::from(&w[1]));

    let manifest: Manifest = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path).context("failed to read manifest")?,
    )
    .context("invalid benchmark manifest JSON")?;

    let out_dir = PathBuf::from(&manifest.out_dir);
    std::fs::create_dir_all(&out_dir)?;

    let options = match manifest.profile.as_str() {
        "strict" => reforge_engine::TranslateOptions::strict(),
        "general" => reforge_engine::TranslateOptions::general_c(),
        other => anyhow::bail!("unknown benchmark profile {other:?} (expected strict or general)"),
    };

    let workers = manifest.workers.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 16)
    });
    let total = manifest.jobs.len();
    let next_idx = AtomicUsize::new(0);
    let collected: Mutex<Vec<Option<UnitResult>>> = Mutex::new((0..total).map(|_| None).collect());

    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next_idx.fetch_add(1, Ordering::Relaxed);
                    if i >= total {
                        break;
                    }
                    let res = run_one(
                        i,
                        &manifest.jobs[i],
                        &out_dir,
                        manifest.compile_rustc,
                        options,
                    );
                    collected.lock().unwrap()[i] = Some(res);
                }
            });
        }
    });

    let results: Vec<UnitResult> = collected
        .into_inner()
        .unwrap()
        .into_iter()
        .map(Option::unwrap)
        .collect();

    let mut clang_ok = 0usize;
    let mut translated_ok = 0usize;
    let mut translated_empty = 0usize;
    let mut rustc_nostd_ok = 0usize;
    let mut total_functions = 0usize;
    let mut total_safe_wrappers = 0usize;
    let mut clang_buckets = BTreeMap::new();
    let mut translate_buckets = BTreeMap::new();
    let mut rustc_buckets = BTreeMap::new();

    for r in &results {
        total_functions += r.functions;
        total_safe_wrappers += r.safe_wrappers;
        match r.stage.as_str() {
            "clang_failed" => {
                if let Some(err) = &r.error {
                    record_bucket(&mut clang_buckets, normalize_blocker(err), &r.id, err);
                }
            }
            "translate_failed" => {
                clang_ok += 1;
                if let Some(err) = &r.error {
                    record_bucket(&mut translate_buckets, normalize_blocker(err), &r.id, err);
                }
            }
            "translated_empty" => {
                clang_ok += 1;
                translated_empty += 1;
            }
            "translated_ok" => {
                clang_ok += 1;
                translated_ok += 1;
            }
            "rustc_nostd_ok" => {
                clang_ok += 1;
                translated_ok += 1;
                rustc_nostd_ok += 1;
            }
            "rustc_failed" => {
                clang_ok += 1;
                translated_ok += 1;
                if let Some(err) = &r.error {
                    record_bucket(&mut rustc_buckets, normalize_blocker(err), &r.id, err);
                }
            }
            _ => {}
        }
    }

    let summary = BenchSummary {
        profile: manifest.profile,
        total_units: results.len(),
        clang_ok,
        translated_ok,
        translated_empty,
        rustc_nostd_ok,
        total_functions,
        total_safe_wrappers,
        top_translate_blockers: sort_buckets(translate_buckets, 40),
        top_rustc_blockers: sort_buckets(rustc_buckets, 25),
        top_clang_failures: sort_buckets(clang_buckets, 15),
        results,
    };

    let json = serde_json::to_string_pretty(&summary)?;
    if let Some(out) = output_path {
        std::fs::write(out, json)?;
    } else {
        println!("{json}");
    }
    Ok(())
}
