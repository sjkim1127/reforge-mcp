//! Fail-closed translation of a typed C subset (fixed-width/unsigned/bool integers,
//! enums, void, structs with bitfields, unions, fixed-size arrays, pointers,
//! function pointers, string literals, static const globals, loops, switch,
//! forward goto cleanup labels, sizeof/alignof, external prototypes, and C ABI export) to Rust.
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::model::{ExternalFunction, Function, StructDef, StructField};

pub struct TranslationUnitOutput {
    pub structs: Vec<StructDef>,
    pub constants: Vec<String>,
    pub functions: Vec<Function>,
    pub prototypes: Vec<ExternalFunction>,
}

type StructFieldsMap = BTreeMap<String, Vec<(String, CType, Option<u32>)>>;
type UnionFieldsMap = BTreeMap<String, Vec<(String, CType)>>;
type LabeledSegment<'a> = (Option<(String, String)>, Vec<&'a Value>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntKind {
    Bool,
    Char,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    Long,
    ULong,
    LongLong,
    ULongLong,
}

impl IntKind {
    fn is_signed(self) -> bool {
        match self {
            Self::Bool | Self::U8 | Self::U16 | Self::U32 | Self::ULong | Self::ULongLong => false,
            Self::I8 | Self::I16 | Self::I32 | Self::Long | Self::LongLong => true,
            Self::Char => !cfg!(all(
                target_os = "linux",
                any(target_arch = "aarch64", target_arch = "arm")
            )),
        }
    }

    fn bit_width(self) -> u32 {
        match self {
            Self::Bool | Self::Char | Self::I8 | Self::U8 => 8,
            Self::I16 | Self::U16 => 16,
            Self::I32 | Self::U32 => 32,
            Self::Long | Self::ULong | Self::LongLong | Self::ULongLong => 64,
        }
    }

    fn unsigned_rust_type(self) -> &'static str {
        match self.bit_width() {
            8 => "u8",
            16 => "u16",
            32 => "u32",
            _ => "u64",
        }
    }

    fn rust_type(self) -> &'static str {
        match self {
            Self::Bool | Self::U8 => "u8",
            Self::Char => {
                if Self::Char.is_signed() {
                    "i8"
                } else {
                    "u8"
                }
            }
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::U16 => "u16",
            Self::I32 => "i32",
            Self::U32 => "u32",
            Self::Long | Self::LongLong => "i64",
            Self::ULong | Self::ULongLong => "u64",
        }
    }

    fn c_type(self) -> &'static str {
        match self {
            Self::Bool => "_Bool",
            Self::Char => "char",
            Self::I8 => "signed char",
            Self::U8 => "unsigned char",
            Self::I16 => "short",
            Self::U16 => "unsigned short",
            Self::I32 => "int",
            Self::U32 => "unsigned int",
            Self::Long => "long",
            Self::ULong => "unsigned long",
            Self::LongLong => "long long",
            Self::ULongLong => "unsigned long long",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CType {
    Void,
    Int(IntKind),
    Struct(String),
    Union(String),
    Ptr {
        inner: Box<CType>,
        inner_const: bool,
    },
    Array(Box<CType>, usize),
    FnPtr {
        params: Vec<CType>,
        ret: Box<CType>,
    },
}

impl CType {
    fn same_shape(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Void, Self::Void) => true,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Struct(a), Self::Struct(b)) => a == b,
            (Self::Union(a), Self::Union(b)) => a == b,
            (Self::Ptr { inner: a, .. }, Self::Ptr { inner: b, .. }) => a.same_shape(b),
            (Self::Array(a, al), Self::Array(b, bl)) => al == bl && a.same_shape(b),
            (
                Self::FnPtr {
                    params: ap,
                    ret: ar,
                },
                Self::FnPtr {
                    params: bp,
                    ret: br,
                },
            ) => {
                ap.len() == bp.len()
                    && ar.same_shape(br)
                    && ap.iter().zip(bp).all(|(x, y)| x.same_shape(y))
            }
            _ => false,
        }
    }

    fn is_pointer(&self) -> bool {
        matches!(self, Self::Ptr { .. })
    }

    fn is_pointer_like(&self) -> bool {
        matches!(self, Self::Ptr { .. } | Self::FnPtr { .. })
    }

    fn contains_raw_ptr(&self, structs: &StructFieldsMap, unions: &UnionFieldsMap) -> bool {
        match self {
            Self::Ptr { .. } => true,
            Self::Array(elem, _) => elem.contains_raw_ptr(structs, unions),
            Self::Struct(s) => structs.get(s).is_some_and(|fs| {
                fs.iter()
                    .any(|(_, t, _)| t.contains_raw_ptr(structs, unions))
            }),
            Self::Union(u) => unions
                .get(u)
                .is_some_and(|fs| fs.iter().any(|(_, t)| t.contains_raw_ptr(structs, unions))),
            _ => false,
        }
    }

    fn fn_type_only(&self) -> Result<String> {
        match self {
            Self::FnPtr { params, ret } => {
                let p = params
                    .iter()
                    .map(Self::rust_type)
                    .collect::<Vec<_>>()
                    .join(", ");
                let r = if matches!(**ret, Self::Void) {
                    String::new()
                } else {
                    format!(" -> {}", ret.rust_type())
                };
                Ok(format!("fn({p}){r}"))
            }
            _ => bail!("expected function pointer type"),
        }
    }

    fn rust_type(&self) -> String {
        match self {
            Self::Void => "()".into(),
            Self::Int(kind) => kind.rust_type().into(),
            Self::Struct(name) => format!("RfStruct_{name}"),
            Self::Union(name) => format!("RfUnion_{name}"),
            Self::Ptr { inner, .. } => {
                if matches!(**inner, Self::Void) {
                    "*mut u8".into()
                } else {
                    format!("*mut {}", inner.rust_type())
                }
            }
            Self::Array(elem, len) => format!("[{}; {len}]", elem.rust_type()),
            Self::FnPtr { .. } => {
                let fnty = self.fn_type_only().unwrap_or_else(|_| "fn()".into());
                format!("Option<{fnty}>")
            }
        }
    }

    fn c_type(&self) -> String {
        match self {
            Self::Void => "void".into(),
            Self::Int(kind) => kind.c_type().into(),
            Self::Struct(name) => format!("struct {name}"),
            Self::Union(name) => format!("union {name}"),
            Self::Ptr { inner, inner_const } => {
                if *inner_const && !inner.is_pointer() {
                    format!("const {} *", inner.c_type())
                } else {
                    format!("{} *", inner.c_type())
                }
            }
            Self::Array(elem, len) => format!("{}[{len}]", elem.c_type()),
            Self::FnPtr { params, ret } => {
                let p = if params.is_empty() {
                    "void".to_string()
                } else {
                    params
                        .iter()
                        .map(Self::c_type)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                format!("{} (*)({p})", ret.c_type())
            }
        }
    }

    fn zero_expr(&self, structs: &StructFieldsMap) -> Result<String> {
        match self {
            Self::Int(kind) => Ok(format!("0{}", kind.rust_type())),
            Self::Ptr { .. } => Ok("core::ptr::null_mut()".into()),
            Self::FnPtr { .. } => Ok("None".into()),
            Self::Array(elem, len) => Ok(format!("[{}; {len}]", elem.zero_expr(structs)?)),
            Self::Struct(name) => {
                let fields = structs
                    .get(name)
                    .with_context(|| format!("unknown struct {name} for zero-init"))?;
                let inits = fields
                    .iter()
                    .map(|(fname, fty, _)| Ok(format!("{fname}: {}", fty.zero_expr(structs)?)))
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("RfStruct_{name} {{ {} }}", inits.join(", ")))
            }
            Self::Union(name) => Ok(format!("core::mem::zeroed::<RfUnion_{name}>()")),
            Self::Void => bail!("void cannot be zero-initialized"),
        }
    }

    fn from_node_type(type_obj: &Value, typedefs: &BTreeMap<String, String>) -> Result<Self> {
        let qual = text(type_obj, "qualType");
        let desugared = text(type_obj, "desugaredQualType");
        let chosen = if !desugared.is_empty()
            && !desugared.contains("unnamed")
            && !desugared.contains("anonymous")
        {
            desugared
        } else {
            qual
        };
        Self::parse_str(chosen, typedefs)
    }

    fn parse_str(raw: &str, typedefs: &BTreeMap<String, String>) -> Result<Self> {
        Self::parse_str_depth(raw, typedefs, 0)
    }

    fn parse_str_depth(
        raw: &str,
        typedefs: &BTreeMap<String, String>,
        depth: usize,
    ) -> Result<Self> {
        ensure!(depth < 16, "recursive typedef expansion in {raw}");
        let s = strip_trailing_qualifiers(raw.trim());

        // Function pointer syntax: `<ret> (*)(<params>)`
        if let Some((ret_str, params_str)) = split_fn_ptr(s) {
            let ret = Self::parse_str_depth(ret_str, typedefs, depth + 1)?;
            let params = parse_param_list(params_str, typedefs, depth + 1)?;
            return Ok(Self::FnPtr {
                params,
                ret: Box::new(ret),
            });
        }

        if let Some(prefix) = s.strip_suffix('*') {
            let prefix = prefix.trim();
            let (inner_str, inner_const) = strip_pointee_qualifiers(prefix);
            let inner = Self::parse_str_depth(inner_str, typedefs, depth + 1)?;
            return Ok(Self::Ptr {
                inner: Box::new(inner),
                inner_const,
            });
        }

        // Fixed-size array syntax: `T[N]` or `T[M][N]` or `T (*)[N]`
        if s.ends_with(']')
            && let Some((elem_str, len, is_ptr_to_arr)) = split_array_type(s)
        {
            let elem = Self::parse_str_depth(&elem_str, typedefs, depth + 1)?;
            let arr = Self::Array(Box::new(elem), len);
            if is_ptr_to_arr {
                return Ok(Self::Ptr {
                    inner: Box::new(arr),
                    inner_const: false,
                });
            }
            return Ok(arr);
        }

        let (base_raw, _) = strip_pointee_qualifiers(s);
        if let Some(resolved) = typedefs.get(base_raw) {
            return Self::parse_str_depth(resolved, typedefs, depth + 1);
        }

        if let Some(kind) = parse_int_kind(base_raw) {
            return Ok(Self::Int(kind));
        }

        match base_raw {
            "void" => Ok(Self::Void),
            other => {
                if let Some(tag) = other.strip_prefix("struct ") {
                    let tag = tag.trim();
                    identifier(tag)?;
                    Ok(Self::Struct(tag.to_owned()))
                } else if let Some(tag) = other.strip_prefix("union ") {
                    let tag = tag.trim();
                    identifier(tag)?;
                    Ok(Self::Union(tag.to_owned()))
                } else if let Some(tag) = other.strip_prefix("enum ") {
                    let tag = tag.trim();
                    identifier(tag)?;
                    Ok(Self::Int(IntKind::I32))
                } else {
                    bail!("unsupported type {:?}", raw)
                }
            }
        }
    }
}

