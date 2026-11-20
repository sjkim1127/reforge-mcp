//! Small accessors over the Clang JSON AST and source locations.
use anyhow::{Result, ensure};
use serde_json::Value;

pub(super) fn text<'a>(node: &'a Value, key: &str) -> &'a str {
    node[key].as_str().unwrap_or("")
}

pub(super) fn children(node: &Value) -> &[Value] {
    node["inner"].as_array().map(Vec::as_slice).unwrap_or(&[])
}

pub(super) fn kind(node: &Value) -> &str {
    text(node, "kind")
}

pub(super) fn identifier(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "unsupported identifier {name}"
    );
    Ok(())
}

pub(super) fn expansion_location(location: &Value) -> &Value {
    if location.get("expansionLoc").is_some() {
        &location["expansionLoc"]
    } else {
        location
    }
}

pub(super) fn file_from_loc(loc: &Value) -> Option<&str> {
    loc["expansionLoc"]["file"]
        .as_str()
        .or_else(|| loc["file"].as_str())
        .or_else(|| loc["spellingLoc"]["file"].as_str())
}

pub(super) fn offset_to_line_col(source: &str, loc: &Value) -> Result<(usize, usize)> {
    let exp = expansion_location(loc);
    if let Some(offset) = exp["offset"].as_u64().map(|o| o as usize)
        && offset <= source.len()
        && source.is_char_boundary(offset)
    {
        let prefix = &source[..offset];
        let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
        let col = match prefix.rfind('\n') {
            Some(pos) => offset - pos,
            None => offset + 1,
        };
        return Ok((line, col));
    }
    let line = exp["line"].as_u64().map(|l| l.max(1) as usize).unwrap_or(1);
    let col = exp["col"].as_u64().map(|c| c.max(1) as usize).unwrap_or(1);
    Ok((line, col))
}

pub(super) fn line_from_offset(source: &str, loc: &Value) -> Result<usize> {
    Ok(offset_to_line_col(source, loc)?.0)
}

pub(super) fn is_benign_attr_or_comment(kind_name: &str) -> bool {
    kind_name.ends_with("Comment")
        || matches!(
            kind_name,
            "BuiltinAttr"
                | "NoThrowAttr"
                | "NoInlineAttr"
                | "UsedAttr"
                | "ConstAttr"
                | "PureAttr"
                | "VisibilityAttr"
                | "UnusedAttr"
                | "AlwaysInlineAttr"
                | "GNUInlineAttr"
                | "InlineAttr"
                | "ColdAttr"
                | "HotAttr"
                | "NonNullAttr"
                | "AllocSizeAttr"
                | "FormatAttr"
                | "WarnUnusedResultAttr"
                | "NoreturnAttr"
                | "C11NoReturnAttr"
                | "ReturnsTwiceAttr"
                | "DisableTailCallsAttr"
                | "NoProfileFunctionAttr"
                | "RestrictAttr"
                | "DeprecatedAttr"
                | "UnavailableAttr"
                | "AlignedAttr"
                | "MaxFieldAlignmentAttr"
                | "PackedAttr"
                | "TransparentUnionAttr"
                | "MayAliasAttr"
                | "WeakAttr"
        )
}
