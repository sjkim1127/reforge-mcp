//! Flattening of C `switch` bodies into labeled arms.
use anyhow::{Result, ensure};
use serde_json::Value;

use crate::clang::ast::{children, kind};

pub(super) enum SwitchLabel<'a> {
    Case(&'a Value),
    Default,
}

pub(super) struct SwitchArm<'a> {
    pub(super) labels: Vec<SwitchLabel<'a>>,
    pub(super) stmts: Vec<&'a Value>,
}

pub(super) fn flatten_switch_node<'a>(
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
