//! Fail-closed translation of a typed C subset (fixed-width/unsigned/bool integers,
//! enums, void, structs with bitfields, unions, fixed-size arrays, pointers,
//! function pointers, string literals, static const globals, loops, switch,
//! forward goto cleanup labels, sizeof/alignof, external prototypes, and C ABI export) to Rust.
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::model::{ExternalFunction, Function, PointerParamContract, StructDef, StructField};

pub struct TranslationUnitOutput {
    pub structs: Vec<StructDef>,
    pub constants: Vec<String>,
    pub functions: Vec<Function>,
    pub prototypes: Vec<ExternalFunction>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TranslateOptions {
    pub allow_variadic_calls: bool,
    pub allow_mutable_globals: bool,
    pub allow_mutable_static_locals: bool,
    pub allow_main: bool,
    pub allow_uninit_locals: bool,
    pub allow_general_cfg: bool,
    pub export_c_symbols: bool,
}

impl TranslateOptions {
    pub const fn strict() -> Self {
        Self {
            allow_variadic_calls: false,
            allow_mutable_globals: false,
            allow_mutable_static_locals: false,
            allow_main: false,
            allow_uninit_locals: false,
            allow_general_cfg: false,
            export_c_symbols: false,
        }
    }

    pub const fn general_c() -> Self {
        Self {
            allow_variadic_calls: true,
            allow_mutable_globals: true,
            allow_mutable_static_locals: true,
            allow_main: true,
            allow_uninit_locals: true,
            allow_general_cfg: true,
            export_c_symbols: false,
        }
    }

