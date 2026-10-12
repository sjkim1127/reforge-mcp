//! Fail-closed translation of a deliberately small, typed C subset.
use anyhow::{Result, bail, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::model::Function;

fn text<'a>(node: &'a Value, key: &str) -> &'a str {
    node[key].as_str().unwrap_or("")
}
fn children(node: &Value) -> &[Value] {
    node["inner"].as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn kind(node: &Value) -> &str {
    text(node, "kind")
}
fn int_type(node: &Value) -> Result<()> {
    ensure!(
        node["type"]["qualType"] == "int",
        "unsupported type {:?} on {}",
        node["type"]["qualType"],
        kind(node)
    );
    Ok(())
}
fn identifier(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "unsupported identifier {name}"
    );
    Ok(())
}

pub fn translate(ast: &Value, translation_unit: &Path, root: &Path) -> Result<Vec<Function>> {
    ensure!(
        kind(ast) == "TranslationUnitDecl",
        "unexpected Clang AST root"
    );
    let mut functions = Vec::new();
    let mut current_file = translation_unit.to_path_buf();
    for node in children(ast) {
        if node["isImplicit"].as_bool() == Some(true) {
            continue;
        }
        let location = expansion_location(&node["loc"]);
        if let Some(file) = location["file"].as_str() {
            current_file = Path::new(file).to_path_buf();
        }
        // Clang elides repeated filenames in JSON. Track that context and never
        // confuse declarations from system headers with the source translation unit.
        let physical_file = current_file.canonicalize()?;
        if !physical_file.starts_with(root) {
            continue;
        }
        let file = physical_file
            .strip_prefix(root)?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF8 source path"))?;
        let source = std::fs::read_to_string(&physical_file)?;
        ensure!(
            kind(node) == "FunctionDecl",
            "unsupported top-level {}",
            kind(node)
        );
        let Some(body) = children(node).iter().find(|n| kind(n) == "CompoundStmt") else {
            // Prototypes are compile-checked by Clang; external calls are rejected
            // against the final project definition inventory.
            continue;
        };
        let name = text(node, "name");
        identifier(name)?;
        ensure!(
            name != "main",
            "main functions are not supported; select a library entry function"
        );
        ensure!(
            !node["variadic"].as_bool().unwrap_or(false),
            "variadic functions are unsupported"
        );
        ensure!(
            matches!(text(node, "storageClass"), "" | "static" | "extern"),
            "unsupported function storage class"
        );
        let params: Vec<_> = children(node)
            .iter()
            .filter(|n| kind(n) == "ParmVarDecl")
            .collect();
        let ty = text(&node["type"], "qualType");
        let expected = format!("int ({})", vec!["int"; params.len()].join(", "));
        ensure!(
            ty == expected || (params.is_empty() && ty == "int (void)"),
            "unsupported function type {ty}"
        );
        for param in &params {
            int_type(param)?;
        }
        ensure!(
            children(node)
                .iter()
                .all(|n| matches!(kind(n), "ParmVarDecl" | "CompoundStmt")
                    || (matches!(kind(n), "BuiltinAttr" | "NoThrowAttr" | "ConstAttr")
                        && (n["inherited"].as_bool() == Some(true)
                            || n["implicit"].as_bool() == Some(true)))),
            "function attributes or other declarations are unsupported"
        );
        let mut emitter = Emitter {
            variables: BTreeMap::new(),
            dependencies: BTreeSet::new(),
            next_variable: 0,
        };
        let mut parameters = Vec::new();
        let mut signature = Vec::new();
        for (i, param) in params.iter().enumerate() {
            let original = text(param, "name");
            ensure!(!original.is_empty(), "unnamed definition parameter");
            parameters.push(original.to_string());
            let generated = format!("v{i}");
            emitter
                .variables
                .insert(text(param, "id").to_owned(), generated.clone());
            signature.push(format!("mut {generated}: i32"));
        }
        emitter.next_variable = params.len();
        let body = emitter.statement(body)?;
        let rust = format!(
            "pub fn rf_{name}({}) -> i32 {{\n{body}\npanic!(\"C function reached end without a return\");\n}}\n",
            signature.join(", ")
        );
        let line = |loc: &Value| -> Result<usize> {
            let loc = expansion_location(loc);
            let offset = loc["offset"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("missing source offset"))?
                as usize;
            ensure!(
                offset <= source.len() && source.is_char_boundary(offset),
                "invalid source offset"
            );
            Ok(source[..offset].bytes().filter(|b| *b == b'\n').count() + 1)
        };
        functions.push(Function {
            name: name.into(),
            parameters,
            source_file: file.into(),
            line_start: line(&node["range"]["begin"])?,
            line_end: line(&node["range"]["end"])?,
            dependencies: emitter.dependencies.into_iter().collect(),
            rust,
        });
    }
    Ok(functions)
}

fn expansion_location(location: &Value) -> &Value {
    if location.get("expansionLoc").is_some() {
        &location["expansionLoc"]
    } else {
        location
    }
}