fn is_const_qualified(type_obj: &Value) -> bool {
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
        if !s.contains('*') && s.starts_with("const ") {
            return true;
        }
        false
    };
    check(qual) || (!desugared.is_empty() && check(desugared))
}

fn parse_int_kind(base: &str) -> Option<IntKind> {
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
        _ => return None,
    })
}

fn split_fn_ptr(s: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if depth == 0 && &s[i..i + 3] == "(*)" {
            let after = s[i + 3..].trim_start();
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
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_top_level_commas(s: &str) -> Vec<&str> {
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

fn parse_param_list(
    params_str: &str,
    typedefs: &BTreeMap<String, String>,
    depth: usize,
) -> Result<Vec<CType>> {
    let trimmed = params_str.trim();
    if trimmed.is_empty() || trimmed == "void" {
        return Ok(Vec::new());
    }
    let mut params = Vec::new();
    for p in split_top_level_commas(trimmed) {
        ensure!(p != "...", "variadic functions are unsupported");
        let pty = CType::parse_str_depth(p, typedefs, depth)?;
        ensure!(!matches!(pty, CType::Void), "void parameter unsupported");
        params.push(pty);
    }
    Ok(params)
}

fn split_array_type(s: &str) -> Option<(String, usize, bool)> {
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
    let len = len_str.parse::<usize>().ok()?;
    let before = s[..open].trim();
    let after = s[close + 1..].trim();
    if let Some(stripped) = before.strip_suffix("(*)") {
        let elem = format!("{}{}", stripped.trim(), after);
        return Some((elem, len, true));
    }
    let elem = format!("{before}{after}");
    Some((elem, len, false))
}

fn strip_trailing_qualifiers(mut s: &str) -> &str {
    loop {
        let trimmed = s.trim();
        let next = trimmed
            .strip_suffix("const")
            .or_else(|| trimmed.strip_suffix("volatile"))
            .or_else(|| trimmed.strip_suffix("restrict"))
            .or_else(|| trimmed.strip_suffix("__restrict"));
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

fn strip_pointee_qualifiers(mut s: &str) -> (&str, bool) {
    let mut saw_const = false;
    loop {
        let trimmed = s.trim();
        if let Some(rest) = trimmed.strip_prefix("const ") {
            saw_const = true;
            s = rest;
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("volatile ")
            .or_else(|| trimmed.strip_prefix("restrict "))
            .or_else(|| trimmed.strip_prefix("__restrict "))
        {
            s = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" const") {
            saw_const = true;
            s = rest;
            continue;
        }
        if let Some(rest) = trimmed
            .strip_suffix(" volatile")
            .or_else(|| trimmed.strip_suffix(" restrict"))
            .or_else(|| trimmed.strip_suffix(" __restrict"))
        {
            s = rest;
            continue;
        }
        return (trimmed, saw_const);
    }
}

fn text<'a>(node: &'a Value, key: &str) -> &'a str {
    node[key].as_str().unwrap_or("")
}

fn children(node: &Value) -> &[Value] {
    node["inner"].as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn kind(node: &Value) -> &str {
    text(node, "kind")
}

fn identifier(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "unsupported identifier {name}"
    );
    Ok(())
}

fn expansion_location(location: &Value) -> &Value {
    if location.get("expansionLoc").is_some() {
        &location["expansionLoc"]
    } else {
        location
    }
}

fn line_from_offset(source: &str, loc: &Value) -> Result<usize> {
    let loc = expansion_location(loc);
    let offset = loc["offset"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing source offset"))? as usize;
    ensure!(
        offset <= source.len() && source.is_char_boundary(offset),
        "invalid source offset"
    );
    Ok(source[..offset].bytes().filter(|b| *b == b'\n').count() + 1)
}

fn parse_fn_signature(
    qual_type: &str,
    typedefs: &BTreeMap<String, String>,
) -> Result<(Vec<CType>, CType)> {
    let trimmed = qual_type.trim();
    ensure!(
        trimmed.ends_with(')'),
        "invalid function type {qual_type:?}"
    );
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
    let open = open_idx.with_context(|| format!("invalid function type {qual_type:?}"))?;
    let ret_str = trimmed[..open].trim();
    let params_str = trimmed[open + 1..trimmed.len() - 1].trim();
    let ret = CType::parse_str(ret_str, typedefs)?;
    let params = parse_param_list(params_str, typedefs, 0)?;
    Ok((params, ret))
}

fn make_external_function(name: &str, param_types: &[CType], ret: &CType) -> ExternalFunction {
    let params_rust = param_types
        .iter()
        .enumerate()
        .map(|(i, t)| format!("v{i}: {}", t.rust_type()))
        .collect::<Vec<_>>();
    let args_rust = (0..param_types.len())
        .map(|i| format!("v{i}"))
        .collect::<Vec<_>>();
    let ret_suffix = if matches!(ret, CType::Void) {
        String::new()
    } else {
        format!(" -> {}", ret.rust_type())
    };
    let rust = format!(
        "unsafe extern \"C\" {{\n    fn {name}({}){ret_suffix};\n}}\n#[inline]\npub fn rf_{name}({}){ret_suffix} {{\n    unsafe {{ {name}({}) }}\n}}\n",
        params_rust.join(", "),
        params_rust.join(", "),
        args_rust.join(", "),
    );
    ExternalFunction {
        name: name.to_owned(),
        parameter_types: param_types.iter().map(CType::c_type).collect(),
        return_type: ret.c_type(),
        rust,
    }
}

fn eval_constant_int(node: &Value) -> Option<i64> {
    if let Some(v) = node["value"].as_str().and_then(|s| s.parse::<i64>().ok()) {
        return Some(v);
    }
    if let Some(v) = node["value"].as_i64() {
        return Some(v);
    }
    match kind(node) {
        "ConstantExpr" | "ParenExpr" => children(node).first().and_then(eval_constant_int),
        "UnaryOperator" if text(node, "opcode") == "-" => children(node)
            .first()
            .and_then(eval_constant_int)
            .map(|v| -v),
        "UnaryOperator" if text(node, "opcode") == "+" => {
            children(node).first().and_then(eval_constant_int)
        }
        _ => None,
    }
}

fn normalize_bitfield(expr: &str, ik: IntKind, width: u32) -> String {
    if matches!(ik, IntKind::Bool) {
        return format!("u8::from(({expr}) != 0)");
    }
    let type_bits = ik.bit_width();
    if width >= type_bits {
        return expr.to_owned();
    }
    if !ik.is_signed() {
        let mask = (1u128 << width) - 1;
        format!("(({expr}) & ({mask}{}))", ik.rust_type())
    } else {
        let shift = type_bits - width;
        let uty = ik.unsigned_rust_type();
        let sty = ik.rust_type();
        format!("((((({expr}) as {uty}) << {shift}) as {sty}) >> {shift})")
    }
}

fn parse_c_string_literal(raw: &str) -> Result<Vec<u8>> {
    let trimmed = raw.trim();
    ensure!(
        trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2,
        "unsupported string literal prefix or format {raw:?}"
    );
    let inner = &trimmed[1..trimmed.len() - 1];
    let bytes = inner.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + 1);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        ensure!(i < bytes.len(), "trailing backslash in string literal");
        match bytes[i] {
            b'\\' => {
                out.push(b'\\');
                i += 1;
            }
            b'"' => {
                out.push(b'"');
                i += 1;
            }
            b'\'' => {
                out.push(b'\'');
                i += 1;
            }
            b'?' => {
                out.push(b'?');
                i += 1;
            }
            b'a' => {
                out.push(0x07);
                i += 1;
            }
            b'b' => {
                out.push(0x08);
                i += 1;
            }
            b'f' => {
                out.push(0x0c);
                i += 1;
            }
            b'n' => {
                out.push(b'\n');
                i += 1;
            }
            b'r' => {
                out.push(b'\r');
                i += 1;
            }
            b't' => {
                out.push(b'\t');
                i += 1;
            }
            b'v' => {
                out.push(0x0b);
                i += 1;
            }
            b'x' => {
                i += 1;
                let mut val = 0u8;
                let mut count = 0;
                while i < bytes.len() && count < 2 && bytes[i].is_ascii_hexdigit() {
                    let d = (bytes[i] as char).to_digit(16).unwrap() as u8;
                    val = val * 16 + d;
                    i += 1;
                    count += 1;
                }
                ensure!(count > 0, "invalid hex escape in string literal");
                out.push(val);
            }
            c if (b'0'..=b'7').contains(&c) => {
                let mut val = 0u8;
                let mut count = 0;
                while i < bytes.len() && count < 3 && (b'0'..=b'7').contains(&bytes[i]) {
                    val = val.wrapping_mul(8).wrapping_add(bytes[i] - b'0');
                    i += 1;
                    count += 1;
                }
                out.push(val);
            }
            other => bail!("unsupported string escape \\{}", other as char),
        }
    }
    out.push(0u8);
    Ok(out)
}

pub fn translate(
    ast: &Value,
    translation_unit: &Path,
    root: &Path,
) -> Result<TranslationUnitOutput> {
    ensure!(
        kind(ast) == "TranslationUnitDecl",
        "unexpected Clang AST root"
    );

    // Pass 1: track file context, typedefs, enums, structs, and unions.
    let mut typedefs: BTreeMap<String, String> = BTreeMap::new();
    let mut anon_typedefs: BTreeMap<String, String> = BTreeMap::new();
    let mut anon_tags: BTreeMap<String, String> = BTreeMap::new();
    let mut enum_constants: BTreeMap<String, (i64, IntKind)> = BTreeMap::new();
    let mut record_nodes: Vec<(String, &Value)> = Vec::new();

    let mut current_file = translation_unit.to_path_buf();
    for node in children(ast) {
        if node["isImplicit"].as_bool() == Some(true) {
            continue;
        }
        let location = expansion_location(&node["loc"]);
        if let Some(file) = location["file"].as_str() {
            current_file = Path::new(file).to_path_buf();
        }
        let Ok(physical_file) = current_file.canonicalize() else {
            continue;
        };
        let in_project = physical_file.starts_with(root);

        match kind(node) {
            "RecordDecl" if in_project => {
                if text(node, "name").is_empty() {
                    anon_tags.insert(
                        text(node, "id").to_owned(),
                        text(node, "tagUsed").to_owned(),
                    );
                }
                if let Some(rel) = physical_file
                    .strip_prefix(root)
                    .ok()
                    .and_then(|p| p.to_str())
                {
                    record_nodes.push((rel.to_owned(), node));
                }
            }
            "EnumDecl" => {
                let mut next_val: i64 = 0;
                for item in children(node) {
                    if kind(item) != "EnumConstantDecl" {
                        continue;
                    }
                    let val = if let Some(first) = children(item).first() {
                        eval_constant_int(first).unwrap_or(next_val)
                    } else {
                        next_val
                    };
                    let ik = CType::from_node_type(&item["type"], &typedefs)
                        .ok()
                        .and_then(|t| match t {
                            CType::Int(k) => Some(k),
                            _ => None,
                        })
                        .unwrap_or(IntKind::I32);
                    enum_constants.insert(text(item, "id").to_owned(), (val, ik));
                    next_val = val.saturating_add(1);
                }
            }
            "TypedefDecl" => {
                let td_name = text(node, "name");
                if td_name.is_empty() {
                    continue;
                }
                let inner = children(node);
                if let Some(first) = inner.first()
                    && kind(first) == "RecordType"
                    && text(&first["decl"], "name").is_empty()
                {
                    let decl_id = text(&first["decl"], "id");
                    if !decl_id.is_empty() {
                        let tag = anon_tags
                            .get(decl_id)
                            .map(String::as_str)
                            .unwrap_or("struct");
                        anon_typedefs.insert(decl_id.to_owned(), td_name.to_owned());
                        typedefs.insert(td_name.to_owned(), format!("{tag} {td_name}"));
                        continue;
                    }
                }
                let underlying = text(&node["type"], "qualType");
                if !underlying.is_empty()
                    && !underlying.contains("unnamed")
                    && !underlying.contains("anonymous")
                {
                    typedefs.insert(td_name.to_owned(), underlying.to_owned());
                }
            }
            _ => {}
        }
    }

    let mut structs_map: BTreeMap<String, Vec<(String, CType, Option<u32>)>> = BTreeMap::new();
    let mut unions_map: BTreeMap<String, Vec<(String, CType)>> = BTreeMap::new();
    let mut structs = Vec::new();
    let mut seen_records = BTreeSet::new();

    for (rel_file, node) in record_nodes {
        let tag = text(node, "tagUsed");
        ensure!(
            matches!(tag, "struct" | "union"),
            "unsupported record tag {tag:?}; only struct and union are supported"
        );
        if node["completeDefinition"].as_bool() != Some(true) {
            continue;
        }
        let name = if !text(node, "name").is_empty() {
            text(node, "name").to_owned()
        } else {
            let id = text(node, "id");
            anon_typedefs
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unnamed {tag} without typedef is unsupported"))?
        };
        identifier(&name)?;
        if !seen_records.insert(format!("{tag}:{name}")) {
            continue;
        }
        let physical_file = root.join(&rel_file);
        let source = std::fs::read_to_string(&physical_file)?;

        if tag == "struct" {
            let mut fields_typed = Vec::new();
            let mut fields_model = Vec::new();
            let mut field_lines = Vec::new();
            let mut default_lines = Vec::new();

            for child in children(node) {
                if kind(child) == "RecordDecl" {
                    continue;
                }
                ensure!(
                    kind(child) == "FieldDecl",
                    "unsupported declaration {} inside struct {name}",
                    kind(child)
                );
                let fname = text(child, "name");
                identifier(fname)?;
                let fty = CType::from_node_type(&child["type"], &typedefs)?;
                ensure!(
                    !matches!(fty, CType::Void),
                    "void field in struct {name} is unsupported"
                );
                let bitwidth = if child["isBitfield"].as_bool() == Some(true) {
                    let CType::Int(ik) = fty else {
                        bail!("bitfield {name}.{fname} must have integer type");
                    };
                    let w = children(child)
                        .first()
                        .and_then(eval_constant_int)
                        .with_context(|| format!("missing bitfield width on {name}.{fname}"))?;
                    ensure!(
                        w >= 1 && w <= ik.bit_width() as i64,
                        "bitfield width {w} out of range on {name}.{fname}"
                    );
                    Some(w as u32)
                } else {
                    None
                };
                let rty = fty.rust_type();
                field_lines.push(format!("    pub {fname}: {rty},"));
                fields_model.push(StructField {
                    name: fname.to_owned(),
                    c_type: fty.c_type(),
                    rust_type: rty,
                });
                fields_typed.push((fname.to_owned(), fty, bitwidth));
            }
            structs_map.insert(name.clone(), fields_typed);
            for (fname, fty, _) in &structs_map[&name] {
                let z = fty.zero_expr(&structs_map)?;
                default_lines.push(format!("            {fname}: {z},"));
            }
            let rust_name = format!("RfStruct_{name}");
            let rust = format!(
                "#[repr(C)]\n#[derive(Clone, Copy, Debug)]\npub struct {rust_name} {{\n{}\n}}\nimpl Default for {rust_name} {{\n    fn default() -> Self {{\n        unsafe {{\n            Self {{\n{}\n            }}\n        }}\n    }}\n}}\n",
                field_lines.join("\n"),
                default_lines.join("\n"),
            );
            structs.push(StructDef {
                name,
                rust_name,
                fields: fields_model,
                source_file: rel_file,
                line_start: line_from_offset(&source, &node["range"]["begin"])?,
                line_end: line_from_offset(&source, &node["range"]["end"])?,
                rust,
            });
        } else {
            // union
            let mut fields_typed = Vec::new();
            let mut fields_model = Vec::new();
            let mut field_lines = Vec::new();

            for child in children(node) {
                if kind(child) == "RecordDecl" {
                    continue;
                }
                ensure!(
                    kind(child) == "FieldDecl",
                    "unsupported declaration {} inside union {name}",
                    kind(child)
                );
                ensure!(
                    child["isBitfield"].as_bool() != Some(true),
                    "bitfield inside union {name} is unsupported"
                );
                let fname = text(child, "name");
                identifier(fname)?;
                let fty = CType::from_node_type(&child["type"], &typedefs)?;
                ensure!(
                    !matches!(fty, CType::Void),
                    "void field in union {name} is unsupported"
                );
                let rty = fty.rust_type();
                field_lines.push(format!("    pub {fname}: {rty},"));
                fields_model.push(StructField {
                    name: fname.to_owned(),
                    c_type: fty.c_type(),
                    rust_type: rty,
                });
                fields_typed.push((fname.to_owned(), fty));
            }
            unions_map.insert(name.clone(), fields_typed);
            let rust_name = format!("RfUnion_{name}");
            let rust = format!(
                "#[repr(C)]\n#[derive(Clone, Copy)]\npub union {rust_name} {{\n{}\n}}\nimpl Default for {rust_name} {{\n    fn default() -> Self {{\n        unsafe {{ core::mem::zeroed() }}\n    }}\n}}\nimpl core::fmt::Debug for {rust_name} {{\n    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {{\n        f.write_str(\"{rust_name}\")\n    }}\n}}\n",
                field_lines.join("\n"),
            );
            structs.push(StructDef {
                name: format!("union:{name}"),
                rust_name,
                fields: fields_model,
                source_file: rel_file,
                line_start: line_from_offset(&source, &node["range"]["begin"])?,
                line_end: line_from_offset(&source, &node["range"]["end"])?,
                rust,
            });
        }
    }

    // Pass 2: collect static const globals, translate project functions, and collect prototypes.
    let mut constants = Vec::new();
    let mut const_globals: BTreeMap<String, (String, CType, bool)> = BTreeMap::new();
    let mut functions = Vec::new();
    let mut prototypes: BTreeMap<String, ExternalFunction> = BTreeMap::new();
    let mut current_file = translation_unit.to_path_buf();

    for node in children(ast) {
        if node["isImplicit"].as_bool() == Some(true) {
            continue;
        }
        let location = expansion_location(&node["loc"]);
        if let Some(file) = location["file"].as_str() {
            current_file = Path::new(file).to_path_buf();
        }
        let physical_file = current_file.canonicalize()?;
        let in_project = physical_file.starts_with(root);

        if !in_project {
            if kind(node) == "FunctionDecl"
                && !children(node).iter().any(|n| kind(n) == "CompoundStmt")
                && !node["variadic"].as_bool().unwrap_or(false)
            {
                let name = text(node, "name");
                if identifier(name).is_ok()
                    && let Ok((param_types, ret_type)) =
                        parse_fn_signature(text(&node["type"], "qualType"), &typedefs)
                {
                    prototypes
                        .entry(name.to_owned())
                        .or_insert_with(|| make_external_function(name, &param_types, &ret_type));
                }
            }
            continue;
        }

        let file = physical_file
            .strip_prefix(root)?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF8 source path"))?;
        match kind(node) {
            "RecordDecl" | "TypedefDecl" | "EnumDecl" => continue,
            "VarDecl" => {
                let gname = text(node, "name");
                identifier(gname)?;
                ensure!(
                    is_const_qualified(&node["type"]),
                    "mutable global variable {gname} is unsupported"
                );
                ensure!(
                    children(node).len() == 1,
                    "const global {gname} must have an initializer"
                );
                let gty = CType::from_node_type(&node["type"], &typedefs)?;
                let mut temp_deps = prototypes.clone();
                let mut temp_emitter = Emitter {
                    variables: BTreeMap::new(),
                    const_globals: &const_globals,
                    dependencies: BTreeSet::new(),
                    next_variable: 0,
                    loop_labels: Vec::new(),
                    break_target: Vec::new(),
                    forward_labels: BTreeMap::new(),
                    return_type: CType::Void,
                    typedefs: &typedefs,
                    enum_constants: &enum_constants,
                    structs: &structs_map,
                    unions: &unions_map,
                    prototypes: &mut temp_deps,
                };
                let (init_expr, init_ty) = temp_emitter.typed_expression(&children(node)[0])?;
                ensure!(
                    gty.same_shape(&init_ty),
                    "const global {gname} initializer type mismatch"
                );
                let rust_name = format!("RF_CONST_{gname}");
                let is_static = !gty.contains_raw_ptr(&structs_map, &unions_map);
                let kw = if is_static { "pub static" } else { "pub const" };
                let rty = gty.rust_type();
                let val_block = if is_static {
                    format!("unsafe {{ {init_expr} }}")
                } else {
                    init_expr
                };
                constants.push(format!("{kw} {rust_name}: {rty} = {val_block};\n"));
                const_globals.insert(text(node, "id").to_owned(), (rust_name, gty, is_static));
                continue;
            }
            "FunctionDecl" => {}
            other => bail!("unsupported top-level {other}"),
        }

        let name = text(node, "name");
        identifier(name)?;
        ensure!(
            !node["variadic"].as_bool().unwrap_or(false),
            "variadic functions are unsupported"
        );
        let storage_class = text(node, "storageClass");
        ensure!(
            matches!(storage_class, "" | "static" | "extern"),
            "unsupported function storage class"
        );
        let is_static = storage_class == "static";

        let params: Vec<_> = children(node)
            .iter()
            .filter(|n| kind(n) == "ParmVarDecl")
            .collect();
        let mut param_ctypes = Vec::with_capacity(params.len());
        for param in &params {
            let pty = CType::from_node_type(&param["type"], &typedefs)?;
            ensure!(
                !matches!(pty, CType::Void),
                "void parameter type unsupported"
            );
            param_ctypes.push(pty);
        }
        let (sig_params, return_ctype) =
            parse_fn_signature(text(&node["type"], "qualType"), &typedefs)?;
        ensure!(
            sig_params.len() == param_ctypes.len()
                && sig_params
                    .iter()
                    .zip(&param_ctypes)
                    .all(|(a, b)| a.same_shape(b)),
            "unsupported function type {}",
            text(&node["type"], "qualType")
        );

        let Some(body) = children(node).iter().find(|n| kind(n) == "CompoundStmt") else {
            prototypes
                .entry(name.to_owned())
                .or_insert_with(|| make_external_function(name, &param_ctypes, &return_ctype));
            continue;
        };

        ensure!(
            name != "main",
            "main functions are not supported; select a library entry function"
        );
        ensure!(
            children(node)
                .iter()
                .all(|n| matches!(kind(n), "ParmVarDecl" | "CompoundStmt")
                    || (matches!(
                        kind(n),
                        "BuiltinAttr" | "NoThrowAttr" | "ConstAttr" | "PureAttr"
                    ) && (n["inherited"].as_bool() == Some(true)
                        || n["implicit"].as_bool() == Some(true)))),
            "function attributes or other declarations are unsupported"
        );

        let source = std::fs::read_to_string(&physical_file)?;
        let mut emitter = Emitter {
            variables: BTreeMap::new(),
            const_globals: &const_globals,
            dependencies: BTreeSet::new(),
            next_variable: 0,
            loop_labels: Vec::new(),
            break_target: Vec::new(),
            forward_labels: BTreeMap::new(),
            return_type: return_ctype.clone(),
            typedefs: &typedefs,
            enum_constants: &enum_constants,
            structs: &structs_map,
            unions: &unions_map,
            prototypes: &mut prototypes,
        };
        let mut parameters = Vec::new();
        let mut signature = Vec::new();
        let mut abi_signature = Vec::new();
        let mut abi_args = Vec::new();
        for (i, (param, pty)) in params.iter().zip(param_ctypes.iter()).enumerate() {
            let original = text(param, "name");
            ensure!(!original.is_empty(), "unnamed definition parameter");
            parameters.push(original.to_string());
            let generated = format!("v{i}");
            emitter.variables.insert(
                text(param, "id").to_owned(),
                (generated.clone(), pty.clone()),
            );
            signature.push(format!("mut {generated}: {}", pty.rust_type()));
            abi_signature.push(format!("{generated}: {}", pty.rust_type()));
            abi_args.push(generated);
        }
        emitter.next_variable = params.len();
        let body_code = emitter.statement(body)?;
        let abi_export = if is_static {
            String::new()
        } else {
            let ret_suffix = if matches!(return_ctype, CType::Void) {
                String::new()
            } else {
                format!(" -> {}", return_ctype.rust_type())
            };
            format!(
                "#[unsafe(no_mangle)]\npub extern \"C\" fn rf_abi_{name}({}){ret_suffix} {{\n    rf_{name}({})\n}}\n",
                abi_signature.join(", "),
                abi_args.join(", "),
            )
        };
        let rust = if matches!(return_ctype, CType::Void) {
            format!(
                "pub fn rf_{name}({}) {{\nunsafe {{\n{body_code}\n}}\n}}\n{abi_export}",
                signature.join(", ")
            )
        } else {
            format!(
                "pub fn rf_{name}({}) -> {} {{\nunsafe {{\n{body_code}\npanic!(\"C function reached end without a return\");\n}}\n}}\n{abi_export}",
                signature.join(", "),
                return_ctype.rust_type()
            )
        };

        functions.push(Function {
            name: name.into(),
            parameters,
            parameter_types: param_ctypes.iter().map(CType::c_type).collect(),
            return_type: return_ctype.c_type(),
            source_file: file.into(),
            line_start: line_from_offset(&source, &node["range"]["begin"])?,
            line_end: line_from_offset(&source, &node["range"]["end"])?,
            dependencies: emitter.dependencies.into_iter().collect(),
            external_dependencies: Vec::new(),
            is_static,
            rust,
        });
    }

    Ok(TranslationUnitOutput {
        structs,
        constants,
        functions,
        prototypes: prototypes.into_values().collect(),
    })
}

enum SwitchLabel<'a> {
    Case(&'a Value),
    Default,
}

struct SwitchArm<'a> {
    labels: Vec<SwitchLabel<'a>>,
    stmts: Vec<&'a Value>,
}

fn flatten_switch_node<'a>(
    node: &'a Value,
    arms: &mut Vec<SwitchArm<'a>>,
    pending_labels: &mut Vec<SwitchLabel<'a>>,
) -> Result<()> {
    match kind(node) {
        "CaseStmt" => {
            let inner = children(node);
            ensure!(inner.len() >= 2, "invalid case statement");
            pending_labels.push(SwitchLabel::Case(&inner[0]));
            flatten_switch_node(inner.last().unwrap(), arms, pending_labels)
        }
        "DefaultStmt" => {
            let inner = children(node);
            ensure!(!inner.is_empty(), "invalid default statement");
            pending_labels.push(SwitchLabel::Default);
            flatten_switch_node(inner.last().unwrap(), arms, pending_labels)
        }
        _ => {
            if !pending_labels.is_empty() || arms.is_empty() {
                arms.push(SwitchArm {
                    labels: std::mem::take(pending_labels),
                    stmts: vec![node],
                });
            } else {
                arms.last_mut().unwrap().stmts.push(node);
            }
            Ok(())
        }
    }
}

struct Emitter<'a> {
    variables: BTreeMap<String, (String, CType)>,
    const_globals: &'a BTreeMap<String, (String, CType, bool)>,
    dependencies: BTreeSet<String>,
    next_variable: usize,
    loop_labels: Vec<String>,
    break_target: Vec<Option<String>>,
    forward_labels: BTreeMap<String, String>,
    return_type: CType,
    typedefs: &'a BTreeMap<String, String>,
    enum_constants: &'a BTreeMap<String, (i64, IntKind)>,
    structs: &'a StructFieldsMap,
    unions: &'a UnionFieldsMap,
    prototypes: &'a mut BTreeMap<String, ExternalFunction>,
}

