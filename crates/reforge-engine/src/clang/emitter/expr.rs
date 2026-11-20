//! Typed expression emission.
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

use super::Emitter;
use crate::clang::ast::{children, identifier, kind, text};
use crate::clang::decls::{
    eval_constant_int, make_external_function, normalize_bitfield, parse_fn_signature,
};
use crate::clang::literals::format_string_literal_array;
use crate::clang::pointers::{bare_decl_id, unwrap_paren_cast};
use crate::clang::types::{CType, FloatKind, GlobalKind, IntKind, rust_field_name};

impl Emitter<'_> {
    pub(in crate::clang) fn typed_expression(&mut self, node: &Value) -> Result<(String, CType)> {
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
}
