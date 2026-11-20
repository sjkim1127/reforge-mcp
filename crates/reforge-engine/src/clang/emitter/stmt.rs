//! Statement and labeled compound emission.
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use super::Emitter;
use crate::clang::ast::{children, kind, text};
use crate::clang::cfg::{CfgBuilder, CfgTerm};
use crate::clang::decls::{is_scalar_type, local_initializer};
use crate::clang::switch::{SwitchArm, SwitchLabel, flatten_switch_node};
use crate::clang::types::{CType, LabeledSegment};

impl Emitter<'_> {
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

    pub(in crate::clang) fn statement(&mut self, node: &Value) -> Result<String> {
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

    pub(in crate::clang) fn emit_general_cfg_body(&mut self, body: &Value) -> Result<String> {
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