struct Emitter {
    variables: BTreeMap<String, String>,
    dependencies: BTreeSet<String>,
    next_variable: usize,
}
impl Emitter {
    fn variable(&self, node: &Value) -> Result<String> {
        ensure!(
            kind(node) == "DeclRefExpr",
            "assignment must target a local variable"
        );
        self.variables
            .get(text(&node["referencedDecl"], "id"))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown or nonlocal variable"))
    }
    fn expression(&mut self, node: &Value) -> Result<String> {
        int_type(node)?;
        let inner = children(node);
        match kind(node) {
            "IntegerLiteral" => {
                let value = text(node, "value").parse::<i32>()?;
                Ok(format!("{value}i32"))
            }
            "DeclRefExpr" => self.variable(node),
            "ParenExpr" => {
                ensure!(inner.len() == 1, "invalid parentheses");
                Ok(format!("({})", self.expression(&inner[0])?))
            }
            "ImplicitCastExpr" => {
                ensure!(
                    text(node, "castKind") == "LValueToRValue" && inner.len() == 1,
                    "unsupported implicit conversion"
                );
                self.expression(&inner[0])
            }
            "BinaryOperator" => {
                ensure!(inner.len() == 2, "invalid binary expression");
                let op = text(node, "opcode");
                let left = self.expression(&inner[0])?;
                let right = self.expression(&inner[1])?;
                Ok(match op {
                    "+" | "-" | "*" | "/" | "%" => {
                        let method = match op {
                            "+" => "checked_add",
                            "-" => "checked_sub",
                            "*" => "checked_mul",
                            "/" => "checked_div",
                            _ => "checked_rem",
                        };
                        format!("({left}).{method}({right}).expect(\"undefined C arithmetic\")")
                    }
                    "<" | ">" | "<=" | ">=" | "==" | "!=" => {
                        format!("(({left}) {op} ({right})) as i32")
                    }
                    "&&" | "||" => format!("((({left}) != 0) {op} (({right}) != 0)) as i32"),
                    "&" | "|" | "^" => format!("(({left}) {op} ({right}))"),
                    _ => bail!("unsupported binary operator {op}"),
                })
            }
            "UnaryOperator" => {
                ensure!(inner.len() == 1, "invalid unary expression");
                let value = self.expression(&inner[0])?;
                Ok(match text(node, "opcode") {
                    "-" => format!("({value}).checked_neg().expect(\"undefined C negation\")"),
                    "+" => value,
                    "!" => format!("(({value}) == 0) as i32"),
                    "~" => format!("!({value})"),
                    op => bail!("unsupported unary operator {op}"),
                })
            }
            "ConditionalOperator" => {
                ensure!(inner.len() == 3, "invalid conditional expression");
                Ok(format!(
                    "(if ({}) != 0 {{ {} }} else {{ {} }})",
                    self.expression(&inner[0])?,
                    self.expression(&inner[1])?,
                    self.expression(&inner[2])?
                ))
            }
            "CallExpr" => {
                ensure!(!inner.is_empty(), "invalid function call");
                let callee = &inner[0];
                ensure!(
                    kind(callee) == "ImplicitCastExpr"
                        && text(callee, "castKind") == "FunctionToPointerDecay"
                        && children(callee).len() == 1,
                    "only direct calls are supported"
                );
                let target = &children(callee)[0];
                ensure!(
                    kind(target) == "DeclRefExpr"
                        && kind(&target["referencedDecl"]) == "FunctionDecl",
                    "indirect call unsupported"
                );
                let name = text(&target["referencedDecl"], "name");
                identifier(name)?;
                self.dependencies.insert(name.into());
                let arguments = inner[1..]
                    .iter()
                    .map(|n| self.expression(n))
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("rf_{name}({})", arguments.join(", ")))
            }
            other => bail!("unsupported expression {other}"),
        }
    }
    fn statement(&mut self, node: &Value) -> Result<String> {
        let inner = children(node);
        match kind(node) {
            "CompoundStmt" => inner
                .iter()
                .map(|n| self.statement(n))
                .collect::<Result<Vec<_>>>()
                .map(|v| format!("{{\n{}\n}}", v.join("\n"))),
            "ReturnStmt" => {
                ensure!(inner.len() == 1, "int return requires a value");
                Ok(format!("return {};", self.expression(&inner[0])?))
            }
            "IfStmt" => {
                ensure!(
                    (inner.len() == 2 || inner.len() == 3)
                        && !node["hasInit"].as_bool().unwrap_or(false),
                    "unsupported if structure"
                );
                let condition = self.expression(&inner[0])?;
                let branch = self.statement(&inner[1])?;
                let other = if inner.len() == 3 {
                    format!(" else {{ {} }}", self.statement(&inner[2])?)
                } else {
                    String::new()
                };
                Ok(format!("if ({condition}) != 0 {{ {branch} }}{other}"))
            }
            "DeclStmt" => {
                let mut statements = Vec::new();
                for declaration in inner {
                    ensure!(
                        kind(declaration) == "VarDecl",
                        "unsupported local declaration"
                    );
                    int_type(declaration)?;
                    ensure!(
                        text(declaration, "storageClass").is_empty(),
                        "static/extern locals unsupported"
                    );
                    ensure!(
                        children(declaration).len() == 1,
                        "locals must have one initializer and no attributes"
                    );
                    let value = self.expression(&children(declaration)[0])?;
                    let name = format!("v{}", self.next_variable);
                    self.next_variable += 1;
                    self.variables
                        .insert(text(declaration, "id").into(), name.clone());
                    statements.push(format!("let mut {name}: i32 = {value};"));
                }
                Ok(statements.join("\n"))
            }
            "BinaryOperator" if text(node, "opcode") == "=" => {
                int_type(node)?;
                ensure!(inner.len() == 2, "invalid assignment");
                Ok(format!(
                    "{} = {};",
                    self.variable(&inner[0])?,
                    self.expression(&inner[1])?
                ))
            }
            "NullStmt" => Ok(";".into()),
            _ => Ok(format!("{};", self.expression(node)?)),
        }
    }
}
