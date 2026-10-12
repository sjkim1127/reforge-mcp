//! Clang Makefile dependencies, including escaped whitespace and continuations.
use anyhow::{Result, bail, ensure};
use std::path::PathBuf;

pub fn parse(contents: &str) -> Result<Vec<PathBuf>> {
    let (_, body) = contents
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("missing dependency target"))?;
    let body = body.replace("\\\r\n", " ").replace("\\\n", " ");
    let mut chars = body.chars().peekable();
    let mut word = String::new();
    let mut paths = Vec::new();
    while let Some(c) = chars.next() {
        match c {
            '\\' => word.push(
                chars
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("trailing dependency escape"))?,
            ),
            '$' => {
                ensure!(chars.next() == Some('$'), "unsupported dependency variable");
                word.push('$');
            }
            '#' => bail!("unescaped dependency comment"),
            c if c.is_whitespace() => {
                if !word.is_empty() {
                    paths.push(PathBuf::from(std::mem::take(&mut word)));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        paths.push(PathBuf::from(word));
    }
    ensure!(!paths.is_empty(), "empty dependency list");
    Ok(paths)
}

#[cfg(test)]
mod tests {
    #[test]
    fn clang_escaped_dependencies() {
        let result = super::parse("reforge: /tmp/a\\ b.c \\\n /tmp/a\\#b.h /tmp/a$$b.h\n").unwrap();
        assert_eq!(
            result,
            vec![
                std::path::PathBuf::from("/tmp/a b.c"),
                "/tmp/a#b.h".into(),
                "/tmp/a$b.h".into()
            ]
        );
    }
}
