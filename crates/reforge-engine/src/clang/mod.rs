//! Fail-closed translation of a typed C subset (fixed-width/unsigned/bool integers,
//! enums, void, structs with bitfields, unions, fixed-size arrays, pointers,
//! function pointers, string literals, static const globals, loops, switch,
//! forward goto cleanup labels, sizeof/alignof, external prototypes, and C ABI export) to Rust.

use crate::model::{ExternalFunction, Function, StructDef};

pub struct TranslationUnitOutput {
    pub structs: Vec<StructDef>,
    pub constants: Vec<String>,
    pub functions: Vec<Function>,
    pub prototypes: Vec<ExternalFunction>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TranslateOptions {
    pub allow_variadic_calls: bool,
    pub allow_mutable_globals: bool,
    pub allow_mutable_static_locals: bool,
    pub allow_main: bool,
    pub allow_uninit_locals: bool,
    pub allow_general_cfg: bool,
    pub export_c_symbols: bool,
}

impl TranslateOptions {
    pub const fn strict() -> Self {
        Self {
            allow_variadic_calls: false,
            allow_mutable_globals: false,
            allow_mutable_static_locals: false,
            allow_main: false,
            allow_uninit_locals: false,
            allow_general_cfg: false,
            export_c_symbols: false,
        }
    }

    pub const fn general_c() -> Self {
        Self {
            allow_variadic_calls: true,
            allow_mutable_globals: true,
            allow_mutable_static_locals: true,
            allow_main: true,
            allow_uninit_locals: true,
            allow_general_cfg: true,
            export_c_symbols: false,
        }
    }

    pub const fn drop_in_c_abi() -> Self {
        Self {
            allow_variadic_calls: true,
            allow_mutable_globals: true,
            allow_mutable_static_locals: true,
            allow_main: true,
            allow_uninit_locals: true,
            allow_general_cfg: true,
            export_c_symbols: true,
        }
    }
}

mod ast;
mod cfg;
mod decls;
mod emitter;
mod literals;
mod pointers;
mod records;
mod switch;
mod translate;
mod type_parse;
mod types;

pub use translate::{translate, translate_with_options};