    pub const fn drop_in_c_abi() -> Self {
        Self {
            allow_variadic_calls: true,
            allow_mutable_globals: true,
            allow_mutable_static_locals: true,
            allow_main: true,
            allow_uninit_locals: true,
            allow_general_cfg: true,
            export_c_symbols: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobalKind {
    StaticConst,
    ConstItem,
    StaticMut,
}

#[derive(Debug, Clone)]
struct GlobalBinding {
    rust_name: String,
    c_name: String,
    ty: CType,
    kind: GlobalKind,
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
    I128,
    U128,
}

impl IntKind {
    fn is_signed(self) -> bool {
        match self {
            Self::Bool
            | Self::U8
            | Self::U16
            | Self::U32
            | Self::ULong
            | Self::ULongLong
            | Self::U128 => false,
            Self::I8 | Self::I16 | Self::I32 | Self::Long | Self::LongLong | Self::I128 => true,
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
            Self::I128 | Self::U128 => 128,
        }
    }

    fn unsigned_rust_type(self) -> &'static str {
        match self.bit_width() {
            8 => "u8",
            16 => "u16",
            32 => "u32",
            64 => "u64",
            _ => "u128",
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
            Self::I128 => "i128",
            Self::U128 => "u128",
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
            Self::I128 => "__int128",
            Self::U128 => "unsigned __int128",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatKind {
    F32,
    F64,
}

impl FloatKind {
    fn rust_type(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }

    fn c_type(self) -> &'static str {
        match self {
            Self::F32 => "float",
            Self::F64 => "double",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CType {
    Void,
    Int(IntKind),
    Float(FloatKind),
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

fn rust_field_name(name: &str) -> String {
    match name {
        "self" | "super" | "crate" | "Self" | "_" => format!("rf_kw_{name}"),
        "as" | "break" | "const" | "continue" | "else" | "enum" | "extern" | "false" | "fn"
        | "for" | "if" | "impl" | "in" | "let" | "loop" | "match" | "mod" | "move" | "mut"
        | "pub" | "ref" | "return" | "static" | "struct" | "trait" | "true" | "type" | "unsafe"
        | "use" | "where" | "while" | "async" | "await" | "dyn" | "abstract" | "become" | "box"
        | "do" | "final" | "macro" | "override" | "priv" | "typeof" | "unsized" | "virtual"
        | "yield" | "try" | "gen" => format!("r#{name}"),
        _ => name.to_owned(),
    }
}

impl CType {
    fn same_shape(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Void, Self::Void) => true,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a == b,
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
                Ok(format!("extern \"C\" fn({p}){r}"))
            }
            _ => bail!("expected function pointer type"),
        }
    }

    fn rust_type(&self) -> String {
        match self {
            Self::Void => "()".into(),
            Self::Int(kind) => kind.rust_type().into(),
            Self::Float(kind) => kind.rust_type().into(),
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
                let fnty = self
                    .fn_type_only()
                    .unwrap_or_else(|_| "extern \"C\" fn()".into());
                format!("Option<{fnty}>")
            }
        }
    }

    fn c_type(&self) -> String {
        match self {
            Self::Void => "void".into(),
            Self::Int(kind) => kind.c_type().into(),
            Self::Float(kind) => kind.c_type().into(),
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
            Self::Float(kind) => Ok(format!("0.0{}", kind.rust_type())),
            Self::Ptr { .. } => Ok("core::ptr::null_mut()".into()),
            Self::FnPtr { .. } => Ok("None".into()),
            Self::Array(elem, len) => Ok(format!("[{}; {len}]", elem.zero_expr(structs)?)),
            Self::Struct(name) => {
                if let Some(fields) = structs.get(name) {
                    let inits = fields
                        .iter()
                        .map(|(fname, fty, _)| {
                            Ok(format!(
                                "{}: {}",
                                rust_field_name(fname),
                                fty.zero_expr(structs)?
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Ok(format!("RfStruct_{name} {{ {} }}", inits.join(", ")))
                } else {
                    Ok(format!(
                        "unsafe {{ core::mem::zeroed::<RfStruct_{name}>() }}"
                    ))
                }
            }
            Self::Union(name) => Ok(format!(
                "unsafe {{ core::mem::zeroed::<RfUnion_{name}>() }}"
            )),
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
            if let Some(res) = resolve_bare_fn_type(inner_str, typedefs, depth + 1) {
                let (params, ret) = res?;
                return Ok(Self::FnPtr {
                    params,
                    ret: Box::new(ret),
                });
            }
            let inner = Self::parse_str_depth(inner_str, typedefs, depth + 1)?;
            return Ok(Self::Ptr {
                inner: Box::new(inner),
                inner_const,
            });
        }

        // Fixed-size array syntax: `T[N]` or `T[M][N]` or `T (*)[N]` or flexible `T[]`
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
        if let Some(resolved) = resolve_anon_record_type(base_raw, typedefs) {
            return Self::parse_str_depth(resolved, typedefs, depth + 1);
        }

        if let Some(kind) = parse_int_kind(base_raw) {
            return Ok(Self::Int(kind));
        }

        match base_raw {
            "void" => Ok(Self::Void),
            "float" => Ok(Self::Float(FloatKind::F32)),
            "double" | "long double" => Ok(Self::Float(FloatKind::F64)),
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
        !s.contains('*') && s.starts_with("const ")
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
        "__int128" | "__int128_t" | "int128_t" => IntKind::I128,
        "unsigned __int128" | "__uint128_t" | "uint128_t" => IntKind::U128,
        _ => return None,
    })
}

fn is_ptr_inner_qualifier(tok: &str) -> bool {
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

fn split_fn_ptr(s: &str) -> Option<(&str, &str)> {
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

fn parse_param_list_with_variadic(
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

fn parse_param_list(
    params_str: &str,
    typedefs: &BTreeMap<String, String>,
    depth: usize,
) -> Result<Vec<CType>> {
    let (params, is_variadic) = parse_param_list_with_variadic(params_str, typedefs, depth)?;
    ensure!(!is_variadic, "variadic functions are unsupported");
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

fn split_trailing_parens(s: &str) -> Option<(&str, &str)> {
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

fn resolve_bare_fn_type(
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

fn resolve_anon_record_type<'a>(
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
    let at_pos = rest
        .rfind("unnamed at ")
        .map(|i| i + "unnamed at ".len())
        .or_else(|| {
            rest.rfind("anonymous at ")
                .map(|i| i + "anonymous at ".len())
        })?;
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

fn strip_trailing_qualifiers(mut s: &str) -> &str {
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

fn strip_pointee_qualifiers(mut s: &str) -> (&str, bool) {
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

fn file_from_loc(loc: &Value) -> Option<&str> {
    loc["expansionLoc"]["file"]
        .as_str()
        .or_else(|| loc["file"].as_str())
        .or_else(|| loc["spellingLoc"]["file"].as_str())
}

fn offset_to_line_col(source: &str, loc: &Value) -> Result<(usize, usize)> {
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

fn line_from_offset(source: &str, loc: &Value) -> Result<usize> {
    Ok(offset_to_line_col(source, loc)?.0)
}

fn is_benign_attr_or_comment(kind_name: &str) -> bool {
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

fn collect_enum_decl(
    node: &Value,
    typedefs: &BTreeMap<String, String>,
    enum_constants: &mut BTreeMap<String, (i64, IntKind)>,
) {
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
        let ik = CType::from_node_type(&item["type"], typedefs)
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

fn parse_fn_signature_variadic(
    qual_type: &str,
    typedefs: &BTreeMap<String, String>,
) -> Result<(Vec<CType>, CType, bool)> {
    let clean = strip_trailing_qualifiers(qual_type);
    let (ret_str, params_str) = split_trailing_parens(clean)
        .with_context(|| format!("invalid function type {qual_type:?}"))?;
    let ret = CType::parse_str(ret_str, typedefs)?;
    let (params, is_variadic) = parse_param_list_with_variadic(params_str, typedefs, 0)?;
    Ok((params, ret, is_variadic))
}

fn parse_fn_signature(
    qual_type: &str,
    typedefs: &BTreeMap<String, String>,
) -> Result<(Vec<CType>, CType)> {
    let (params, ret, is_variadic) = parse_fn_signature_variadic(qual_type, typedefs)?;
    ensure!(!is_variadic, "variadic functions are unsupported");
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
        "unsafe extern \"C\" {{\n    #[link_name = \"{name}\"]\n    fn rf_ext_{name}({}){ret_suffix};\n}}\n#[inline]\npub extern \"C\" fn rf_{name}({}){ret_suffix} {{\n    unsafe {{ rf_ext_{name}({}) }}\n}}\n",
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

fn make_external_variadic_function(
    name: &str,
    fixed_params: &[CType],
    ret: &CType,
) -> ExternalFunction {
    let params_rust = fixed_params
        .iter()
        .enumerate()
        .map(|(i, t)| format!("v{i}: {}", t.rust_type()))
        .collect::<Vec<_>>();
    let fixed_str = params_rust.join(", ");
    let sig = if fixed_str.is_empty() {
        "...".to_owned()
    } else {
        format!("{fixed_str}, ...")
    };
    let ret_suffix = if matches!(ret, CType::Void) {
        String::new()
    } else {
        format!(" -> {}", ret.rust_type())
    };
    let rust = format!(
        "unsafe extern \"C\" {{\n    #[link_name = \"{name}\"]\n    fn rf_var_{name}({sig}){ret_suffix};\n}}\n",
    );
    ExternalFunction {
        name: name.to_owned(),
        parameter_types: fixed_params.iter().map(CType::c_type).collect(),
        return_type: ret.c_type(),
        rust,
    }
}

fn promote_variadic_arg(expr: String, ty: &CType) -> Result<String> {
    ensure!(
        !matches!(ty, CType::Void),
        "cannot pass void value as variadic argument"
    );
    Ok(match ty {
        CType::Int(IntKind::Bool | IntKind::I8 | IntKind::I16) => format!("(({expr}) as i32)"),
        CType::Int(IntKind::Char) => {
            if IntKind::Char.is_signed() {
                format!("(({expr}) as i32)")
            } else {
                format!("(({expr}) as u32)")
            }
        }
        CType::Int(IntKind::U8 | IntKind::U16) => format!("(({expr}) as u32)"),
        CType::Float(FloatKind::F32) => format!("(({expr}) as f64)"),
        _ => expr,
    })
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

fn format_string_literal_array(
    raw: &str,
    expected_len: Option<usize>,
    elem_ik: IntKind,
) -> Result<String> {
    let mut bytes = parse_c_string_literal(raw)?;
    if let Some(len) = expected_len {
        if bytes.len() == len + 1 && bytes.last() == Some(&0) {
            bytes.pop();
        }
        ensure!(
            bytes.len() <= len,
            "string literal exceeds target array length {len}"
        );
        bytes.resize(len, 0);
    }
    let char_rty = elem_ik.rust_type();
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
    Ok(format!("[{elems}]"))
}

fn local_initializer(declaration: &Value) -> Result<Option<&Value>> {
    let mut init: Option<&Value> = None;
    for child in children(declaration) {
        let k = kind(child);
        if is_benign_attr_or_comment(k) {
            continue;
        }
        ensure!(
            !k.ends_with("Attr"),
            "locals must have zero or one initializer and no unsupported attributes"
        );
        ensure!(
            init.is_none(),
            "locals must have zero or one initializer and no unsupported attributes"
        );
        init = Some(child);
    }
    Ok(init)
}

fn is_scalar_type(ty: &CType) -> bool {
    matches!(
        ty,
        CType::Int(_) | CType::Float(_) | CType::Ptr { .. } | CType::FnPtr { .. }
    )
}

#[derive(Default, Clone)]
struct PtrUsage {
    mutated: bool,
    indexed_or_offset: bool,
}

fn unwrap_paren_cast(mut node: &Value) -> &Value {
    while matches!(
        kind(node),
        "ParenExpr"
            | "ImplicitCastExpr"
            | "CStyleCastExpr"
            | "ExprWithCleanups"
            | "MaterializeTemporaryExpr"
    ) {
        if let Some(ch) = children(node).first() {
            node = ch;
        } else {
            break;
        }
    }
    node
}

fn bare_decl_id(node: &Value) -> Option<&str> {
    let inner = unwrap_paren_cast(node);
    (kind(inner) == "DeclRefExpr").then(|| text(&inner["referencedDecl"], "id"))
}

fn pointee_decl_id(node: &Value) -> Option<&str> {
    let mut cur = unwrap_paren_cast(node);
    loop {
        match kind(cur) {
            "MemberExpr" => {
                if cur["isArrow"].as_bool().unwrap_or(false) {
                    return children(cur).first().and_then(|c| bare_decl_id(c));
                }
                cur = unwrap_paren_cast(children(cur).first()?);
            }
            "ArraySubscriptExpr" => {
                return children(cur).iter().find_map(|c| bare_decl_id(c));
            }
            "UnaryOperator" if text(cur, "opcode") == "*" => {
                return children(cur).first().and_then(|c| bare_decl_id(c));
            }
            _ => return None,
        }
    }
}

fn collect_ptr_usage(
    node: &Value,
    ptr_ids: &BTreeSet<String>,
    usage: &mut BTreeMap<String, PtrUsage>,
) {
    let inner = children(node);
    match kind(node) {
        "BinaryOperator" if text(node, "opcode") == "=" && inner.len() == 2 => {
            if let Some(id) = pointee_decl_id(&inner[0])
                && ptr_ids.contains(id)
            {
                usage.entry(id.to_owned()).or_default().mutated = true;
            }
        }
        "BinaryOperator" if matches!(text(node, "opcode"), "+" | "-") && inner.len() == 2 => {
            for side in &inner[..2] {
                if let Some(id) = bare_decl_id(side)
                    && ptr_ids.contains(id)
                {
                    usage.entry(id.to_owned()).or_default().indexed_or_offset = true;
                }
            }
        }
        "CompoundAssignOperator" if inner.len() == 2 => {
            if let Some(id) = bare_decl_id(&inner[0])
                && ptr_ids.contains(id)
            {
                usage.entry(id.to_owned()).or_default().indexed_or_offset = true;
            } else if let Some(id) = pointee_decl_id(&inner[0])
                && ptr_ids.contains(id)
            {
                usage.entry(id.to_owned()).or_default().mutated = true;
            }
        }
        "UnaryOperator" if matches!(text(node, "opcode"), "++" | "--") && inner.len() == 1 => {
            if let Some(id) = bare_decl_id(&inner[0])
                && ptr_ids.contains(id)
            {
                usage.entry(id.to_owned()).or_default().indexed_or_offset = true;
            } else if let Some(id) = pointee_decl_id(&inner[0])
                && ptr_ids.contains(id)
            {
                usage.entry(id.to_owned()).or_default().mutated = true;
            }
        }
        "UnaryOperator" if text(node, "opcode") == "&" && inner.len() == 1 => {
            if let Some(id) = pointee_decl_id(&inner[0])
                && ptr_ids.contains(id)
            {
                usage.entry(id.to_owned()).or_default().mutated = true;
            }
        }
        "ArraySubscriptExpr" if inner.len() == 2 => {
            for side in &inner[..2] {
                if let Some(id) = bare_decl_id(side)
                    && ptr_ids.contains(id)
                {
                    let u = usage.entry(id.to_owned()).or_default();
                    u.indexed_or_offset = true;
                }
            }
        }
        "CallExpr" if !inner.is_empty() => {
            for arg in &inner[1..] {
                if let Some(id) = bare_decl_id(arg)
                    && ptr_ids.contains(id)
                    && let Ok(arg_ty) = CType::from_node_type(&arg["type"], &BTreeMap::new())
                    && let CType::Ptr {
                        inner_const: false, ..
                    } = arg_ty
                {
                    let u = usage.entry(id.to_owned()).or_default();
                    u.mutated = true;
                }
            }
        }
        _ => {}
    }
    for child in inner {
        collect_ptr_usage(child, ptr_ids, usage);
    }
}

fn analyze_pointer_params(
    fn_name: &str,
    body: &Value,
    param_nodes: &[&Value],
    param_types: &[CType],
    param_names: &[String],
    return_ctype: &CType,
) -> (Vec<PointerParamContract>, Option<String>) {
    let mut ptr_ids = BTreeSet::new();
    for (pnode, pty) in param_nodes.iter().zip(param_types) {
        if matches!(pty, CType::Ptr { .. }) {
            ptr_ids.insert(text(pnode, "id").to_owned());
        }
    }
    if ptr_ids.is_empty() {
        return (Vec::new(), None);
    }
    let mut usage = BTreeMap::new();
    collect_ptr_usage(body, &ptr_ids, &mut usage);

    let mut contracts = Vec::new();
    let mut all_ptr_refs = true;
    for (idx, ((pnode, pty), pname)) in param_nodes
        .iter()
        .zip(param_types)
        .zip(param_names)
        .enumerate()
    {
        let CType::Ptr { inner, inner_const } = pty else {
            continue;
        };
        let id = text(pnode, "id");
        let stats = usage.get(id).cloned().unwrap_or_default();
        let read_only = *inner_const || !stats.mutated;
        let access = if read_only { "read_only" } else { "read_write" };
        let is_raw_only = matches!(**inner, CType::Void | CType::Ptr { .. } | CType::Array(..));
        let (kind, safe_rust_type) = if is_raw_only {
            all_ptr_refs = false;
            ("raw".to_string(), pty.rust_type())
        } else if stats.indexed_or_offset {
            all_ptr_refs = false;
            let raw_ty = if read_only {
                format!("*const {}", inner.rust_type())
            } else {
                format!("*mut {}", inner.rust_type())
            };
            ("slice_or_offset".to_string(), raw_ty)
        } else {
            let ref_ty = if read_only {
                format!("&{}", inner.rust_type())
            } else {
                format!("&mut {}", inner.rust_type())
            };
            ("ref".to_string(), ref_ty)
        };
        contracts.push(PointerParamContract {
            param_index: idx,
            name: pname.clone(),
            c_type: pty.c_type(),
            access: access.into(),
            kind,
            safe_rust_type,
        });
    }

    let non_ptrs_scalar = param_types
        .iter()
        .all(|t| matches!(t, CType::Int(_) | CType::Float(_) | CType::Ptr { .. }));
    let ret_scalar_or_void = matches!(return_ctype, CType::Void | CType::Int(_) | CType::Float(_));
    let wrapper = if all_ptr_refs && non_ptrs_scalar && ret_scalar_or_void {
        let mut sig_parts = Vec::with_capacity(param_types.len());
        let mut call_args = Vec::with_capacity(param_types.len());
        for (i, pty) in param_types.iter().enumerate() {
            if let CType::Ptr { inner, .. } = pty {
                let contract = contracts.iter().find(|c| c.param_index == i).unwrap();
                sig_parts.push(format!("v{i}: {}", contract.safe_rust_type));
                if contract.access == "read_only" {
                    call_args.push(format!("(v{i} as *const {}).cast_mut()", inner.rust_type()));
                } else {
                    call_args.push(format!("v{i} as *mut {}", inner.rust_type()));
                }
            } else {
                sig_parts.push(format!("v{i}: {}", pty.rust_type()));
                call_args.push(format!("v{i}"));
            }
        }
        let ret_suffix = if matches!(return_ctype, CType::Void) {
            String::new()
        } else {
            format!(" -> {}", return_ctype.rust_type())
        };
        Some(format!(
            "pub fn rf_safe_{fn_name}({}){ret_suffix} {{\n    rf_{fn_name}({})\n}}\n",
            sig_parts.join(", "),
            call_args.join(", "),
        ))
    } else {
        None
    };

    (contracts, wrapper)
}

struct RecordEntry<'a> {
    rel_file: String,
    node: &'a Value,
    name: String,
}

#[allow(clippy::too_many_arguments)]
fn collect_nested_records<'a>(
    node: &'a Value,
    rel_file: &str,
    physical_file: &Path,
    clang_file: &str,
    anon_typedefs: &BTreeMap<String, String>,
    anon_counter: &mut usize,
    typedefs: &mut BTreeMap<String, String>,
    file_sources: &mut BTreeMap<String, String>,
    out: &mut Vec<RecordEntry<'a>>,
) -> Result<()> {
    let mut cur_clang_file = clang_file.to_owned();
    if let Some(f) = file_from_loc(&node["loc"]) {
        cur_clang_file = f.to_owned();
    }
    for child in children(node) {
        if kind(child) == "RecordDecl" && child["completeDefinition"].as_bool() == Some(true) {
            collect_nested_records(
                child,
                rel_file,
                physical_file,
                &cur_clang_file,
                anon_typedefs,
                anon_counter,
                typedefs,
                file_sources,
                out,
            )?;
        }
    }
    let tag = text(node, "tagUsed");
    let raw_name = text(node, "name");
    let name = if !raw_name.is_empty() {
        raw_name.to_owned()
    } else if let Some(td_name) = anon_typedefs.get(text(node, "id")) {
        td_name.clone()
    } else {
        let synth = format!("anon_{}", *anon_counter);
        *anon_counter += 1;
        synth
    };
    if raw_name.is_empty() && matches!(tag, "struct" | "union") {
        if !file_sources.contains_key(rel_file)
            && let Ok(src) = std::fs::read_to_string(physical_file)
        {
            file_sources.insert(rel_file.to_owned(), src);
        }
        if let Some(source) = file_sources.get(rel_file) {
            let begin = if node["range"]["begin"].is_object() {
                &node["range"]["begin"]
            } else {
                &node["loc"]
            };
            if let Ok((line, col)) = offset_to_line_col(source, begin) {
                let target = format!("{tag} {name}");
                typedefs.insert(
                    format!("{tag} @anon:{cur_clang_file}:{line}:{col}"),
                    target.clone(),
                );
                typedefs.insert(
                    format!("{tag} @anon:{}:{line}:{col}", physical_file.display()),
                    target.clone(),
                );
                let sp = begin
                    .get("spellingLoc")
                    .or_else(|| node["loc"].get("spellingLoc"));
                if let Some(sp) = sp
                    && let (Some(sl), Some(sc)) = (sp["line"].as_u64(), sp["col"].as_u64())
                {
                    let sf = sp["file"].as_str().unwrap_or(&cur_clang_file);
                    typedefs.insert(format!("{tag} @anon:{sf}:{sl}:{sc}"), target.clone());
                    if let Ok(canon_sf) = Path::new(sf).canonicalize() {
                        typedefs.insert(
                            format!("{tag} @anon:{}:{sl}:{sc}", canon_sf.display()),
                            target,
                        );
                    }
                }
            }
        }
    }
    out.push(RecordEntry {
        rel_file: rel_file.to_owned(),
        node,
        name,
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_record_entry(
    node: &Value,
    mut name: String,
    rel_file: String,
    source: &str,
    rename_on_collision: bool,
    anon_counter: &mut usize,
    typedefs: &mut BTreeMap<String, String>,
    enum_constants: &mut BTreeMap<String, (i64, IntKind)>,
    structs_map: &mut StructFieldsMap,
    unions_map: &mut UnionFieldsMap,
    seen_records: &mut BTreeSet<String>,
    structs_out: &mut Vec<StructDef>,
) -> Result<()> {
    let tag = text(node, "tagUsed");
    ensure!(
        matches!(tag, "struct" | "union"),
        "unsupported record tag {tag:?}; only struct and union are supported"
    );
    if node["completeDefinition"].as_bool() != Some(true) {
        return Ok(());
    }
    identifier(&name)?;
    let key = format!("{tag}:{name}");
    if !seen_records.insert(key) {
        if !rename_on_collision {
            return Ok(());
        }
        let orig = name.clone();
        name = format!("{orig}_local_{}", *anon_counter);
        *anon_counter += 1;
        seen_records.insert(format!("{tag}:{name}"));
        typedefs.insert(format!("{tag} {orig}"), format!("{tag} {name}"));
    }

    if tag == "struct" {
        let mut fields_typed = Vec::new();
        let mut fields_model = Vec::new();
        let mut field_lines = Vec::new();
        let mut default_lines = Vec::new();
        let mut field_idx = 0usize;

        for child in children(node) {
            let ck = kind(child);
            if matches!(ck, "RecordDecl" | "IndirectFieldDecl") || is_benign_attr_or_comment(ck) {
                continue;
            }
            if ck == "EnumDecl" {
                collect_enum_decl(child, typedefs, enum_constants);
                continue;
            }
            ensure!(
                ck == "FieldDecl",
                "unsupported declaration {ck} inside struct {name}"
            );
            let raw_fname = text(child, "name");
            if raw_fname.is_empty() && child["isBitfield"].as_bool() == Some(true) {
                continue;
            }
            let fname = if raw_fname.is_empty() {
                let f = format!("rf_anon_{field_idx}");
                field_idx += 1;
                f
            } else {
                identifier(raw_fname)?;
                field_idx += 1;
                raw_fname.to_owned()
            };
            let fty = CType::from_node_type(&child["type"], typedefs)?;
            ensure!(
                !matches!(fty, CType::Void),
                "void field in struct {name} is unsupported"
            );
            let bitwidth = if child["isBitfield"].as_bool() == Some(true) {
                let CType::Int(ik) = fty else {
                    bail!("bitfield {name}.{fname} must have integer type");
                };
                let w = children(child)
                    .iter()
                    .filter(|c| !is_benign_attr_or_comment(kind(c)))
                    .find_map(eval_constant_int)
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
            let rf_fname = rust_field_name(&fname);
            field_lines.push(format!("    pub {rf_fname}: {rty},"));
            fields_model.push(StructField {
                name: fname.clone(),
                c_type: fty.c_type(),
                rust_type: rty,
            });
            fields_typed.push((fname, fty, bitwidth));
        }
        structs_map.insert(name.clone(), fields_typed);
        for (fname, fty, _) in &structs_map[&name] {
            let z = fty.zero_expr(structs_map)?;
            let rf_fname = rust_field_name(fname);
            default_lines.push(format!("            {rf_fname}: {z},"));
        }
        let rust_name = format!("RfStruct_{name}");
        let rust = format!(
            "#[repr(C)]\n#[derive(Clone, Copy, Debug)]\npub struct {rust_name} {{\n{}\n}}\nimpl Default for {rust_name} {{\n    fn default() -> Self {{\n        unsafe {{\n            Self {{\n{}\n            }}\n        }}\n    }}\n}}\n",
            field_lines.join("\n"),
            default_lines.join("\n"),
        );
        structs_out.push(StructDef {
            name,
            rust_name,
            fields: fields_model,
            source_file: rel_file,
            line_start: line_from_offset(source, &node["range"]["begin"])?,
            line_end: line_from_offset(source, &node["range"]["end"])?,
            rust,
        });
    } else {
        // union
        let mut fields_typed = Vec::new();
        let mut fields_model = Vec::new();
        let mut field_lines = Vec::new();
        let mut field_idx = 0usize;

        for child in children(node) {
            let ck = kind(child);
            if matches!(ck, "RecordDecl" | "IndirectFieldDecl") || is_benign_attr_or_comment(ck) {
                continue;
            }
            if ck == "EnumDecl" {
                collect_enum_decl(child, typedefs, enum_constants);
                continue;
            }
            ensure!(
                ck == "FieldDecl",
                "unsupported declaration {ck} inside union {name}"
            );
            let raw_fname = text(child, "name");
            if raw_fname.is_empty() && child["isBitfield"].as_bool() == Some(true) {
                continue;
            }
            ensure!(
                child["isBitfield"].as_bool() != Some(true),
                "bitfield inside union {name} is unsupported"
            );
            let fname = if raw_fname.is_empty() {
                let f = format!("rf_anon_{field_idx}");
                field_idx += 1;
                f
            } else {
                identifier(raw_fname)?;
                field_idx += 1;
                raw_fname.to_owned()
            };
            let fty = CType::from_node_type(&child["type"], typedefs)?;
            ensure!(
                !matches!(fty, CType::Void),
                "void field in union {name} is unsupported"
            );
            let rty = fty.rust_type();
            let rf_fname = rust_field_name(&fname);
            field_lines.push(format!("    pub {rf_fname}: {rty},"));
            fields_model.push(StructField {
                name: fname.clone(),
                c_type: fty.c_type(),
                rust_type: rty,
            });
            fields_typed.push((fname, fty));
        }
        unions_map.insert(name.clone(), fields_typed);
        let rust_name = format!("RfUnion_{name}");
        let rust = if fields_model.is_empty() {
            format!(
                "#[repr(C)]\n#[derive(Clone, Copy, Debug, Default)]\npub struct {rust_name} {{\n    _private: [u8; 0],\n}}\n"
            )
        } else {
            format!(
                "#[repr(C)]\n#[derive(Clone, Copy)]\npub union {rust_name} {{\n{}\n}}\nimpl Default for {rust_name} {{\n    fn default() -> Self {{\n        unsafe {{ core::mem::zeroed() }}\n    }}\n}}\nimpl core::fmt::Debug for {rust_name} {{\n    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {{\n        f.write_str(\"{rust_name}\")\n    }}\n}}\n",
                field_lines.join("\n"),
            )
        };
        structs_out.push(StructDef {
            name: format!("union:{name}"),
            rust_name,
            fields: fields_model,
            source_file: rel_file,
            line_start: line_from_offset(source, &node["range"]["begin"])?,
            line_end: line_from_offset(source, &node["range"]["end"])?,
            rust,
        });
    }
    Ok(())
}

pub fn translate(
    ast: &Value,
    translation_unit: &Path,
    root: &Path,
) -> Result<TranslationUnitOutput> {
    translate_with_options(ast, translation_unit, root, TranslateOptions::strict())
}

pub fn translate_with_options(
    ast: &Value,
    translation_unit: &Path,
    root: &Path,
    options: TranslateOptions,
) -> Result<TranslationUnitOutput> {
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let root = root_canon.as_path();
    ensure!(
        kind(ast) == "TranslationUnitDecl",
        "unexpected Clang AST root"
    );

    // Pass 1: track file context, typedefs, enums, structs, and unions.
    let mut typedefs: BTreeMap<String, String> = BTreeMap::new();
    let mut anon_typedefs: BTreeMap<String, String> = BTreeMap::new();
    let mut anon_tags: BTreeMap<String, String> = BTreeMap::new();
    let mut enum_constants: BTreeMap<String, (i64, IntKind)> = BTreeMap::new();
    let mut top_records: Vec<(String, std::path::PathBuf, String, &Value)> = Vec::new();
    let mut file_sources: BTreeMap<String, String> = BTreeMap::new();

    let mut current_file = translation_unit.to_path_buf();
    for node in children(ast) {
        if node["isImplicit"].as_bool() == Some(true) {
            continue;
        }
        if let Some(file) = file_from_loc(&node["loc"]) {
            current_file = Path::new(file).to_path_buf();
        }
        let Ok(physical_file) = current_file.canonicalize() else {
            continue;
        };
        let in_project = physical_file.starts_with(root);

        match kind(node) {
            "RecordDecl" => {
                if text(node, "name").is_empty() {
                    anon_tags.insert(
                        text(node, "id").to_owned(),
                        text(node, "tagUsed").to_owned(),
                    );
                }
                if in_project
                    && let Some(rel) = physical_file
                        .strip_prefix(root)
                        .ok()
                        .and_then(|p| p.to_str())
                {
                    top_records.push((
                        rel.to_owned(),
                        physical_file.clone(),
                        current_file.to_string_lossy().into_owned(),
                        node,
                    ));
                }
            }
            "EnumDecl" => {
                collect_enum_decl(node, &typedefs, &mut enum_constants);
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

    let mut record_nodes: Vec<RecordEntry<'_>> = Vec::new();
    let mut anon_counter = 0usize;
    for (rel_file, physical_file, clang_file, node) in top_records {
        if node["completeDefinition"].as_bool() != Some(true) {
            continue;
        }
        collect_nested_records(
            node,
            &rel_file,
            &physical_file,
            &clang_file,
            &anon_typedefs,
            &mut anon_counter,
            &mut typedefs,
            &mut file_sources,
            &mut record_nodes,
        )?;
    }

    let mut structs_map: BTreeMap<String, Vec<(String, CType, Option<u32>)>> = BTreeMap::new();
    let mut unions_map: BTreeMap<String, Vec<(String, CType)>> = BTreeMap::new();
    let mut structs = Vec::new();
    let mut seen_records = BTreeSet::new();

    for RecordEntry {
        rel_file,
        node,
        name,
    } in record_nodes
    {
        let physical_file = root.join(&rel_file);
        if !file_sources.contains_key(&rel_file) {
            file_sources.insert(rel_file.clone(), std::fs::read_to_string(&physical_file)?);
        }
        let source = file_sources[&rel_file].clone();
        emit_record_entry(
            node,
            name,
            rel_file,
            &source,
            false,
            &mut anon_counter,
            &mut typedefs,
            &mut enum_constants,
            &mut structs_map,
            &mut unions_map,
            &mut seen_records,
            &mut structs,
        )?;
    }

    // Pass 2: collect globals, translate project functions, and collect prototypes.
    let mut constants: Vec<String> = Vec::new();
    let mut global_defs_rust: Vec<Option<String>> = Vec::new();
    let mut defined_global_slots: BTreeMap<String, (usize, bool)> = BTreeMap::new();
    let mut extern_globals: BTreeMap<String, (String, CType)> = BTreeMap::new();
    let mut global_bindings: BTreeMap<String, GlobalBinding> = BTreeMap::new();
    let mut global_id_to_name: BTreeMap<String, String> = BTreeMap::new();
    let mut referenced_globals: BTreeSet<String> = BTreeSet::new();
    let mut used_fn_deps: BTreeSet<String> = BTreeSet::new();
    let mut unsupported_globals: BTreeMap<String, String> = BTreeMap::new();
    let mut variadic_prototypes: BTreeMap<
        String,
        std::result::Result<(Vec<CType>, CType), String>,
    > = BTreeMap::new();
    let mut unsupported_prototypes: BTreeMap<String, String> = BTreeMap::new();
    let mut functions = Vec::new();
    let mut prototypes: BTreeMap<String, ExternalFunction> = BTreeMap::new();
    let mut current_file = translation_unit.to_path_buf();

    for node in children(ast) {
        if node["isImplicit"].as_bool() == Some(true) {
            continue;
        }
        if let Some(file) = file_from_loc(&node["loc"]) {
            current_file = Path::new(file).to_path_buf();
        }
        let physical_file = current_file.canonicalize()?;
        let in_project = physical_file.starts_with(root);

        if !in_project {
            match kind(node) {
                "FunctionDecl" if !children(node).iter().any(|n| kind(n) == "CompoundStmt") => {
                    let name = text(node, "name");
                    if identifier(name).is_ok() {
                        if node["variadic"].as_bool().unwrap_or(false) {
                            let parsed = parse_fn_signature_variadic(
                                text(&node["type"], "qualType"),
                                &typedefs,
                            )
                            .map(|(p, r, _)| (p, r))
                            .map_err(|e| e.to_string());
                            variadic_prototypes.insert(name.to_owned(), parsed);
                        } else {
                            match parse_fn_signature(text(&node["type"], "qualType"), &typedefs) {
                                Ok((param_types, ret_type)) => {
                                    variadic_prototypes.remove(name);
                                    unsupported_prototypes.remove(name);
                                    prototypes.entry(name.to_owned()).or_insert_with(|| {
                                        make_external_function(name, &param_types, &ret_type)
                                    });
                                }
                                Err(err) => {
                                    unsupported_prototypes
                                        .entry(name.to_owned())
                                        .or_insert_with(|| err.to_string());
                                }
                            }
                        }
                    }
                }
                "VarDecl" if options.allow_mutable_globals => {
                    let gname = text(node, "name");
                    if text(node, "storageClass") == "extern"
                        && identifier(gname).is_ok()
                        && let Ok(gty) = CType::from_node_type(&node["type"], &typedefs)
                        && !matches!(gty, CType::Void)
                    {
                        global_id_to_name.insert(text(node, "id").to_owned(), gname.to_owned());
                        if !defined_global_slots.contains_key(gname) {
                            let rust_name = format!("RF_GLOBAL_{gname}");
                            extern_globals
                                .entry(gname.to_owned())
                                .or_insert_with(|| (rust_name.clone(), gty.clone()));
                            global_bindings
                                .entry(gname.to_owned())
                                .or_insert(GlobalBinding {
                                    rust_name,
                                    c_name: gname.to_owned(),
                                    ty: gty,
                                    kind: GlobalKind::StaticMut,
                                });
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        let file = physical_file
            .strip_prefix(root)?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF8 source path"))?;
        match kind(node) {
            "RecordDecl" | "TypedefDecl" | "EnumDecl" | "StaticAssertDecl" | "EmptyDecl" => {
                continue;
            }
            "VarDecl" => {
                let gname = text(node, "name");
                identifier(gname)?;
                let id = text(node, "id").to_owned();
                let value_children: Vec<_> = children(node)
                    .iter()
                    .filter(|c| !is_benign_attr_or_comment(kind(c)))
                    .collect();
                let storage_class = text(node, "storageClass");
                let is_const = is_const_qualified(&node["type"]);
                if storage_class == "extern" && value_children.is_empty() {
                    if options.allow_mutable_globals {
                        if let Ok(gty) = CType::from_node_type(&node["type"], &typedefs)
                            && !matches!(gty, CType::Void)
                        {
                            global_id_to_name.insert(id, gname.to_owned());
                            if !defined_global_slots.contains_key(gname) {
                                let rust_name = format!("RF_GLOBAL_{gname}");
                                extern_globals
                                    .entry(gname.to_owned())
                                    .or_insert_with(|| (rust_name.clone(), gty.clone()));
                                global_bindings
                                    .entry(gname.to_owned())
                                    .or_insert(GlobalBinding {
                                        rust_name,
                                        c_name: gname.to_owned(),
                                        ty: gty,
                                        kind: GlobalKind::StaticMut,
                                    });
                            }
                        } else {
                            unsupported_globals
                                .insert(id, format!("external global {gname} is unsupported"));
                        }
                    } else if is_const
                        && let Ok(gty) = CType::from_node_type(&node["type"], &typedefs)
                        && !matches!(gty, CType::Void | CType::Array(_, 0))
                    {
                        let rust_name = format!("RF_EXTERN_{gname}");
                        let rty = gty.rust_type();
                        constants.push(format!(
                            "unsafe extern \"C\" {{\n    #[link_name = \"{gname}\"]\n    pub static {rust_name}: {rty};\n}}\n"
                        ));
                        global_id_to_name.insert(id, gname.to_owned());
                        global_bindings.insert(
                            gname.to_owned(),
                            GlobalBinding {
                                rust_name,
                                c_name: gname.to_owned(),
                                ty: gty,
                                kind: GlobalKind::ConstItem,
                            },
                        );
                    } else {
                        unsupported_globals
                            .insert(id, format!("external global {gname} is unsupported"));
                    }
                    continue;
                }
                if !options.allow_mutable_globals {
                    ensure!(is_const, "mutable global variable {gname} is unsupported");
                    ensure!(
                        value_children.len() == 1,
                        "const global {gname} must have an initializer"
                    );
                } else {
                    ensure!(
                        value_children.len() <= 1,
                        "global {gname} must have zero or one initializer"
                    );
                }
                let gty = CType::from_node_type(&node["type"], &typedefs)?;
                let is_tentative = value_children.is_empty();
                let init_expr = if let Some(init_node) = value_children.first() {
                    let cur_clang_str = current_file.to_string_lossy().into_owned();
                    let mut temp_emitter = Emitter {
                        variables: BTreeMap::new(),
                        uninit_scalars: BTreeSet::new(),
                        global_id_to_name: Cow::Borrowed(&global_id_to_name),
                        global_bindings: Cow::Borrowed(&global_bindings),
                        referenced_globals: BTreeSet::new(),
                        unsupported_globals: Cow::Borrowed(&unsupported_globals),
                        variadic_prototypes: &variadic_prototypes,
                        unsupported_prototypes: &unsupported_prototypes,
                        dependencies: BTreeSet::new(),
                        next_variable: 0,
                        loop_labels: Vec::new(),
                        break_target: Vec::new(),
                        forward_labels: BTreeMap::new(),
                        return_type: CType::Void,
                        typedefs: Cow::Borrowed(&typedefs),
                        enum_constants: Cow::Borrowed(&enum_constants),
                        structs: Cow::Borrowed(&structs_map),
                        unions: Cow::Borrowed(&unions_map),
                        structs_out: &mut structs,
                        seen_records: &mut seen_records,
                        anon_counter: &mut anon_counter,
                        file_sources: &mut file_sources,
                        current_rel_file: file,
                        current_physical_file: &physical_file,
                        current_clang_file: &cur_clang_str,
                        current_source: "",
                        prototypes: &mut prototypes,
                        options,
                    };
                    let (expr, init_ty) = temp_emitter.typed_expression(init_node)?;
                    ensure!(
                        gty.same_shape(&init_ty),
                        "global {gname} initializer type mismatch"
                    );
                    referenced_globals.extend(temp_emitter.referenced_globals);
                    used_fn_deps.extend(temp_emitter.dependencies);
                    expr
                } else {
                    gty.zero_expr(&structs_map)?
                };
                let has_raw_ptr = gty.contains_raw_ptr(&structs_map, &unions_map);
                let export_attr = if options.export_c_symbols && storage_class != "static" {
                    format!("#[unsafe(export_name = \"{gname}\")]\n")
                } else {
                    String::new()
                };
                let (rust_name, kind_out, decl_str) = if !is_const || is_tentative {
                    let rname = format!("RF_GLOBAL_{gname}");
                    let rty = gty.rust_type();
                    let vis = if storage_class == "static" {
                        "static mut"
                    } else {
                        "pub static mut"
                    };
                    (
                        rname.clone(),
                        GlobalKind::StaticMut,
                        format!("{export_attr}{vis} {rname}: {rty} = unsafe {{ {init_expr} }};\n"),
                    )
                } else if !has_raw_ptr {
                    let rname = format!("RF_CONST_{gname}");
                    let rty = gty.rust_type();
                    let vis = if storage_class == "static" && options.export_c_symbols {
                        "static"
                    } else {
                        "pub static"
                    };
                    (
                        rname.clone(),
                        GlobalKind::StaticConst,
                        format!("{export_attr}{vis} {rname}: {rty} = unsafe {{ {init_expr} }};\n"),
                    )
                } else if options.export_c_symbols && storage_class != "static" {
                    let rname = format!("RF_CONST_{gname}");
                    let rty = gty.rust_type();
                    (
                        rname.clone(),
                        GlobalKind::StaticMut,
                        format!(
                            "{export_attr}pub static mut {rname}: {rty} = unsafe {{ {init_expr} }};\n"
                        ),
                    )
                } else {
                    let rname = format!("RF_CONST_{gname}");
                    let rty = gty.rust_type();
                    (
                        rname.clone(),
                        GlobalKind::ConstItem,
                        format!("pub const {rname}: {rty} = {init_expr};\n"),
                    )
                };

                global_id_to_name.insert(id, gname.to_owned());
                extern_globals.remove(gname);
                if let Some((slot, prev_tentative)) = defined_global_slots.get_mut(gname) {
                    if *prev_tentative || !is_tentative {
                        *prev_tentative = is_tentative;
                        global_defs_rust[*slot] = Some(decl_str);
                        global_bindings.insert(
                            gname.to_owned(),
                            GlobalBinding {
                                rust_name,
                                c_name: gname.to_owned(),
                                ty: gty,
                                kind: kind_out,
                            },
                        );
                    }
                } else {
                    let slot = global_defs_rust.len();
                    global_defs_rust.push(Some(decl_str));
                    defined_global_slots.insert(gname.to_owned(), (slot, is_tentative));
                    global_bindings.insert(
                        gname.to_owned(),
                        GlobalBinding {
                            rust_name,
                            c_name: gname.to_owned(),
                            ty: gty,
                            kind: kind_out,
                        },
                    );
                }
                continue;
            }
            "FunctionDecl" => {}
            other => bail!("unsupported top-level {other}"),
        }

        let name = text(node, "name");
        identifier(name)?;
        let storage_class = text(node, "storageClass");
        ensure!(
            matches!(storage_class, "" | "static" | "extern"),
            "unsupported function storage class"
        );
        let is_static = storage_class == "static";
        let body = children(node).iter().find(|n| kind(n) == "CompoundStmt");

        if body.is_none() {
            if node["variadic"].as_bool().unwrap_or(false) {
                let parsed =
                    parse_fn_signature_variadic(text(&node["type"], "qualType"), &typedefs)
                        .map(|(p, r, _)| (p, r))
                        .map_err(|e| e.to_string());
                variadic_prototypes.insert(name.to_owned(), parsed);
                continue;
            }
            let parse_proto = || -> Result<(Vec<CType>, CType)> {
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
                Ok((param_ctypes, return_ctype))
            };
            match parse_proto() {
                Ok((param_ctypes, return_ctype)) => {
                    variadic_prototypes.remove(name);
                    unsupported_prototypes.remove(name);
                    prototypes.entry(name.to_owned()).or_insert_with(|| {
                        make_external_function(name, &param_ctypes, &return_ctype)
                    });
                }
                Err(err) => {
                    unsupported_prototypes
                        .entry(name.to_owned())
                        .or_insert_with(|| err.to_string());
                }
            }
            continue;
        }

        ensure!(
            !node["variadic"].as_bool().unwrap_or(false),
            "variadic functions are unsupported"
        );
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

        let body = body.expect("checked above");
        if !options.allow_main {
            ensure!(
                name != "main",
                "main functions are not supported; select a library entry function"
            );
        }
        ensure!(
            children(node)
                .iter()
                .all(|n| matches!(kind(n), "ParmVarDecl" | "CompoundStmt")
                    || is_benign_attr_or_comment(kind(n))),
            "function attributes or other declarations are unsupported"
        );

        let source = if let Some(s) = file_sources.get(file) {
            s.clone()
        } else {
            let s = std::fs::read_to_string(&physical_file)?;
            file_sources.insert(file.to_owned(), s.clone());
            s
        };
        let cur_clang_str = current_file.to_string_lossy().into_owned();
        let mut emitter = Emitter {
            variables: BTreeMap::new(),
            uninit_scalars: BTreeSet::new(),
            global_id_to_name: Cow::Borrowed(&global_id_to_name),
            global_bindings: Cow::Borrowed(&global_bindings),
            referenced_globals: BTreeSet::new(),
            unsupported_globals: Cow::Borrowed(&unsupported_globals),
            variadic_prototypes: &variadic_prototypes,
            unsupported_prototypes: &unsupported_prototypes,
            dependencies: BTreeSet::new(),
            next_variable: 0,
            loop_labels: Vec::new(),
            break_target: Vec::new(),
            forward_labels: BTreeMap::new(),
            return_type: return_ctype.clone(),
            typedefs: Cow::Borrowed(&typedefs),
            enum_constants: Cow::Borrowed(&enum_constants),
            structs: Cow::Borrowed(&structs_map),
            unions: Cow::Borrowed(&unions_map),
            structs_out: &mut structs,
            seen_records: &mut seen_records,
            anon_counter: &mut anon_counter,
            file_sources: &mut file_sources,
            current_rel_file: file,
            current_physical_file: &physical_file,
            current_clang_file: &cur_clang_str,
            current_source: &source,
            prototypes: &mut prototypes,
            options,
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
        let body_code = if options.allow_general_cfg && needs_general_cfg(body) {
            emitter.emit_general_cfg_body(body)?
        } else {
            emitter.statement(body)?
        };
        referenced_globals.extend(emitter.referenced_globals);
        used_fn_deps.extend(emitter.dependencies.iter().cloned());
        let (pointer_contracts, safe_rust_wrapper) = analyze_pointer_params(
            name,
            body,
            &params,
            &param_ctypes,
            &parameters,
            &return_ctype,
        );
        let abi_export = if is_static
            || (options.allow_main && name == "main" && options.export_c_symbols)
        {
            String::new()
        } else {
            let ret_suffix = if matches!(return_ctype, CType::Void) {
                String::new()
            } else {
                format!(" -> {}", return_ctype.rust_type())
            };
            let attr = if options.export_c_symbols {
                format!("#[unsafe(export_name = \"{name}\")]")
            } else {
                "#[unsafe(no_mangle)]".to_owned()
            };
            format!(
                "{attr}\npub extern \"C\" fn rf_abi_{name}({}){ret_suffix} {{\n    rf_{name}({})\n}}\n",
                abi_signature.join(", "),
                abi_args.join(", "),
            )
        };
        let safe_export = safe_rust_wrapper.as_deref().unwrap_or("");
        let rust = if matches!(return_ctype, CType::Void) {
            format!(
                "pub extern \"C\" fn rf_{name}({}) {{\nunsafe {{\n{body_code}\n}}\n}}\n{abi_export}{safe_export}",
                signature.join(", ")
            )
        } else {
            let fallthrough = if options.allow_main && name == "main" {
                "return 0i32;"
            } else {
                "panic!(\"C function reached end without a return\");"
            };
            format!(
                "pub extern \"C\" fn rf_{name}({}) -> {} {{\nunsafe {{\n{body_code}\n{fallthrough}\n}}\n}}\n{abi_export}{safe_export}",
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
            pointer_contracts,
            safe_rust_wrapper,
            rust,
        });
    }

    for (gname, (rust_name, gty)) in extern_globals {
        if referenced_globals.contains(&gname) && !defined_global_slots.contains_key(&gname) {
            let rty = gty.rust_type();
            constants.push(format!(
                "unsafe extern \"C\" {{\n    #[link_name = \"{gname}\"]\n    static mut {rust_name}: {rty};\n}}\n"
            ));
        }
    }
    for decl in global_defs_rust.into_iter().flatten() {
        constants.push(decl);
    }

    let defined_names: BTreeSet<&str> = functions.iter().map(|f| f.name.as_str()).collect();
    if options.export_c_symbols {
        prototypes.retain(|name, _| {
            !defined_names.contains(name.as_str()) && used_fn_deps.contains(name.as_str())
        });
    } else {
        prototypes.retain(|name, _| !defined_names.contains(name.as_str()));
    }

    let mut used_struct_tags = BTreeSet::new();
    let mut used_union_tags = BTreeSet::new();
    let scan_all = |code: &str, st: &mut BTreeSet<String>, un: &mut BTreeSet<String>| {
        scan_record_prefixes(code, "RfStruct_", st);
        scan_record_prefixes(code, "RfUnion_", un);
    };
    for s in &structs {
        scan_all(&s.rust, &mut used_struct_tags, &mut used_union_tags);
    }
    for c in &constants {
        scan_all(c, &mut used_struct_tags, &mut used_union_tags);
    }
    for f in &functions {
        scan_all(&f.rust, &mut used_struct_tags, &mut used_union_tags);
    }
    for p in prototypes.values() {
        scan_all(&p.rust, &mut used_struct_tags, &mut used_union_tags);
    }
    for tag in used_struct_tags {
        if !seen_records.contains(&format!("struct:{tag}")) && identifier(&tag).is_ok() {
            structs.push(StructDef {
                name: tag.clone(),
                rust_name: format!("RfStruct_{tag}"),
                fields: Vec::new(),
                source_file: translation_unit.to_string_lossy().into_owned(),
                line_start: 1,
                line_end: 1,
                rust: format!(
                    "#[repr(C)]\n#[derive(Clone, Copy, Debug, Default)]\npub struct RfStruct_{tag} {{\n    _private: [u8; 0],\n}}\n"
                ),
            });
        }
    }
    for tag in used_union_tags {
        if !seen_records.contains(&format!("union:{tag}")) && identifier(&tag).is_ok() {
            structs.push(StructDef {
                name: format!("union:{tag}"),
                rust_name: format!("RfUnion_{tag}"),
                fields: Vec::new(),
                source_file: translation_unit.to_string_lossy().into_owned(),
                line_start: 1,
                line_end: 1,
                rust: format!(
                    "#[repr(C)]\n#[derive(Clone, Copy, Debug, Default)]\npub struct RfUnion_{tag} {{\n    _private: [u8; 0],\n}}\n"
                ),
            });
        }
    }

    Ok(TranslationUnitOutput {
        structs,
        constants,
        functions,
        prototypes: prototypes.into_values().collect(),
    })
}

fn scan_record_prefixes(code: &str, prefix: &str, out: &mut BTreeSet<String>) {
    let mut rest = code;
    while let Some(pos) = rest.find(prefix) {
        let after = &rest[pos + prefix.len()..];
        let len = after
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
            .count();
        if len > 0 {
            out.insert(after[..len].to_owned());
        }
        rest = &after[len..];
    }
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
    uninit_scalars: BTreeSet<String>,
    global_id_to_name: Cow<'a, BTreeMap<String, String>>,
    global_bindings: Cow<'a, BTreeMap<String, GlobalBinding>>,
    referenced_globals: BTreeSet<String>,
    unsupported_globals: Cow<'a, BTreeMap<String, String>>,
    variadic_prototypes: &'a BTreeMap<String, std::result::Result<(Vec<CType>, CType), String>>,
    unsupported_prototypes: &'a BTreeMap<String, String>,
    dependencies: BTreeSet<String>,
    next_variable: usize,
    loop_labels: Vec<String>,
    break_target: Vec<Option<String>>,
    forward_labels: BTreeMap<String, String>,
    return_type: CType,
    typedefs: Cow<'a, BTreeMap<String, String>>,
    enum_constants: Cow<'a, BTreeMap<String, (i64, IntKind)>>,
    structs: Cow<'a, StructFieldsMap>,
    unions: Cow<'a, UnionFieldsMap>,
    structs_out: &'a mut Vec<StructDef>,
    seen_records: &'a mut BTreeSet<String>,
    anon_counter: &'a mut usize,
    file_sources: &'a mut BTreeMap<String, String>,
    current_rel_file: &'a str,
    current_physical_file: &'a Path,
    current_clang_file: &'a str,
    current_source: &'a str,
    prototypes: &'a mut BTreeMap<String, ExternalFunction>,
    options: TranslateOptions,
}

impl Emitter<'_> {
    fn fresh_var(&mut self, prefix: &str) -> String {
        let id = self.next_variable;
        self.next_variable += 1;
        format!("{prefix}{id}")
    }

    fn lookup_global(&self, ref_decl: &Value) -> Option<GlobalBinding> {
        let id = text(ref_decl, "id");
        if let Some(key) = self.global_id_to_name.get(id)
            && let Some(binding) = self.global_bindings.get(key)
        {
            return Some(binding.clone());
        }
        let name = text(ref_decl, "name");
        if !name.is_empty()
            && let Some(binding) = self.global_bindings.get(name)
        {
            return Some(binding.clone());
        }
        None
    }

    fn variable(&mut self, node: &Value) -> Result<(String, CType)> {
        ensure!(
            kind(node) == "DeclRefExpr",
            "expected local variable reference"
        );
        let id = text(&node["referencedDecl"], "id");
        if let Some(v) = self.variables.get(id) {
            ensure!(
                !self.uninit_scalars.contains(id),
                "read of uninitialized local variable"
            );
            return Ok(v.clone());
        }
        if let Some(binding) = self.lookup_global(&node["referencedDecl"]) {
            self.referenced_globals.insert(binding.c_name.clone());
            let expr = match binding.kind {
                GlobalKind::StaticMut => {
                    format!("(*core::ptr::addr_of_mut!({}))", binding.rust_name)
                }
                GlobalKind::StaticConst | GlobalKind::ConstItem => binding.rust_name,
            };
            return Ok((expr, binding.ty));
        }
        if let Some(reason) = self.unsupported_globals.get(id) {
            bail!("{reason}");
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

    fn resolve_member_name(
        &self,
        base_ty: &CType,
        raw_field: &str,
        node: &Value,
    ) -> Result<(String, CType)> {
        if !raw_field.is_empty() {
            identifier(raw_field)?;
            let fty = self.member_type(base_ty, raw_field)?;
            return Ok((rust_field_name(raw_field), fty));
        }
        let target_ty = CType::from_node_type(&node["type"], &self.typedefs)?;
        match base_ty {
            CType::Struct(sname) => {
                let fields = self
                    .structs
                    .get(sname)
                    .with_context(|| format!("unknown struct {sname}"))?;
                fields
                    .iter()
                    .find(|(n, ty, _)| n.starts_with("rf_anon_") && ty.same_shape(&target_ty))
                    .map(|(n, ty, _)| (rust_field_name(n), ty.clone()))
                    .with_context(|| format!("anonymous member not found on struct {sname}"))
            }
            CType::Union(uname) => {
                let fields = self
                    .unions
                    .get(uname)
                    .with_context(|| format!("unknown union {uname}"))?;
                fields
                    .iter()
                    .find(|(n, ty)| n.starts_with("rf_anon_") && ty.same_shape(&target_ty))
                    .map(|(n, ty)| (rust_field_name(n), ty.clone()))
                    .with_context(|| format!("anonymous member not found on union {uname}"))
            }
            _ => bail!("member access requires struct or union"),
        }
    }

    fn handle_non_var_local_decl(&mut self, declaration: &Value) -> Result<bool> {
        match kind(declaration) {
            "EnumDecl" => {
                let typedefs_snapshot = self.typedefs.clone();
                collect_enum_decl(
                    declaration,
                    typedefs_snapshot.as_ref(),
                    self.enum_constants.to_mut(),
                );
                Ok(true)
            }
            "TypedefDecl" => {
                let td_name = text(declaration, "name");
                let underlying = text(&declaration["type"], "qualType");
                if !td_name.is_empty()
                    && !underlying.is_empty()
                    && !underlying.contains("unnamed")
                    && !underlying.contains("anonymous")
                {
                    self.typedefs
                        .to_mut()
                        .insert(td_name.to_owned(), underlying.to_owned());
                }
                Ok(true)
            }
            "FunctionDecl"
                if !children(declaration)
                    .iter()
                    .any(|c| kind(c) == "CompoundStmt") =>
            {
                let name = text(declaration, "name");
                identifier(name)?;
                Ok(true)
            }
            "RecordDecl" => {
                if declaration["completeDefinition"].as_bool() != Some(true) {
                    return Ok(true);
                }
                let empty_anon = BTreeMap::new();
                let mut entries = Vec::new();
                collect_nested_records(
                    declaration,
                    self.current_rel_file,
                    self.current_physical_file,
                    self.current_clang_file,
                    &empty_anon,
                    self.anon_counter,
                    self.typedefs.to_mut(),
                    self.file_sources,
                    &mut entries,
                )?;
                for entry in entries {
                    emit_record_entry(
                        entry.node,
                        entry.name,
                        entry.rel_file,
                        self.current_source,
                        true,
                        self.anon_counter,
                        self.typedefs.to_mut(),
                        self.enum_constants.to_mut(),
                        self.structs.to_mut(),
                        self.unions.to_mut(),
                        self.seen_records,
                        self.structs_out,
                    )?;
                }
                Ok(true)
            }
            "StaticAssertDecl" => Ok(true),
            _ => Ok(false),
        }
    }

    fn try_static_or_extern_local(&mut self, declaration: &Value) -> Result<Option<String>> {
        let storage_class = text(declaration, "storageClass");
        if matches!(storage_class, "" | "auto" | "register") {
            return Ok(None);
        }
        let decl_id = text(declaration, "id").to_owned();
        if storage_class == "extern" {
            let init = local_initializer(declaration)?;
            ensure!(init.is_none(), "extern local cannot have initializer");
            let gname = text(declaration, "name");
            identifier(gname)?;
            if self.global_bindings.contains_key(gname) {
                self.global_id_to_name
                    .to_mut()
                    .insert(decl_id, gname.to_owned());
                return Ok(Some(String::new()));
            }
            if self.options.allow_mutable_globals {
                if let Ok(gty) = CType::from_node_type(&declaration["type"], &self.typedefs)
                    && !matches!(gty, CType::Void)
                {
                    let rust_name = self.fresh_var("RF_EXTERN_");
                    let rty = gty.rust_type();
                    self.global_id_to_name
                        .to_mut()
                        .insert(decl_id.clone(), decl_id.clone());
                    self.global_bindings.to_mut().insert(
                        decl_id.clone(),
                        GlobalBinding {
                            rust_name: rust_name.clone(),
                            c_name: decl_id,
                            ty: gty,
                            kind: GlobalKind::StaticMut,
                        },
                    );
                    return Ok(Some(format!(
                        "unsafe extern \"C\" {{\n    #[link_name = \"{gname}\"]\n    static mut {rust_name}: {rty};\n}}"
                    )));
                }
            } else if is_const_qualified(&declaration["type"])
                && let Ok(gty) = CType::from_node_type(&declaration["type"], &self.typedefs)
                && !matches!(gty, CType::Void | CType::Array(_, 0))
            {
                let rust_name = self.fresh_var("RF_EXTERN_");
                let rty = gty.rust_type();
                let kind = if matches!(gty, CType::Array(..)) {
                    GlobalKind::StaticConst
                } else {
                    GlobalKind::ConstItem
                };
                self.global_id_to_name
                    .to_mut()
                    .insert(decl_id.clone(), decl_id.clone());
                self.global_bindings.to_mut().insert(
                    decl_id.clone(),
                    GlobalBinding {
                        rust_name: rust_name.clone(),
                        c_name: decl_id,
                        ty: gty,
                        kind,
                    },
                );
                return Ok(Some(format!(
                    "unsafe extern \"C\" {{\n    #[link_name = \"{gname}\"]\n    static {rust_name}: {rty};\n}}"
                )));
            }
            self.unsupported_globals
                .to_mut()
                .insert(decl_id, format!("external global {gname} is unsupported"));
            return Ok(Some(String::new()));
        }
        let is_const = is_const_qualified(&declaration["type"]);
        ensure!(
            storage_class == "static" && (is_const || self.options.allow_mutable_static_locals),
            "static/extern locals unsupported"
        );
        let var_ty = CType::from_node_type(&declaration["type"], &self.typedefs)?;
        ensure!(
            !matches!(var_ty, CType::Void),
            "void local variable unsupported"
        );
        let name = self.fresh_var("RF_STATIC_");
        let rust_ty = var_ty.rust_type();
        let val = if let Some(init_node) = local_initializer(declaration)? {
            let (v, vty) = self.typed_expression(init_node)?;
            ensure!(var_ty.same_shape(&vty), "local initializer type mismatch");
            v
        } else {
            var_ty.zero_expr(&self.structs)?
        };
        let (kind, kw, val_block) = if !is_const {
            (
                GlobalKind::StaticMut,
                "static mut",
                format!("unsafe {{ {val} }}"),
            )
        } else if matches!(var_ty, CType::Array(..))
            && !var_ty.contains_raw_ptr(&self.structs, &self.unions)
        {
            (
                GlobalKind::StaticConst,
                "static",
                format!("unsafe {{ {val} }}"),
            )
        } else {
            (GlobalKind::ConstItem, "const", val)
        };
        self.global_id_to_name
            .to_mut()
            .insert(decl_id.clone(), decl_id.clone());
        self.global_bindings.to_mut().insert(
            decl_id.clone(),
            GlobalBinding {
                rust_name: name.clone(),
                c_name: decl_id,
                ty: var_ty,
                kind,
            },
        );
        Ok(Some(format!("{kw} {name}: {rust_ty} = {val_block};")))
    }

    fn lvalue_bitfield(&self, mut node: &Value) -> Option<(IntKind, u32)> {
        while kind(node) == "ParenExpr" {
            node = children(node).first()?;
        }
        if kind(node) != "MemberExpr" {
            return None;
        }
        let field = text(node, "name");
        if field.is_empty() {
            return None;
        }
        let inner = children(node).first()?;
        let base_ty = CType::from_node_type(&inner["type"], &self.typedefs).ok()?;
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
            "ParenExpr" | "ExprWithCleanups" => {
                ensure!(inner.len() == 1, "invalid parenthesized lvalue");
                self.lvalue(&inner[0])
            }
            "MaterializeTemporaryExpr" => {
                ensure!(inner.len() == 1, "invalid temporary lvalue");
                self.typed_expression(&inner[0])
            }
            "UnaryOperator" if text(node, "opcode") == "*" => {
                ensure!(inner.len() == 1, "invalid dereference");
                let (ptr, ty) = self.typed_expression(&inner[0])?;
                match ty {
                    CType::Ptr { inner, .. } => {
                        ensure!(!matches!(*inner, CType::Void), "cannot dereference void *");
                        Ok((format!("(*rf_ptr({ptr}))"), *inner))
                    }
                    CType::FnPtr { .. } => Ok((ptr, ty)),
                    _ => bail!("dereference requires a pointer"),
                }
            }
            "MemberExpr" => {
                ensure!(inner.len() == 1, "invalid member expression");
                let raw_field = text(node, "name");
                if node["isArrow"].as_bool().unwrap_or(false) {
                    let (ptr, ty) = self.typed_expression(&inner[0])?;
                    match ty {
                        CType::Ptr { inner, .. } => {
                            let (field, fty) = self.resolve_member_name(&inner, raw_field, node)?;
                            Ok((format!("(*rf_ptr({ptr})).{field}"), fty))
                        }
                        _ => bail!("arrow member access requires pointer"),
                    }
                } else {
                    let (base, ty) = self.lvalue(&inner[0])?;
                    let (field, fty) = self.resolve_member_name(&ty, raw_field, node)?;
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
            CType::Float(_) => Ok(format!("(({expr}) != 0.0)")),
            CType::Ptr { .. } => Ok(format!("(!({expr}).is_null())")),
            CType::FnPtr { .. } => Ok(format!("(({expr}).is_some())")),
            _ => bail!("unsupported condition type"),
        }
    }

    fn int_binary_op(
        op: &str,
        kind: IntKind,
        left: &str,
        right: &str,
        rhs_signed: bool,
    ) -> Result<String> {
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
                if rhs_signed {
                    format!(
                        "({{ let rf_l = {left}; let rf_r = {right}; let rf_s = rf_r as i128; if rf_s < 0 || rf_s > (u32::MAX as i128) {{ panic!(\"undefined C shift\"); }} rf_l.{method}(rf_s as u32).expect(\"undefined C shift\") }})"
                    )
                } else {
                    format!(
                        "({{ let rf_l = {left}; let rf_r = {right}; let rf_s = rf_r as u128; if rf_s > (u32::MAX as u128) {{ panic!(\"undefined C shift\"); }} rf_l.{method}(rf_s as u32).expect(\"undefined C shift\") }})"
                    )
                }
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
                        fty.zero_expr(&self.structs)?
                    };
                    if let (Some(w), CType::Int(ik)) = (bitwidth, fty) {
                        init_expr = normalize_bitfield(&init_expr, *ik, *w);
                    }
                    let rf_fname = rust_field_name(fname);
                    field_inits.push(format!("{rf_fname}: {init_expr}"));
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
                    return Ok(format!(
                        "unsafe {{ core::mem::zeroed::<RfUnion_{uname}>() }}"
                    ));
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
                let rf_fname = rust_field_name(&fname);
                let u_var = self.fresh_var("rf_u");
                Ok(format!(
                    "unsafe {{ let mut {u_var}: RfUnion_{uname} = core::mem::zeroed(); {u_var}.{rf_fname} = {val_expr}; {u_var} }}"
                ))
            }
            CType::Array(elem_ty, len) => {
                let inner = children(node);
                if inner.len() == 1
                    && kind(&inner[0]) == "StringLiteral"
                    && let CType::Int(elem_ik @ (IntKind::Char | IntKind::I8 | IntKind::U8)) =
                        **elem_ty
                {
                    return format_string_literal_array(
                        text(&inner[0], "value"),
                        Some(*len),
                        elem_ik,
                    );
                }
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
                            elem_ty.zero_expr(&self.structs)?
                        };
                        elem_exprs.push(e);
                    }
                }
                Ok(format!("[{}]", elem_exprs.join(", ")))
            }
            _ if is_scalar_type(ty) => {
                let inner = children(node);
                if inner.is_empty() || kind(&inner[0]) == "ImplicitValueInitExpr" {
                    ty.zero_expr(&self.structs)
                } else {
                    ensure!(
                        inner.len() == 1,
                        "scalar initializer list must have at most 1 element"
                    );
                    let (e, ety) = self.typed_expression(&inner[0])?;
                    ensure!(ety.same_shape(ty), "scalar initializer list type mismatch");
                    Ok(e)
                }
            }
            _ => {
                bail!("initializer list only supported for struct, union, array, and scalar types")
            }
        }
    }

    fn sizeof_node_type(&self, node: &Value) -> Result<CType> {
        let mut cur = node;
        while kind(cur) == "ParenExpr" {
            let inner = children(cur);
            ensure!(inner.len() == 1, "invalid parenthesized sizeof operand");
            cur = &inner[0];
        }
        CType::from_node_type(&cur["type"], &self.typedefs)
    }

    fn typed_expression(&mut self, node: &Value) -> Result<(String, CType)> {
        let inner = children(node);
        match kind(node) {
            "IntegerLiteral" => {
                let ty = CType::from_node_type(&node["type"], &self.typedefs)?;
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
            "FloatingLiteral" => {
                let ty = CType::from_node_type(&node["type"], &self.typedefs)?;
                let CType::Float(fk) = ty else {
                    bail!("non-float floating literal");
                };
                let raw = text(node, "value");
                let formatted = match fk {
                    FloatKind::F32 => {
                        let val = raw.parse::<f32>().context("invalid float literal")?;
                        if val.is_infinite() {
                            if val.is_sign_positive() {
                                "f32::INFINITY".to_string()
                            } else {
                                "f32::NEG_INFINITY".to_string()
                            }
                        } else {
                            let mut s = format!("{val}");
                            if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                                s.push_str(".0");
                            }
                            format!("{s}f32")
                        }
                    }
                    FloatKind::F64 => {
                        let val = raw.parse::<f64>().context("invalid double literal")?;
                        if val.is_infinite() {
                            if val.is_sign_positive() {
                                "f64::INFINITY".to_string()
                            } else {
                                "f64::NEG_INFINITY".to_string()
                            }
                        } else {
                            let mut s = format!("{val}");
                            if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                                s.push_str(".0");
                            }
                            format!("{s}f64")
                        }
                    }
                };
                Ok((formatted, ty))
            }
            "CharacterLiteral" => {
                let ty = CType::from_node_type(&node["type"], &self.typedefs)?;
                let CType::Int(ik) = ty else {
                    bail!("non-int character literal");
                };
                let raw_val = node["value"]
                    .as_u64()
                    .map(|v| v as u128)
                    .or_else(|| node["value"].as_i64().map(|v| v as u64 as u128))
                    .context("missing character literal value")?;
                let rty = ik.rust_type();
                let expr = if raw_val <= 0x7f {
                    format!("{raw_val}{rty}")
                } else {
                    let uty = ik.unsigned_rust_type();
                    format!("(({raw_val}_u64 as {uty}) as {rty})")
                };
                Ok((expr, ty))
            }
            "StringLiteral" => {
                let ty = CType::from_node_type(&node["type"], &self.typedefs)?;
                let (expected_len, elem_ik) = match &ty {
                    CType::Array(elem, len) => {
                        let ik = match **elem {
                            CType::Int(k) => k,
                            _ => IntKind::Char,
                        };
                        (Some(*len), ik)
                    }
                    _ => (None, IntKind::Char),
                };
                let expr = format_string_literal_array(text(node, "value"), expected_len, elem_ik)?;
                Ok((expr, ty))
            }
            "PredefinedExpr" => {
                ensure!(inner.len() == 1, "invalid PredefinedExpr");
                self.typed_expression(&inner[0])
            }
            "ConstantExpr" => {
                if let Some(raw) = node["value"].as_str()
                    && let Ok(ty) = CType::from_node_type(&node["type"], &self.typedefs)
                    && let CType::Int(ik) = ty
                {
                    if ik.is_signed()
                        && let Ok(val) = raw.parse::<i128>()
                    {
                        return Ok((format!("{val}{}", ik.rust_type()), ty));
                    }
                    if !ik.is_signed()
                        && let Ok(val) = raw.parse::<u128>()
                    {
                        return Ok((format!("{val}{}", ik.rust_type()), ty));
                    }
                }
                if let Some(first) = inner.first() {
                    self.typed_expression(first)
                } else {
                    let ty = CType::from_node_type(&node["type"], &self.typedefs)?;
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
                        ensure!(
                            !self.variadic_prototypes.contains_key(name),
                            "variadic functions are unsupported"
                        );
                        if let Some(reason) = self.unsupported_prototypes.get(name) {
                            bail!("{reason}");
                        }
                        let (param_types, ret_type) = parse_fn_signature(
                            text(&ref_decl["type"], "qualType"),
                            &self.typedefs,
                        )?;
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
                    let raw_field = text(node, "name");
                    let (base, ty) = self.typed_expression(&inner[0])?;
                    let (field, fty) = self.resolve_member_name(&ty, raw_field, node)?;
                    Ok((format!("({base}).{field}"), fty))
                }
            }
            "ParenExpr" => {
                ensure!(inner.len() == 1, "invalid parentheses");
                let (expr, ty) = self.typed_expression(&inner[0])?;
                Ok((format!("({expr})"), ty))
            }
            "ExprWithCleanups" | "MaterializeTemporaryExpr" => {
                ensure!(inner.len() == 1, "invalid cleanup/temporary expression");
                self.typed_expression(&inner[0])
            }
            "ImplicitValueInitExpr" => {
                let ty = CType::from_node_type(&node["type"], &self.typedefs)?;
                let z = ty.zero_expr(&self.structs)?;
                Ok((z, ty))
            }
            "InitListExpr" => {
                let ty = CType::from_node_type(&node["type"], &self.typedefs)?;
                let expr = self.emit_init_list(node, &ty)?;
                Ok((expr, ty))
            }
            "CompoundLiteralExpr" => {
                ensure!(inner.len() == 1, "invalid compound literal");
                self.typed_expression(&inner[0])
            }
            "StmtExpr" => {
                ensure!(
                    inner.len() == 1 && kind(&inner[0]) == "CompoundStmt",
                    "invalid statement expression"
                );
                let target_ty = CType::from_node_type(&node["type"], &self.typedefs)?;
                let stmts = children(&inner[0]);
                ensure!(
                    !stmts.iter().any(|s| kind(s) == "LabelStmt"),
                    "labels inside statement expression are unsupported"
                );
                if matches!(target_ty, CType::Void) || stmts.is_empty() {
                    let block = self.statement(&inner[0])?;
                    return Ok((block, CType::Void));
                }
                let mut lines = Vec::with_capacity(stmts.len());
                for stmt in &stmts[..stmts.len() - 1] {
                    lines.push(self.statement(stmt)?);
                }
                let (tail_expr, tail_ty) = self.typed_expression(&stmts[stmts.len() - 1])?;
                ensure!(
                    tail_ty.same_shape(&target_ty),
                    "statement expression result type mismatch"
                );
                lines.push(tail_expr);
                Ok((format!("{{\n{}\n}}", lines.join("\n")), target_ty))
            }
            "UnaryExprOrTypeTraitExpr" => {
                let trait_name = text(node, "name");
                let mem_fn = match trait_name {
                    "sizeof" => "size_of",
                    "alignof" | "_Alignof" | "__alignof" | "__alignof__" => "align_of",
                    other => bail!("unsupported type trait {other}"),
                };
                let result_ty = CType::from_node_type(&node["type"], &self.typedefs)?;
                let target_ty = if node.get("argType").is_some() {
                    CType::from_node_type(&node["argType"], &self.typedefs)?
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
                let target_ty = CType::from_node_type(&node["type"], &self.typedefs)?;
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
                        let str_node = if kind(&inner[0]) == "PredefinedExpr" {
                            children(&inner[0]).first().unwrap_or(&inner[0])
                        } else {
                            &inner[0]
                        };
                        if kind(str_node) == "StringLiteral" {
                            let elem_ik = match &target_ty {
                                CType::Ptr { inner, .. } => match **inner {
                                    CType::Int(k) => k,
                                    _ => IntKind::Char,
                                },
                                _ => IntKind::Char,
                            };
                            let elem_rty = elem_ik.rust_type();
                            let arr = format_string_literal_array(
                                text(str_node, "value"),
                                None,
                                elem_ik,
                            )?;
                            return Ok((
                                format!("((&{arr}).as_ptr() as *mut {elem_rty})"),
                                target_ty,
                            ));
                        }
                        if kind(&inner[0]) == "CompoundLiteralExpr" {
                            let (arr_expr, arr_ty) = self.typed_expression(&inner[0])?;
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
                            let tmp = self.fresh_var("rf_cl");
                            return Ok((
                                format!(
                                    "{{ let mut {tmp} = {arr_expr}; (&mut {tmp}).as_mut_ptr() }}"
                                ),
                                target_ty,
                            ));
                        }
                        if kind(&inner[0]) == "DeclRefExpr"
                            && let Some(binding) = self.lookup_global(&inner[0]["referencedDecl"])
                        {
                            let CType::Array(elem_ty, _) = &binding.ty else {
                                bail!("ArrayToPointerDecay requires array operand");
                            };
                            ensure!(
                                target_ty.same_shape(&CType::Ptr {
                                    inner: elem_ty.clone(),
                                    inner_const: false
                                }),
                                "ArrayToPointerDecay type mismatch"
                            );
                            self.referenced_globals.insert(binding.c_name);
                            let elem_rty = elem_ty.rust_type();
                            let ptr_expr = match binding.kind {
                                GlobalKind::StaticMut => format!(
                                    "(core::ptr::addr_of_mut!({}) as *mut {elem_rty})",
                                    binding.rust_name
                                ),
                                GlobalKind::StaticConst => {
                                    format!("({}.as_ptr() as *mut {elem_rty})", binding.rust_name)
                                }
                                GlobalKind::ConstItem => format!(
                                    "((&{}).as_ptr() as *mut {elem_rty})",
                                    binding.rust_name
                                ),
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
                    "ToVoid" => {
                        let (expr, _) = self.typed_expression(&inner[0])?;
                        Ok((format!("{{ let _ = {expr}; }}"), CType::Void))
                    }
                    "ToUnion" => {
                        let (expr, inner_ty) = self.typed_expression(&inner[0])?;
                        let CType::Union(uname) = &target_ty else {
                            bail!("ToUnion requires union target type");
                        };
                        let fields = self
                            .unions
                            .get(uname)
                            .with_context(|| format!("unknown union {uname}"))?;
                        let (fname, _) = fields
                            .iter()
                            .find(|(_, fty)| fty.same_shape(&inner_ty))
                            .with_context(|| {
                            format!("no matching field in union {uname} for ToUnion cast")
                        })?;
                        let rf_fname = rust_field_name(fname);
                        let u_var = self.fresh_var("rf_u");
                        Ok((
                            format!(
                                "unsafe {{ let mut {u_var}: RfUnion_{uname} = core::mem::zeroed(); {u_var}.{rf_fname} = {expr}; {u_var} }}"
                            ),
                            target_ty,
                        ))
                    }
                    "NullToPointer" => {
                        let null_val = match &target_ty {
                            CType::FnPtr { .. } => {
                                let fnty = target_ty.fn_type_only()?;
                                format!("None::<{fnty}>")
                            }
                            CType::Ptr { inner, .. } => {
                                let pointee = if matches!(**inner, CType::Void) {
                                    "u8".into()
                                } else {
                                    inner.rust_type()
                                };
                                format!("core::ptr::null_mut::<{pointee}>()")
                            }
                            _ => bail!("NullToPointer requires pointer or function pointer type"),
                        };
                        Ok((null_val, target_ty))
                    }
                    "BitCast" => {
                        let (expr, inner_ty) = self.typed_expression(&inner[0])?;
                        match (&inner_ty, &target_ty) {
                            (CType::Ptr { .. }, CType::Ptr { .. }) => {
                                Ok((format!("({expr} as {})", target_ty.rust_type()), target_ty))
                            }
                            (CType::Ptr { .. }, CType::FnPtr { .. }) => {
                                let fnty = target_ty.fn_type_only()?;
                                let trimmed = expr.trim_matches(|c| c == '(' || c == ')');
                                if trimmed == "core::ptr::null_mut()"
                                    || trimmed.starts_with("core::ptr::null_mut::<")
                                {
                                    Ok((format!("None::<{fnty}>"), target_ty))
                                } else {
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
                            (CType::FnPtr { .. }, CType::FnPtr { .. }) => {
                                let fnty = target_ty.fn_type_only()?;
                                let tmp = self.fresh_var("rf_fp");
                                Ok((
                                    format!(
                                        "{{ match {expr} {{ Some({tmp}) => Some(core::mem::transmute::<_, {fnty}>({tmp})), None => None }} }}"
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
                    "IntegralToFloating" => {
                        let (expr, inner_ty) = self.typed_expression(&inner[0])?;
                        ensure!(
                            matches!(inner_ty, CType::Int(_))
                                && matches!(target_ty, CType::Float(_)),
                            "IntegralToFloating requires integer source and float target"
                        );
                        Ok((
                            format!("(({expr}) as {})", target_ty.rust_type()),
                            target_ty,
                        ))
                    }
                    "FloatingToIntegral" => {
                        let (expr, inner_ty) = self.typed_expression(&inner[0])?;
                        ensure!(
                            matches!(inner_ty, CType::Float(_))
                                && matches!(target_ty, CType::Int(_)),
                            "FloatingToIntegral requires float source and integer target"
                        );
                        if matches!(target_ty, CType::Int(IntKind::Bool)) {
                            Ok((format!("u8::from(({expr}) != 0.0)"), target_ty))
                        } else {
                            Ok((
                                format!("(({expr}) as {})", target_ty.rust_type()),
                                target_ty,
                            ))
                        }
                    }
                    "FloatingCast" => {
                        let (expr, inner_ty) = self.typed_expression(&inner[0])?;
                        ensure!(
                            matches!(inner_ty, CType::Float(_))
                                && matches!(target_ty, CType::Float(_)),
                            "FloatingCast requires floating types"
                        );
                        Ok((
                            format!("(({expr}) as {})", target_ty.rust_type()),
                            target_ty,
                        ))
                    }
                    "FloatingToBoolean" => {
                        let (expr, inner_ty) = self.typed_expression(&inner[0])?;
                        ensure!(
                            matches!(inner_ty, CType::Float(_)),
                            "FloatingToBoolean requires floating operand"
                        );
                        Ok((format!("u8::from(({expr}) != 0.0)"), target_ty))
                    }
                    "PointerToIntegral" => {
                        let (expr, inner_ty) = self.typed_expression(&inner[0])?;
                        ensure!(
                            matches!(target_ty, CType::Int(_)),
                            "PointerToIntegral requires integer target"
                        );
                        match inner_ty {
                            CType::Ptr { .. } => Ok((
                                format!("(({expr}) as usize as {})", target_ty.rust_type()),
                                target_ty,
                            )),
                            CType::FnPtr { .. } => {
                                let tmp = self.fresh_var("rf_fp");
                                let rty = target_ty.rust_type();
                                Ok((
                                    format!(
                                        "{{ match {expr} {{ Some({tmp}) => ({tmp} as usize as {rty}), None => (0{rty}) }} }}"
                                    ),
                                    target_ty,
                                ))
                            }
                            _ => bail!("PointerToIntegral requires pointer source"),
                        }
                    }
                    "IntegralToPointer" => {
                        let (expr, inner_ty) = self.typed_expression(&inner[0])?;
                        ensure!(
                            matches!(inner_ty, CType::Int(_)),
                            "IntegralToPointer requires integer source"
                        );
                        match target_ty {
                            CType::Ptr { .. } => Ok((
                                format!("(({expr}) as usize as {})", target_ty.rust_type()),
                                target_ty,
                            )),
                            CType::FnPtr { .. } => {
                                let fnty = target_ty.fn_type_only()?;
                                let tmp = self.fresh_var("rf_ip");
                                Ok((
                                    format!(
                                        "{{ let {tmp} = ({expr}) as usize; if {tmp} == 0 {{ None }} else {{ Some(core::mem::transmute::<usize, {fnty}>({tmp})) }} }}"
                                    ),
                                    target_ty,
                                ))
                            }
                            _ => bail!("IntegralToPointer requires pointer target"),
                        }
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
                        let peeled = unwrap_paren_cast(&inner[0]);
                        if kind(peeled) == "DeclRefExpr"
                            && kind(&peeled["referencedDecl"]) == "FunctionDecl"
                        {
                            return self.typed_expression(&inner[0]);
                        }
                        if kind(peeled) == "CompoundLiteralExpr" {
                            let (val, val_ty) = self.typed_expression(&inner[0])?;
                            let rty = val_ty.rust_type();
                            let tmp = self.fresh_var("rf_cl");
                            return Ok((
                                format!(
                                    "{{ let mut {tmp}: {rty} = {val}; (&mut {tmp} as *mut {rty}) }}"
                                ),
                                CType::Ptr {
                                    inner: Box::new(val_ty),
                                    inner_const: false,
                                },
                            ));
                        }
                        if let Some(id) = bare_decl_id(&inner[0]) {
                            self.uninit_scalars.remove(id);
                        }
                        let (lv, lv_ty) = self.lvalue(&inner[0])?;
                        let rty = lv_ty.rust_type();
                        let mut root_lval = peeled;
                        while matches!(
                            kind(root_lval),
                            "MemberExpr"
                                | "ArraySubscriptExpr"
                                | "ParenExpr"
                                | "ImplicitCastExpr"
                                | "CStyleCastExpr"
                        ) {
                            if let Some(first) = children(root_lval).first() {
                                root_lval = first;
                            } else {
                                break;
                            }
                        }
                        let root_binding = (kind(root_lval) == "DeclRefExpr")
                            .then(|| self.lookup_global(&root_lval["referencedDecl"]))
                            .flatten();
                        let ptr_expr = if kind(peeled) == "DeclRefExpr"
                            && let Some(binding) = self.lookup_global(&peeled["referencedDecl"])
                            && binding.kind == GlobalKind::StaticMut
                        {
                            self.referenced_globals.insert(binding.c_name);
                            format!("core::ptr::addr_of_mut!({})", binding.rust_name)
                        } else if let Some(binding) = root_binding
                            && matches!(
                                binding.kind,
                                GlobalKind::StaticConst | GlobalKind::ConstItem
                            )
                        {
                            format!("(&({lv}) as *const {rty} as *mut {rty})")
                        } else {
                            format!("(&mut ({lv}) as *mut {rty})")
                        };
                        Ok((
                            ptr_expr,
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
                            CType::Float(fk) => {
                                let bin = if op == "++" { "+" } else { "-" };
                                format!("((*{ptr_var}) {bin} 1.0{})", fk.rust_type())
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
                        match ty {
                            CType::Int(ik) => {
                                let expr = if ik.is_signed() {
                                    format!(
                                        "({val}).checked_neg().expect(\"undefined C negation\")"
                                    )
                                } else {
                                    format!("({val}).wrapping_neg()")
                                };
                                Ok((expr, ty))
                            }
                            CType::Float(_) => Ok((format!("(-({val}))"), ty)),
                            _ => bail!("unary - requires numeric type"),
                        }
                    }
                    "+" => {
                        let (val, ty) = self.typed_expression(&inner[0])?;
                        ensure!(
                            matches!(ty, CType::Int(_) | CType::Float(_)),
                            "unary + requires numeric type"
                        );
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
                        if let Some(id) = bare_decl_id(&inner[0]) {
                            self.uninit_scalars.remove(id);
                        }
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
                            (CType::Float(a), CType::Float(b)) if a == b => {
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
                            (_, CType::Int(a), CType::Int(b)) if a == b => Ok((
                                Self::int_binary_op(op, *a, &left, &right, b.is_signed())?,
                                lty,
                            )),
                            (_, CType::Float(a), CType::Float(b)) if a == b => {
                                Ok((format!("(({left}) {op} ({right}))"), lty))
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
                                Ok((
                                    Self::int_binary_op(op, a, &left, &right, b.is_signed())?,
                                    lty,
                                ))
                            }
                            (CType::Float(a), CType::Float(b))
                                if a == b && matches!(op, "*" | "/") =>
                            {
                                Ok((format!("(({left}) {op} ({right}))"), lty))
                            }
                            _ => bail!("binary {op} requires matching numeric operands"),
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
                        let (rhs_cast, rhs_signed) = if lk == rk || matches!(base_op, "<<" | ">>") {
                            (rhs_var.clone(), rk.is_signed())
                        } else {
                            (format!("({rhs_var} as {})", lk.rust_type()), lk.is_signed())
                        };
                        Self::int_binary_op(
                            base_op,
                            *lk,
                            &format!("(*{ptr_var})"),
                            &rhs_cast,
                            rhs_signed,
                        )?
                    }
                    (CType::Float(lk), CType::Float(_), "+" | "-" | "*" | "/")
                    | (CType::Float(lk), CType::Int(_), "+" | "-" | "*" | "/") => {
                        format!("((*{ptr_var}) {base_op} ({rhs_var} as {}))", lk.rust_type())
                    }
                    (CType::Int(lk), CType::Float(rk), "+" | "-" | "*" | "/") => {
                        format!(
                            "(((*{ptr_var} as {}) {base_op} {rhs_var}) as {})",
                            rk.rust_type(),
                            lk.rust_type()
                        )
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
            "BinaryConditionalOperator" => {
                ensure!(inner.len() == 4, "invalid binary conditional expression");
                let (lhs_expr, lhs_ty) = self.typed_expression(&inner[0])?;
                let (rhs_expr, rhs_ty) = self.typed_expression(&inner[3])?;
                ensure!(
                    lhs_ty.same_shape(&rhs_ty),
                    "conditional branches must have matching types"
                );
                let tmp = self.fresh_var("rf_bc");
                let cond = match &lhs_ty {
                    CType::Int(_) => format!("{tmp} != 0"),
                    CType::Float(_) => format!("{tmp} != 0.0"),
                    CType::Ptr { .. } => format!("!{tmp}.is_null()"),
                    CType::FnPtr { .. } => format!("{tmp}.is_some()"),
                    _ => bail!("unsupported binary conditional operand type"),
                };
                Ok((
                    format!(
                        "{{ let {tmp} = {lhs_expr}; if {cond} {{ {tmp} }} else {{ {rhs_expr} }} }}"
                    ),
                    lhs_ty,
                ))
            }
            "CallExpr" => {
                ensure!(!inner.is_empty(), "invalid function call");
                let callee = &inner[0];
                if kind(callee) == "ImplicitCastExpr"
                    && matches!(
                        text(callee, "castKind"),
                        "FunctionToPointerDecay" | "BuiltinFnToFnPtr"
                    )
                    && children(callee).len() == 1
                    && kind(&children(callee)[0]) == "DeclRefExpr"
                    && kind(&children(callee)[0]["referencedDecl"]) == "FunctionDecl"
                {
                    let ref_decl = &children(callee)[0]["referencedDecl"];
                    let name = text(ref_decl, "name");
                    identifier(name)?;
                    if name.starts_with("__builtin_") {
                        return self.try_emit_builtin(
                            name,
                            node,
                            &inner[1..],
                            text(&ref_decl["type"], "qualType"),
                        );
                    }
                    if let Some(variadic_entry) = self.variadic_prototypes.get(name).cloned() {
                        ensure!(
                            self.options.allow_variadic_calls,
                            "variadic functions are unsupported"
                        );
                        let (fixed_params, ret_type) = match variadic_entry {
                            Ok(sig) => sig,
                            Err(reason) => bail!("{reason}"),
                        };
                        ensure!(
                            inner.len() > fixed_params.len(),
                            "call to {name} argument count mismatch"
                        );
                        let fixed_refs: Vec<&Value> =
                            inner[1..1 + fixed_params.len()].iter().collect();
                        return self.emit_variadic_call(
                            name,
                            &fixed_params,
                            &fixed_refs,
                            &inner[1 + fixed_params.len()..],
                            ret_type,
                        );
                    }
                    if let Some(reason) = self.unsupported_prototypes.get(name) {
                        bail!("{reason}");
                    }
                    let (param_types, ret_type) =
                        parse_fn_signature(text(&ref_decl["type"], "qualType"), &self.typedefs)?;
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

    fn emit_overflow_builtin(
        &mut self,
        name: &str,
        op_char: char,
        arg_nodes: &[Value],
        ret_type: CType,
    ) -> Result<(String, CType)> {
        ensure!(
            arg_nodes.len() == 3,
            "overflow builtin {name} requires 3 arguments"
        );
        let (lhs, lty) = self.typed_expression(&arg_nodes[0])?;
        let (rhs, rty) = self.typed_expression(&arg_nodes[1])?;
        let (out_ptr, out_ptr_ty) = self.typed_expression(&arg_nodes[2])?;
        ensure!(
            matches!(lty, CType::Int(_)) && matches!(rty, CType::Int(_)),
            "overflow builtin {name} integer operands required"
        );
        let CType::Ptr { inner, .. } = out_ptr_ty else {
            bail!("overflow builtin {name} third operand must be integer pointer");
        };
        let CType::Int(out_ik) = *inner else {
            bail!("overflow builtin {name} output pointee must be integer");
        };
        let out_rust = out_ik.rust_type();
        let l_var = self.fresh_var("rf_ol");
        let r_var = self.fresh_var("rf_or");
        let p_var = self.fresh_var("rf_op");
        let t_var = self.fresh_var("rf_ot");
        let o_var = self.fresh_var("rf_ovf");
        let calc = match op_char {
            '+' | '-' => format!(
                "let rf_exact: i128 = ({l_var} as i128) {op_char} ({r_var} as i128); let {t_var}: {out_rust} = rf_exact as {out_rust}; let {o_var}: bool = ({t_var} as i128) != rf_exact;"
            ),
            '*' => format!(
                "let ({t_var}, {o_var}): ({out_rust}, bool) = match ({l_var} as i128).checked_mul({r_var} as i128) {{ Some(rf_exact) => {{ let rf_t = rf_exact as {out_rust}; (rf_t, (rf_t as i128) != rf_exact) }} None => ((({l_var} as u128).wrapping_mul({r_var} as u128) as {out_rust}), true) }};"
            ),
            _ => bail!("unsupported overflow op {op_char}"),
        };
        let ret_expr = match ret_type {
            CType::Int(IntKind::Bool) => format!("u8::from({o_var})"),
            CType::Int(ik) => format!("({}::from({o_var}))", ik.rust_type()),
            _ => bail!("overflow builtin must return boolean or integer"),
        };
        Ok((
            format!(
                "{{ let {l_var} = {lhs}; let {r_var} = {rhs}; let {p_var} = rf_ptr({out_ptr}); {calc} *{p_var} = {t_var}; {ret_expr} }}"
            ),
            ret_type,
        ))
    }

    fn emit_variadic_call(
        &mut self,
        c_name: &str,
        fixed_params: &[CType],
        fixed_arg_nodes: &[&Value],
        extra_arg_nodes: &[Value],
        ret_type: CType,
    ) -> Result<(String, CType)> {
        ensure!(
            fixed_arg_nodes.len() == fixed_params.len(),
            "call to {c_name} argument count mismatch"
        );
        let mut arguments = Vec::with_capacity(fixed_params.len() + extra_arg_nodes.len());
        for (arg_node, expected_ty) in fixed_arg_nodes.iter().zip(fixed_params) {
            let (arg_expr, arg_ty) = self.typed_expression(arg_node)?;
            ensure!(
                arg_ty.same_shape(expected_ty),
                "call to {c_name} argument type mismatch"
            );
            arguments.push(arg_expr);
        }
        for arg_node in extra_arg_nodes {
            let (arg_expr, arg_ty) = self.typed_expression(arg_node)?;
            arguments.push(promote_variadic_arg(arg_expr, &arg_ty)?);
        }
        self.dependencies.insert(c_name.into());
        self.prototypes
            .entry(c_name.to_owned())
            .or_insert_with(|| make_external_variadic_function(c_name, fixed_params, &ret_type));
        Ok((
            format!("unsafe {{ rf_var_{c_name}({}) }}", arguments.join(", ")),
            ret_type,
        ))
    }

    fn try_emit_builtin(
        &mut self,
        name: &str,
        call_node: &Value,
        arg_nodes: &[Value],
        qual_type: &str,
    ) -> Result<(String, CType)> {
        let generic_ret = CType::from_node_type(&call_node["type"], &self.typedefs).ok();
        match name {
            "__builtin_add_overflow"
            | "__builtin_sadd_overflow"
            | "__builtin_saddl_overflow"
            | "__builtin_saddll_overflow"
            | "__builtin_uadd_overflow"
            | "__builtin_uaddl_overflow"
            | "__builtin_uaddll_overflow" => {
                let rt = generic_ret.unwrap_or(CType::Int(IntKind::Bool));
                return self.emit_overflow_builtin(name, '+', arg_nodes, rt);
            }
            "__builtin_sub_overflow"
            | "__builtin_ssub_overflow"
            | "__builtin_ssubl_overflow"
            | "__builtin_ssubll_overflow"
            | "__builtin_usub_overflow"
            | "__builtin_usubl_overflow"
            | "__builtin_usubll_overflow" => {
                let rt = generic_ret.unwrap_or(CType::Int(IntKind::Bool));
                return self.emit_overflow_builtin(name, '-', arg_nodes, rt);
            }
            "__builtin_mul_overflow"
            | "__builtin_smul_overflow"
            | "__builtin_smull_overflow"
            | "__builtin_smulll_overflow"
            | "__builtin_umul_overflow"
            | "__builtin_umull_overflow"
            | "__builtin_umulll_overflow" => {
                let rt = generic_ret.unwrap_or(CType::Int(IntKind::Bool));
                return self.emit_overflow_builtin(name, '*', arg_nodes, rt);
            }
            "__builtin_constant_p" => {
                let rt = generic_ret.unwrap_or(CType::Int(IntKind::I32));
                return Ok((format!("0{}", rt.rust_type()), rt));
            }
            "__builtin_isnan" | "__builtin_isfinite" | "__builtin_isinf" | "__builtin_signbit"
            | "__builtin_signbitf" => {
                ensure!(arg_nodes.len() == 1, "builtin {name} requires 1 argument");
                let (expr, ty) = self.typed_expression(&arg_nodes[0])?;
                ensure!(
                    matches!(ty, CType::Float(_)),
                    "builtin {name} requires float or double operand"
                );
                let rt = generic_ret.unwrap_or(CType::Int(IntKind::I32));
                let method = match name {
                    "__builtin_isnan" => "is_nan",
                    "__builtin_isfinite" => "is_finite",
                    "__builtin_isinf" => "is_infinite",
                    _ => "is_sign_negative",
                };
                return Ok((
                    format!("({}::from(({expr}).{method}()))", rt.rust_type()),
                    rt,
                ));
            }
            "__builtin_isinf_sign" => {
                ensure!(arg_nodes.len() == 1, "builtin {name} requires 1 argument");
                let (expr, ty) = self.typed_expression(&arg_nodes[0])?;
                ensure!(
                    matches!(ty, CType::Float(_)),
                    "builtin {name} requires float or double operand"
                );
                let rt = generic_ret.unwrap_or(CType::Int(IntKind::I32));
                let x = self.fresh_var("rf_inf");
                let rty = rt.rust_type();
                return Ok((
                    format!(
                        "{{ let {x} = {expr}; (if {x}.is_infinite() {{ if {x}.is_sign_negative() {{ -1i32 }} else {{ 1i32 }} }} else {{ 0i32 }}) as {rty} }}"
                    ),
                    rt,
                ));
            }
            "__builtin___snprintf_chk"
            | "__builtin___sprintf_chk"
            | "__builtin___fprintf_chk"
            | "__builtin___printf_chk"
                if self.options.allow_variadic_calls =>
            {
                let (fixed_params, ret_type, _) =
                    parse_fn_signature_variadic(qual_type, &self.typedefs)?;
                let (target_name, keep_indices): (&str, &[usize]) = match name {
                    "__builtin___snprintf_chk" => ("snprintf", &[0, 1, 4]),
                    "__builtin___sprintf_chk" => ("sprintf", &[0, 3]),
                    "__builtin___fprintf_chk" => ("fprintf", &[0, 2]),
                    _ => ("printf", &[1]),
                };
                ensure!(
                    fixed_params.len() > *keep_indices.last().unwrap()
                        && arg_nodes.len() >= fixed_params.len(),
                    "builtin {name} argument count mismatch"
                );
                let forwarded_params: Vec<CType> = keep_indices
                    .iter()
                    .map(|&idx| fixed_params[idx].clone())
                    .collect();
                let fixed_refs: Vec<&Value> =
                    keep_indices.iter().map(|&idx| &arg_nodes[idx]).collect();
                return self.emit_variadic_call(
                    target_name,
                    &forwarded_params,
                    &fixed_refs,
                    &arg_nodes[fixed_params.len()..],
                    ret_type,
                );
            }
            _ => {}
        }

        let (param_types, ret_type) = parse_fn_signature(qual_type, &self.typedefs)?;
        ensure!(
            arg_nodes.len() == param_types.len(),
            "builtin {name} argument count mismatch"
        );
        if name == "__builtin_object_size" {
            return Ok((format!("usize::MAX as {}", ret_type.rust_type()), ret_type));
        }
        let mut args = Vec::with_capacity(arg_nodes.len());
        for (node, expected_ty) in arg_nodes.iter().zip(&param_types) {
            let (expr, ty) = self.typed_expression(node)?;
            ensure!(
                ty.same_shape(expected_ty),
                "builtin {name} argument type mismatch"
            );
            args.push(expr);
        }
        match name {
            "__builtin___memcpy_chk" | "__builtin_memcpy" => {
                let d_var = self.fresh_var("rf_dst");
                let s_var = self.fresh_var("rf_src");
                let n_var = self.fresh_var("rf_len");
                Ok((
                    format!(
                        "{{ let {d_var} = {}; let {s_var} = {}; let {n_var} = ({}) as usize; if {n_var} > 0 {{ core::ptr::copy_nonoverlapping({s_var} as *const u8, {d_var} as *mut u8, {n_var}); }} {d_var} }}",
                        args[0], args[1], args[2]
                    ),
                    ret_type,
                ))
            }
            "__builtin___memmove_chk" | "__builtin_memmove" => {
                let d_var = self.fresh_var("rf_dst");
                let s_var = self.fresh_var("rf_src");
                let n_var = self.fresh_var("rf_len");
                Ok((
                    format!(
                        "{{ let {d_var} = {}; let {s_var} = {}; let {n_var} = ({}) as usize; if {n_var} > 0 {{ core::ptr::copy({s_var} as *const u8, {d_var} as *mut u8, {n_var}); }} {d_var} }}",
                        args[0], args[1], args[2]
                    ),
                    ret_type,
                ))
            }
            "__builtin___memset_chk" | "__builtin_memset" => {
                let d_var = self.fresh_var("rf_dst");
                let v_var = self.fresh_var("rf_val");
                let n_var = self.fresh_var("rf_len");
                Ok((
                    format!(
                        "{{ let {d_var} = {}; let {v_var} = ({}) as u8; let {n_var} = ({}) as usize; if {n_var} > 0 {{ core::ptr::write_bytes({d_var} as *mut u8, {v_var}, {n_var}); }} {d_var} }}",
                        args[0], args[1], args[2]
                    ),
                    ret_type,
                ))
            }
            "__builtin_bzero" => {
                let d_var = self.fresh_var("rf_dst");
                let n_var = self.fresh_var("rf_len");
                Ok((
                    format!(
                        "{{ let {d_var} = {}; let {n_var} = ({}) as usize; if {n_var} > 0 {{ core::ptr::write_bytes({d_var} as *mut u8, 0u8, {n_var}); }} }}",
                        args[0], args[1]
                    ),
                    ret_type,
                ))
            }
            "__builtin___strcpy_chk" => {
                let d_var = self.fresh_var("rf_dst");
                let s_var = self.fresh_var("rf_src");
                let i_var = self.fresh_var("rf_idx");
                let ch_var = self.fresh_var("rf_ch");
                Ok((
                    format!(
                        "{{ let {d_var} = {}; let {s_var} = {}; let mut {i_var}: isize = 0; loop {{ let {ch_var} = *{s_var}.offset({i_var}); *{d_var}.offset({i_var}) = {ch_var}; if {ch_var} == 0 {{ break; }} {i_var} += 1; }} {d_var} }}",
                        args[0], args[1]
                    ),
                    ret_type,
                ))
            }
            "__builtin___strncpy_chk" => {
                let d_var = self.fresh_var("rf_dst");
                let s_var = self.fresh_var("rf_src");
                let n_var = self.fresh_var("rf_len");
                let i_var = self.fresh_var("rf_idx");
                let ch_var = self.fresh_var("rf_ch");
                Ok((
                    format!(
                        "{{ let {d_var} = {}; let {s_var} = {}; let {n_var} = ({}) as usize; let mut {i_var}: usize = 0; while {i_var} < {n_var} {{ let {ch_var} = *{s_var}.add({i_var}); *{d_var}.add({i_var}) = {ch_var}; {i_var} += 1; if {ch_var} == 0 {{ while {i_var} < {n_var} {{ *{d_var}.add({i_var}) = 0; {i_var} += 1; }} break; }} }} {d_var} }}",
                        args[0], args[1], args[2]
                    ),
                    ret_type,
                ))
            }
            "__builtin___strcat_chk" => {
                let d_var = self.fresh_var("rf_dst");
                let s_var = self.fresh_var("rf_src");
                let n_var = self.fresh_var("rf_len");
                let i_var = self.fresh_var("rf_idx");
                let ch_var = self.fresh_var("rf_ch");
                Ok((
                    format!(
                        "{{ let {d_var} = {}; let {s_var} = {}; let mut {n_var}: isize = 0; while *{d_var}.offset({n_var}) != 0 {{ {n_var} += 1; }} let mut {i_var}: isize = 0; loop {{ let {ch_var} = *{s_var}.offset({i_var}); *{d_var}.offset({n_var} + {i_var}) = {ch_var}; if {ch_var} == 0 {{ break; }} {i_var} += 1; }} {d_var} }}",
                        args[0], args[1]
                    ),
                    ret_type,
                ))
            }
            "__builtin___strncat_chk" => {
                let d_var = self.fresh_var("rf_dst");
                let s_var = self.fresh_var("rf_src");
                let max_var = self.fresh_var("rf_max");
                let n_var = self.fresh_var("rf_len");
                let i_var = self.fresh_var("rf_idx");
                let ch_var = self.fresh_var("rf_ch");
                Ok((
                    format!(
                        "{{ let {d_var} = {}; let {s_var} = {}; let {max_var} = ({}) as usize; let mut {n_var}: usize = 0; while *{d_var}.add({n_var}) != 0 {{ {n_var} += 1; }} let mut {i_var}: usize = 0; while {i_var} < {max_var} {{ let {ch_var} = *{s_var}.add({i_var}); *{d_var}.add({n_var} + {i_var}) = {ch_var}; if {ch_var} == 0 {{ break; }} {i_var} += 1; }} *{d_var}.add({n_var} + {i_var}) = 0; {d_var} }}",
                        args[0], args[1], args[2]
                    ),
                    ret_type,
                ))
            }
            "__builtin_expect" => Ok((args.into_iter().next().unwrap(), ret_type)),
            "__builtin_prefetch" => Ok(("()".into(), CType::Void)),
            "__builtin_unreachable" => {
                Ok(("core::hint::unreachable_unchecked()".into(), CType::Void))
            }
            "__builtin_trap" => Ok(("panic!(\"__builtin_trap\")".into(), CType::Void)),
            "__builtin_bswap16" | "__builtin_bswap32" | "__builtin_bswap64" => Ok((
                format!("(({}) as {}).swap_bytes()", args[0], ret_type.rust_type()),
                ret_type,
            )),
            "__builtin_clz" | "__builtin_clzl" | "__builtin_clzll" => Ok((
                format!(
                    "((({}) as {}).leading_zeros() as {})",
                    args[0],
                    param_types[0].rust_type(),
                    ret_type.rust_type()
                ),
                ret_type,
            )),
            "__builtin_ctz" | "__builtin_ctzl" | "__builtin_ctzll" => Ok((
                format!(
                    "((({}) as {}).trailing_zeros() as {})",
                    args[0],
                    param_types[0].rust_type(),
                    ret_type.rust_type()
                ),
                ret_type,
            )),
            "__builtin_ffs" | "__builtin_ffsl" | "__builtin_ffsll" => {
                let CType::Int(ik) = param_types[0] else {
                    bail!("ffs requires integer argument");
                };
                let uty = ik.unsigned_rust_type();
                let x_var = self.fresh_var("rf_ffs");
                Ok((
                    format!(
                        "{{ let {x_var} = ({}) as {uty}; if {x_var} == 0 {{ 0{} }} else {{ ({x_var}.trailing_zeros() + 1) as {} }} }}",
                        args[0],
                        ret_type.rust_type(),
                        ret_type.rust_type()
                    ),
                    ret_type,
                ))
            }
            "__builtin_popcount" | "__builtin_popcountl" | "__builtin_popcountll" => Ok((
                format!(
                    "((({}) as {}).count_ones() as {})",
                    args[0],
                    param_types[0].rust_type(),
                    ret_type.rust_type()
                ),
                ret_type,
            )),
            "__builtin_parity" | "__builtin_parityl" | "__builtin_parityll" => Ok((
                format!(
                    "(((({}) as {}).count_ones() & 1) as {})",
                    args[0],
                    param_types[0].rust_type(),
                    ret_type.rust_type()
                ),
                ret_type,
            )),
            "__builtin_rotateleft8"
            | "__builtin_rotateleft16"
            | "__builtin_rotateleft32"
            | "__builtin_rotateleft64" => Ok((
                format!(
                    "(({}) as {}).rotate_left(({}) as u32)",
                    args[0],
                    ret_type.rust_type(),
                    args[1]
                ),
                ret_type,
            )),
            "__builtin_rotateright8"
            | "__builtin_rotateright16"
            | "__builtin_rotateright32"
            | "__builtin_rotateright64" => Ok((
                format!(
                    "(({}) as {}).rotate_right(({}) as u32)",
                    args[0],
                    ret_type.rust_type(),
                    args[1]
                ),
                ret_type,
            )),
            "__builtin_inf" | "__builtin_huge_val" => Ok(("f64::INFINITY".into(), ret_type)),
            "__builtin_inff" | "__builtin_huge_valf" => Ok(("f32::INFINITY".into(), ret_type)),
            "__builtin_nan" => Ok(("f64::NAN".into(), ret_type)),
            "__builtin_nanf" => Ok(("f32::NAN".into(), ret_type)),
            "__builtin_fabs" => Ok((
                format!(
                    "f64::from_bits((({}) as f64).to_bits() & !(1u64 << 63))",
                    args[0]
                ),
                ret_type,
            )),
            "__builtin_fabsf" => Ok((
                format!(
                    "f32::from_bits((({}) as f32).to_bits() & !(1u32 << 31))",
                    args[0]
                ),
                ret_type,
            )),
            _ => bail!("unsupported builtin {name}"),
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
        let mut decl_inits: BTreeMap<usize, Vec<(String, String, CType, Value)>> = BTreeMap::new();
        for (seg_idx, (_, stmts)) in segments.iter().enumerate() {
            for (stmt_idx, stmt) in stmts.iter().enumerate() {
                if kind(stmt) == "DeclStmt" {
                    let mut init_pairs = Vec::new();
                    for declaration in children(stmt) {
                        if self.handle_non_var_local_decl(declaration)? {
                            continue;
                        }
                        ensure!(
                            kind(declaration) == "VarDecl",
                            "unsupported local declaration"
                        );
                        if let Some(static_or_extern_stmt) =
                            self.try_static_or_extern_local(declaration)?
                        {
                            if !static_or_extern_stmt.is_empty() {
                                hoisted.push(static_or_extern_stmt);
                            }
                            continue;
                        }
                        let var_ty = CType::from_node_type(&declaration["type"], &self.typedefs)?;
                        ensure!(
                            !matches!(var_ty, CType::Void),
                            "void local variable unsupported"
                        );
                        let decl_id = text(declaration, "id").to_owned();
                        let name = self.fresh_var("v");
                        let rust_ty = var_ty.rust_type();
                        let zero = var_ty.zero_expr(&self.structs)?;
                        self.variables
                            .insert(decl_id.clone(), (name.clone(), var_ty.clone()));
                        hoisted.push(format!("let mut {name}: {rust_ty} = {zero};"));
                        if let Some(init_node) = local_initializer(declaration)? {
                            init_pairs.push((decl_id, name, var_ty, init_node.clone()));
                        } else if !self.options.allow_uninit_locals && is_scalar_type(&var_ty) {
                            self.uninit_scalars.insert(decl_id);
                        }
                    }
                    if !init_pairs.is_empty() {
                        decl_inits.insert((seg_idx << 16) | stmt_idx, init_pairs);
                    }
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
                        for (decl_id, vname, vty, init_node) in inits {
                            let (val_expr, val_ty) = self.typed_expression(init_node)?;
                            ensure!(vty.same_shape(&val_ty), "local initializer type mismatch");
                            self.uninit_scalars.remove(decl_id);
                            out_stmts.push(format!("{vname} = {val_expr};"));
                        }
                    } else if kind(stmt) != "DeclStmt" {
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
                let saved = self.uninit_scalars.clone();
                let branch = self.statement(&inner[1])?;
                let after_then = self.uninit_scalars.clone();
                let other = if inner.len() == 3 {
                    self.uninit_scalars = saved;
                    let res = format!(" else {{ {} }}", self.statement(&inner[2])?);
                    self.uninit_scalars = self
                        .uninit_scalars
                        .intersection(&after_then)
                        .cloned()
                        .collect();
                    res
                } else {
                    self.uninit_scalars = saved;
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
                let saved = self.uninit_scalars.clone();
                let loop_label = self.fresh_var("'rf_lp");
                self.loop_labels.push(loop_label.clone());
                self.break_target.push(None);
                let body = self.statement(&inner[1]);
                self.break_target.pop();
                self.loop_labels.pop();
                self.uninit_scalars = saved;
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
                    if self.handle_non_var_local_decl(declaration)? {
                        continue;
                    }
                    ensure!(
                        kind(declaration) == "VarDecl",
                        "unsupported local declaration"
                    );
                    if let Some(static_or_extern_stmt) =
                        self.try_static_or_extern_local(declaration)?
                    {
                        if !static_or_extern_stmt.is_empty() {
                            statements.push(static_or_extern_stmt);
                        }
                        continue;
                    }
                    let var_ty = CType::from_node_type(&declaration["type"], &self.typedefs)?;
                    ensure!(
                        !matches!(var_ty, CType::Void),
                        "void local variable unsupported"
                    );
                    let decl_id = text(declaration, "id").to_owned();
                    let name = self.fresh_var("v");
                    let rust_ty = var_ty.rust_type();
                    let value = if let Some(init_node) = local_initializer(declaration)? {
                        let (val, val_ty) = self.typed_expression(init_node)?;
                        ensure!(
                            var_ty.same_shape(&val_ty),
                            "local initializer type mismatch"
                        );
                        val
                    } else {
                        let z = var_ty.zero_expr(&self.structs)?;
                        if !self.options.allow_uninit_locals && is_scalar_type(&var_ty) {
                            self.uninit_scalars.insert(decl_id.clone());
                        }
                        z
                    };
                    self.variables.insert(decl_id, (name.clone(), var_ty));
                    statements.push(format!("let mut {name}: {rust_ty} = {value};"));
                }
                Ok(statements.join("\n"))
            }
            "NullStmt" => Ok(";".into()),
            _ => Ok(format!("{};", self.typed_expression(node)?.0)),
        }
    }

    fn emit_general_cfg_body(&mut self, body: &Value) -> Result<String> {
        let mut builder = CfgBuilder {
            emitter: self,
            blocks: Vec::new(),
            label_blocks: BTreeMap::new(),
            static_or_extern_ids: BTreeSet::new(),
            hoisted_items: Vec::new(),
            break_targets: Vec::new(),
            continue_targets: Vec::new(),
            switch_stack: Vec::new(),
        };
        builder.hoist_local_decls(body)?;
        builder.preallocate_labels(body)?;
        let entry_bb = builder.new_block();
        let exit_bb = builder.new_block();
        builder.blocks[exit_bb].term = CfgTerm::Exit;
        let end_bb = builder.lower_stmt(body, entry_bb)?;
        builder.set_jump(end_bb, exit_bb);
        builder.render(entry_bb, exit_bb)
    }
}

struct CfgProbeState {
    pos: usize,
    compound_stack: Vec<usize>,
    labels: BTreeMap<String, (usize, Option<usize>)>,
    complex: bool,
}

fn check_switch_complexity(node: &Value, in_control: bool) -> bool {
    let k = kind(node);
    if k == "LabelStmt" {
        return true;
    }
    if matches!(k, "CaseStmt" | "DefaultStmt") && in_control {
        return true;
    }
    if matches!(k, "SwitchStmt" | "StmtExpr") {
        return false;
    }
    let next_in_control = in_control
        || matches!(
            k,
            "IfStmt" | "WhileStmt" | "DoStmt" | "ForStmt" | "LabelStmt"
        );
    children(node)
        .iter()
        .any(|c| check_switch_complexity(c, next_in_control))
}

fn cfg_probe_pass1(node: &Value, direct_compound: Option<usize>, state: &mut CfgProbeState) {
    if state.complex {
        return;
    }
    let k = kind(node);
    if k == "StmtExpr" {
        return;
    }
    let my_pos = state.pos;
    state.pos += 1;
    match k {
        "CompoundStmt" => {
            state.compound_stack.push(my_pos);
            for c in children(node) {
                cfg_probe_pass1(c, Some(my_pos), state);
            }
            state.compound_stack.pop();
        }
        "LabelStmt" => {
            let decl_id = text(node, "declId").to_owned();
            state.labels.insert(decl_id, (my_pos, direct_compound));
            if direct_compound.is_none() {
                state.complex = true;
                return;
            }
            for c in children(node) {
                cfg_probe_pass1(c, direct_compound, state);
            }
        }
        "SwitchStmt" => {
            if children(node)
                .get(1)
                .is_some_and(|b| check_switch_complexity(b, false))
            {
                state.complex = true;
                return;
            }
            for c in children(node) {
                cfg_probe_pass1(c, None, state);
            }
        }
        _ => {
            for c in children(node) {
                cfg_probe_pass1(c, None, state);
            }
        }
    }
}

fn cfg_probe_pass2(node: &Value, state: &mut CfgProbeState) -> bool {
    let k = kind(node);
    if k == "StmtExpr" {
        return false;
    }
    let my_pos = state.pos;
    state.pos += 1;
    match k {
        "CompoundStmt" => {
            state.compound_stack.push(my_pos);
            let res = children(node).iter().any(|c| cfg_probe_pass2(c, state));
            state.compound_stack.pop();
            res
        }
        "GotoStmt" => {
            let target = text(node, "targetLabelDeclId");
            let Some(&(lbl_pos, Some(lbl_compound))) = state.labels.get(target) else {
                return true;
            };
            !(lbl_pos > my_pos && state.compound_stack.contains(&lbl_compound))
        }
        _ => children(node).iter().any(|c| cfg_probe_pass2(c, state)),
    }
}

fn needs_general_cfg(body: &Value) -> bool {
    let mut state = CfgProbeState {
        pos: 0,
        compound_stack: Vec::new(),
        labels: BTreeMap::new(),
        complex: false,
    };
    cfg_probe_pass1(body, None, &mut state);
    if state.complex {
        return true;
    }
    state.pos = 0;
    state.compound_stack.clear();
    cfg_probe_pass2(body, &mut state)
}

enum CfgTerm {
    Unreachable,
    Jump(usize),
    Branch {
        cond: String,
        then_bb: usize,
        else_bb: usize,
    },
    Switch {
        sw_var: String,
        cases: Vec<(String, usize)>,
        default_bb: usize,
    },
    Return,
    Exit,
}

struct CfgBlock {
    stmts: Vec<String>,
    term: CfgTerm,
}

struct CfgSwitch {
    sw_ty: CType,
    cases: Vec<(String, usize)>,
    default_bb: Option<usize>,
}

struct CfgBuilder<'a, 'b> {
    emitter: &'a mut Emitter<'b>,
    blocks: Vec<CfgBlock>,
    label_blocks: BTreeMap<String, usize>,
    static_or_extern_ids: BTreeSet<String>,
    hoisted_items: Vec<String>,
    break_targets: Vec<usize>,
    continue_targets: Vec<usize>,
    switch_stack: Vec<CfgSwitch>,
}

impl<'a, 'b> CfgBuilder<'a, 'b> {
    fn new_block(&mut self) -> usize {
        let id = self.blocks.len();
        self.blocks.push(CfgBlock {
            stmts: Vec::new(),
            term: CfgTerm::Unreachable,
        });
        id
    }

    fn set_jump(&mut self, bb: usize, target: usize) {
        if matches!(self.blocks[bb].term, CfgTerm::Unreachable) {
            self.blocks[bb].term = CfgTerm::Jump(target);
        }
    }

    fn hoist_local_decls(&mut self, node: &Value) -> Result<()> {
        let k = kind(node);
        if k == "StmtExpr" {
            return Ok(());
        }
        if k == "DeclStmt" {
            for declaration in children(node) {
                if self.emitter.handle_non_var_local_decl(declaration)? {
                    continue;
                }
                ensure!(
                    kind(declaration) == "VarDecl",
                    "unsupported local declaration"
                );
                let decl_id = text(declaration, "id").to_owned();
                if let Some(static_or_extern_stmt) =
                    self.emitter.try_static_or_extern_local(declaration)?
                {
                    if !static_or_extern_stmt.is_empty() {
                        self.hoisted_items.push(static_or_extern_stmt);
                    }
                    self.static_or_extern_ids.insert(decl_id);
                    continue;
                }
                let var_ty = CType::from_node_type(&declaration["type"], &self.emitter.typedefs)?;
                ensure!(
                    !matches!(var_ty, CType::Void),
                    "void local variable unsupported"
                );
                let name = self.emitter.fresh_var("v");
                let rust_ty = var_ty.rust_type();
                let zero = var_ty.zero_expr(&self.emitter.structs)?;
                self.emitter
                    .variables
                    .insert(decl_id, (name.clone(), var_ty));
                self.hoisted_items
                    .push(format!("let mut {name}: {rust_ty} = {zero};"));
            }
            return Ok(());
        }
        if matches!(
            k,
            "CompoundStmt"
                | "IfStmt"
                | "WhileStmt"
                | "DoStmt"
                | "ForStmt"
                | "SwitchStmt"
                | "CaseStmt"
                | "DefaultStmt"
                | "LabelStmt"
        ) {
            for child in children(node) {
                self.hoist_local_decls(child)?;
            }
        }
        Ok(())
    }

    fn preallocate_labels(&mut self, node: &Value) -> Result<()> {
        let k = kind(node);
        if k == "StmtExpr" {
            return Ok(());
        }
        if k == "LabelStmt" {
            let decl_id = text(node, "declId");
            ensure!(!decl_id.is_empty(), "missing label declId");
            let bb = self.new_block();
            self.label_blocks.insert(decl_id.to_owned(), bb);
        }
        if matches!(
            k,
            "CompoundStmt"
                | "IfStmt"
                | "WhileStmt"
                | "DoStmt"
                | "ForStmt"
                | "SwitchStmt"
                | "CaseStmt"
                | "DefaultStmt"
                | "LabelStmt"
        ) {
            for child in children(node) {
                self.preallocate_labels(child)?;
            }
        }
        Ok(())
    }

    fn lower_stmt(&mut self, node: &Value, cur_bb: usize) -> Result<usize> {
        let inner = children(node);
        match kind(node) {
            "CompoundStmt" => {
                let mut bb = cur_bb;
                for item in inner {
                    bb = self.lower_stmt(item, bb)?;
                }
                Ok(bb)
            }
            "LabelStmt" => {
                let decl_id = text(node, "declId");
                let lbl_bb = *self
                    .label_blocks
                    .get(decl_id)
                    .with_context(|| format!("unknown label {decl_id}"))?;
                self.set_jump(cur_bb, lbl_bb);
                if let Some(sub) = inner.first() {
                    self.lower_stmt(sub, lbl_bb)
                } else {
                    Ok(lbl_bb)
                }
            }
            "GotoStmt" => {
                let target_id = text(node, "targetLabelDeclId");
                let target_bb = *self
                    .label_blocks
                    .get(target_id)
                    .with_context(|| format!("unknown goto target {target_id}"))?;
                self.set_jump(cur_bb, target_bb);
                Ok(self.new_block())
            }
            "ReturnStmt" => {
                if matches!(self.emitter.return_type, CType::Void) {
                    ensure!(inner.is_empty(), "void return cannot return a value");
                    self.blocks[cur_bb].stmts.push("return;".into());
                } else {
                    ensure!(inner.len() == 1, "non-void return requires a value");
                    let (expr, ty) = self.emitter.typed_expression(&inner[0])?;
                    ensure!(
                        ty.same_shape(&self.emitter.return_type),
                        "return type mismatch with function signature"
                    );
                    self.blocks[cur_bb].stmts.push(format!("return {expr};"));
                }
                self.blocks[cur_bb].term = CfgTerm::Return;
                Ok(self.new_block())
            }
            "IfStmt" => {
                ensure!(
                    (inner.len() == 2 || inner.len() == 3)
                        && !node["hasInit"].as_bool().unwrap_or(false),
                    "unsupported if structure"
                );
                let cond = self.emitter.condition(&inner[0])?;
                let then_bb = self.new_block();
                let else_bb = (inner.len() == 3).then(|| self.new_block());
                let join_bb = self.new_block();
                self.blocks[cur_bb].term = CfgTerm::Branch {
                    cond,
                    then_bb,
                    else_bb: else_bb.unwrap_or(join_bb),
                };
                let then_end = self.lower_stmt(&inner[1], then_bb)?;
                self.set_jump(then_end, join_bb);
                if let Some(ebb) = else_bb {
                    let else_end = self.lower_stmt(&inner[2], ebb)?;
                    self.set_jump(else_end, join_bb);
                }
                Ok(join_bb)
            }
            "WhileStmt" => {
                ensure!(
                    inner.len() == 2 && !node["hasVar"].as_bool().unwrap_or(false),
                    "unsupported while structure"
                );
                let header_bb = self.new_block();
                let body_bb = self.new_block();
                let after_bb = self.new_block();
                self.set_jump(cur_bb, header_bb);
                let cond = self.emitter.condition(&inner[0])?;
                self.blocks[header_bb].term = CfgTerm::Branch {
                    cond,
                    then_bb: body_bb,
                    else_bb: after_bb,
                };
                self.continue_targets.push(header_bb);
                self.break_targets.push(after_bb);
                let body_res = self.lower_stmt(&inner[1], body_bb);
                self.break_targets.pop();
                self.continue_targets.pop();
                let body_end = body_res?;
                self.set_jump(body_end, header_bb);
                Ok(after_bb)
            }
            "DoStmt" => {
                ensure!(inner.len() == 2, "unsupported do-while structure");
                let body_bb = self.new_block();
                let cond_bb = self.new_block();
                let after_bb = self.new_block();
                self.set_jump(cur_bb, body_bb);
                self.continue_targets.push(cond_bb);
                self.break_targets.push(after_bb);
                let body_res = self.lower_stmt(&inner[0], body_bb);
                self.break_targets.pop();
                self.continue_targets.pop();
                let body_end = body_res?;
                self.set_jump(body_end, cond_bb);
                let cond = self.emitter.condition(&inner[1])?;
                self.blocks[cond_bb].term = CfgTerm::Branch {
                    cond,
                    then_bb: body_bb,
                    else_bb: after_bb,
                };
                Ok(after_bb)
            }
            "ForStmt" => {
                ensure!(inner.len() == 5, "unsupported for structure");
                ensure!(
                    inner[1].get("kind").is_none(),
                    "for condition variable declaration is unsupported"
                );
                let mut bb = cur_bb;
                if inner[0].get("kind").is_some() {
                    if kind(&inner[0]) == "DeclStmt" {
                        bb = self.lower_stmt(&inner[0], bb)?;
                    } else {
                        let expr = self.emitter.typed_expression(&inner[0])?.0;
                        self.blocks[bb].stmts.push(format!("{expr};"));
                    }
                }
                let header_bb = self.new_block();
                let body_bb = self.new_block();
                let inc_bb = self.new_block();
                let after_bb = self.new_block();
                self.set_jump(bb, header_bb);
                if inner[2].get("kind").is_some() {
                    let cond = self.emitter.condition(&inner[2])?;
                    self.blocks[header_bb].term = CfgTerm::Branch {
                        cond,
                        then_bb: body_bb,
                        else_bb: after_bb,
                    };
                } else {
                    self.set_jump(header_bb, body_bb);
                }
                self.continue_targets.push(inc_bb);
                self.break_targets.push(after_bb);
                let body_res = self.lower_stmt(&inner[4], body_bb);
                self.break_targets.pop();
                self.continue_targets.pop();
                let body_end = body_res?;
                self.set_jump(body_end, inc_bb);
                if inner[3].get("kind").is_some() {
                    let inc_expr = self.emitter.typed_expression(&inner[3])?.0;
                    self.blocks[inc_bb].stmts.push(format!("{inc_expr};"));
                }
                self.set_jump(inc_bb, header_bb);
                Ok(after_bb)
            }
            "SwitchStmt" => {
                ensure!(
                    inner.len() == 2 && !node["hasInit"].as_bool().unwrap_or(false),
                    "unsupported switch structure"
                );
                let (sw_expr, sw_ty) = self.emitter.typed_expression(&inner[0])?;
                ensure!(
                    matches!(sw_ty, CType::Int(_)),
                    "switch condition must be integer type"
                );
                let sw_var = self.emitter.fresh_var("rf_sw_val");
                self.blocks[cur_bb]
                    .stmts
                    .push(format!("let {sw_var} = {sw_expr};"));
                let pre_bb = self.new_block();
                let break_bb = self.new_block();
                self.switch_stack.push(CfgSwitch {
                    sw_ty,
                    cases: Vec::new(),
                    default_bb: None,
                });
                self.break_targets.push(break_bb);
                let body_res = self.lower_stmt(&inner[1], pre_bb);
                self.break_targets.pop();
                let sw_info = self.switch_stack.pop().unwrap();
                let body_end = body_res?;
                self.set_jump(body_end, break_bb);
                let default_bb = sw_info.default_bb.unwrap_or(break_bb);
                self.blocks[cur_bb].term = CfgTerm::Switch {
                    sw_var,
                    cases: sw_info.cases,
                    default_bb,
                };
                Ok(break_bb)
            }
            "CaseStmt" => {
                ensure!(inner.len() == 2, "unsupported case statement");
                let sw_ty = self
                    .switch_stack
                    .last()
                    .context("case label outside switch statement")?
                    .sw_ty
                    .clone();
                let (c_expr, c_ty) = self.emitter.typed_expression(&inner[0])?;
                let rhs = if c_ty.same_shape(&sw_ty) {
                    c_expr
                } else {
                    format!("(({c_expr}) as {})", sw_ty.rust_type())
                };
                let case_bb = self.new_block();
                self.set_jump(cur_bb, case_bb);
                self.switch_stack
                    .last_mut()
                    .unwrap()
                    .cases
                    .push((rhs, case_bb));
                self.lower_stmt(&inner[1], case_bb)
            }
            "DefaultStmt" => {
                ensure!(inner.len() == 1, "invalid default statement");
                ensure!(
                    !self.switch_stack.is_empty(),
                    "default label outside switch statement"
                );
                let def_bb = self.new_block();
                self.set_jump(cur_bb, def_bb);
                self.switch_stack.last_mut().unwrap().default_bb = Some(def_bb);
                self.lower_stmt(&inner[0], def_bb)
            }
            "BreakStmt" => {
                let target = *self
                    .break_targets
                    .last()
                    .context("break outside of loop or switch")?;
                self.set_jump(cur_bb, target);
                Ok(self.new_block())
            }
            "ContinueStmt" => {
                let target = *self
                    .continue_targets
                    .last()
                    .context("continue outside of loop")?;
                self.set_jump(cur_bb, target);
                Ok(self.new_block())
            }
            "DeclStmt" => {
                for declaration in inner {
                    if kind(declaration) != "VarDecl" {
                        continue;
                    }
                    let decl_id = text(declaration, "id");
                    if self.static_or_extern_ids.contains(decl_id) {
                        continue;
                    }
                    let (name, var_ty) = self
                        .emitter
                        .variables
                        .get(decl_id)
                        .cloned()
                        .context("missing hoisted local variable")?;
                    if let Some(init_node) = local_initializer(declaration)? {
                        let (val, val_ty) = self.emitter.typed_expression(init_node)?;
                        ensure!(
                            var_ty.same_shape(&val_ty),
                            "local initializer type mismatch"
                        );
                        self.blocks[cur_bb].stmts.push(format!("{name} = {val};"));
                    }
                }
                Ok(cur_bb)
            }
            "NullStmt" => Ok(cur_bb),
            _ => {
                let expr = self.emitter.typed_expression(node)?.0;
                self.blocks[cur_bb].stmts.push(format!("{expr};"));
                Ok(cur_bb)
            }
        }
    }

    fn render(self, entry_bb: usize, exit_bb: usize) -> Result<String> {
        let mut reachable = vec![false; self.blocks.len()];
        let mut work = vec![entry_bb];
        reachable[entry_bb] = true;
        while let Some(bb) = work.pop() {
            match &self.blocks[bb].term {
                CfgTerm::Unreachable => bail!("CFG block {bb} missing terminator"),
                CfgTerm::Jump(t) => {
                    if !reachable[*t] {
                        reachable[*t] = true;
                        work.push(*t);
                    }
                }
                CfgTerm::Branch {
                    then_bb, else_bb, ..
                } => {
                    for &t in &[then_bb, else_bb] {
                        if !reachable[*t] {
                            reachable[*t] = true;
                            work.push(*t);
                        }
                    }
                }
                CfgTerm::Switch {
                    cases, default_bb, ..
                } => {
                    for &(_, t) in cases {
                        if !reachable[t] {
                            reachable[t] = true;
                            work.push(t);
                        }
                    }
                    if !reachable[*default_bb] {
                        reachable[*default_bb] = true;
                        work.push(*default_bb);
                    }
                }
                CfgTerm::Return | CfgTerm::Exit => {}
            }
        }

        let mut arms = Vec::new();
        for (bb, block) in self.blocks.iter().enumerate() {
            if !reachable[bb] {
                continue;
            }
            if bb == exit_bb {
                arms.push(format!("{bb} => break 'rf_cfg,"));
                continue;
            }
            let mut body_lines = block.stmts.clone();
            match &block.term {
                CfgTerm::Jump(t) => body_lines.push(format!("rf_pc = {t};")),
                CfgTerm::Branch {
                    cond,
                    then_bb,
                    else_bb,
                } => {
                    body_lines.push(format!(
                        "rf_pc = if {cond} {{ {then_bb} }} else {{ {else_bb} }};"
                    ));
                }
                CfgTerm::Switch {
                    sw_var,
                    cases,
                    default_bb,
                } => {
                    if cases.is_empty() {
                        body_lines.push(format!("rf_pc = {default_bb};"));
                    } else {
                        let mut chain = String::from("if false {}");
                        for (rhs, target) in cases {
                            chain.push_str(&format!(
                                " else if {sw_var} == ({rhs}) {{ rf_pc = {target}; }}"
                            ));
                        }
                        chain.push_str(&format!(" else {{ rf_pc = {default_bb}; }}"));
                        body_lines.push(chain);
                    }
                }
                CfgTerm::Return => {}
                CfgTerm::Exit | CfgTerm::Unreachable => unreachable!(),
            }
            arms.push(format!("{bb} => {{\n{}\n}}", body_lines.join("\n")));
        }

        Ok(format!(
            "{}\nlet mut rf_pc: usize = {entry_bb};\n'rf_cfg: loop {{\nmatch rf_pc {{\n{}\n_ => unreachable!(),\n}}\n}}",
            self.hoisted_items.join("\n"),
            arms.join("\n"),
        ))
    }
}
