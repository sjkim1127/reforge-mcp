//! Struct/union record collection and Rust `#[repr(C)]` emission.
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::model::{StructDef, StructField};

use crate::clang::ast::{
    children, file_from_loc, identifier, is_benign_attr_or_comment, kind, line_from_offset,
    offset_to_line_col, text,
};
use crate::clang::decls::{collect_enum_decl, eval_constant_int};
use crate::clang::types::{CType, IntKind, StructFieldsMap, UnionFieldsMap, rust_field_name};

pub(super) struct RecordEntry<'a> {
    pub(super) rel_file: String,
    pub(super) node: &'a Value,
    pub(super) name: String,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn collect_nested_records<'a>(
    node: &'a Value,
    rel_file: &str,
    physical_file: &Path,
    clang_file: &str,
    anon_typedefs: &BTreeMap<String, String>,
    anon_counter: &mut usize,
    typedefs: &mut BTreeMap<String, String>,
    file_sources: &mut BTreeMap<String, String>,
    out: &mut Vec<RecordEntry<'a>>,
) -> Result<()> {
    let mut cur_clang_file = clang_file.to_owned();
    if let Some(f) = file_from_loc(&node["loc"]) {
        cur_clang_file = f.to_owned();
    }
    for child in children(node) {
        if kind(child) == "RecordDecl" && child["completeDefinition"].as_bool() == Some(true) {
            collect_nested_records(
                child,
                rel_file,
                physical_file,
                &cur_clang_file,
                anon_typedefs,
                anon_counter,
                typedefs,
                file_sources,
                out,
            )?;
        }
    }
    let tag = text(node, "tagUsed");
    let raw_name = text(node, "name");
    let name = if !raw_name.is_empty() {
        raw_name.to_owned()
    } else if let Some(td_name) = anon_typedefs.get(text(node, "id")) {
        td_name.clone()
    } else {
        let synth = format!("anon_{}", *anon_counter);
        *anon_counter += 1;
        synth
    };
    if raw_name.is_empty() && matches!(tag, "struct" | "union") {
        if !file_sources.contains_key(rel_file)
            && let Ok(src) = std::fs::read_to_string(physical_file)
        {
            file_sources.insert(rel_file.to_owned(), src);
        }
        if let Some(source) = file_sources.get(rel_file) {
            let begin = if node["range"]["begin"].is_object() {
                &node["range"]["begin"]
            } else {
                &node["loc"]
            };
            if let Ok((line, col)) = offset_to_line_col(source, begin) {
                let target = format!("{tag} {name}");
                typedefs.insert(
                    format!("{tag} @anon:{cur_clang_file}:{line}:{col}"),
                    target.clone(),
                );
                typedefs.insert(
                    format!("{tag} @anon:{}:{line}:{col}", physical_file.display()),
                    target.clone(),
                );
                let sp = begin
                    .get("spellingLoc")
                    .or_else(|| node["loc"].get("spellingLoc"));
                if let Some(sp) = sp
                    && let (Some(sl), Some(sc)) = (sp["line"].as_u64(), sp["col"].as_u64())
                {
                    let sf = sp["file"].as_str().unwrap_or(&cur_clang_file);
                    typedefs.insert(format!("{tag} @anon:{sf}:{sl}:{sc}"), target.clone());
                    if let Ok(canon_sf) = Path::new(sf).canonicalize() {
                        typedefs.insert(
                            format!("{tag} @anon:{}:{sl}:{sc}", canon_sf.display()),
                            target,
                        );
                    }
                }
            }
        }
    }
    out.push(RecordEntry {
        rel_file: rel_file.to_owned(),
        node,
        name,
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_record_entry(
    node: &Value,
    mut name: String,
    rel_file: String,
    source: &str,
    rename_on_collision: bool,
    anon_counter: &mut usize,
    typedefs: &mut BTreeMap<String, String>,
    enum_constants: &mut BTreeMap<String, (i64, IntKind)>,
    structs_map: &mut StructFieldsMap,
    unions_map: &mut UnionFieldsMap,
    seen_records: &mut BTreeSet<String>,
    structs_out: &mut Vec<StructDef>,
) -> Result<()> {
    let tag = text(node, "tagUsed");
    ensure!(
        matches!(tag, "struct" | "union"),
        "unsupported record tag {tag:?}; only struct and union are supported"
    );
    if node["completeDefinition"].as_bool() != Some(true) {
        return Ok(());
    }
    identifier(&name)?;
    let key = format!("{tag}:{name}");
    if !seen_records.insert(key) {
        if !rename_on_collision {
            return Ok(());
        }
        let orig = name.clone();
        name = format!("{orig}_local_{}", *anon_counter);
        *anon_counter += 1;
        seen_records.insert(format!("{tag}:{name}"));
        typedefs.insert(format!("{tag} {orig}"), format!("{tag} {name}"));
    }

    if tag == "struct" {
        let mut fields_typed = Vec::new();
        let mut fields_model = Vec::new();
        let mut field_lines = Vec::new();
        let mut default_lines = Vec::new();
        let mut field_idx = 0usize;

        for child in children(node) {
            let ck = kind(child);
            if matches!(ck, "RecordDecl" | "IndirectFieldDecl") || is_benign_attr_or_comment(ck) {
                continue;
            }
            if ck == "EnumDecl" {
                collect_enum_decl(child, typedefs, enum_constants);
                continue;
            }
            ensure!(
                ck == "FieldDecl",
                "unsupported declaration {ck} inside struct {name}"
            );
            let raw_fname = text(child, "name");
            if raw_fname.is_empty() && child["isBitfield"].as_bool() == Some(true) {
                continue;
            }
            let fname = if raw_fname.is_empty() {
                let f = format!("rf_anon_{field_idx}");
                field_idx += 1;
                f
            } else {
                identifier(raw_fname)?;
                field_idx += 1;
                raw_fname.to_owned()
            };
            let fty = CType::from_node_type(&child["type"], typedefs)?;
            ensure!(
                !matches!(fty, CType::Void),
                "void field in struct {name} is unsupported"
            );
            let bitwidth = if child["isBitfield"].as_bool() == Some(true) {
                let CType::Int(ik) = fty else {
                    bail!("bitfield {name}.{fname} must have integer type");
                };
                let w = children(child)
                    .iter()
                    .filter(|c| !is_benign_attr_or_comment(kind(c)))
                    .find_map(eval_constant_int)
                    .with_context(|| format!("missing bitfield width on {name}.{fname}"))?;
                ensure!(
                    w >= 1 && w <= ik.bit_width() as i64,
                    "bitfield width {w} out of range on {name}.{fname}"
                );
                Some(w as u32)
            } else {
                None
            };
            let rty = fty.rust_type();
            let rf_fname = rust_field_name(&fname);
            field_lines.push(format!("    pub {rf_fname}: {rty},"));
            fields_model.push(StructField {
                name: fname.clone(),
                c_type: fty.c_type(),
                rust_type: rty,
            });
            fields_typed.push((fname, fty, bitwidth));
        }
        structs_map.insert(name.clone(), fields_typed);
        for (fname, fty, _) in &structs_map[&name] {
            let z = fty.zero_expr(structs_map)?;
            let rf_fname = rust_field_name(fname);
            default_lines.push(format!("            {rf_fname}: {z},"));
        }
        let rust_name = format!("RfStruct_{name}");
        let rust = format!(
            "#[repr(C)]\n#[derive(Clone, Copy, Debug)]\npub struct {rust_name} {{\n{}\n}}\nimpl Default for {rust_name} {{\n    fn default() -> Self {{\n        unsafe {{\n            Self {{\n{}\n            }}\n        }}\n    }}\n}}\n",
            field_lines.join("\n"),
            default_lines.join("\n"),
        );
        structs_out.push(StructDef {
            name,
            rust_name,
            fields: fields_model,
            source_file: rel_file,
            line_start: line_from_offset(source, &node["range"]["begin"])?,
            line_end: line_from_offset(source, &node["range"]["end"])?,
            rust,
        });
    } else {
        // union
        let mut fields_typed = Vec::new();
        let mut fields_model = Vec::new();
        let mut field_lines = Vec::new();
        let mut field_idx = 0usize;

        for child in children(node) {
            let ck = kind(child);
            if matches!(ck, "RecordDecl" | "IndirectFieldDecl") || is_benign_attr_or_comment(ck) {
                continue;
            }
            if ck == "EnumDecl" {
                collect_enum_decl(child, typedefs, enum_constants);
                continue;
            }
            ensure!(
                ck == "FieldDecl",
                "unsupported declaration {ck} inside union {name}"
            );
            let raw_fname = text(child, "name");
            if raw_fname.is_empty() && child["isBitfield"].as_bool() == Some(true) {
                continue;
            }
            ensure!(
                child["isBitfield"].as_bool() != Some(true),
                "bitfield inside union {name} is unsupported"
            );
            let fname = if raw_fname.is_empty() {
                let f = format!("rf_anon_{field_idx}");
                field_idx += 1;
                f
            } else {
                identifier(raw_fname)?;
                field_idx += 1;
                raw_fname.to_owned()
            };
            let fty = CType::from_node_type(&child["type"], typedefs)?;
            ensure!(
                !matches!(fty, CType::Void),
                "void field in union {name} is unsupported"
            );
            let rty = fty.rust_type();
            let rf_fname = rust_field_name(&fname);
            field_lines.push(format!("    pub {rf_fname}: {rty},"));
            fields_model.push(StructField {
                name: fname.clone(),
                c_type: fty.c_type(),
                rust_type: rty,
            });
            fields_typed.push((fname, fty));
        }
        unions_map.insert(name.clone(), fields_typed);
        let rust_name = format!("RfUnion_{name}");
        let rust = if fields_model.is_empty() {
            format!(
                "#[repr(C)]\n#[derive(Clone, Copy, Debug, Default)]\npub struct {rust_name} {{\n    _private: [u8; 0],\n}}\n"
            )
        } else {
            format!(
                "#[repr(C)]\n#[derive(Clone, Copy)]\npub union {rust_name} {{\n{}\n}}\nimpl Default for {rust_name} {{\n    fn default() -> Self {{\n        unsafe {{ core::mem::zeroed() }}\n    }}\n}}\nimpl core::fmt::Debug for {rust_name} {{\n    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {{\n        f.write_str(\"{rust_name}\")\n    }}\n}}\n",
                field_lines.join("\n"),
            )
        };
        structs_out.push(StructDef {
            name: format!("union:{name}"),
            rust_name,
            fields: fields_model,
            source_file: rel_file,
            line_start: line_from_offset(source, &node["range"]["begin"])?,
            line_end: line_from_offset(source, &node["range"]["end"])?,
            rust,
        });
    }
    Ok(())
}
