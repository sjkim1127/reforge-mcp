//! Parsing of Clang `qualType` spellings into `CType` components.
use anyhow::{Result, ensure};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

use crate::clang::ast::text;
use crate::clang::types::{CType, IntKind};

pub(super) fn is_const_qualified(type_obj: &Value) -> bool {
    let qual = text(type_obj, "qualType").trim();
    let desugared = text(type_obj, "desugaredQualType").trim();
    let check = |s: &str| -> bool {
        let s = s.trim();
        if s.ends_with("const") {
            let before = s.strip_suffix("const").unwrap_or("");
            if before
                .chars()
                .last()
                .is_none_or(|c| c.is_whitespace() || c == '*')
            {
                return true;
            }
        }
        !s.contains('*') && s.starts_with("const ")
    };
    check(qual) || (!desugared.is_empty() && check(desugared))
}

pub(super) fn parse_int_kind(base: &str) -> Option<IntKind> {
    Some(match base {
        "_Bool" | "bool" => IntKind::Bool,
        "char" => IntKind::Char,
        "signed char" | "int8_t" | "s8" => IntKind::I8,
        "unsigned char" | "uint8_t" | "u8" => IntKind::U8,
        "short" | "short int" | "signed short" | "signed short int" | "int16_t" | "s16" => {
            IntKind::I16
        }
        "unsigned short" | "unsigned short int" | "uint16_t" | "u16" => IntKind::U16,
        "int" | "signed" | "signed int" | "int32_t" | "s32" => IntKind::I32,
        "unsigned" | "unsigned int" | "uint32_t" | "u32" => IntKind::U32,
        "long" | "long int" | "signed long" | "signed long int" | "ssize_t" | "ptrdiff_t"
        | "intptr_t" => IntKind::Long,
        "unsigned long" | "unsigned long int" | "size_t" | "__size_t" | "uintptr_t" => {
            IntKind::ULong
        }
        "long long"
        | "long long int"
        | "signed long long"
        | "signed long long int"
        | "int64_t"
        | "s64" => IntKind::LongLong,
        "unsigned long long" | "unsigned long long int" | "uint64_t" | "u64" => IntKind::ULongLong,
        "__int128" | "__int128_t" | "int128_t" => IntKind::I128,
        "unsigned __int128" | "__uint128_t" | "uint128_t" => IntKind::U128,
        _ => return None,
    })
}

pub(super) fn is_ptr_inner_qualifier(tok: &str) -> bool {
    matches!(
        tok,
        "const"
            | "volatile"
            | "restrict"
            | "__restrict"
            | "__restrict__"
            | "_Nonnull"
            | "_Nullable"
            | "_Null_unspecified"
            | "__nonnull"
            | "__nullable"
    )
}

pub(super) fn split_fn_ptr(s: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if depth == 0 && s[i..].starts_with("(*") {
            let inside_start = i + 2;
            if let Some(rel_close) = s[inside_start..].find(')') {
                let inside = s[inside_start..inside_start + rel_close].trim();
                if inside.is_empty() || inside.split_whitespace().all(is_ptr_inner_qualifier) {
                    let after = s[inside_start + rel_close + 1..].trim_start();
                    if after.starts_with('(') && after.ends_with(')') {
                        let mut inner_depth = 0usize;
                        let mut matched_end = None;
                        for (off, ch) in after.char_indices() {
                            match ch {
                                '(' => inner_depth += 1,
                                ')' => {
                                    inner_depth -= 1;
                                    if inner_depth == 0 {
                                        matched_end = Some(off);
                                        break;
                                    }
                                }
                                _ => {}
                            }
                        }
                        if matched_end == Some(after.len() - 1) {
                            return Some((s[..i].trim(), &after[1..after.len() - 1]));
                        }
                    }
                }
            }
        }
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    None
}

pub(super) fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (idx, ch) in s.char_indices() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                result.push(s[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }
    let tail = s[start..].trim();
    if !tail.is_empty() {
        result.push(tail);
    }
    result
}

