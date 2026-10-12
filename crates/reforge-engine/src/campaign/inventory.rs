use super::model::FileStamp;
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use walkdir::WalkDir;

pub fn file_hash(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let count = file.read(&mut buf)?;
        if count == 0 {
            break;
        }
        hash.update(&buf[..count]);
    }
    Ok(hex::encode(hash.finalize()))
}
pub fn scan(root: &Path) -> Result<Vec<FileStamp>> {
    ensure!(root.is_dir(), "project directory is unavailable");
    let mut files = Vec::new();
    let mut total = 0u64;
    let started = std::time::Instant::now();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            !matches!(
                e.file_name().to_str(),
                Some(".git" | "target" | "node_modules" | ".gradle" | ".venv" | "__pycache__")
            )
        })
    {
        let entry = entry?;
        ensure!(
            started.elapsed().as_secs() < 120,
            "inventory scan time budget exceeded"
        );
        ensure!(
            !entry.file_type().is_symlink(),
            "inventory symlink unsupported: {}",
            entry.path().display()
        );
        if !entry.file_type().is_file() {
            continue;
        }
        ensure!(
            files.len() < 100_000 && started.elapsed().as_secs() < 120,
            "inventory scan budget exceeded; use a smaller project root"
        );
        let bytes = entry.metadata()?.len();
        total = total
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("inventory size overflow"))?;
        ensure!(
            bytes <= 512 * 1024 * 1024 && total <= 4 * 1024 * 1024 * 1024,
            "inventory byte budget exceeded"
        );
        let path = entry
            .path()
            .strip_prefix(root)?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF8 inventory path"))?
            .to_owned();
        let language = match entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
        {
            "c" | "h" => "c",
            "cpp" | "cc" | "cxx" | "hpp" => "cpp",
            "java" => "java",
            "py" => "python",
            "go" => "go",
            "rs" => "rust",
            _ => "other",
        }
        .to_owned();
        let manifest = matches!(
            entry.file_name().to_str(),
            Some(
                "pom.xml"
                    | "build.gradle"
                    | "build.gradle.kts"
                    | "settings.gradle"
                    | "settings.gradle.kts"
                    | "Cargo.toml"
                    | "Cargo.lock"
                    | "go.mod"
                    | "go.sum"
                    | "pyproject.toml"
                    | "requirements.txt"
                    | "CMakeLists.txt"
                    | "Makefile"
                    | "Kbuild"
                    | "Kconfig"
                    | ".config"
                    | "compile_commands.json"
            )
        );
        let sha256 = file_hash(entry.path())?;
        ensure!(
            started.elapsed().as_secs() < 120,
            "inventory scan time budget exceeded"
        );
        files.push(FileStamp {
            path,
            sha256,
            bytes,
            language,
            manifest,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    ensure!(!files.is_empty(), "project inventory is empty");
    Ok(files)
}
pub fn fingerprint(files: &[FileStamp]) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(files)?)))
}
