//! Core C type model (`CType`, integer/float kinds) and global binding metadata.
use anyhow::{Result, bail, ensure};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::clang::ast::{identifier, text};
use crate::clang::type_parse::{
    parse_int_kind, parse_param_list, resolve_anon_record_type, resolve_bare_fn_type,
    split_array_type, split_fn_ptr, strip_pointee_qualifiers, strip_trailing_qualifiers,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GlobalKind {
    StaticConst,
    ConstItem,
    StaticMut,
}

#[derive(Debug, Clone)]
pub(super) struct GlobalBinding {
    pub(super) rust_name: String,
    pub(super) c_name: String,
    pub(super) ty: CType,
    pub(super) kind: GlobalKind,
}

pub(super) type StructFieldsMap = BTreeMap<String, Vec<(String, CType, Option<u32>)>>;
pub(super) type UnionFieldsMap = BTreeMap<String, Vec<(String, CType)>>;
pub(super) type LabeledSegment<'a> = (Option<(String, String)>, Vec<&'a Value>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IntKind {
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
    pub(super) fn is_signed(self) -> bool {
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

    pub(super) fn bit_width(self) -> u32 {
        match self {
            Self::Bool | Self::Char | Self::I8 | Self::U8 => 8,
            Self::I16 | Self::U16 => 16,
            Self::I32 | Self::U32 => 32,
            Self::Long | Self::ULong | Self::LongLong | Self::ULongLong => 64,
            Self::I128 | Self::U128 => 128,
        }
    }

    pub(super) fn unsigned_rust_type(self) -> &'static str {
        match self.bit_width() {
            8 => "u8",
            16 => "u16",
            32 => "u32",
            64 => "u64",
            _ => "u128",
        }
    }

    pub(super) fn rust_type(self) -> &'static str {
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
pub(super) enum FloatKind {
    F32,
    F64,
}

impl FloatKind {
    pub(super) fn rust_type(self) -> &'static str {
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
pub(super) enum CType {
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

pub(super) fn rust_field_name(name: &str) -> String {
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
    pub(super) fn same_shape(&self, other: &Self) -> bool {
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

    pub(super) fn contains_raw_ptr(
        &self,
        structs: &StructFieldsMap,
        unions: &UnionFieldsMap,
    ) -> bool {
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

    pub(super) fn fn_type_only(&self) -> Result<String> {
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

    pub(super) fn rust_type(&self) -> String {
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

    pub(super) fn c_type(&self) -> String {
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

    pub(super) fn zero_expr(&self, structs: &StructFieldsMap) -> Result<String> {
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

    pub(super) fn from_node_type(
        type_obj: &Value,
        typedefs: &BTreeMap<String, String>,
    ) -> Result<Self> {
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

    pub(super) fn parse_str(raw: &str, typedefs: &BTreeMap<String, String>) -> Result<Self> {
        Self::parse_str_depth(raw, typedefs, 0)
    }

    pub(super) fn parse_str_depth(
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