pub(super) fn parse_param_list_with_variadic(
    params_str: &str,
    typedefs: &BTreeMap<String, String>,
    depth: usize,
) -> Result<(Vec<CType>, bool)> {
    let trimmed = params_str.trim();
    if trimmed.is_empty() || trimmed == "void" {
        return Ok((Vec::new(), false));
    }
    let items = split_top_level_commas(trimmed);
    let mut params = Vec::with_capacity(items.len());
    let mut is_variadic = false;
    for (idx, p) in items.iter().enumerate() {
        if *p == "..." {
            ensure!(
                idx + 1 == items.len(),
                "variadic ellipsis must be last parameter"
            );
            is_variadic = true;
            break;
        }
        let pty = match CType::parse_str_depth(p, typedefs, depth) {
            Ok(t) => t,
            Err(orig_err) => {
                if let Some(res) = resolve_bare_fn_type(p, typedefs, depth + 1) {
                    let (fn_params, ret) = res?;
                    CType::FnPtr {
                        params: fn_params,
                        ret: Box::new(ret),
                    }
                } else {
                    return Err(orig_err);
                }
            }
        };
        ensure!(!matches!(pty, CType::Void), "void parameter unsupported");
        params.push(pty);
    }
    Ok((params, is_variadic))
}

pub(super) fn parse_param_list(
    params_str: &str,
    typedefs: &BTreeMap<String, String>,
    depth: usize,
) -> Result<Vec<CType>> {
    let (params, is_variadic) = parse_param_list_with_variadic(params_str, typedefs, depth)?;
    ensure!(!is_variadic, "variadic functions are unsupported");
    Ok(params)
}

pub(super) fn split_array_type(s: &str) -> Option<(String, usize, bool)> {
    let mut paren_depth = 0usize;
    let mut first_bracket = None;
    for (idx, ch) in s.char_indices() {
        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' if paren_depth == 0 => {
                first_bracket = Some(idx);
                break;
            }
            _ => {}
        }
    }
    let open = first_bracket?;
    let rel_close = s[open..].find(']')?;
    let close = open + rel_close;
    let len_str = s[open + 1..close].trim();
    let len = if len_str.is_empty() {
        0
    } else {
        len_str.parse::<usize>().ok()?
    };
    let before = s[..open].trim();
    let after = s[close + 1..].trim();
    if let Some(stripped) = before.strip_suffix("(*)") {
        let elem = format!("{}{}", stripped.trim(), after);
        return Some((elem, len, true));
    }
    let elem = format!("{before}{after}");
    Some((elem, len, false))
}

pub(super) fn split_trailing_parens(s: &str) -> Option<(&str, &str)> {
    let trimmed = s.trim();
    if !trimmed.ends_with(')') {
        return None;
    }
    let mut depth = 0usize;
    let mut open_idx = None;
    for (idx, ch) in trimmed.char_indices().rev() {
        match ch {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth == 0 {
                    open_idx = Some(idx);
                    break;
                }
            }
            _ => {}
        }
    }
    let open = open_idx?;
    Some((
        trimmed[..open].trim(),
        trimmed[open + 1..trimmed.len() - 1].trim(),
    ))
}

pub(super) fn resolve_bare_fn_type(
    raw: &str,
    typedefs: &BTreeMap<String, String>,
    depth: usize,
) -> Option<Result<(Vec<CType>, CType)>> {
    if depth >= 16 {
        return None;
    }
    let mut cur = strip_trailing_qualifiers(raw.trim());
    for _ in 0..(16 - depth) {
        if let Some(next) = typedefs.get(cur) {
            cur = strip_trailing_qualifiers(next.trim());
        } else {
            break;
        }
    }
    if cur.starts_with("struct ")
        || cur.starts_with("union ")
        || cur.starts_with("enum ")
        || split_fn_ptr(cur).is_some()
    {
        return None;
    }
    let (ret_str, params_str) = split_trailing_parens(cur)?;
    if ret_str.is_empty() || ret_str.ends_with("*)") {
        return None;
    }
    let res = (|| {
        let ret = CType::parse_str_depth(ret_str, typedefs, depth + 1)?;
        let params = parse_param_list(params_str, typedefs, depth + 1)?;
        Ok((params, ret))
    })();
    Some(res)
}

