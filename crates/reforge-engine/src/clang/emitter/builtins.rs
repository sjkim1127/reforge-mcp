//! Compiler builtin, overflow builtin, and variadic call emission.
use anyhow::{Result, bail, ensure};
use serde_json::Value;

use super::Emitter;
use crate::clang::decls::{
    make_external_variadic_function, parse_fn_signature, parse_fn_signature_variadic,
    promote_variadic_arg,
};
use crate::clang::types::{CType, IntKind};

impl Emitter<'_> {
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

    pub(super) fn emit_variadic_call(
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

    pub(super) fn try_emit_builtin(
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
}
