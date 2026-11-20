//! Pointer parameter usage analysis and safe-wrapper contract inference.
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use crate::model::PointerParamContract;

use crate::clang::ast::{children, kind, text};
use crate::clang::types::CType;

#[derive(Default, Clone)]
pub(super) struct PtrUsage {
    mutated: bool,
    indexed_or_offset: bool,
}

pub(super) fn unwrap_paren_cast(mut node: &Value) -> &Value {
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

pub(super) fn bare_decl_id(node: &Value) -> Option<&str> {
    let inner = unwrap_paren_cast(node);
    (kind(inner) == "DeclRefExpr").then(|| text(&inner["referencedDecl"], "id"))
}

pub(super) fn pointee_decl_id(node: &Value) -> Option<&str> {
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

pub(super) fn collect_ptr_usage(
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

pub(super) fn analyze_pointer_params(
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
