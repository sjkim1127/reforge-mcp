//! Recursive Kbuild and Kconfig (`.config`, `Kbuild`, `Makefile`) build-context collector.
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};
use walkdir::WalkDir;

#[derive(Debug, Default)]
pub struct KbuildContext {
    pub kconfig: BTreeMap<String, String>,
    pub cflags: Vec<String>,
    pub dir_cflags: BTreeMap<String, Vec<String>>,
    pub active_units: BTreeSet<String>,
    pub inactive_units: Vec<String>,
}

impl KbuildContext {
    pub fn cflags_for(&self, rel_file: &str) -> Vec<String> {
        let mut flags = self.cflags.clone();
        let mut cur = Path::new(rel_file).parent();
        let mut dir_layers = Vec::new();
        while let Some(parent) = cur {
            let key = normalize_rel(parent);
            if !key.is_empty()
                && let Some(df) = self.dir_cflags.get(&key)
            {
                dir_layers.push(df);
            }
            cur = parent.parent();
        }
        for df in dir_layers.into_iter().rev() {
            flags.extend(df.iter().cloned());
        }
        flags
    }
}

pub fn load(snapshot: &Path) -> Result<KbuildContext> {
    let mut ctx = KbuildContext::default();
    let dot_config = snapshot.join(".config");
    if dot_config.is_file() {
        let raw = std::fs::read_to_string(&dot_config)?;
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("# ")
                && let Some(key) = rest.strip_suffix(" is not set")
            {
                let key = key.trim();
                if key.starts_with("CONFIG_") {
                    ctx.kconfig.insert(key.to_owned(), "n".into());
                }
                continue;
            }
            if line.starts_with('#') {
                continue;
            }
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim();
                if !key.starts_with("CONFIG_") {
                    continue;
                }
                ctx.kconfig.insert(key.to_owned(), val.to_owned());
                match val {
                    "y" => ctx.cflags.push(format!("-D{key}=1")),
                    "m" => ctx.cflags.push(format!("-D{key}_MODULE=1")),
                    "n" => {}
                    _ => ctx.cflags.push(format!("-D{key}={val}")),
                }
            }
        }
    }

    let mut inactive = BTreeSet::new();
    let mut visited_dirs = BTreeSet::new();
    scan_dir(
        snapshot,
        Path::new(""),
        &mut ctx,
        &mut inactive,
        &mut visited_dirs,
        0,
    )?;

    for active in &ctx.active_units {
        inactive.remove(active);
    }
    ctx.inactive_units = inactive.into_iter().collect();
    Ok(ctx)
}

