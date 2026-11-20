//! Enum, function-signature, external prototype, and declaration helpers.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::model::ExternalFunction;

use crate::clang::ast::{children, is_benign_attr_or_comment, kind, text};
use crate::clang::type_parse::{
    parse_param_list_with_variadic, split_trailing_parens, strip_trailing_qualifiers,
};
use crate::clang::types::{CType, FloatKind, IntKind};

pub(super) fn collect_enum_decl(
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

pub(super) fn parse_fn_signature_variadic(
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

pub(super) fn parse_fn_signature(
    qual_type: &str,
    typedefs: &BTreeMap<String, String>,
) -> Result<(Vec<CType>, CType)> {
    let (params, ret, is_variadic) = parse_fn_signature_variadic(qual_type, typedefs)?;
    ensure!(!is_variadic, "variadic functions are unsupported");
    Ok((params, ret))
}

pub(super) fn make_external_function(
    name: &str,
    param_types: &[CType],
    ret: &CType,
) -> ExternalFunction {
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

pub(super) fn make_external_variadic_function(
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

pub(super) fn promote_variadic_arg(expr: String, ty: &CType) -> Result<String> {
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

pub(super) fn eval_constant_int(node: &Value) -> Option<i64> {
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

pub(super) fn normalize_bitfield(expr: &str, ik: IntKind, width: u32) -> String {
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

pub(super) fn local_initializer(declaration: &Value) -> Result<Option<&Value>> {
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

pub(super) fn is_scalar_type(ty: &CType) -> bool {
    matches!(
        ty,
        CType::Int(_) | CType::Float(_) | CType::Ptr { .. } | CType::FnPtr { .. }
    )
}