pub(super) fn resolve_anon_record_type<'a>(
    base_raw: &str,
    typedefs: &'a BTreeMap<String, String>,
) -> Option<&'a str> {
    let tag = if base_raw.starts_with("struct ") {
        "struct"
    } else if base_raw.starts_with("union ") {
        "union"
    } else {
        return None;
    };
    let rest = base_raw.strip_suffix(')')?;
    let at_pos = [
        "unnamed at ",
        "unnamed struct at ",
        "unnamed union at ",
        "anonymous at ",
        "anonymous struct at ",
        "anonymous union at ",
    ]
    .iter()
    .filter_map(|m| rest.rfind(m).map(|i| i + m.len()))
    .max()?;
    let loc_str = &rest[at_pos..];
    let mut parts = loc_str.rsplitn(3, ':');
    let col = parts.next()?.parse::<usize>().ok()?;
    let line = parts.next()?.parse::<usize>().ok()?;
    let file_part = parts.next()?;
    let key = format!("{tag} @anon:{file_part}:{line}:{col}");
    if let Some(found) = typedefs.get(&key) {
        return Some(found.as_str());
    }
    if let Ok(canon) = Path::new(file_part).canonicalize() {
        let canon_key = format!("{tag} @anon:{}:{line}:{col}", canon.display());
        if let Some(found) = typedefs.get(&canon_key) {
            return Some(found.as_str());
        }
    }
    None
}

pub(super) fn strip_trailing_qualifiers(mut s: &str) -> &str {
    loop {
        let trimmed = s.trim();
        if trimmed.ends_with(')') {
            let mut depth = 0usize;
            let mut matched_open = None;
            for (idx, ch) in trimmed.char_indices().rev() {
                match ch {
                    ')' => depth += 1,
                    '(' => {
                        depth -= 1;
                        if depth == 0 {
                            matched_open = Some(idx);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(open) = matched_open {
                let before = trimmed[..open].trim_end();
                if let Some(rest) = before.strip_suffix("__attribute__")
                    && rest
                        .chars()
                        .last()
                        .is_none_or(|c| c.is_whitespace() || c == ')' || c == '*')
                {
                    s = rest.trim();
                    continue;
                }
            }
        }
        let next = [
            "const",
            "volatile",
            "restrict",
            "__restrict",
            "__restrict__",
            "_Nonnull",
            "_Nullable",
            "_Null_unspecified",
            "__nonnull",
            "__nullable",
            "_Noreturn",
            "noreturn",
        ]
        .into_iter()
        .find_map(|q| trimmed.strip_suffix(q));
        if let Some(candidate) = next {
            let c_end = candidate.chars().last();
            if c_end.is_none_or(|c| c.is_whitespace() || c == '*') {
                s = candidate.trim();
                continue;
            }
        }
        return trimmed;
    }
}

pub(super) fn strip_pointee_qualifiers(mut s: &str) -> (&str, bool) {
    let mut saw_const = false;
    loop {
        let trimmed = s.trim();
        if let Some(rest) = trimmed.strip_prefix("const ") {
            saw_const = true;
            s = rest;
            continue;
        }
        if let Some(rest) = [
            "volatile ",
            "restrict ",
            "__restrict ",
            "__restrict__ ",
            "_Nonnull ",
            "_Nullable ",
            "_Null_unspecified ",
            "__nonnull ",
            "__nullable ",
        ]
        .into_iter()
        .find_map(|q| trimmed.strip_prefix(q))
        {
            s = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" const") {
            saw_const = true;
            s = rest;
            continue;
        }
        if let Some(rest) = [
            " volatile",
            " restrict",
            " __restrict",
            " __restrict__",
            " _Nonnull",
            " _Nullable",
            " _Null_unspecified",
            " __nonnull",
            " __nullable",
        ]
        .into_iter()
        .find_map(|q| trimmed.strip_suffix(q))
        {
            s = rest;
            continue;
        }
        return (trimmed, saw_const);
    }
}