impl Emitter<'_> {
    fn fresh_var(&mut self, prefix: &str) -> String {
        let id = self.next_variable;
        self.next_variable += 1;
        format!("{prefix}{id}")
    }

    fn variable(&self, node: &Value) -> Result<(String, CType)> {
        ensure!(
            kind(node) == "DeclRefExpr",
            "expected local variable reference"
        );
        let id = text(&node["referencedDecl"], "id");
        if let Some(v) = self.variables.get(id) {
            return Ok(v.clone());
        }
        if let Some((name, ty, _)) = self.const_globals.get(id) {
            return Ok((name.clone(), ty.clone()));
        }
        bail!("unknown or nonlocal variable")
    }

    fn member_type(&self, base_ty: &CType, field_name: &str) -> Result<CType> {
        match base_ty {
            CType::Struct(sname) => {
                let fields = self
                    .structs
                    .get(sname)
                    .with_context(|| format!("unknown struct {sname}"))?;
                fields
                    .iter()
                    .find(|(n, _, _)| n == field_name)
                    .map(|(_, ty, _)| ty.clone())
                    .with_context(|| format!("unknown field {field_name} on struct {sname}"))
            }
            CType::Union(uname) => {
                let fields = self
                    .unions
                    .get(uname)
                    .with_context(|| format!("unknown union {uname}"))?;
                fields
                    .iter()
                    .find(|(n, _)| n == field_name)
                    .map(|(_, ty)| ty.clone())
                    .with_context(|| format!("unknown field {field_name} on union {uname}"))
            }
            _ => bail!("member access requires struct or union"),
        }
    }

    fn lvalue_bitfield(&self, mut node: &Value) -> Option<(IntKind, u32)> {
        while kind(node) == "ParenExpr" {
            node = children(node).first()?;
        }
        if kind(node) != "MemberExpr" {
            return None;
        }
        let field = text(node, "name");
        let inner = children(node).first()?;
        let base_ty = CType::from_node_type(&inner["type"], self.typedefs).ok()?;
        let sname = if node["isArrow"].as_bool().unwrap_or(false) {
            match base_ty {
                CType::Ptr { inner, .. } => match *inner {
                    CType::Struct(s) => s,
                    _ => return None,
                },
                _ => return None,
            }
        } else {
            match base_ty {
                CType::Struct(s) => s,
                _ => return None,
            }
        };
        self.structs
            .get(&sname)?
            .iter()
            .find(|(n, _, _)| n == field)
            .and_then(|(_, ty, bw)| match (ty, bw) {
                (CType::Int(ik), Some(w)) => Some((*ik, *w)),
                _ => None,
            })
    }

    fn lvalue(&mut self, node: &Value) -> Result<(String, CType)> {
        let inner = children(node);
        match kind(node) {
            "DeclRefExpr" => self.variable(node),
            "ParenExpr" => {
                ensure!(inner.len() == 1, "invalid parenthesized lvalue");
                self.lvalue(&inner[0])
            }
            "UnaryOperator" if text(node, "opcode") == "*" => {
                ensure!(inner.len() == 1, "invalid dereference");
                let (ptr, ty) = self.typed_expression(&inner[0])?;
                match ty {
                    CType::Ptr { inner, .. } => {
                        ensure!(!matches!(*inner, CType::Void), "cannot dereference void *");
                        Ok((format!("(*rf_ptr({ptr}))"), *inner))
                    }
                    _ => bail!("dereference requires a pointer"),
                }
            }
            "MemberExpr" => {
                ensure!(inner.len() == 1, "invalid member expression");
                let field = text(node, "name");
                identifier(field)?;
                if node["isArrow"].as_bool().unwrap_or(false) {
                    let (ptr, ty) = self.typed_expression(&inner[0])?;
                    match ty {
                        CType::Ptr { inner, .. } => {
                            let fty = self.member_type(&inner, field)?;
                            Ok((format!("(*rf_ptr({ptr})).{field}"), fty))
                        }
                        _ => bail!("arrow member access requires pointer"),
                    }
                } else {
                    let (base, ty) = self.lvalue(&inner[0])?;
                    let fty = self.member_type(&ty, field)?;
                    Ok((format!("({base}).{field}"), fty))
                }
            }
            "ArraySubscriptExpr" => {
                ensure!(inner.len() == 2, "invalid array subscript");
                let (lhs, lty) = self.typed_expression(&inner[0])?;
                let (rhs, rty) = self.typed_expression(&inner[1])?;
                let (ptr, elem_ty, idx) = match (lty, rty) {
                    (CType::Ptr { inner, .. }, CType::Int(_)) => (lhs, *inner, rhs),
                    (CType::Int(_), CType::Ptr { inner, .. }) => (rhs, *inner, lhs),
                    _ => bail!("array subscript requires pointer and int index"),
                };
                ensure!(!matches!(elem_ty, CType::Void), "cannot index into void *");
                Ok((
                    format!("(*rf_ptr(({ptr}).wrapping_offset(({idx}) as isize)))"),
                    elem_ty,
                ))
            }
            other => bail!("unsupported lvalue {other}"),
        }
    }

    fn condition(&mut self, node: &Value) -> Result<String> {
        let (expr, ty) = self.typed_expression(node)?;
        match ty {
            CType::Int(_) => Ok(format!("(({expr}) != 0)")),
            CType::Ptr { .. } => Ok(format!("(!({expr}).is_null())")),
            CType::FnPtr { .. } => Ok(format!("(({expr}).is_some())")),
            _ => bail!("unsupported condition type"),
        }
    }

    fn int_binary_op(op: &str, kind: IntKind, left: &str, right: &str) -> Result<String> {
        Ok(match op {
            "+" | "-" | "*" => {
                if kind.is_signed() {
                    let method = match op {
                        "+" => "checked_add",
                        "-" => "checked_sub",
                        _ => "checked_mul",
                    };
                    format!("({left}).{method}({right}).expect(\"undefined C arithmetic\")")
                } else {
                    let method = match op {
                        "+" => "wrapping_add",
                        "-" => "wrapping_sub",
                        _ => "wrapping_mul",
                    };
                    format!("({left}).{method}({right})")
                }
            }
            "/" | "%" => {
                let method = if op == "/" {
                    "checked_div"
                } else {
                    "checked_rem"
                };
                format!("({left}).{method}({right}).expect(\"undefined C arithmetic\")")
            }
            "<<" | ">>" => {
                let method = if op == "<<" {
                    "checked_shl"
                } else {
                    "checked_shr"
                };
                format!(
                    "({{ let rf_l = {left}; let rf_r = {right}; u32::try_from(rf_r).ok().and_then(|s| rf_l.{method}(s)).expect(\"undefined C shift\") }})"
                )
            }
            "&" | "|" | "^" => format!("(({left}) {op} ({right}))"),
            _ => bail!("unsupported integer binary operator {op}"),
        })
    }

    fn emit_init_list(&mut self, node: &Value, ty: &CType) -> Result<String> {
        match ty {
            CType::Struct(sname) => {
                let fields = self
                    .structs
                    .get(sname)
                    .with_context(|| format!("unknown struct {sname}"))?
                    .clone();
                let inner = children(node);
                ensure!(
                    inner.len() <= fields.len(),
                    "too many initializers for struct {sname}"
                );
                let mut field_inits = Vec::with_capacity(fields.len());
                for (i, (fname, fty, bitwidth)) in fields.iter().enumerate() {
                    let mut init_expr = if let Some(expr_node) = inner.get(i) {
                        let (e, ety) = self.typed_expression(expr_node)?;
                        ensure!(
                            ety.same_shape(fty),
                            "struct initializer type mismatch on {sname}.{fname}"
                        );
                        e
                    } else {
                        fty.zero_expr(self.structs)?
                    };
                    if let (Some(w), CType::Int(ik)) = (bitwidth, fty) {
                        init_expr = normalize_bitfield(&init_expr, *ik, *w);
                    }
                    field_inits.push(format!("{fname}: {init_expr}"));
                }
                Ok(format!("RfStruct_{sname} {{ {} }}", field_inits.join(", ")))
            }
            CType::Union(uname) => {
                let fields = self
                    .unions
                    .get(uname)
                    .with_context(|| format!("unknown union {uname}"))?
                    .clone();
                let inner = children(node);
                if inner.is_empty() || kind(&inner[0]) == "ImplicitValueInitExpr" {
                    return Ok(format!("core::mem::zeroed::<RfUnion_{uname}>()"));
                }
                let active_field_name = node["field"]["name"].as_str();
                let (fname, fty) = if let Some(target) = active_field_name {
                    fields
                        .iter()
                        .find(|(n, _)| n == target)
                        .cloned()
                        .with_context(|| format!("unknown union field {uname}.{target}"))?
                } else {
                    fields
                        .first()
                        .cloned()
                        .with_context(|| format!("empty union {uname}"))?
                };
                let (val_expr, val_ty) = self.typed_expression(&inner[0])?;
                ensure!(
                    val_ty.same_shape(&fty),
                    "union initializer type mismatch on {uname}.{fname}"
                );
                let u_var = self.fresh_var("rf_u");
                Ok(format!(
                    "{{ let mut {u_var}: RfUnion_{uname} = core::mem::zeroed(); {u_var}.{fname} = {val_expr}; {u_var} }}"
                ))
            }
            CType::Array(elem_ty, len) => {
                let mut elem_exprs = Vec::with_capacity(*len);
                if let Some(filler_arr) = node["array_filler"].as_array()
                    && !filler_arr.is_empty()
                {
                    let filler_node = &filler_arr[0];
                    let explicit = &filler_arr[1..];
                    ensure!(explicit.len() <= *len, "too many array initializers");
                    for idx in 0..*len {
                        let src = explicit.get(idx).unwrap_or(filler_node);
                        let (e, ety) = self.typed_expression(src)?;
                        ensure!(
                            ety.same_shape(elem_ty),
                            "array initializer element type mismatch"
                        );
                        elem_exprs.push(e);
                    }
                } else {
                    let inner = children(node);
                    ensure!(inner.len() <= *len, "too many array initializers");
                    for idx in 0..*len {
                        let e = if let Some(src) = inner.get(idx) {
                            let (e, ety) = self.typed_expression(src)?;
                            ensure!(
                                ety.same_shape(elem_ty),
                                "array initializer element type mismatch"
                            );
                            e
                        } else {
                            elem_ty.zero_expr(self.structs)?
                        };
                        elem_exprs.push(e);
                    }
                }
                Ok(format!("[{}]", elem_exprs.join(", ")))
            }
            _ => bail!("initializer list only supported for struct, union, and array types"),
        }
    }

    fn sizeof_node_type(&self, node: &Value) -> Result<CType> {
        let mut cur = node;
        while kind(cur) == "ParenExpr" {
            let inner = children(cur);
            ensure!(inner.len() == 1, "invalid parenthesized sizeof operand");
            cur = &inner[0];
        }
        CType::from_node_type(&cur["type"], self.typedefs)
    }

    fn typed_expression(&mut self, node: &Value) -> Result<(String, CType)> {
        let inner = children(node);
        match kind(node) {
            "IntegerLiteral" => {
                let ty = CType::from_node_type(&node["type"], self.typedefs)?;
                let CType::Int(ik) = ty else {
                    bail!("non-int integer literal");
                };
                let raw = text(node, "value");
                let formatted = if ik.is_signed() {
                    let val = raw.parse::<i128>()?;
                    format!("{val}{}", ik.rust_type())
                } else {
                    let val = raw.parse::<u128>()?;
                    format!("{val}{}", ik.rust_type())
                };
                Ok((formatted, ty))
            }
            "CharacterLiteral" => {
                let ty = CType::from_node_type(&node["type"], self.typedefs)?;
                let CType::Int(ik) = ty else {
                    bail!("non-int character literal");
                };
                let val = node["value"]
                    .as_i64()
                    .context("missing character literal value")?;
                Ok((format!("{val}{}", ik.rust_type()), ty))
            }
            "StringLiteral" => {
                let ty = CType::from_node_type(&node["type"], self.typedefs)?;
                let bytes = parse_c_string_literal(text(node, "value"))?;
                let char_rty = IntKind::Char.rust_type();
                let elems = bytes
                    .iter()
                    .map(|b| {
                        if char_rty == "i8" {
                            format!("{}i8", (*b as i8))
                        } else {
                            format!("{b}u8")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok((format!("[{elems}]"), ty))
            }
            "ConstantExpr" => {
                if let Some(first) = inner.first() {
                    self.typed_expression(first)
                } else {
                    let ty = CType::from_node_type(&node["type"], self.typedefs)?;
                    let CType::Int(ik) = ty else {
                        bail!("unsupported ConstantExpr");
                    };
                    let val = eval_constant_int(node).context("invalid ConstantExpr")?;
                    Ok((format!("{val}{}", ik.rust_type()), ty))
                }
            }
            "DeclRefExpr" => {
                let ref_decl = &node["referencedDecl"];
                match kind(ref_decl) {
                    "EnumConstantDecl" => {
                        let (val, ik) = self
                            .enum_constants
                            .get(text(ref_decl, "id"))
                            .copied()
                            .with_context(|| {
                                format!("unknown enum constant {}", text(ref_decl, "name"))
                            })?;
                        Ok((format!("{val}{}", ik.rust_type()), CType::Int(ik)))
                    }
                    "FunctionDecl" => {
                        let name = text(ref_decl, "name");
                        identifier(name)?;
                        let (param_types, ret_type) =
                            parse_fn_signature(text(&ref_decl["type"], "qualType"), self.typedefs)?;
                        self.dependencies.insert(name.into());
                        self.prototypes.entry(name.to_owned()).or_insert_with(|| {
                            make_external_function(name, &param_types, &ret_type)
                        });
                        let fn_ty = CType::FnPtr {
                            params: param_types,
                            ret: Box::new(ret_type),
                        };
                        let rust_fn_ty = fn_ty.fn_type_only()?;
                        Ok((format!("Some(rf_{name} as {rust_fn_ty})"), fn_ty))
                    }
                    _ => self.lvalue(node),
                }
            }
            "ArraySubscriptExpr" => self.lvalue(node),
            "MemberExpr" => {
                ensure!(inner.len() == 1, "invalid member expression");
                if node["isArrow"].as_bool().unwrap_or(false) {
                    self.lvalue(node)
                } else {
                    let field = text(node, "name");
                    identifier(field)?;
                    let (base, ty) = self.typed_expression(&inner[0])?;
                    let fty = self.member_type(&ty, field)?;
                    Ok((format!("({base}).{field}"), fty))
                }
            }
            "ParenExpr" => {
                ensure!(inner.len() == 1, "invalid parentheses");
                let (expr, ty) = self.typed_expression(&inner[0])?;
                Ok((format!("({expr})"), ty))
            }
            "ImplicitValueInitExpr" => {
                let ty = CType::from_node_type(&node["type"], self.typedefs)?;
                let z = ty.zero_expr(self.structs)?;
                Ok((z, ty))
            }
            "InitListExpr" => {
                let ty = CType::from_node_type(&node["type"], self.typedefs)?;
                let expr = self.emit_init_list(node, &ty)?;
                Ok((expr, ty))
            }
            "CompoundLiteralExpr" => {
                ensure!(inner.len() == 1, "invalid compound literal");
                self.typed_expression(&inner[0])
            }
            "UnaryExprOrTypeTraitExpr" => {
                let trait_name = text(node, "name");
                let mem_fn = match trait_name {
                    "sizeof" => "size_of",
                    "alignof" | "_Alignof" => "align_of",
                    other => bail!("unsupported type trait {other}"),
                };
                let result_ty = CType::from_node_type(&node["type"], self.typedefs)?;
                let target_ty = if node.get("argType").is_some() {
                    CType::from_node_type(&node["argType"], self.typedefs)?
                } else {
                    ensure!(inner.len() == 1, "invalid {trait_name} operand");
                    self.sizeof_node_type(&inner[0])?
                };
                Ok((
                    format!(
                        "(core::mem::{mem_fn}::<{}>() as {})",
                        target_ty.rust_type(),
                        result_ty.rust_type()
                    ),
                    result_ty,
                ))
            }
            "ImplicitCastExpr" | "CStyleCastExpr" => {
                ensure!(inner.len() == 1, "invalid cast expression");
                let target_ty = CType::from_node_type(&node["type"], self.typedefs)?;
                match text(node, "castKind") {
                    "LValueToRValue" | "NoOp" | "FunctionToPointerDecay" => {
                        let (expr, inner_ty) = self.typed_expression(&inner[0])?;
                        ensure!(
                            target_ty.same_shape(&inner_ty),
                            "unsupported implicit/noop conversion"
                        );
                        Ok((expr, target_ty))
                    }
                    "ArrayToPointerDecay" => {
                        if kind(&inner[0]) == "StringLiteral" {
                            let bytes = parse_c_string_literal(text(&inner[0], "value"))?;
                            let char_rty = IntKind::Char.rust_type();
                            let elems = bytes
                                .iter()
                                .map(|b| {
                                    if char_rty == "i8" {
                                        format!("{}i8", (*b as i8))
                                    } else {
                                        format!("{b}u8")
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            return Ok((
                                format!("((&[{elems}]).as_ptr() as *mut {char_rty})"),
                                target_ty,
                            ));
                        }
                        if kind(&inner[0]) == "DeclRefExpr"
                            && let Some((cname, cty, is_static)) = self
                                .const_globals
                                .get(text(&inner[0]["referencedDecl"], "id"))
                        {
                            let CType::Array(elem_ty, _) = cty else {
                                bail!("ArrayToPointerDecay requires array operand");
                            };
                            ensure!(
                                target_ty.same_shape(&CType::Ptr {
                                    inner: elem_ty.clone(),
                                    inner_const: false
                                }),
                                "ArrayToPointerDecay type mismatch"
                            );
                            let ptr_expr = if *is_static {
                                format!("({cname}.as_ptr() as *mut {})", elem_ty.rust_type())
                            } else {
                                format!("((&{cname}).as_ptr() as *mut {})", elem_ty.rust_type())
                            };
                            return Ok((ptr_expr, target_ty));
                        }
                        let (lv, arr_ty) = self.lvalue(&inner[0])?;
                        let CType::Array(elem_ty, _) = arr_ty else {
                            bail!("ArrayToPointerDecay requires array operand");
                        };
                        ensure!(
                            target_ty.same_shape(&CType::Ptr {
                                inner: elem_ty,
                                inner_const: false
                            }),
                            "ArrayToPointerDecay type mismatch"
                        );
                        Ok((format!("(&mut ({lv})).as_mut_ptr()"), target_ty))
                    }
                    "NullToPointer" => {
                        ensure!(
                            target_ty.is_pointer_like(),
                            "NullToPointer requires pointer or function pointer type"
                        );
                        let null_val = if matches!(target_ty, CType::FnPtr { .. }) {
                            "None"
                        } else {
                            "core::ptr::null_mut()"
                        };
                        Ok((null_val.into(), target_ty))
                    }
                    "BitCast" => {
                        let (expr, inner_ty) = self.typed_expression(&inner[0])?;
                        match (&inner_ty, &target_ty) {
                            (CType::Ptr { .. }, CType::Ptr { .. }) => {
                                Ok((format!("({expr} as {})", target_ty.rust_type()), target_ty))
                            }
                            (CType::Ptr { .. }, CType::FnPtr { .. }) => {
                                if expr.trim_matches(|c| c == '(' || c == ')')
                                    == "core::ptr::null_mut()"
                                {
                                    Ok(("None".into(), target_ty))
                                } else {
                                    let fnty = target_ty.fn_type_only()?;
                                    let inner_rust = inner_ty.rust_type();
                                    let tmp = self.fresh_var("rf_fp");
                                    Ok((
                                        format!(
                                            "{{ let {tmp}: {inner_rust} = {expr}; if {tmp}.is_null() {{ None }} else {{ Some(core::mem::transmute::<_, {fnty}>({tmp})) }} }}"
                                        ),
                                        target_ty,
                                    ))
                                }
                            }
                            (CType::FnPtr { .. }, CType::Ptr { .. }) => {
                                let tmp = self.fresh_var("rf_fp");
                                let target_rust = target_ty.rust_type();
                                Ok((
                                    format!(
                                        "{{ match {expr} {{ Some({tmp}) => ({tmp} as {target_rust}), None => core::ptr::null_mut() }} }}"
                                    ),
                                    target_ty,
                                ))
                            }
                            _ => bail!("BitCast only supported between pointer types"),
                        }
                    }
                    "IntegralCast" => {
                        let (expr, inner_ty) = self.typed_expression(&inner[0])?;
                        ensure!(
                            matches!(target_ty, CType::Int(_)) && matches!(inner_ty, CType::Int(_)),
                            "unsupported integral cast"
                        );
                        if matches!(target_ty, CType::Int(IntKind::Bool)) {
                            Ok((format!("u8::from(({expr}) != 0)"), target_ty))
                        } else {
                            Ok((
                                format!("(({expr}) as {})", target_ty.rust_type()),
                                target_ty,
                            ))
                        }
                    }
                    "IntegralToBoolean" => {
                        let (expr, inner_ty) = self.typed_expression(&inner[0])?;
                        ensure!(
                            matches!(inner_ty, CType::Int(_)),
                            "IntegralToBoolean requires integer operand"
                        );
                        Ok((format!("u8::from(({expr}) != 0)"), target_ty))
                    }
                    "PointerToBoolean" => {
                        let (expr, inner_ty) = self.typed_expression(&inner[0])?;
                        let cond = match inner_ty {
                            CType::Ptr { .. } => format!("!({expr}).is_null()"),
                            CType::FnPtr { .. } => format!("({expr}).is_some()"),
                            _ => bail!("PointerToBoolean requires pointer operand"),
                        };
                        Ok((format!("u8::from({cond})"), target_ty))
                    }
                    other => bail!("unsupported castKind {other}"),
                }
            }
            "UnaryOperator" => {
                ensure!(inner.len() == 1, "invalid unary expression");
                let op = text(node, "opcode");
                match op {
                    "&" => {
                        ensure!(
                            self.lvalue_bitfield(&inner[0]).is_none(),
                            "cannot take address of bitfield"
                        );
                        let (lv, lv_ty) = self.lvalue(&inner[0])?;
                        let rty = lv_ty.rust_type();
                        Ok((
                            format!("(&mut ({lv}) as *mut {rty})"),
                            CType::Ptr {
                                inner: Box::new(lv_ty),
                                inner_const: false,
                            },
                        ))
                    }
                    "*" => self.lvalue(node),
                    "!" => {
                        let cond = self.condition(&inner[0])?;
                        Ok((format!("((!({cond})) as i32)"), CType::Int(IntKind::I32)))
                    }
                    "++" | "--" => {
                        let is_postfix = node["isPostfix"].as_bool().unwrap_or(false);
                        let bf = self.lvalue_bitfield(&inner[0]);
                        let (lv, lv_ty) = self.lvalue(&inner[0])?;
                        let ptr_var = self.fresh_var("rf_p");
                        let old_var = self.fresh_var("rf_old");
                        let mut update = match &lv_ty {
                            CType::Int(ik) => {
                                let one = format!("1{}", ik.rust_type());
                                if ik.is_signed() {
                                    let method = if op == "++" {
                                        "checked_add"
                                    } else {
                                        "checked_sub"
                                    };
                                    format!(
                                        "(*{ptr_var}).{method}({one}).expect(\"undefined C arithmetic\")"
                                    )
                                } else {
                                    let method = if op == "++" {
                                        "wrapping_add"
                                    } else {
                                        "wrapping_sub"
                                    };
                                    format!("(*{ptr_var}).{method}({one})")
                                }
                            }
                            CType::Ptr { inner, .. } => {
                                ensure!(
                                    !matches!(**inner, CType::Void),
                                    "cannot increment/decrement void *"
                                );
                                let offset = if op == "++" { "1" } else { "-1" };
                                format!("(*{ptr_var}).wrapping_offset({offset})")
                            }
                            _ => bail!("unsupported type for {op}"),
                        };
                        if let Some((ik, w)) = bf {
                            update = normalize_bitfield(&update, ik, w);
                        }
                        let block = if is_postfix {
                            format!(
                                "{{ let {ptr_var} = &mut ({lv}); let {old_var} = *{ptr_var}; *{ptr_var} = {update}; {old_var} }}"
                            )
                        } else {
                            format!(
                                "{{ let {ptr_var} = &mut ({lv}); *{ptr_var} = {update}; *{ptr_var} }}"
                            )
                        };
                        Ok((block, lv_ty))
                    }
                    "-" => {
                        let (val, ty) = self.typed_expression(&inner[0])?;
                        let CType::Int(ik) = ty else {
                            bail!("unary - requires integer");
                        };
                        let expr = if ik.is_signed() {
                            format!("({val}).checked_neg().expect(\"undefined C negation\")")
                        } else {
                            format!("({val}).wrapping_neg()")
                        };
                        Ok((expr, ty))
                    }
                    "+" => {
                        let (val, ty) = self.typed_expression(&inner[0])?;
                        ensure!(matches!(ty, CType::Int(_)), "unary + requires integer");
                        Ok((val, ty))
                    }
                    "~" => {
                        let (val, ty) = self.typed_expression(&inner[0])?;
                        ensure!(matches!(ty, CType::Int(_)), "unary ~ requires integer");
                        Ok((format!("!({val})"), ty))
                    }
                    _ => bail!("unsupported unary operator {op}"),
                }
            }
            "BinaryOperator" => {
                ensure!(inner.len() == 2, "invalid binary expression");
                let op = text(node, "opcode");
                match op {
                    "=" => {
                        let (mut rv, rty) = self.typed_expression(&inner[1])?;
                        let bf = self.lvalue_bitfield(&inner[0]);
                        let (lv, lty) = self.lvalue(&inner[0])?;
                        ensure!(lty.same_shape(&rty), "assignment type mismatch");
                        if let Some((ik, w)) = bf {
                            rv = normalize_bitfield(&rv, ik, w);
                        }
                        let val_var = self.fresh_var("rf_v");
                        Ok((
                            format!("{{ let {val_var} = {rv}; {lv} = {val_var}; {val_var} }}"),
                            lty,
                        ))
                    }
                    "," => {
                        let (l, _) = self.typed_expression(&inner[0])?;
                        let (r, rty) = self.typed_expression(&inner[1])?;
                        Ok((format!("{{ let _ = {l}; {r} }}"), rty))
                    }
                    "&&" | "||" => {
                        let lcond = self.condition(&inner[0])?;
                        let rcond = self.condition(&inner[1])?;
                        Ok((
                            format!("((({lcond}) {op} ({rcond})) as i32)"),
                            CType::Int(IntKind::I32),
                        ))
                    }
                    "<" | ">" | "<=" | ">=" | "==" | "!=" => {
                        let (left, lty) = self.typed_expression(&inner[0])?;
                        let (right, rty) = self.typed_expression(&inner[1])?;
                        let expr = match (&lty, &rty) {
                            (CType::Int(a), CType::Int(b)) if a == b => {
                                format!("((({left}) {op} ({right})) as i32)")
                            }
                            (CType::Ptr { .. }, CType::Ptr { .. }) if lty.same_shape(&rty) => {
                                format!("((({left}) {op} ({right})) as i32)")
                            }
                            (CType::FnPtr { .. }, CType::FnPtr { .. })
                                if lty.same_shape(&rty) && matches!(op, "==" | "!=") =>
                            {
                                if right.trim_matches(|c| c == '(' || c == ')') == "None" {
                                    let method = if op == "==" { "is_none" } else { "is_some" };
                                    format!("((({left}).{method}()) as i32)")
                                } else if left.trim_matches(|c| c == '(' || c == ')') == "None" {
                                    let method = if op == "==" { "is_none" } else { "is_some" };
                                    format!("((({right}).{method}()) as i32)")
                                } else {
                                    format!(
                                        "(((({left}).map(|f| f as usize)) {op} (({right}).map(|f| f as usize))) as i32)"
                                    )
                                }
                            }
                            _ => bail!("incompatible comparison operands"),
                        };
                        Ok((expr, CType::Int(IntKind::I32)))
                    }
                    "+" | "-" => {
                        let (left, lty) = self.typed_expression(&inner[0])?;
                        let (right, rty) = self.typed_expression(&inner[1])?;
                        match (op, &lty, &rty) {
                            (_, CType::Int(a), CType::Int(b)) if a == b => {
                                Ok((Self::int_binary_op(op, *a, &left, &right)?, lty))
                            }
                            ("+", CType::Ptr { inner, .. }, CType::Int(_)) => {
                                ensure!(
                                    !matches!(**inner, CType::Void),
                                    "pointer arithmetic on void * unsupported"
                                );
                                Ok((format!("({left}).wrapping_offset(({right}) as isize)"), lty))
                            }
                            ("+", CType::Int(_), CType::Ptr { inner, .. }) => {
                                ensure!(
                                    !matches!(**inner, CType::Void),
                                    "pointer arithmetic on void * unsupported"
                                );
                                Ok((format!("({right}).wrapping_offset(({left}) as isize)"), rty))
                            }
                            ("-", CType::Ptr { inner, .. }, CType::Int(_)) => {
                                ensure!(
                                    !matches!(**inner, CType::Void),
                                    "pointer arithmetic on void * unsupported"
                                );
                                Ok((
                                    format!("({left}).wrapping_offset(-(({right}) as isize))"),
                                    lty,
                                ))
                            }
                            ("-", CType::Ptr { inner, .. }, CType::Ptr { .. })
                                if lty.same_shape(&rty) =>
                            {
                                ensure!(
                                    !matches!(**inner, CType::Void),
                                    "pointer difference on void * unsupported"
                                );
                                Ok((
                                    format!("(({left}).offset_from({right}) as i64)"),
                                    CType::Int(IntKind::Long),
                                ))
                            }
                            _ => bail!("unsupported operands for binary {op}"),
                        }
                    }
                    _ => {
                        let (left, lty) = self.typed_expression(&inner[0])?;
                        let (right, rty) = self.typed_expression(&inner[1])?;
                        match (lty.clone(), rty) {
                            (CType::Int(a), CType::Int(b))
                                if a == b || matches!(op, "<<" | ">>") =>
                            {
                                Ok((Self::int_binary_op(op, a, &left, &right)?, lty))
                            }
                            _ => bail!("binary {op} requires matching integer operands"),
                        }
                    }
                }
            }
            "CompoundAssignOperator" => {
                ensure!(inner.len() == 2, "invalid compound assignment");
                let op = text(node, "opcode");
                let base_op = op
                    .strip_suffix('=')
                    .with_context(|| format!("unsupported compound operator {op}"))?;
                let (rv, rty) = self.typed_expression(&inner[1])?;
                let bf = self.lvalue_bitfield(&inner[0]);
                let (lv, lty) = self.lvalue(&inner[0])?;
                let rhs_var = self.fresh_var("rf_rhs");
                let ptr_var = self.fresh_var("rf_lhs");
                let mut updated = match (&lty, &rty, base_op) {
                    (CType::Int(lk), CType::Int(rk), _) => {
                        let rhs_cast = if lk == rk || matches!(base_op, "<<" | ">>") {
                            rhs_var.clone()
                        } else {
                            format!("({rhs_var} as {})", lk.rust_type())
                        };
                        Self::int_binary_op(base_op, *lk, &format!("(*{ptr_var})"), &rhs_cast)?
                    }
                    (CType::Ptr { inner, .. }, CType::Int(_), "+") => {
                        ensure!(
                            !matches!(**inner, CType::Void),
                            "pointer arithmetic on void * unsupported"
                        );
                        format!("(*{ptr_var}).wrapping_offset(({rhs_var}) as isize)")
                    }
                    (CType::Ptr { inner, .. }, CType::Int(_), "-") => {
                        ensure!(
                            !matches!(**inner, CType::Void),
                            "pointer arithmetic on void * unsupported"
                        );
                        format!("(*{ptr_var}).wrapping_offset(-(({rhs_var}) as isize))")
                    }
                    _ => bail!("unsupported operands for compound assignment {op}"),
                };
                if let Some((ik, w)) = bf {
                    updated = normalize_bitfield(&updated, ik, w);
                }
                Ok((
                    format!(
                        "{{ let {rhs_var} = {rv}; let {ptr_var} = &mut ({lv}); *{ptr_var} = {updated}; *{ptr_var} }}"
                    ),
                    lty,
                ))
            }
            "ConditionalOperator" => {
                ensure!(inner.len() == 3, "invalid conditional expression");
                let cond = self.condition(&inner[0])?;
                let (then_expr, then_ty) = self.typed_expression(&inner[1])?;
                let (else_expr, else_ty) = self.typed_expression(&inner[2])?;
                ensure!(
                    then_ty.same_shape(&else_ty),
                    "conditional branches must have matching types"
                );
                Ok((
                    format!("(if {cond} {{ {then_expr} }} else {{ {else_expr} }})"),
                    then_ty,
                ))
            }
            "CallExpr" => {
                ensure!(!inner.is_empty(), "invalid function call");
                let callee = &inner[0];
                if kind(callee) == "ImplicitCastExpr"
                    && text(callee, "castKind") == "FunctionToPointerDecay"
                    && children(callee).len() == 1
                    && kind(&children(callee)[0]) == "DeclRefExpr"
                    && kind(&children(callee)[0]["referencedDecl"]) == "FunctionDecl"
                {
                    let ref_decl = &children(callee)[0]["referencedDecl"];
                    let name = text(ref_decl, "name");
                    identifier(name)?;
                    let (param_types, ret_type) =
                        parse_fn_signature(text(&ref_decl["type"], "qualType"), self.typedefs)?;
                    ensure!(
                        inner.len() - 1 == param_types.len(),
                        "call to {name} argument count mismatch"
                    );
                    let mut arguments = Vec::with_capacity(param_types.len());
                    for (arg_node, expected_ty) in inner[1..].iter().zip(&param_types) {
                        let (arg_expr, arg_ty) = self.typed_expression(arg_node)?;
                        ensure!(
                            arg_ty.same_shape(expected_ty),
                            "call to {name} argument type mismatch"
                        );
                        arguments.push(arg_expr);
                    }
                    self.dependencies.insert(name.into());
                    self.prototypes
                        .entry(name.to_owned())
                        .or_insert_with(|| make_external_function(name, &param_types, &ret_type));
                    return Ok((format!("rf_{name}({})", arguments.join(", ")), ret_type));
                }

                let (fn_expr, fn_ty) = self.typed_expression(callee)?;
                let CType::FnPtr { params, ret } = fn_ty else {
                    bail!("call target is neither a direct function nor a function pointer");
                };
                ensure!(
                    inner.len() - 1 == params.len(),
                    "indirect call argument count mismatch"
                );
                let mut arguments = Vec::with_capacity(params.len());
                for (arg_node, expected_ty) in inner[1..].iter().zip(&params) {
                    let (arg_expr, arg_ty) = self.typed_expression(arg_node)?;
                    ensure!(
                        arg_ty.same_shape(expected_ty),
                        "indirect call argument type mismatch"
                    );
                    arguments.push(arg_expr);
                }
                Ok((
                    format!(
                        "(({fn_expr}).expect(\"null function pointer call\"))({})",
                        arguments.join(", ")
                    ),
                    *ret,
                ))
            }
            other => bail!("unsupported expression {other}"),
        }
    }

    fn compound_with_labels(&mut self, inner: &[Value]) -> Result<String> {
        let mut hoisted = Vec::new();
        let mut segments: Vec<LabeledSegment<'_>> = vec![(None, Vec::new())];

        for item in inner {
            let mut cur = item;
            while kind(cur) == "LabelStmt" {
                let decl_id = text(cur, "declId");
                ensure!(!decl_id.is_empty(), "missing label declId");
                let label_name = self.fresh_var("'rf_lbl");
                segments.push((Some((decl_id.to_owned(), label_name)), Vec::new()));
                if let Some(sub) = children(cur).first() {
                    cur = sub;
                } else {
                    break;
                }
            }
            if kind(cur) != "LabelStmt" {
                segments.last_mut().unwrap().1.push(cur);
            }
        }

        // Hoist top-level DeclStmt bindings so variables remain in scope after forward gotos.
        let mut decl_inits: BTreeMap<usize, Vec<(String, CType, Value)>> = BTreeMap::new();
        for (seg_idx, (_, stmts)) in segments.iter().enumerate() {
            for (stmt_idx, stmt) in stmts.iter().enumerate() {
                if kind(stmt) == "DeclStmt" {
                    let mut init_pairs = Vec::new();
                    for declaration in children(stmt) {
                        ensure!(
                            kind(declaration) == "VarDecl",
                            "unsupported local declaration"
                        );
                        let var_ty = CType::from_node_type(&declaration["type"], self.typedefs)?;
                        ensure!(
                            !matches!(var_ty, CType::Void),
                            "void local variable unsupported"
                        );
                        ensure!(
                            text(declaration, "storageClass").is_empty(),
                            "static/extern locals unsupported"
                        );
                        ensure!(
                            children(declaration).len() == 1,
                            "locals must have one initializer and no attributes"
                        );
                        let name = self.fresh_var("v");
                        let rust_ty = var_ty.rust_type();
                        let zero = var_ty.zero_expr(self.structs)?;
                        self.variables.insert(
                            text(declaration, "id").into(),
                            (name.clone(), var_ty.clone()),
                        );
                        hoisted.push(format!("let mut {name}: {rust_ty} = {zero};"));
                        init_pairs.push((name, var_ty, children(declaration)[0].clone()));
                    }
                    decl_inits.insert((seg_idx << 16) | stmt_idx, init_pairs);
                }
            }
        }

        let mut registered_ids = Vec::new();
        for (label_info, _) in &segments {
            if let Some((decl_id, rust_label)) = label_info {
                self.forward_labels
                    .insert(decl_id.clone(), rust_label.clone());
                registered_ids.push(decl_id.clone());
            }
        }

        let mut translated_segments: Vec<(Option<String>, Vec<String>)> = Vec::new();
        let res = (|| -> Result<()> {
            for (seg_idx, (label_info, stmts)) in segments.iter().enumerate() {
                if let Some((decl_id, _)) = label_info {
                    self.forward_labels.remove(decl_id);
                }
                let mut out_stmts = Vec::new();
                for (stmt_idx, stmt) in stmts.iter().enumerate() {
                    if let Some(inits) = decl_inits.get(&((seg_idx << 16) | stmt_idx)) {
                        for (vname, vty, init_node) in inits {
                            let (val_expr, val_ty) = self.typed_expression(init_node)?;
                            ensure!(vty.same_shape(&val_ty), "local initializer type mismatch");
                            out_stmts.push(format!("{vname} = {val_expr};"));
                        }
                    } else {
                        out_stmts.push(self.statement(stmt)?);
                    }
                }
                translated_segments.push((label_info.as_ref().map(|(_, l)| l.clone()), out_stmts));
            }
            Ok(())
        })();

        for id in registered_ids {
            self.forward_labels.remove(&id);
        }
        res?;

        let mut body = format!("{{\n{}\n}}", translated_segments[0].1.join("\n"));
        for (label_opt, seg_stmts) in translated_segments.into_iter().skip(1) {
            let label = label_opt.expect("labeled segment");
            body = format!("{label}: {{\n{body}\n}}\n{}", seg_stmts.join("\n"));
        }
        Ok(format!("{{\n{}\n{body}\n}}", hoisted.join("\n")))
    }

    fn statement(&mut self, node: &Value) -> Result<String> {
        let inner = children(node);
        match kind(node) {
            "CompoundStmt" => {
                if inner.iter().any(|n| kind(n) == "LabelStmt") {
                    self.compound_with_labels(inner)
                } else {
                    inner
                        .iter()
                        .map(|n| self.statement(n))
                        .collect::<Result<Vec<_>>>()
                        .map(|v| format!("{{\n{}\n}}", v.join("\n")))
                }
            }
            "GotoStmt" => {
                let target_id = text(node, "targetLabelDeclId");
                let label = self.forward_labels.get(target_id).cloned().ok_or_else(|| {
                    anyhow::anyhow!("backward or cross-scope goto is unsupported")
                })?;
                Ok(format!("break {label};"))
            }
            "LabelStmt" => {
                bail!("label outside compound block is unsupported");
            }
            "ReturnStmt" => {
                if matches!(self.return_type, CType::Void) {
                    ensure!(inner.is_empty(), "void return cannot return a value");
                    Ok("return;".into())
                } else {
                    ensure!(inner.len() == 1, "non-void return requires a value");
                    let (expr, ty) = self.typed_expression(&inner[0])?;
                    ensure!(
                        ty.same_shape(&self.return_type),
                        "return type mismatch with function signature"
                    );
                    Ok(format!("return {expr};"))
                }
            }
            "IfStmt" => {
                ensure!(
                    (inner.len() == 2 || inner.len() == 3)
                        && !node["hasInit"].as_bool().unwrap_or(false),
                    "unsupported if structure"
                );
                let condition = self.condition(&inner[0])?;
                let branch = self.statement(&inner[1])?;
                let other = if inner.len() == 3 {
                    format!(" else {{ {} }}", self.statement(&inner[2])?)
                } else {
                    String::new()
                };
                Ok(format!("if {condition} {{ {branch} }}{other}"))
            }
            "WhileStmt" => {
                ensure!(
                    inner.len() == 2 && !node["hasVar"].as_bool().unwrap_or(false),
                    "unsupported while structure"
                );
                let condition = self.condition(&inner[0])?;
                let loop_label = self.fresh_var("'rf_lp");
                self.loop_labels.push(loop_label.clone());
                self.break_target.push(None);
                let body = self.statement(&inner[1]);
                self.break_target.pop();
                self.loop_labels.pop();
                Ok(format!("{loop_label}: while {condition} {{ {} }}", body?))
            }
            "DoStmt" => {
                ensure!(inner.len() == 2, "unsupported do-while structure");
                let loop_label = self.fresh_var("'rf_lp");
                self.loop_labels.push(loop_label.clone());
                self.break_target.push(None);
                let body = self.statement(&inner[0]);
                self.break_target.pop();
                self.loop_labels.pop();
                let body = body?;
                let condition = self.condition(&inner[1])?;
                let first_var = self.fresh_var("rf_first");
                Ok(format!(
                    "{{ let mut {first_var}: bool = true; {loop_label}: while {first_var} || {condition} {{ {first_var} = false; {body} }} }}"
                ))
            }
            "ForStmt" => {
                ensure!(inner.len() == 5, "unsupported for structure");
                ensure!(
                    inner[1].get("kind").is_none(),
                    "for condition variable declaration is unsupported"
                );
                let init = if inner[0].get("kind").is_some() {
                    if kind(&inner[0]) == "DeclStmt" {
                        self.statement(&inner[0])?
                    } else {
                        format!("{};", self.typed_expression(&inner[0])?.0)
                    }
                } else {
                    String::new()
                };
                let cond = if inner[2].get("kind").is_some() {
                    self.condition(&inner[2])?
                } else {
                    "true".into()
                };
                let inc = if inner[3].get("kind").is_some() {
                    format!("{};", self.typed_expression(&inner[3])?.0)
                } else {
                    String::new()
                };
                let loop_label = self.fresh_var("'rf_lp");
                self.loop_labels.push(loop_label.clone());
                self.break_target.push(None);
                let body = self.statement(&inner[4]);
                self.break_target.pop();
                self.loop_labels.pop();
                let body = body?;
                let step_var = self.fresh_var("rf_step");
                Ok(format!(
                    "{{ {init} let mut {step_var}: bool = false; {loop_label}: loop {{ if {step_var} {{ {inc} }} else {{ {step_var} = true; }} if !({cond}) {{ break; }} {body} }} }}"
                ))
            }
            "SwitchStmt" => {
                ensure!(
                    inner.len() == 2 && !node["hasInit"].as_bool().unwrap_or(false),
                    "unsupported switch structure"
                );
                let (sw_expr, sw_ty) = self.typed_expression(&inner[0])?;
                ensure!(
                    matches!(sw_ty, CType::Int(_)),
                    "switch condition must be integer type"
                );
                let mut arms = Vec::new();
                let mut pending = Vec::new();
                if kind(&inner[1]) == "CompoundStmt" {
                    for s in children(&inner[1]) {
                        flatten_switch_node(s, &mut arms, &mut pending)?;
                    }
                } else {
                    flatten_switch_node(&inner[1], &mut arms, &mut pending)?;
                }
                if !pending.is_empty() {
                    arms.push(SwitchArm {
                        labels: pending,
                        stmts: Vec::new(),
                    });
                }

                let label_name = self.fresh_var("'rf_sw");
                let val_var = self.fresh_var("rf_sw_val");
                let arm_var = self.fresh_var("rf_arm");

                let mut default_idx = arms.len();
                let mut case_checks = Vec::new();
                for (arm_idx, arm) in arms.iter().enumerate() {
                    for lbl in &arm.labels {
                        match lbl {
                            SwitchLabel::Default => default_idx = arm_idx,
                            SwitchLabel::Case(case_node) => {
                                let (c_expr, c_ty) = self.typed_expression(case_node)?;
                                let rhs = if c_ty.same_shape(&sw_ty) {
                                    c_expr
                                } else {
                                    format!("(({c_expr}) as {})", sw_ty.rust_type())
                                };
                                case_checks.push(format!(
                                    "else if {val_var} == ({rhs}) {{ {arm_var} = {arm_idx}; }}"
                                ));
                            }
                        }
                    }
                }

                self.break_target.push(Some(label_name.clone()));
                let mut arm_blocks = Vec::with_capacity(arms.len());
                let res = (|| -> Result<()> {
                    for (arm_idx, arm) in arms.iter().enumerate() {
                        let mut stmts_rust = Vec::with_capacity(arm.stmts.len());
                        for s in &arm.stmts {
                            stmts_rust.push(self.statement(s)?);
                        }
                        arm_blocks.push(format!(
                            "if {arm_var} <= {arm_idx} {{\n{}\n}}",
                            stmts_rust.join("\n")
                        ));
                    }
                    Ok(())
                })();
                self.break_target.pop();
                res?;

                let dispatch = if case_checks.is_empty() {
                    String::new()
                } else {
                    format!("if false {{}} {}", case_checks.join(" "))
                };
                Ok(format!(
                    "{label_name}: {{\nlet {val_var} = {sw_expr};\nlet mut {arm_var}: usize = {default_idx};\n{dispatch}\n{}\n}}",
                    arm_blocks.join("\n")
                ))
            }
            "CaseStmt" | "DefaultStmt" => {
                bail!("case/default label outside switch statement")
            }
            "BreakStmt" => match self.break_target.last() {
                Some(Some(label)) => Ok(format!("break {label};")),
                Some(None) => Ok("break;".into()),
                None => bail!("break outside of loop or switch"),
            },
            "ContinueStmt" => {
                let label = self
                    .loop_labels
                    .last()
                    .context("continue outside of loop")?;
                Ok(format!("continue {label};"))
            }
            "DeclStmt" => {
                let mut statements = Vec::new();
                for declaration in inner {
                    ensure!(
                        kind(declaration) == "VarDecl",
                        "unsupported local declaration"
                    );
                    let var_ty = CType::from_node_type(&declaration["type"], self.typedefs)?;
                    ensure!(
                        !matches!(var_ty, CType::Void),
                        "void local variable unsupported"
                    );
                    ensure!(
                        text(declaration, "storageClass").is_empty(),
                        "static/extern locals unsupported"
                    );
                    ensure!(
                        children(declaration).len() == 1,
                        "locals must have one initializer and no attributes"
                    );
                    let (value, val_ty) = self.typed_expression(&children(declaration)[0])?;
                    ensure!(
                        var_ty.same_shape(&val_ty),
                        "local initializer type mismatch"
                    );
                    let name = self.fresh_var("v");
                    let rust_ty = var_ty.rust_type();
                    self.variables
                        .insert(text(declaration, "id").into(), (name.clone(), var_ty));
                    statements.push(format!("let mut {name}: {rust_ty} = {value};"));
                }
                Ok(statements.join("\n"))
            }
            "NullStmt" => Ok(";".into()),
            _ => Ok(format!("{};", self.typed_expression(node)?.0)),
        }
    }
}
