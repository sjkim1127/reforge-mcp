//! Expression/statement emitter state and core variable, lvalue, and initializer handling.
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::model::{ExternalFunction, StructDef};

use crate::clang::TranslateOptions;
use crate::clang::ast::{children, identifier, kind, text};
use crate::clang::decls::{
    collect_enum_decl, is_scalar_type, local_initializer, normalize_bitfield,
};
use crate::clang::literals::format_string_literal_array;
use crate::clang::records::{collect_nested_records, emit_record_entry};
use crate::clang::type_parse::is_const_qualified;
use crate::clang::types::{
    CType, GlobalBinding, GlobalKind, IntKind, StructFieldsMap, UnionFieldsMap, rust_field_name,
};

mod builtins;
mod expr;
mod stmt;

pub(super) struct Emitter<'a> {
    pub(super) variables: BTreeMap<String, (String, CType)>,
    pub(super) uninit_scalars: BTreeSet<String>,
    pub(super) global_id_to_name: Cow<'a, BTreeMap<String, String>>,
    pub(super) global_bindings: Cow<'a, BTreeMap<String, GlobalBinding>>,
    pub(super) referenced_globals: BTreeSet<String>,
    pub(super) unsupported_globals: Cow<'a, BTreeMap<String, String>>,
    pub(super) variadic_prototypes:
        &'a BTreeMap<String, std::result::Result<(Vec<CType>, CType), String>>,
    pub(super) unsupported_prototypes: &'a BTreeMap<String, String>,
    pub(super) dependencies: BTreeSet<String>,
    pub(super) next_variable: usize,
    pub(super) loop_labels: Vec<String>,
    pub(super) break_target: Vec<Option<String>>,
    pub(super) forward_labels: BTreeMap<String, String>,
    pub(super) return_type: CType,
    pub(super) typedefs: Cow<'a, BTreeMap<String, String>>,
    pub(super) enum_constants: Cow<'a, BTreeMap<String, (i64, IntKind)>>,
    pub(super) structs: Cow<'a, StructFieldsMap>,
    pub(super) unions: Cow<'a, UnionFieldsMap>,
    pub(super) structs_out: &'a mut Vec<StructDef>,
    pub(super) seen_records: &'a mut BTreeSet<String>,
    pub(super) anon_counter: &'a mut usize,
    pub(super) file_sources: &'a mut BTreeMap<String, String>,
    pub(super) current_rel_file: &'a str,
    pub(super) current_physical_file: &'a Path,
    pub(super) current_clang_file: &'a str,
    pub(super) current_source: &'a str,
    pub(super) prototypes: &'a mut BTreeMap<String, ExternalFunction>,
    pub(super) options: TranslateOptions,
}

impl Emitter<'_> {
    pub(super) fn fresh_var(&mut self, prefix: &str) -> String {
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

    pub(super) fn handle_non_var_local_decl(&mut self, declaration: &Value) -> Result<bool> {
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

    pub(super) fn try_static_or_extern_local(
        &mut self,
        declaration: &Value,
    ) -> Result<Option<String>> {
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

    pub(super) fn condition(&mut self, node: &Value) -> Result<String> {
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
}
