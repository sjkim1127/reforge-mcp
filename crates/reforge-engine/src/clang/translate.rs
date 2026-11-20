//! Translation-unit driver: collects declarations and emits Rust functions.
use anyhow::{Result, bail, ensure};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::model::{ExternalFunction, Function, StructDef};

use crate::clang::ast::{
    children, file_from_loc, identifier, is_benign_attr_or_comment, kind, line_from_offset, text,
};
use crate::clang::cfg::needs_general_cfg;
use crate::clang::decls::{
    collect_enum_decl, make_external_function, parse_fn_signature, parse_fn_signature_variadic,
};
use crate::clang::emitter::Emitter;
use crate::clang::pointers::analyze_pointer_params;
use crate::clang::records::{RecordEntry, collect_nested_records, emit_record_entry};
use crate::clang::type_parse::is_const_qualified;
use crate::clang::types::{CType, GlobalBinding, GlobalKind, IntKind};
use crate::clang::{TranslateOptions, TranslationUnitOutput};

pub fn translate(
    ast: &Value,
    translation_unit: &Path,
    root: &Path,
) -> Result<TranslationUnitOutput> {
    translate_with_options(ast, translation_unit, root, TranslateOptions::strict())
}

pub fn translate_with_options(
    ast: &Value,
    translation_unit: &Path,
    root: &Path,
    options: TranslateOptions,
) -> Result<TranslationUnitOutput> {
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let root = root_canon.as_path();
    ensure!(
        kind(ast) == "TranslationUnitDecl",
        "unexpected Clang AST root"
    );

    // Pass 1: track file context, typedefs, enums, structs, and unions.
    let mut typedefs: BTreeMap<String, String> = BTreeMap::new();
    let mut anon_typedefs: BTreeMap<String, String> = BTreeMap::new();
    let mut anon_tags: BTreeMap<String, String> = BTreeMap::new();
    let mut enum_constants: BTreeMap<String, (i64, IntKind)> = BTreeMap::new();
    let mut top_records: Vec<(String, std::path::PathBuf, String, &Value)> = Vec::new();
    let mut file_sources: BTreeMap<String, String> = BTreeMap::new();

    let mut current_file = translation_unit.to_path_buf();
    for node in children(ast) {
        if node["isImplicit"].as_bool() == Some(true) {
            continue;
        }
        if let Some(file) = file_from_loc(&node["loc"]) {
            current_file = Path::new(file).to_path_buf();
        }
        let Ok(physical_file) = current_file.canonicalize() else {
            continue;
        };
        let in_project = physical_file.starts_with(root);

        match kind(node) {
            "RecordDecl" => {
                if text(node, "name").is_empty() {
                    anon_tags.insert(
                        text(node, "id").to_owned(),
                        text(node, "tagUsed").to_owned(),
                    );
                }
                if in_project
                    && let Some(rel) = physical_file
                        .strip_prefix(root)
                        .ok()
                        .and_then(|p| p.to_str())
                {
                    top_records.push((
                        rel.to_owned(),
                        physical_file.clone(),
                        current_file.to_string_lossy().into_owned(),
                        node,
                    ));
                }
            }
            "EnumDecl" => {
                collect_enum_decl(node, &typedefs, &mut enum_constants);
            }
            "TypedefDecl" => {
                let td_name = text(node, "name");
                if td_name.is_empty() {
                    continue;
                }
                let inner = children(node);
                let mut ty_node = inner.first();
                while let Some(cur) = ty_node
                    && kind(cur) == "ElaboratedType"
                {
                    ty_node = children(cur).first();
                }
                if let Some(rec_ty) = ty_node
                    && kind(rec_ty) == "RecordType"
                    && text(&rec_ty["decl"], "name").is_empty()
                {
                    let decl_id = text(&rec_ty["decl"], "id");
                    if !decl_id.is_empty() {
                        let tag = anon_tags
                            .get(decl_id)
                            .map(String::as_str)
                            .unwrap_or("struct");
                        anon_typedefs.insert(decl_id.to_owned(), td_name.to_owned());
                        typedefs.insert(td_name.to_owned(), format!("{tag} {td_name}"));
                        continue;
                    }
                }
                let underlying = text(&node["type"], "qualType");
                if !underlying.is_empty()
                    && !underlying.contains("unnamed")
                    && !underlying.contains("anonymous")
                {
                    typedefs.insert(td_name.to_owned(), underlying.to_owned());
                }
            }
            _ => {}
        }
    }

    let mut record_nodes: Vec<RecordEntry<'_>> = Vec::new();
    let mut anon_counter = 0usize;
    for (rel_file, physical_file, clang_file, node) in top_records {
        if node["completeDefinition"].as_bool() != Some(true) {
            continue;
        }
        collect_nested_records(
            node,
            &rel_file,
            &physical_file,
            &clang_file,
            &anon_typedefs,
            &mut anon_counter,
            &mut typedefs,
            &mut file_sources,
            &mut record_nodes,
        )?;
    }

    let mut structs_map: BTreeMap<String, Vec<(String, CType, Option<u32>)>> = BTreeMap::new();
    let mut unions_map: BTreeMap<String, Vec<(String, CType)>> = BTreeMap::new();
    let mut structs = Vec::new();
    let mut seen_records = BTreeSet::new();

    for RecordEntry {
        rel_file,
        node,
        name,
    } in record_nodes
    {
        let physical_file = root.join(&rel_file);
        if !file_sources.contains_key(&rel_file) {
            file_sources.insert(rel_file.clone(), std::fs::read_to_string(&physical_file)?);
        }
        let source = file_sources[&rel_file].clone();
        emit_record_entry(
            node,
            name,
            rel_file,
            &source,
            false,
            &mut anon_counter,
            &mut typedefs,
            &mut enum_constants,
            &mut structs_map,
            &mut unions_map,
            &mut seen_records,
            &mut structs,
        )?;
    }

    // Pass 2: collect globals, translate project functions, and collect prototypes.
    let mut constants: Vec<String> = Vec::new();
    let mut global_defs_rust: Vec<Option<String>> = Vec::new();
    let mut defined_global_slots: BTreeMap<String, (usize, bool)> = BTreeMap::new();
    let mut extern_globals: BTreeMap<String, (String, CType)> = BTreeMap::new();
    let mut global_bindings: BTreeMap<String, GlobalBinding> = BTreeMap::new();
    let mut global_id_to_name: BTreeMap<String, String> = BTreeMap::new();
    let mut referenced_globals: BTreeSet<String> = BTreeSet::new();
    let mut used_fn_deps: BTreeSet<String> = BTreeSet::new();
    let mut unsupported_globals: BTreeMap<String, String> = BTreeMap::new();
    let mut variadic_prototypes: BTreeMap<
        String,
        std::result::Result<(Vec<CType>, CType), String>,
    > = BTreeMap::new();
    let mut unsupported_prototypes: BTreeMap<String, String> = BTreeMap::new();
    let mut functions = Vec::new();
    let mut prototypes: BTreeMap<String, ExternalFunction> = BTreeMap::new();
    let mut current_file = translation_unit.to_path_buf();

    for node in children(ast) {
        if node["isImplicit"].as_bool() == Some(true) {
            continue;
        }
        if let Some(file) = file_from_loc(&node["loc"]) {
            current_file = Path::new(file).to_path_buf();
        }
        let physical_file = current_file.canonicalize()?;
        let in_project = physical_file.starts_with(root);

        if !in_project {
            match kind(node) {
                "FunctionDecl" if !children(node).iter().any(|n| kind(n) == "CompoundStmt") => {
                    let name = text(node, "name");
                    if identifier(name).is_ok() {
                        if node["variadic"].as_bool().unwrap_or(false) {
                            let parsed = parse_fn_signature_variadic(
                                text(&node["type"], "qualType"),
                                &typedefs,
                            )
                            .map(|(p, r, _)| (p, r))
                            .map_err(|e| e.to_string());
                            variadic_prototypes.insert(name.to_owned(), parsed);
                        } else {
                            match parse_fn_signature(text(&node["type"], "qualType"), &typedefs) {
                                Ok((param_types, ret_type)) => {
                                    variadic_prototypes.remove(name);
                                    unsupported_prototypes.remove(name);
                                    prototypes.entry(name.to_owned()).or_insert_with(|| {
                                        make_external_function(name, &param_types, &ret_type)
                                    });
                                }
                                Err(err) => {
                                    unsupported_prototypes
                                        .entry(name.to_owned())
                                        .or_insert_with(|| err.to_string());
                                }
                            }
                        }
                    }
                }
                "VarDecl" if options.allow_mutable_globals => {
                    let gname = text(node, "name");
                    if text(node, "storageClass") == "extern"
                        && identifier(gname).is_ok()
                        && let Ok(gty) = CType::from_node_type(&node["type"], &typedefs)
                        && !matches!(gty, CType::Void)
                    {
                        global_id_to_name.insert(text(node, "id").to_owned(), gname.to_owned());
                        if !defined_global_slots.contains_key(gname) {
                            let rust_name = format!("RF_GLOBAL_{gname}");
                            extern_globals
                                .entry(gname.to_owned())
                                .or_insert_with(|| (rust_name.clone(), gty.clone()));
                            global_bindings
                                .entry(gname.to_owned())
                                .or_insert(GlobalBinding {
                                    rust_name,
                                    c_name: gname.to_owned(),
                                    ty: gty,
                                    kind: GlobalKind::StaticMut,
                                });
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        let file = physical_file
            .strip_prefix(root)?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF8 source path"))?;
        match kind(node) {
            "RecordDecl" | "TypedefDecl" | "EnumDecl" | "StaticAssertDecl" | "EmptyDecl" => {
                continue;
            }
            "VarDecl" => {
                let gname = text(node, "name");
                identifier(gname)?;
                let id = text(node, "id").to_owned();
                let value_children: Vec<_> = children(node)
                    .iter()
                    .filter(|c| !is_benign_attr_or_comment(kind(c)))
                    .collect();
                let storage_class = text(node, "storageClass");
                let is_const = is_const_qualified(&node["type"]);
                if storage_class == "extern" && value_children.is_empty() {
                    if options.allow_mutable_globals {
                        if let Ok(gty) = CType::from_node_type(&node["type"], &typedefs)
                            && !matches!(gty, CType::Void)
                        {
                            global_id_to_name.insert(id, gname.to_owned());
                            if !defined_global_slots.contains_key(gname) {
                                let rust_name = format!("RF_GLOBAL_{gname}");
                                extern_globals
                                    .entry(gname.to_owned())
                                    .or_insert_with(|| (rust_name.clone(), gty.clone()));
                                global_bindings
                                    .entry(gname.to_owned())
                                    .or_insert(GlobalBinding {
                                        rust_name,
                                        c_name: gname.to_owned(),
                                        ty: gty,
                                        kind: GlobalKind::StaticMut,
                                    });
                            }
                        } else {
                            unsupported_globals
                                .insert(id, format!("external global {gname} is unsupported"));
                        }
                    } else if is_const
                        && let Ok(gty) = CType::from_node_type(&node["type"], &typedefs)
                        && !matches!(gty, CType::Void | CType::Array(_, 0))
                    {
                        let rust_name = format!("RF_EXTERN_{gname}");
                        let rty = gty.rust_type();
                        constants.push(format!(
                            "unsafe extern \"C\" {{\n    #[link_name = \"{gname}\"]\n    pub static {rust_name}: {rty};\n}}\n"
                        ));
                        global_id_to_name.insert(id, gname.to_owned());
                        global_bindings.insert(
                            gname.to_owned(),
                            GlobalBinding {
                                rust_name,
                                c_name: gname.to_owned(),
                                ty: gty,
                                kind: GlobalKind::ConstItem,
                            },
                        );
                    } else {
                        unsupported_globals
                            .insert(id, format!("external global {gname} is unsupported"));
                    }
                    continue;
                }
                if !options.allow_mutable_globals {
                    ensure!(is_const, "mutable global variable {gname} is unsupported");
                    ensure!(
                        value_children.len() == 1,
                        "const global {gname} must have an initializer"
                    );
                } else {
                    ensure!(
                        value_children.len() <= 1,
                        "global {gname} must have zero or one initializer"
                    );
                }
                let gty = CType::from_node_type(&node["type"], &typedefs)?;
                let is_tentative = value_children.is_empty();
                let init_expr = if let Some(init_node) = value_children.first() {
                    let cur_clang_str = current_file.to_string_lossy().into_owned();
                    let mut temp_emitter = Emitter {
                        variables: BTreeMap::new(),
                        uninit_scalars: BTreeSet::new(),
                        global_id_to_name: Cow::Borrowed(&global_id_to_name),
                        global_bindings: Cow::Borrowed(&global_bindings),
                        referenced_globals: BTreeSet::new(),
                        unsupported_globals: Cow::Borrowed(&unsupported_globals),
                        variadic_prototypes: &variadic_prototypes,
                        unsupported_prototypes: &unsupported_prototypes,
                        dependencies: BTreeSet::new(),
                        next_variable: 0,
                        loop_labels: Vec::new(),
                        break_target: Vec::new(),
                        forward_labels: BTreeMap::new(),
                        return_type: CType::Void,
                        typedefs: Cow::Borrowed(&typedefs),
                        enum_constants: Cow::Borrowed(&enum_constants),
                        structs: Cow::Borrowed(&structs_map),
                        unions: Cow::Borrowed(&unions_map),
                        structs_out: &mut structs,
                        seen_records: &mut seen_records,
                        anon_counter: &mut anon_counter,
                        file_sources: &mut file_sources,
                        current_rel_file: file,
                        current_physical_file: &physical_file,
                        current_clang_file: &cur_clang_str,
                        current_source: "",
                        prototypes: &mut prototypes,
                        options,
                    };
                    let (expr, init_ty) = temp_emitter.typed_expression(init_node)?;
                    ensure!(
                        gty.same_shape(&init_ty),
                        "global {gname} initializer type mismatch"
                    );
                    referenced_globals.extend(temp_emitter.referenced_globals);
                    used_fn_deps.extend(temp_emitter.dependencies);
                    expr
                } else {
                    gty.zero_expr(&structs_map)?
                };
                let has_raw_ptr = gty.contains_raw_ptr(&structs_map, &unions_map);
                let export_attr = if options.export_c_symbols && storage_class != "static" {
                    format!("#[unsafe(export_name = \"{gname}\")]\n")
                } else {
                    String::new()
                };
                let (rust_name, kind_out, decl_str) = if !is_const || is_tentative {
                    let rname = format!("RF_GLOBAL_{gname}");
                    let rty = gty.rust_type();
                    let vis = if storage_class == "static" {
                        "static mut"
                    } else {
                        "pub static mut"
                    };
                    (
                        rname.clone(),
                        GlobalKind::StaticMut,
                        format!("{export_attr}{vis} {rname}: {rty} = unsafe {{ {init_expr} }};\n"),
                    )
                } else if !has_raw_ptr {
                    let rname = format!("RF_CONST_{gname}");
                    let rty = gty.rust_type();
                    let vis = if storage_class == "static" && options.export_c_symbols {
                        "static"
                    } else {
                        "pub static"
                    };
                    (
                        rname.clone(),
                        GlobalKind::StaticConst,
                        format!("{export_attr}{vis} {rname}: {rty} = unsafe {{ {init_expr} }};\n"),
                    )
                } else if options.export_c_symbols && storage_class != "static" {
                    let rname = format!("RF_CONST_{gname}");
                    let rty = gty.rust_type();
                    (
                        rname.clone(),
                        GlobalKind::StaticMut,
                        format!(
                            "{export_attr}pub static mut {rname}: {rty} = unsafe {{ {init_expr} }};\n"
                        ),
                    )
                } else {
                    let rname = format!("RF_CONST_{gname}");
                    let rty = gty.rust_type();
                    (
                        rname.clone(),
                        GlobalKind::ConstItem,
                        format!("pub const {rname}: {rty} = {init_expr};\n"),
                    )
                };

                global_id_to_name.insert(id, gname.to_owned());
                extern_globals.remove(gname);
                if let Some((slot, prev_tentative)) = defined_global_slots.get_mut(gname) {
                    if *prev_tentative || !is_tentative {
                        *prev_tentative = is_tentative;
                        global_defs_rust[*slot] = Some(decl_str);
                        global_bindings.insert(
                            gname.to_owned(),
                            GlobalBinding {
                                rust_name,
                                c_name: gname.to_owned(),
                                ty: gty,
                                kind: kind_out,
                            },
                        );
                    }
                } else {
                    let slot = global_defs_rust.len();
                    global_defs_rust.push(Some(decl_str));
                    defined_global_slots.insert(gname.to_owned(), (slot, is_tentative));
                    global_bindings.insert(
                        gname.to_owned(),
                        GlobalBinding {
                            rust_name,
                            c_name: gname.to_owned(),
                            ty: gty,
                            kind: kind_out,
                        },
                    );
                }
                continue;
            }
            "FunctionDecl" => {}
            other => bail!("unsupported top-level {other}"),
        }

        let name = text(node, "name");
        identifier(name)?;
        let storage_class = text(node, "storageClass");
        ensure!(
            matches!(storage_class, "" | "static" | "extern"),
            "unsupported function storage class"
        );
        let is_static = storage_class == "static";
        let body = children(node).iter().find(|n| kind(n) == "CompoundStmt");

        if body.is_none() {
            if node["variadic"].as_bool().unwrap_or(false) {
                let parsed =
                    parse_fn_signature_variadic(text(&node["type"], "qualType"), &typedefs)
                        .map(|(p, r, _)| (p, r))
                        .map_err(|e| e.to_string());
                variadic_prototypes.insert(name.to_owned(), parsed);
                continue;
            }
            let parse_proto = || -> Result<(Vec<CType>, CType)> {
                let params: Vec<_> = children(node)
                    .iter()
                    .filter(|n| kind(n) == "ParmVarDecl")
                    .collect();
                let mut param_ctypes = Vec::with_capacity(params.len());
                for param in &params {
                    let pty = CType::from_node_type(&param["type"], &typedefs)?;
                    ensure!(
                        !matches!(pty, CType::Void),
                        "void parameter type unsupported"
                    );
                    param_ctypes.push(pty);
                }
                let (sig_params, return_ctype) =
                    parse_fn_signature(text(&node["type"], "qualType"), &typedefs)?;
                ensure!(
                    sig_params.len() == param_ctypes.len()
                        && sig_params
                            .iter()
                            .zip(&param_ctypes)
                            .all(|(a, b)| a.same_shape(b)),
                    "unsupported function type {}",
                    text(&node["type"], "qualType")
                );
                Ok((param_ctypes, return_ctype))
            };
            match parse_proto() {
                Ok((param_ctypes, return_ctype)) => {
                    variadic_prototypes.remove(name);
                    unsupported_prototypes.remove(name);
                    prototypes.entry(name.to_owned()).or_insert_with(|| {
                        make_external_function(name, &param_ctypes, &return_ctype)
                    });
                }
                Err(err) => {
                    unsupported_prototypes
                        .entry(name.to_owned())
                        .or_insert_with(|| err.to_string());
                }
            }
            continue;
        }

        ensure!(
            !node["variadic"].as_bool().unwrap_or(false),
            "variadic functions are unsupported"
        );
        let params: Vec<_> = children(node)
            .iter()
            .filter(|n| kind(n) == "ParmVarDecl")
            .collect();
        let mut param_ctypes = Vec::with_capacity(params.len());
        for param in &params {
            let pty = CType::from_node_type(&param["type"], &typedefs)?;
            ensure!(
                !matches!(pty, CType::Void),
                "void parameter type unsupported"
            );
            param_ctypes.push(pty);
        }
        let (sig_params, return_ctype) =
            parse_fn_signature(text(&node["type"], "qualType"), &typedefs)?;
        ensure!(
            sig_params.len() == param_ctypes.len()
                && sig_params
                    .iter()
                    .zip(&param_ctypes)
                    .all(|(a, b)| a.same_shape(b)),
            "unsupported function type {}",
            text(&node["type"], "qualType")
        );

        let body = body.expect("checked above");
        if !options.allow_main {
            ensure!(
                name != "main",
                "main functions are not supported; select a library entry function"
            );
        }
        ensure!(
            children(node)
                .iter()
                .all(|n| matches!(kind(n), "ParmVarDecl" | "CompoundStmt")
                    || is_benign_attr_or_comment(kind(n))),
            "function attributes or other declarations are unsupported"
        );

        let source = if let Some(s) = file_sources.get(file) {
            s.clone()
        } else {
            let s = std::fs::read_to_string(&physical_file)?;
            file_sources.insert(file.to_owned(), s.clone());
            s
        };
        let cur_clang_str = current_file.to_string_lossy().into_owned();
        let mut emitter = Emitter {
            variables: BTreeMap::new(),
            uninit_scalars: BTreeSet::new(),
            global_id_to_name: Cow::Borrowed(&global_id_to_name),
            global_bindings: Cow::Borrowed(&global_bindings),
            referenced_globals: BTreeSet::new(),
            unsupported_globals: Cow::Borrowed(&unsupported_globals),
            variadic_prototypes: &variadic_prototypes,
            unsupported_prototypes: &unsupported_prototypes,
            dependencies: BTreeSet::new(),
            next_variable: 0,
            loop_labels: Vec::new(),
            break_target: Vec::new(),
            forward_labels: BTreeMap::new(),
            return_type: return_ctype.clone(),
            typedefs: Cow::Borrowed(&typedefs),
            enum_constants: Cow::Borrowed(&enum_constants),
            structs: Cow::Borrowed(&structs_map),
            unions: Cow::Borrowed(&unions_map),
            structs_out: &mut structs,
            seen_records: &mut seen_records,
            anon_counter: &mut anon_counter,
            file_sources: &mut file_sources,
            current_rel_file: file,
            current_physical_file: &physical_file,
            current_clang_file: &cur_clang_str,
            current_source: &source,
            prototypes: &mut prototypes,
            options,
        };
        let mut parameters = Vec::new();
        let mut signature = Vec::new();
        let mut abi_signature = Vec::new();
        let mut abi_args = Vec::new();
        for (i, (param, pty)) in params.iter().zip(param_ctypes.iter()).enumerate() {
            let original = text(param, "name");
            ensure!(!original.is_empty(), "unnamed definition parameter");
            parameters.push(original.to_string());
            let generated = format!("v{i}");
            emitter.variables.insert(
                text(param, "id").to_owned(),
                (generated.clone(), pty.clone()),
            );
            signature.push(format!("mut {generated}: {}", pty.rust_type()));
            abi_signature.push(format!("{generated}: {}", pty.rust_type()));
            abi_args.push(generated);
        }
        emitter.next_variable = params.len();
        let body_code = if options.allow_general_cfg && needs_general_cfg(body) {
            emitter.emit_general_cfg_body(body)?
        } else {
            emitter.statement(body)?
        };
        referenced_globals.extend(emitter.referenced_globals);
        used_fn_deps.extend(emitter.dependencies.iter().cloned());
        let (pointer_contracts, safe_rust_wrapper) = analyze_pointer_params(
            name,
            body,
            &params,
            &param_ctypes,
            &parameters,
            &return_ctype,
        );
        let abi_export = if is_static
            || (options.allow_main && name == "main" && options.export_c_symbols)
        {
            String::new()
        } else {
            let ret_suffix = if matches!(return_ctype, CType::Void) {
                String::new()
            } else {
                format!(" -> {}", return_ctype.rust_type())
            };
            let attr = if options.export_c_symbols {
                format!("#[unsafe(export_name = \"{name}\")]")
            } else {
                "#[unsafe(no_mangle)]".to_owned()
            };
            format!(
                "{attr}\npub extern \"C\" fn rf_abi_{name}({}){ret_suffix} {{\n    rf_{name}({})\n}}\n",
                abi_signature.join(", "),
                abi_args.join(", "),
            )
        };
        let safe_export = safe_rust_wrapper.as_deref().unwrap_or("");
        let rust = if matches!(return_ctype, CType::Void) {
            format!(
                "pub extern \"C\" fn rf_{name}({}) {{\nunsafe {{\n{body_code}\n}}\n}}\n{abi_export}{safe_export}",
                signature.join(", ")
            )
        } else {
            let fallthrough = if options.allow_main && name == "main" {
                "return 0i32;"
            } else {
                "panic!(\"C function reached end without a return\");"
            };
            format!(
                "pub extern \"C\" fn rf_{name}({}) -> {} {{\nunsafe {{\n{body_code}\n{fallthrough}\n}}\n}}\n{abi_export}{safe_export}",
                signature.join(", "),
                return_ctype.rust_type()
            )
        };

        functions.push(Function {
            name: name.into(),
            parameters,
            parameter_types: param_ctypes.iter().map(CType::c_type).collect(),
            return_type: return_ctype.c_type(),
            source_file: file.into(),
            line_start: line_from_offset(&source, &node["range"]["begin"])?,
            line_end: line_from_offset(&source, &node["range"]["end"])?,
            dependencies: emitter.dependencies.into_iter().collect(),
            external_dependencies: Vec::new(),
            is_static,
            pointer_contracts,
            safe_rust_wrapper,
            rust,
        });
    }

    for (gname, (rust_name, gty)) in extern_globals {
        if referenced_globals.contains(&gname) && !defined_global_slots.contains_key(&gname) {
            let rty = gty.rust_type();
            constants.push(format!(
                "unsafe extern \"C\" {{\n    #[link_name = \"{gname}\"]\n    static mut {rust_name}: {rty};\n}}\n"
            ));
        }
    }
    for decl in global_defs_rust.into_iter().flatten() {
        constants.push(decl);
    }

    let defined_names: BTreeSet<&str> = functions.iter().map(|f| f.name.as_str()).collect();
    if options.export_c_symbols {
        prototypes.retain(|name, _| {
            !defined_names.contains(name.as_str()) && used_fn_deps.contains(name.as_str())
        });
    } else {
        prototypes.retain(|name, _| !defined_names.contains(name.as_str()));
    }

    let mut used_struct_tags = BTreeSet::new();
    let mut used_union_tags = BTreeSet::new();
    let scan_all = |code: &str, st: &mut BTreeSet<String>, un: &mut BTreeSet<String>| {
        scan_record_prefixes(code, "RfStruct_", st);
        scan_record_prefixes(code, "RfUnion_", un);
    };
    for s in &structs {
        scan_all(&s.rust, &mut used_struct_tags, &mut used_union_tags);
    }
    for c in &constants {
        scan_all(c, &mut used_struct_tags, &mut used_union_tags);
    }
    for f in &functions {
        scan_all(&f.rust, &mut used_struct_tags, &mut used_union_tags);
    }
    for p in prototypes.values() {
        scan_all(&p.rust, &mut used_struct_tags, &mut used_union_tags);
    }
    for tag in used_struct_tags {
        if !seen_records.contains(&format!("struct:{tag}")) && identifier(&tag).is_ok() {
            structs.push(StructDef {
                name: tag.clone(),
                rust_name: format!("RfStruct_{tag}"),
                fields: Vec::new(),
                source_file: translation_unit.to_string_lossy().into_owned(),
                line_start: 1,
                line_end: 1,
                rust: format!(
                    "#[repr(C)]\n#[derive(Clone, Copy, Debug, Default)]\npub struct RfStruct_{tag} {{\n    _private: [u8; 0],\n}}\n"
                ),
            });
        }
    }
    for tag in used_union_tags {
        if !seen_records.contains(&format!("union:{tag}")) && identifier(&tag).is_ok() {
            structs.push(StructDef {
                name: format!("union:{tag}"),
                rust_name: format!("RfUnion_{tag}"),
                fields: Vec::new(),
                source_file: translation_unit.to_string_lossy().into_owned(),
                line_start: 1,
                line_end: 1,
                rust: format!(
                    "#[repr(C)]\n#[derive(Clone, Copy, Debug, Default)]\npub struct RfUnion_{tag} {{\n    _private: [u8; 0],\n}}\n"
                ),
            });
        }
    }

    Ok(TranslationUnitOutput {
        structs,
        constants,
        functions,
        prototypes: prototypes.into_values().collect(),
    })
}

pub(super) fn scan_record_prefixes(code: &str, prefix: &str, out: &mut BTreeSet<String>) {
    let mut rest = code;
    while let Some(pos) = rest.find(prefix) {
        let after = &rest[pos + prefix.len()..];
        let len = after
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
            .count();
        if len > 0 {
            out.insert(after[..len].to_owned());
        }
        rest = &after[len..];
    }
}