fn scan_dir(
    snapshot: &Path,
    rel_dir: &Path,
    ctx: &mut KbuildContext,
    inactive: &mut BTreeSet<String>,
    visited_dirs: &mut BTreeSet<String>,
    depth: usize,
) -> Result<()> {
    if depth >= 16 {
        return Ok(());
    }
    let dir_key = normalize_rel(rel_dir);
    if !visited_dirs.insert(dir_key.clone()) {
        return Ok(());
    }
    let abs_dir = if rel_dir.as_os_str().is_empty() {
        snapshot.to_path_buf()
    } else {
        snapshot.join(rel_dir)
    };
    if !abs_dir.is_dir() {
        return Ok(());
    }

    let mut active_subdirs = Vec::new();
    for name in ["Kbuild", "Makefile"] {
        let path = abs_dir.join(name);
        if !path.is_file() {
            continue;
        }
        let raw = std::fs::read_to_string(&path)?;
        for logical in logical_lines(&raw) {
            let line = strip_comment(&logical).trim();
            if line.is_empty() {
                continue;
            }
            let Some((lhs, rhs)) = split_assignment(line) else {
                continue;
            };
            let lhs_expanded = expand_kconfig(lhs.trim(), &ctx.kconfig);
            let rhs_expanded = expand_kconfig(rhs.trim(), &ctx.kconfig);
            match lhs_expanded.as_str() {
                "ccflags-y" | "ccflags-m" | "EXTRA_CFLAGS" => {
                    let target_flags = if dir_key.is_empty() {
                        &mut ctx.cflags
                    } else {
                        ctx.dir_cflags.entry(dir_key.clone()).or_default()
                    };
                    collect_cflags(&rhs_expanded, &abs_dir, target_flags);
                }
                "obj-y" | "obj-m" => {
                    for token in rhs_expanded.split_whitespace() {
                        if let Some(sub) = token.strip_suffix('/') {
                            if is_safe_rel(sub) {
                                active_subdirs.push(rel_dir.join(sub));
                            }
                        } else if let Some(stem) = token.strip_suffix(".o")
                            && is_safe_rel(stem)
                        {
                            let unit = normalize_rel(&rel_dir.join(format!("{stem}.c")));
                            ctx.active_units.insert(unit);
                        }
                    }
                }
                "obj-n" | "obj-" => {
                    for token in rhs_expanded.split_whitespace() {
                        if let Some(sub) = token.strip_suffix('/') {
                            if is_safe_rel(sub) {
                                collect_inactive_c_files(snapshot, &rel_dir.join(sub), inactive)?;
                            }
                        } else if let Some(stem) = token.strip_suffix(".o")
                            && is_safe_rel(stem)
                        {
                            let unit = normalize_rel(&rel_dir.join(format!("{stem}.c")));
                            inactive.insert(unit);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    for sub in active_subdirs {
        scan_dir(snapshot, &sub, ctx, inactive, visited_dirs, depth + 1)?;
    }
    Ok(())
}

fn collect_inactive_c_files(
    snapshot: &Path,
    rel_sub: &Path,
    inactive: &mut BTreeSet<String>,
) -> Result<()> {
    let abs = snapshot.join(rel_sub);
    if !abs.is_dir() {
        return Ok(());
    }
    for entry in WalkDir::new(&abs).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|s| s.to_str()) == Some("c")
            && let Ok(rel) = entry.path().strip_prefix(snapshot)
        {
            inactive.insert(normalize_rel(rel));
        }
    }
    Ok(())
}

fn is_safe_rel(raw: &str) -> bool {
    let p = Path::new(raw);
    !p.is_absolute()
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

fn normalize_rel(p: &Path) -> String {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn logical_lines(raw: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    for line in raw.lines() {
        let trimmed = line.trim_end();
        if let Some(prefix) = trimmed.strip_suffix('\\') {
            current.push_str(prefix);
            current.push(' ');
        } else {
            current.push_str(trimmed);
            result.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

fn strip_comment(line: &str) -> &str {
    line.split('#').next().unwrap_or("")
}

fn split_assignment(line: &str) -> Option<(&str, &str)> {
    for op in ["+=", ":=", "?=", "="] {
        if let Some((lhs, rhs)) = line.split_once(op) {
            return Some((lhs, rhs));
        }
    }
    None
}

fn expand_kconfig(input: &str, kconfig: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("$(") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some(end) = after.find(')') {
            let var = after[..end].trim();
            if var.starts_with("CONFIG_")
                && let Some(val) = kconfig.get(var)
                && val != "n"
            {
                out.push_str(val);
            }
            rest = &after[end + 1..];
        } else {
            out.push_str("$(");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn collect_cflags(rhs: &str, base_dir: &Path, out: &mut Vec<String>) {
    let tokens: Vec<&str> = rhs.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        if matches!(tok, "-I" | "-isystem" | "-D" | "-U" | "-include") && i + 1 < tokens.len() {
            let arg = tokens[i + 1];
            out.push(tok.to_owned());
            if matches!(tok, "-I" | "-isystem" | "-include") && Path::new(arg).is_relative() {
                out.push(base_dir.join(arg).display().to_string());
            } else {
                out.push(arg.to_owned());
            }
            i += 2;
            continue;
        }
        if let Some(dir) = tok.strip_prefix("-I")
            && !dir.is_empty()
        {
            if Path::new(dir).is_relative() {
                out.push(format!("-I{}", base_dir.join(dir).display()));
            } else {
                out.push(tok.to_owned());
            }
        } else if tok.starts_with("-D")
            || tok.starts_with("-U")
            || tok.starts_with("-std=")
            || (tok.starts_with("-W") && !tok.starts_with("-Wl,"))
            || (tok.starts_with("-f")
                && !tok.starts_with("-fplugin")
                && !tok.starts_with("-fsanitize"))
        {
            out.push(tok.to_owned());
        }
        i += 1;
    }
}
