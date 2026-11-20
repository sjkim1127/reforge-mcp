//! C string literal decoding and Rust array rendering.
use anyhow::{Result, bail, ensure};

use crate::clang::types::IntKind;

pub(super) fn parse_c_string_literal(raw: &str) -> Result<Vec<u8>> {
    let trimmed = raw.trim();
    ensure!(
        trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2,
        "unsupported string literal prefix or format {raw:?}"
    );
    let inner = &trimmed[1..trimmed.len() - 1];
    let bytes = inner.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + 1);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        ensure!(i < bytes.len(), "trailing backslash in string literal");
        match bytes[i] {
            b'\\' => {
                out.push(b'\\');
                i += 1;
            }
            b'"' => {
                out.push(b'"');
                i += 1;
            }
            b'\'' => {
                out.push(b'\'');
                i += 1;
            }
            b'?' => {
                out.push(b'?');
                i += 1;
            }
            b'a' => {
                out.push(0x07);
                i += 1;
            }
            b'b' => {
                out.push(0x08);
                i += 1;
            }
            b'f' => {
                out.push(0x0c);
                i += 1;
            }
            b'n' => {
                out.push(b'\n');
                i += 1;
            }
            b'r' => {
                out.push(b'\r');
                i += 1;
            }
            b't' => {
                out.push(b'\t');
                i += 1;
            }
            b'v' => {
                out.push(0x0b);
                i += 1;
            }
            b'x' => {
                i += 1;
                let mut val = 0u8;
                let mut count = 0;
                while i < bytes.len() && count < 2 && bytes[i].is_ascii_hexdigit() {
                    let d = (bytes[i] as char).to_digit(16).unwrap() as u8;
                    val = val * 16 + d;
                    i += 1;
                    count += 1;
                }
                ensure!(count > 0, "invalid hex escape in string literal");
                out.push(val);
            }
            c if (b'0'..=b'7').contains(&c) => {
                let mut val = 0u8;
                let mut count = 0;
                while i < bytes.len() && count < 3 && (b'0'..=b'7').contains(&bytes[i]) {
                    val = val.wrapping_mul(8).wrapping_add(bytes[i] - b'0');
                    i += 1;
                    count += 1;
                }
                out.push(val);
            }
            other => bail!("unsupported string escape \\{}", other as char),
        }
    }
    out.push(0u8);
    Ok(out)
}

pub(super) fn format_string_literal_array(
    raw: &str,
    expected_len: Option<usize>,
    elem_ik: IntKind,
) -> Result<String> {
    let mut bytes = parse_c_string_literal(raw)?;
    if let Some(len) = expected_len {
        if bytes.len() == len + 1 && bytes.last() == Some(&0) {
            bytes.pop();
        }
        ensure!(
            bytes.len() <= len,
            "string literal exceeds target array length {len}"
        );
        bytes.resize(len, 0);
    }
    let char_rty = elem_ik.rust_type();
    let elems = bytes
        .iter()
        .map(|b| {
            if char_rty == "i8" {
                format!("{}i8", (*b as i8))
            } else {
                format!("{b}u8")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!("[{elems}]"))
}
