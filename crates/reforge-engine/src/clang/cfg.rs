//! General control-flow-graph lowering for bodies that structured emission cannot express.
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use crate::clang::ast::{children, kind, text};
use crate::clang::decls::local_initializer;
use crate::clang::emitter::Emitter;
use crate::clang::types::CType;

pub(super) struct CfgProbeState {
    pos: usize,
    compound_stack: Vec<usize>,
    labels: BTreeMap<String, (usize, Option<usize>)>,
    complex: bool,
}

pub(super) fn check_switch_complexity(node: &Value, in_control: bool) -> bool {
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

pub(super) fn cfg_probe_pass1(
    node: &Value,
    direct_compound: Option<usize>,
    state: &mut CfgProbeState,
) {
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

pub(super) fn cfg_probe_pass2(node: &Value, state: &mut CfgProbeState) -> bool {
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

pub(super) fn needs_general_cfg(body: &Value) -> bool {
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

pub(super) enum CfgTerm {
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

pub(super) struct CfgBlock {
    stmts: Vec<String>,
    pub(super) term: CfgTerm,
}

pub(super) struct CfgSwitch {
    sw_ty: CType,
    cases: Vec<(String, usize)>,
    default_bb: Option<usize>,
}

pub(super) struct CfgBuilder<'a, 'b> {
    pub(super) emitter: &'a mut Emitter<'b>,
    pub(super) blocks: Vec<CfgBlock>,
    pub(super) label_blocks: BTreeMap<String, usize>,
    pub(super) static_or_extern_ids: BTreeSet<String>,
    pub(super) hoisted_items: Vec<String>,
    pub(super) break_targets: Vec<usize>,
    pub(super) continue_targets: Vec<usize>,
    pub(super) switch_stack: Vec<CfgSwitch>,
}

impl<'a, 'b> CfgBuilder<'a, 'b> {
    pub(super) fn new_block(&mut self) -> usize {
        let id = self.blocks.len();
        self.blocks.push(CfgBlock {
            stmts: Vec::new(),
            term: CfgTerm::Unreachable,
        });
        id
    }

    pub(super) fn set_jump(&mut self, bb: usize, target: usize) {
        if matches!(self.blocks[bb].term, CfgTerm::Unreachable) {
            self.blocks[bb].term = CfgTerm::Jump(target);
        }
    }

    pub(super) fn hoist_local_decls(&mut self, node: &Value) -> Result<()> {
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

    pub(super) fn preallocate_labels(&mut self, node: &Value) -> Result<()> {
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

    pub(super) fn lower_stmt(&mut self, node: &Value, cur_bb: usize) -> Result<usize> {
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

    pub(super) fn render(self, entry_bb: usize, exit_bb: usize) -> Result<String> {
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
