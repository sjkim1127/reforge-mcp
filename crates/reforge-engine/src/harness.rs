//! Entry-function signatures and the C/Rust differential drivers built for them.
//!
//! Both drivers receive one decimal argument per C parameter. Integers are parsed
//! at full 64-bit width and narrowed with C cast semantics (two's complement
//! truncation) on both sides; `float`/`double` arguments are parsed with
//! correctly rounded decimal conversion (`strtod` / `str::parse::<f64>`).
use anyhow::{Context, Result, ensure};

use crate::model::{FixtureValue, Function, Job};
use crate::{is_supported_entry_float, is_supported_entry_int};

pub(crate) fn is_int_pointer(ty: &str) -> bool {
    matches!(ty, "int *" | "const int *")
}

/// Parameter and return types of an entry function that the drivers can call.
pub(crate) struct EntrySignature {
    pub param_types: Vec<String>,
    pub return_type: String,
}

impl EntrySignature {
    pub fn from_function(function: &Function) -> Result<Self> {
        let param_types: Vec<String> = if function.parameter_types.is_empty() {
            vec!["int".into(); function.parameters.len()]
        } else {
            function.parameter_types.clone()
        };
        for pty in &param_types {
            ensure!(
                is_supported_entry_int(pty).is_some()
                    || is_supported_entry_float(pty).is_some()
                    || is_int_pointer(pty),
                "verify_port entry parameter type {pty:?} requires a scalar number or int* wrapper entry"
            );
        }
        let return_type = if function.return_type.is_empty() {
            "int".to_owned()
        } else {
            function.return_type.clone()
        };
        ensure!(
            return_type == "void"
                || is_supported_entry_int(&return_type).is_some()
                || is_supported_entry_float(&return_type).is_some(),
            "verify_port entry return type {return_type:?} requires a scalar number or void wrapper entry"
        );
        Ok(Self {
            param_types,
            return_type,
        })
    }

    /// Reject values the drivers cannot pass losslessly to their parameter.
    pub fn validate(&self, case: &[FixtureValue]) -> Result<()> {
        for (i, (ty, value)) in self.param_types.iter().zip(case).enumerate() {
            if let FixtureValue::Float(v) = value {
                ensure!(
                    is_supported_entry_float(ty).is_some(),
                    "fixture argument {i} is {v:?}, but parameter type {ty:?} requires an integer"
                );
                ensure!(
                    v.is_finite(),
                    "fixture argument {i} must be a finite number"
                );
            }
        }
        Ok(())
    }
}

