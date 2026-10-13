//! Compiler-owned ABI layout rules for values that cross native function
//! boundaries. These layouts do not depend on an external code-generation
//! backend. Result construction and call/return lowering will consume this
//! layout as those codegen paths are added.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultLayout {
    pub ok_payload: ScalarLayout,
    pub err_payload: ScalarLayout,
    pub payload: ScalarLayout,
    pub encoding: ResultEncoding,
    pub ok_payload_offset: usize,
    pub err_payload_offset: usize,
    pub size: usize,
    pub align: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultEncoding {
    ExplicitTag { tag_offset: usize },
    NullPointerNiche { pointer_offset: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScalarLayout {
    pub size: usize,
    pub align: usize,
}

impl ResultLayout {
    /// Calculate the supported target layout for a `Result<T, E>` return.
    ///
    /// This models the supported subset of the pinned rustc layout rules.
    /// Rust enum layout is not a stable external ABI: constructors, calls,
    /// returns, and pattern tests must all use the same target compiler's
    /// layout rules before these types can cross crate boundaries.
    pub fn parse(type_text: &str) -> Option<Self> {
        let [ok_type, err_type] = result_arguments(type_text)?;
        let ok_payload = payload_layout(ok_type)?;
        let err_payload = payload_layout(err_type)?;
        let payload = if (err_payload.size, err_payload.align) > (ok_payload.size, ok_payload.align)
        {
            err_payload
        } else {
            ok_payload
        };
        if ok_payload.size == 0 && is_str_reference(err_type) {
            return Some(Self {
                ok_payload,
                err_payload,
                payload: err_payload,
                encoding: ResultEncoding::NullPointerNiche { pointer_offset: 0 },
                ok_payload_offset: 0,
                err_payload_offset: 0,
                size: err_payload.size,
                align: err_payload.align,
            });
        }
        let tag = ScalarLayout { size: 4, align: 4 };
        let align = tag.align.max(payload.align);
        let ok_payload_offset = align_up(tag.size, ok_payload.align)?;
        let err_payload_offset = align_up(tag.size, err_payload.align)?;
        let ok_variant_size = ok_payload_offset.checked_add(ok_payload.size)?;
        let err_variant_size = err_payload_offset.checked_add(err_payload.size)?;
        let size = align_up(ok_variant_size.max(err_variant_size), align)?;
        Some(Self {
            ok_payload,
            err_payload,
            payload,
            encoding: ResultEncoding::ExplicitTag { tag_offset: 0 },
            ok_payload_offset,
            err_payload_offset,
            size,
            align,
        })
    }

    /// SysV x86-64 returns aggregates of at most 16 bytes in registers; larger
    /// values use the hidden destination pointer convention.
    pub fn is_indirect(self) -> bool {
        self.size > 16
    }
}

fn result_arguments(type_text: &str) -> Option<[&str; 2]> {
    let type_text = type_text.trim();
    let open = type_text.find('<')?;
    if !type_text.ends_with('>') {
        return None;
    }
    let path = type_text[..open].trim();
    if !matches!(
        path,
        "Result" | "core::result::Result" | "std::result::Result"
    ) {
        return None;
    }
    let inner = &type_text[open + 1..type_text.len() - 1];
    let mut depth = 0usize;
    let mut comma = None;
    for (index, character) in inner.char_indices() {
        match character {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth = depth.checked_sub(1)?,
            ',' if depth == 0 && comma.is_some() => return None,
            ',' if depth == 0 => comma = Some(index),
            _ => {}
        }
    }
    let comma = comma?;
    let ok_type = inner[..comma].trim();
    let err_type = inner[comma + 1..].trim();
    (!ok_type.is_empty() && !err_type.is_empty()).then_some([ok_type, err_type])
}

fn payload_layout(type_text: &str) -> Option<ScalarLayout> {
    let type_text = type_text.trim();
    match type_text {
        "()" => Some(ScalarLayout { size: 0, align: 1 }),
        "i8" | "u8" | "bool" => Some(ScalarLayout { size: 1, align: 1 }),
        "i16" | "u16" => Some(ScalarLayout { size: 2, align: 2 }),
        "i32" | "u32" | "f32" => Some(ScalarLayout { size: 4, align: 4 }),
        "i64" | "u64" | "usize" | "isize" | "f64" => Some(ScalarLayout { size: 8, align: 8 }),
        _ if is_str_reference(type_text) => Some(ScalarLayout { size: 16, align: 8 }),
        _ => None,
    }
}

fn is_str_reference(type_text: &str) -> bool {
    let Some(referent) = type_text.strip_prefix('&') else {
        return false;
    };
    let referent = referent.trim_start();
    let referent = if let Some(lifetime_end) = referent.strip_prefix('\'') {
        let Some(lifetime_len) = lifetime_end
            .char_indices()
            .take_while(|(_, character)| character.is_ascii_alphanumeric() || *character == '_')
            .map(|(index, character)| index + character.len_utf8())
            .last()
        else {
            return false;
        };
        referent[lifetime_len + 1..].trim_start()
    } else {
        referent
    };
    referent == "str"
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    if align == 0 || !align.is_power_of_two() {
        return None;
    }
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}

#[cfg(test)]
mod tests {
    use super::{ResultEncoding, ResultLayout, result_arguments};

    #[test]
    fn lays_out_result_of_unit_and_static_string_reference() {
        let layout = ResultLayout::parse("Result<(), &'static str>").unwrap();
        assert_eq!(
            layout.encoding,
            ResultEncoding::NullPointerNiche { pointer_offset: 0 }
        );
        assert_eq!(layout.ok_payload_offset, 0);
        assert_eq!(layout.err_payload_offset, 0);
        assert_eq!(layout.ok_payload.size, 0);
        assert_eq!(layout.err_payload.size, 16);
        assert_eq!(layout.size, 16);
        assert_eq!(layout.align, 8);
        assert!(!layout.is_indirect());
    }

    #[test]
    fn lays_out_small_result_payload_in_register_return_size() {
        let layout = ResultLayout::parse("core::result::Result<i32, u64>").unwrap();
        assert_eq!(
            layout.encoding,
            ResultEncoding::ExplicitTag { tag_offset: 0 }
        );
        assert_eq!(layout.ok_payload_offset, 4);
        assert_eq!(layout.err_payload_offset, 8);
        assert_eq!(layout.size, 16);
        assert!(!layout.is_indirect());
    }

    #[test]
    fn lays_out_unit_and_word_variants_at_their_variant_specific_offsets() {
        let layout = ResultLayout::parse("Result<(), u64>").unwrap();
        assert_eq!(layout.ok_payload_offset, 4);
        assert_eq!(layout.err_payload_offset, 8);
        assert_eq!(layout.size, 16);
        assert!(!layout.is_indirect());
    }

    #[test]
    fn parses_nested_result_payload_types_without_splitting_inner_commas() {
        assert_eq!(
            result_arguments("std::result::Result<(i32, u64), &'static str>"),
            Some(["(i32, u64)", "&'static str"])
        );
        assert!(ResultLayout::parse("Result<(i32, u64), &'static str>").is_none());
    }

    #[test]
    fn rejects_non_result_and_unsupported_payloads() {
        assert!(ResultLayout::parse("Option<i32>").is_none());
        assert!(ResultLayout::parse("Result<Vec<u8>, &'static str>").is_none());
        assert!(ResultLayout::parse("Result<i32>").is_none());
    }
}