/// `rf_abi_*` forwarding shims so C boundary files can link against the original
/// C implementation instead of the Rust exports.
pub(crate) fn abi_shims(job: &Job, entry: &str) -> String {
    if job.c_boundary_files.is_empty() {
        return String::new();
    }
    let mut shims = String::new();
    for func in &job.functions {
        if func.is_static || func.name == entry {
            continue;
        }
        let ret_ok =
            func.return_type == "void" || is_supported_entry_int(&func.return_type).is_some();
        let params_ok = func
            .parameter_types
            .iter()
            .all(|p| is_supported_entry_int(p).is_some() || is_int_pointer(p));
        if !ret_ok || !params_ok {
            continue;
        }
        let plist = if func.parameter_types.is_empty() {
            "void".to_string()
        } else {
            func.parameter_types
                .iter()
                .enumerate()
                .map(|(i, p)| format!("{p} v{i}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let alist = (0..func.parameter_types.len())
            .map(|i| format!("v{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let call = if func.return_type == "void" {
            format!("{}({alist});", func.name)
        } else {
            format!("return {}({alist});", func.name)
        };
        shims.push_str(&format!(
            "{} {}({plist});\n{} rf_abi_{}({plist}) {{ {call} }}\n",
            func.return_type, func.name, func.return_type, func.name
        ));
    }
    shims
}

pub(crate) fn c_prototype(entry: &str, param_types: &[String], return_type: &str) -> String {
    let args = if param_types.is_empty() {
        "void".to_string()
    } else {
        param_types.join(", ")
    };
    format!("{return_type} {entry}({args});\n")
}

/// Render the original-C driver: parse argv, call the entry, print the result.
pub(crate) fn render_c_driver(
    job: &Job,
    entry: &str,
    param_types: &[String],
    return_type: &str,
) -> Result<String> {
    let count = param_types.len();
    let mut decls = String::new();
    let mut call_args = Vec::with_capacity(count);
    for (i, t) in param_types.iter().enumerate() {
        let arg = format!("argv[{}]", i + 1);
        if is_int_pointer(t) {
            decls.push_str(&format!(
                "int rf_p{i} = (int)rf_si({arg}); int rf_b{i} = rf_p{i}; "
            ));
            call_args.push(format!("&rf_p{i}"));
        } else if is_supported_entry_float(t).is_some() {
            decls.push_str(&format!("{t} rf_a{i} = ({t})strtod({arg}, 0); "));
            call_args.push(format!("rf_a{i}"));
        } else if t == "_Bool" {
            decls.push_str(&format!("_Bool rf_a{i} = (_Bool)(rf_si({arg}) != 0); "));
            call_args.push(format!("rf_a{i}"));
        } else {
            decls.push_str(&format!("{t} rf_a{i} = ({t})rf_si({arg}); "));
            call_args.push(format!("rf_a{i}"));
        }
    }
    let call = format!("{entry}({})", call_args.join(", "));
    let pointer_changes = |separated: bool| {
        let mut out = String::new();
        for (i, t) in param_types.iter().enumerate() {
            if !is_int_pointer(t) {
                continue;
            }
            if separated {
                out.push_str(&format!(
                    " if (rf_p{i} != rf_b{i}) {{ printf(\"%sp{i}=%d\", rf_printed ? \" \" : \"\", rf_p{i}); rf_printed = 1; }}"
                ));
            } else {
                out.push_str(&format!(
                    " if (rf_p{i} != rf_b{i}) printf(\" p{i}=%d\", rf_p{i});"
                ));
            }
        }
        out
    };
    let body = if return_type == "void" {
        format!(
            "{call}; int rf_printed = 0;{} if (!rf_printed) printf(\"void\"); printf(\"\\n\");",
            pointer_changes(true)
        )
    } else if is_supported_entry_float(return_type).is_some() {
        format!(
            "{return_type} rf_ret = {call}; if (rf_ret != rf_ret) printf(\"nan\"); else printf(\"%.6f\", (double)rf_ret);{} printf(\"\\n\");",
            pointer_changes(false)
        )
    } else {
        let (_, ret_unsigned) = is_supported_entry_int(return_type)
            .context("unsupported return type in verify_inner")?;
        let print = if return_type == "int" {
            format!("int rf_ret = {call}; printf(\"%d\", rf_ret);")
        } else if ret_unsigned {
            format!("{return_type} rf_ret = {call}; printf(\"%llu\", (unsigned long long)rf_ret);")
        } else {
            format!("{return_type} rf_ret = {call}; printf(\"%lld\", (long long)rf_ret);")
        };
        format!("{print}{} printf(\"\\n\");", pointer_changes(false))
    };
    Ok(format!(
        "#include <stdio.h>\n#include <stdlib.h>\n#include <limits.h>\n_Static_assert(INT_MAX == 2147483647 && INT_MIN == (-2147483647 - 1), \"requires int32\");\n{}{}static long long rf_si(const char *s) {{ return s[0] == '-' ? strtoll(s, 0, 10) : (long long)strtoull(s, 0, 10); }}\nint main(int argc, char **argv) {{ if (argc != {}) return 2; {decls}{body} return 0; }}\n",
        abi_shims(job, entry),
        c_prototype(entry, param_types, return_type),
        count + 1,
    ))
}

/// Render the Rust-candidate driver with the same argv contract as the C driver.
pub(crate) fn render_rust_driver(
    include_path: &str,
    entry: &str,
    param_types: &[String],
    return_type: &str,
) -> String {
    let count = param_types.len();
    let mut decls = String::new();
    let mut call_args = Vec::with_capacity(count);
    for (i, t) in param_types.iter().enumerate() {
        let arg = format!("&rf_args[{i}]");
        if is_int_pointer(t) {
            decls.push_str(&format!(
                "let mut rf_p{i}: i32 = rf_si({arg}) as i32; let rf_b{i} = rf_p{i}; "
            ));
            call_args.push(format!("&raw mut rf_p{i}"));
        } else if let Some(fty) = is_supported_entry_float(t) {
            decls.push_str(&format!("let rf_a{i} = rf_f({arg}) as {fty}; "));
            call_args.push(format!("rf_a{i}"));
        } else if t == "_Bool" {
            decls.push_str(&format!("let rf_a{i} = u8::from(rf_si({arg}) != 0); "));
            call_args.push(format!("rf_a{i}"));
        } else {
            let (rty, _) = is_supported_entry_int(t).expect("validated entry parameter");
            decls.push_str(&format!("let rf_a{i} = rf_si({arg}) as {rty}; "));
            call_args.push(format!("rf_a{i}"));
        }
    }
    let call = format!("port::rf_{entry}({})", call_args.join(", "));
    let has_ptr_param = param_types.iter().any(|t| is_int_pointer(t));
    let pointer_changes = |separated: bool| {
        let mut out = String::new();
        for (i, t) in param_types.iter().enumerate() {
            if !is_int_pointer(t) {
                continue;
            }
            if separated {
                out.push_str(&format!(
                    " if rf_p{i} != rf_b{i} {{ print!(\"{{}}p{i}={{}}\", if rf_printed {{ \" \" }} else {{ \"\" }}, rf_p{i}); rf_printed = true; }}"
                ));
            } else {
                out.push_str(&format!(
                    " if rf_p{i} != rf_b{i} {{ print!(\" p{i}={{}}\", rf_p{i}); }}"
                ));
            }
        }
        out
    };
    let body = if return_type == "void" {
        format!(
            "{call}; let mut rf_printed = false;{} if !rf_printed {{ print!(\"void\"); }} println!();",
            pointer_changes(true)
        )
    } else if is_supported_entry_float(return_type).is_some() {
        format!(
            "let rf_ret = {call}; if (rf_ret as f64).is_nan() {{ print!(\"nan\"); }} else {{ print!(\"{{:.6}}\", rf_ret as f64); }}{} println!();",
            pointer_changes(false)
        )
    } else if has_ptr_param {
        format!(
            "let rf_ret = {call}; print!(\"{{rf_ret}}\");{} println!();",
            pointer_changes(false)
        )
    } else {
        format!("println!(\"{{}}\", {call});")
    };
    format!(
        "#[path={include_path}] mod port;\n#[allow(dead_code)]\nfn rf_si(s: &str) -> i128 {{ s.parse().unwrap() }}\n#[allow(dead_code)]\nfn rf_f(s: &str) -> f64 {{ s.parse().unwrap() }}\nfn main() {{ let rf_args: Vec<String> = std::env::args().skip(1).collect(); assert_eq!(rf_args.len(), {count}); {decls}{body} }}\n"
    )
}
