use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

use quill_parallel::WorkPool;
use quill_syntax::{
    BinaryOperator, BodyExpr, BodyExprKind, BodyStatement, ClosureParameter, FunctionItem,
    LiteralKind, MatchPattern,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct TypeDiagnostic {
    pub span: Range<usize>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ExternalFunctionSignature {
    /// Fully qualified call spelling as it appears in the caller's source.
    pub path: String,
    pub parameter_types: Vec<String>,
    pub return_type: Option<String>,
    pub generic_parameters: Vec<String>,
    pub generic_copy_parameters: Vec<String>,
    pub generic_as_ref_str_parameters: Vec<String>,
    pub generic_add_parameters: Vec<String>,
    pub generic_has_bounds: bool,
}

#[derive(
    Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct StructSignature {
    pub path: String,
    pub generic_parameters: Vec<String>,
    pub derived_traits: Vec<String>,
    pub fields: Vec<StructFieldSignature>,
    pub is_enum: bool,
    pub enum_variants: Vec<EnumVariantSignature>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct EnumVariantSignature {
    pub name: String,
    pub discriminant: i32,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct StructFieldSignature {
    pub name: String,
    pub type_text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ty {
    Unit,
    Bool,
    Char,
    Integer(String),
    InferInteger,
    Float(String),
    InferFloat,
    String,
    StringSlice,
    Vector(Box<Ty>),
    Array(Box<Ty>, usize),
    Slice(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    Iterator(Box<Ty>),
    Into(Box<Ty>),
    Tuple(Vec<Ty>),
    Reference(Box<Ty>),
    Option(Box<Ty>),
    Result(Box<Ty>, Box<Ty>),
    Named(String),
    Never,
    Unknown,
}

impl Ty {
    fn from_syntax(text: &str) -> Self {
        let name = text.trim();
        match name {
            "()" => Self::Unit,
            "bool" => Self::Bool,
            "char" => Self::Char,
            "String" => Self::String,
            "str" => Self::StringSlice,
            "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64"
            | "u128" | "usize" => Self::Integer(name.to_owned()),
            "f32" | "f64" => Self::Float(name.to_owned()),
            _ => Self::Unknown,
        }
    }

    fn is_numeric(&self) -> bool {
        matches!(
            self,
            Self::Integer(_) | Self::InferInteger | Self::Float(_) | Self::InferFloat
        )
    }

    fn is_integer(&self) -> bool {
        matches!(self, Self::Integer(_) | Self::InferInteger)
    }

    fn display_name(&self) -> &str {
        match self {
            Self::Unit => "()",
            Self::Bool => "bool",
            Self::Char => "char",
            Self::Integer(name) | Self::Float(name) => name,
            Self::InferInteger => "integer literal",
            Self::InferFloat => "float literal",
            Self::String => "string",
            Self::StringSlice => "str",
            Self::Vector(_) => "Vec",
            Self::Array(_, _) => "array",
            Self::Slice(_) => "slice",
            Self::Map(_, _) => "map",
            Self::Iterator(_) => "iterator",
            Self::Into(_) => "Into",
            Self::Tuple(_) => "tuple",
            Self::Reference(_) => "reference",
            Self::Option(_) => "Option",
            Self::Result(_, _) => "Result",
            Self::Named(name) if name.starts_with("\0quill_generic:") => "generic parameter",
            Self::Named(name) => name,
            Self::Never => "!",
            Self::Unknown => "unknown type",
        }
    }
}

fn find_struct_signature<'a>(
    structs: &'a [StructSignature],
    path: &str,
) -> Option<&'a StructSignature> {
    let simple_name = path.rsplit("::").next().unwrap_or(path);
    let mut matches = structs.iter().filter(|structure| {
        !structure.is_enum && {
            structure.path == path
                || structure
                    .path
                    .rsplit("::")
                    .next()
                    .unwrap_or(&structure.path)
                    == simple_name
        }
    });
    let structure = matches.next()?;
    matches.next().is_none().then_some(structure)
}

fn find_enum_signature<'a>(
    enums: &'a [StructSignature],
    path: &str,
) -> Option<&'a StructSignature> {
    let simple_name = path.rsplit("::").next().unwrap_or(path);
    let mut matches = enums.iter().filter(|item| {
        item.is_enum
            && (item.path == path
                || item.path.rsplit("::").next().unwrap_or(&item.path) == simple_name)
    });
    let item = matches.next()?;
    matches.next().is_none().then_some(item)
}

fn find_nominal_signature<'a>(
    signatures: &'a [StructSignature],
    path: &str,
) -> Option<&'a StructSignature> {
    find_struct_signature(signatures, path).or_else(|| find_enum_signature(signatures, path))
}

fn find_enum_variant<'a>(
    signatures: &'a [StructSignature],
    path: &str,
) -> Option<(&'a StructSignature, &'a EnumVariantSignature)> {
    let (enum_path, variant_name) = path.rsplit_once("::")?;
    let requested_enum_name = enum_path.rsplit("::").next().unwrap_or(enum_path);
    let mut matches = signatures.iter().filter_map(|item| {
        if !item.is_enum
            || (item.path != enum_path
                && !item.path.ends_with(&format!("::{enum_path}"))
                && item.path.rsplit("::").next() != Some(requested_enum_name))
        {
            return None;
        }
        item.enum_variants
            .iter()
            .find(|variant| variant.name == variant_name)
            .map(|variant| (item, variant))
    });
    let found = matches.next()?;
    matches.next().is_none().then_some(found)
}

struct SignatureIndex<'a> {
    exact: HashMap<&'a str, Vec<&'a FunctionItem>>,
    suffix: HashMap<&'a str, Vec<&'a FunctionItem>>,
    methods: HashMap<&'a str, Vec<&'a FunctionItem>>,
    trait_methods: HashMap<&'a str, Vec<&'a FunctionItem>>,
    trait_impl_methods: HashMap<(&'a str, &'a str), Vec<&'a FunctionItem>>,
}

fn generic_type_marker(function: &FunctionItem, generic: &str) -> Ty {
    let traits = function
        .generic_trait_bounds
        .iter()
        .filter(|(parameter, _)| parameter == generic)
        .map(|(_, trait_name)| trait_name.rsplit("::").next().unwrap_or(trait_name))
        .collect::<Vec<_>>()
        .join("+");
    Ty::Named(format!(
        "\0quill_generic:{generic}:traits={traits}:{}:{}:{}",
        u8::from(function.generic_copy_parameters.iter().any(|item| item == generic)),
        u8::from(function.generic_as_ref_str_parameters.iter().any(|item| item == generic)),
        u8::from(function.generic_add_parameters.iter().any(|item| item == generic))
    ))
}

fn type_from_function_syntax(
    text: &str,
    function: &FunctionItem,
    structs: &[StructSignature],
) -> Ty {
    let text = text.trim();
    if let Some(generic) = function
        .generic_parameters
        .iter()
        .find(|generic| text == generic.as_str())
    {
        return generic_type_marker(function, generic);
    }
    if let Some(reference) = text.strip_prefix('&') {
        let reference = reference.trim_start();
        let referent = if reference.starts_with('\'') {
            reference
                .find(char::is_whitespace)
                .map_or("", |index| reference[index..].trim_start())
        } else {
            reference.strip_prefix("mut ").unwrap_or(reference)
        };
        return Ty::Reference(Box::new(type_from_function_syntax(
            referent, function, structs,
        )));
    }
    if let Some(value) = text.strip_prefix("mut ") {
        return type_from_function_syntax(value, function, structs);
    }
    type_from_syntax(text, structs)
}

fn matching_signatures<'a>(signatures: &SignatureIndex<'a>, path: &str) -> Vec<&'a FunctionItem> {
    if let Some(exact) = signatures.exact.get(path) {
        return exact.clone();
    }
    signatures.suffix.get(path).cloned().unwrap_or_default()
}

fn method_owner(function: &FunctionItem) -> Option<&str> {
    let mut segments = function.name.rsplit("::");
    let _method = segments.next()?;
    segments.next().or_else(|| {
        function
            .scope
            .iter()
            .find_map(|scope| {
                scope
                    .strip_prefix("impl:")
                    .or_else(|| scope.strip_prefix("trait:"))
            })
    })
}

fn index_trait_method<'a>(
    trait_methods: &mut HashMap<&'a str, Vec<&'a FunctionItem>>,
    trait_impl_methods: &mut HashMap<(&'a str, &'a str), Vec<&'a FunctionItem>>,
    function: &'a FunctionItem,
) {
    let trait_owner = function.scope.iter().find_map(|scope| {
        scope
            .strip_prefix("trait:")
            .map(|owner| owner.rsplit("::").next().unwrap_or(owner))
    });
    if let Some(trait_owner) = trait_owner {
        trait_methods
            .entry(trait_owner)
            .or_default()
            .push(function);
    }

    let trait_impl_owner = function.scope.iter().find_map(|scope| {
        scope
            .strip_prefix("traitimpl:")
            .map(|owner| owner.rsplit("::").next().unwrap_or(owner))
    });
    let impl_owner = function.scope.iter().find_map(|scope| {
        scope
            .strip_prefix("impl:")
            .map(|owner| owner.rsplit("::").next().unwrap_or(owner))
    });
    if let (Some(trait_owner), Some(impl_owner)) = (trait_impl_owner, impl_owner) {
        trait_impl_methods
            .entry((trait_owner, impl_owner))
            .or_default()
            .push(function);
    }
}

fn generic_marker_traits(name: &str) -> impl Iterator<Item = &str> {
    name.split(':')
        .find_map(|part| part.strip_prefix("traits="))
        .into_iter()
        .flat_map(|traits| traits.split('+'))
        .filter(|trait_name| !trait_name.is_empty())
}

fn trait_methods_for_generic<'a>(
    signatures: &'a SignatureIndex<'a>,
    receiver_type: &Ty,
    method: &str,
) -> Vec<&'a FunctionItem> {
    let Ty::Named(name) = without_references(receiver_type) else {
        return Vec::new();
    };
    if !name.starts_with("\0quill_generic:") {
        return Vec::new();
    }
    let traits: HashSet<_> = generic_marker_traits(name).collect();
    signatures
        .methods
        .get(method)
        .into_iter()
        .flatten()
        .filter(|function| {
            function.scope.iter().any(|scope| {
                scope
                    .strip_prefix("trait:")
                    .is_some_and(|owner| traits.contains(owner.rsplit("::").next().unwrap_or(owner)))
            })
        })
        .copied()
        .collect()
}

fn trait_is_implemented_for_type(
    signatures: &SignatureIndex<'_>,
    trait_name: &str,
    type_name: &str,
) -> bool {
    let trait_name = trait_name.rsplit("::").next().unwrap_or(trait_name);
    let type_name = type_name.rsplit("::").next().unwrap_or(type_name);
    let Some(required) = signatures.trait_methods.get(trait_name) else {
        return false;
    };
    let Some(provided) = signatures.trait_impl_methods.get(&(trait_name, type_name)) else {
        return false;
    };
    !required.is_empty()
        && required.iter().all(|trait_method| {
            let method_name = trait_method.name.rsplit("::").next().unwrap_or(&trait_method.name);
            provided.iter().any(|implementation| {
                implementation.name.rsplit("::").next() == Some(method_name)
                    && implementation.parameters.len() == trait_method.parameters.len()
                    && implementation
                        .parameters
                        .iter()
                        .zip(&trait_method.parameters)
                        .all(|(actual, expected)| {
                            actual.type_text.replace("Self", type_name)
                                == expected.type_text.replace("Self", type_name)
                        })
                    && implementation
                        .return_type
                        .as_deref()
                        .map(|ty| ty.replace("Self", type_name))
                        == trait_method
                            .return_type
                            .as_deref()
                            .map(|ty| ty.replace("Self", type_name))
                    && implementation.is_async == trait_method.is_async
                    && implementation.is_const == trait_method.is_const
                    && implementation.is_unsafe == trait_method.is_unsafe
                    && implementation.has_abi == trait_method.has_abi
            })
        })
}

fn normalized_crate_path(path: &str) -> &str {
    path.strip_prefix("crate::").unwrap_or(path)
}

fn method_signatures<'a>(
    signatures: &HashMap<&str, Vec<&'a FunctionItem>>,
    receiver: &Ty,
    method: &str,
) -> Vec<&'a FunctionItem> {
    let Ty::Named(receiver_name) = without_references(receiver) else {
        return Vec::new();
    };
    let named_candidates: Vec<_> = signatures
        .get(method)
        .into_iter()
        .flatten()
        .filter(|function| method_owner(function).is_some())
        .copied()
        .collect();
    let exact: Vec<_> = named_candidates
        .iter()
        .copied()
        .filter(|function| {
            method_owner(function).is_some_and(|owner| {
                normalized_crate_path(owner) == normalized_crate_path(receiver_name)
            })
        })
        .collect();
    let qualified: Vec<_> = named_candidates
        .iter()
        .copied()
        .filter(|function| {
            method_owner(function).is_some_and(|owner| {
                normalized_crate_path(owner)
                    .ends_with(&format!("::{}", normalized_crate_path(receiver_name)))
            })
        })
        .collect();
    let receiver_basename = receiver_name.rsplit("::").next().unwrap_or(receiver_name);
    let mut candidates = if !exact.is_empty() {
        exact
    } else if !qualified.is_empty() {
        qualified
    } else {
        named_candidates
            .into_iter()
            .filter(|function| {
                method_owner(function)
                    .is_some_and(|owner| owner.rsplit("::").next() == Some(receiver_basename))
            })
            .collect()
    };
    let local_definitions: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|function| function.body.is_some())
        .collect();
    if !local_definitions.is_empty() {
        candidates = local_definitions;
    }
    candidates.sort_by(|left, right| {
        normalized_crate_path(&left.name).cmp(normalized_crate_path(&right.name))
    });
    candidates.dedup_by(|left, right| {
        normalized_crate_path(&left.name) == normalized_crate_path(&right.name)
    });
    candidates
}

fn type_from_syntax(text: &str, structs: &[StructSignature]) -> Ty {
    let text = text.trim();
    if let Some(value_type) = text.strip_prefix("mut ") {
        return type_from_syntax(value_type, structs);
    }
    if let Some(reference) = text.strip_prefix('&') {
        let reference = reference.trim_start();
        let referent = if reference.starts_with('\'') {
            reference
                .find(char::is_whitespace)
                .map_or("", |index| reference[index..].trim_start())
        } else {
            reference.strip_prefix("mut ").unwrap_or(reference)
        };
        return Ty::Reference(Box::new(type_from_syntax(referent, structs)));
    }
    if let Some((ok_type, err_type)) = generic_pair(text, "Result") {
        return Ty::Result(
            Box::new(type_from_syntax(ok_type, structs)),
            Box::new(type_from_syntax(err_type, structs)),
        );
    }
    if let Some(value_type) =
        generic_single(text, "Option").or_else(|| generic_single(text, "std::option::Option"))
    {
        return Ty::Option(Box::new(type_from_syntax(value_type, structs)));
    }
    if let Some(target) = generic_single(text, "impl Into") {
        return Ty::Into(Box::new(type_from_syntax(target, structs)));
    }
    if let Some(elements) = tuple_elements(text) {
        return Ty::Tuple(
            elements
                .into_iter()
                .map(|element| type_from_syntax(element, structs))
                .collect(),
        );
    }
    if let Some((ok_type, err_type)) = generic_pair(text, "std::result::Result") {
        return Ty::Result(
            Box::new(type_from_syntax(ok_type, structs)),
            Box::new(type_from_syntax(err_type, structs)),
        );
    }
    if let Some(element) = text
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
        if let Some((element, length)) = element.split_once(';')
            && let Ok(length) = length.trim().parse::<usize>()
        {
            return Ty::Array(Box::new(type_from_syntax(element, structs)), length);
        }
        return Ty::Slice(Box::new(type_from_syntax(element, structs)));
    }
    if let Some((key, value)) =
        generic_pair(text, "BTreeMap").or_else(|| generic_pair(text, "std::collections::BTreeMap"))
    {
        return Ty::Map(
            Box::new(type_from_syntax(key, structs)),
            Box::new(type_from_syntax(value, structs)),
        );
    }
    if let Some(element) =
        generic_single(text, "Vec").or_else(|| generic_single(text, "std::vec::Vec"))
    {
        return Ty::Vector(Box::new(type_from_syntax(element, structs)));
    }
    let primitive = Ty::from_syntax(text);
    if primitive != Ty::Unknown {
        return primitive;
    }
    find_struct_signature(structs, text)
        .or_else(|| find_enum_signature(structs, text))
        .map(|structure| Ty::Named(structure.path.clone()))
        .unwrap_or(Ty::Unknown)
}

fn type_mentions_identifier(type_text: &str, name: &str) -> bool {
    type_text
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .any(|token| token == name)
}

fn infer_generic_type(type_text: &str, actual: &Ty, generic: &str) -> Option<Ty> {
    let type_text = type_text.trim();
    if type_text == generic {
        return Some(actual.clone());
    }
    if let Some(referent) = type_text.strip_prefix('&') {
        let Ty::Reference(actual_referent) = actual else {
            return None;
        };
        let referent = referent.trim_start();
        let referent = referent.strip_prefix("mut ").unwrap_or(referent);
        return infer_generic_type(referent, actual_referent, generic);
    }
    if let Some(element) = type_text
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
    {
        let Ty::Slice(actual_element) = actual else {
            return None;
        };
        return infer_generic_type(element, actual_element, generic);
    }
    let pattern_elements = tuple_elements(type_text)?;
    let Ty::Tuple(actual_elements) = actual else {
        return None;
    };
    if pattern_elements.len() != actual_elements.len() {
        return None;
    }
    let mut inferred = None;
    for (pattern, actual) in pattern_elements.into_iter().zip(actual_elements) {
        if let Some(found) = infer_generic_type(pattern, actual, generic) {
            if inferred
                .as_ref()
                .is_some_and(|previous| !types_compatible(previous, &found))
            {
                return None;
            }
            inferred = Some(found);
        }
    }
    inferred
}

fn tuple_elements(text: &str) -> Option<Vec<&str>> {
    let body = text.trim().strip_prefix('(')?.strip_suffix(')')?.trim();
    if body.is_empty() {
        return None;
    }
    let mut depth = 0usize;
    let mut start = 0;
    let mut result = Vec::new();
    for (index, ch) in body.char_indices() {
        match ch {
            '(' | '<' | '[' => depth += 1,
            ')' | '>' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                result.push(body[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    result.push(body[start..].trim());
    (result.len() > 1 || body.ends_with(',')).then_some(result)
}

fn generic_single<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let body = text
        .strip_prefix(name)?
        .trim_start()
        .strip_prefix('<')?
        .strip_suffix('>')?;
    (!body.is_empty() && !body.contains(',')).then_some(body.trim())
}

fn is_option_some_path(path: &str) -> bool {
    matches!(path, "Some" | "Option::Some" | "std::option::Option::Some")
}

fn is_option_none_path(path: &str) -> bool {
    matches!(path, "None" | "Option::None" | "std::option::Option::None")
}

fn without_references(mut ty: &Ty) -> &Ty {
    while let Ty::Reference(inner) = ty {
        ty = inner;
    }
    ty
}

fn is_cloneable(ty: &Ty, structs: &[StructSignature]) -> bool {
    match ty {
        Ty::Bool
        | Ty::Char
        | Ty::Integer(_)
        | Ty::InferInteger
        | Ty::Float(_)
        | Ty::InferFloat
        | Ty::String
        | Ty::StringSlice
        | Ty::Reference(_)
        | Ty::Unknown => true,
        Ty::Vector(item) | Ty::Slice(item) | Ty::Array(item, _) => is_cloneable(item, structs),
        Ty::Map(key, value) => is_cloneable(key, structs) && is_cloneable(value, structs),
        Ty::Tuple(items) => items.iter().all(|item| is_cloneable(item, structs)),
        Ty::Named(name) => find_nominal_signature(structs, name).is_some_and(|structure| {
            structure
                .derived_traits
                .iter()
                .any(|trait_name| trait_name == "Clone")
        }),
        Ty::Option(value) => is_cloneable(value, structs),
        Ty::Unit | Ty::Iterator(_) | Ty::Into(_) | Ty::Result(_, _) | Ty::Never => false,
    }
}

fn generic_pair<'a>(text: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
    let generic = text.strip_prefix(name)?.trim_start();
    let body = generic.strip_prefix('<')?.strip_suffix('>')?;
    let mut depth = 0usize;
    for (index, character) in body.char_indices() {
        match character {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                return Some((body[..index].trim(), body[index + 1..].trim()));
            }
            _ => {}
        }
    }
    None
}

/// Check the supported scalar subset of the function bodies in one parsed
/// source unit. Unsupported expressions produce `Unknown` and are left for
/// later checker stages; this pass is not a Rust compatibility checker.
pub fn check_functions(functions: &[FunctionItem]) -> Vec<TypeDiagnostic> {
    check_functions_impl(functions, false, &[], &[])
}

pub fn check_functions_with_structs(
    functions: &[FunctionItem],
    structs: &[StructSignature],
) -> Vec<TypeDiagnostic> {
    check_functions_impl(functions, false, &[], structs)
}

pub fn check_functions_strict_with_structs(
    functions: &[FunctionItem],
    structs: &[StructSignature],
) -> Vec<TypeDiagnostic> {
    check_functions_impl(functions, true, &[], structs)
}

/// Check the scalar subset and require every non-local direct call to resolve
/// within this source unit. Use for standalone units without external imports.
pub fn check_functions_strict(functions: &[FunctionItem]) -> Vec<TypeDiagnostic> {
    check_functions_impl(functions, true, &[], &[])
}

/// Check the supported scalar subset using signatures imported from resolved
/// crate interfaces. Unsupported types continue to infer as unknown.
pub fn check_functions_with_external_signatures(
    functions: &[FunctionItem],
    external_signatures: &[ExternalFunctionSignature],
) -> Vec<TypeDiagnostic> {
    check_functions_impl(functions, false, external_signatures, &[])
}

pub fn check_functions_with_external_signatures_and_structs(
    functions: &[FunctionItem],
    external_signatures: &[ExternalFunctionSignature],
    structs: &[StructSignature],
) -> Vec<TypeDiagnostic> {
    check_functions_impl(functions, false, external_signatures, structs)
}

pub fn check_functions_strict_with_external_signatures_and_structs(
    functions: &[FunctionItem],
    external_signatures: &[ExternalFunctionSignature],
    structs: &[StructSignature],
) -> Vec<TypeDiagnostic> {
    check_functions_impl(functions, true, external_signatures, structs)
}

pub fn check_functions_strict_with_external_signatures(
    functions: &[FunctionItem],
    external_signatures: &[ExternalFunctionSignature],
) -> Vec<TypeDiagnostic> {
    check_functions_impl(functions, true, external_signatures, &[])
}

fn check_functions_impl(
    functions: &[FunctionItem],
    strict_calls: bool,
    external_signatures: &[ExternalFunctionSignature],
    structs: &[StructSignature],
) -> Vec<TypeDiagnostic> {
    check_selected_functions(
        functions.iter(),
        functions,
        strict_calls,
        external_signatures,
        structs,
        true,
    )
}

/// Check one body against the complete set of declarations in its source unit
/// and the resolved external interfaces. This is the primitive used by
/// per-body incremental queries; it still checks dead functions when asked.
pub fn check_function_with_declarations(
    function: &FunctionItem,
    declarations: &[FunctionItem],
    strict_calls: bool,
    external_signatures: &[ExternalFunctionSignature],
) -> Vec<TypeDiagnostic> {
    check_selected_functions(
        std::iter::once(function),
        declarations,
        strict_calls,
        external_signatures,
        &[],
        true,
    )
}

/// Check the selected bodies against the complete declaration set while
/// constructing the shared function-signature index only once.
pub fn check_functions_with_declarations(
    functions: &[FunctionItem],
    declarations: &[FunctionItem],
    strict_calls: bool,
    external_signatures: &[ExternalFunctionSignature],
) -> Vec<TypeDiagnostic> {
    check_selected_functions(
        functions.iter(),
        declarations,
        strict_calls,
        external_signatures,
        &[],
        true,
    )
}

pub fn check_functions_with_declarations_and_structs(
    functions: &[FunctionItem],
    declarations: &[FunctionItem],
    strict_calls: bool,
    external_signatures: &[ExternalFunctionSignature],
    structs: &[StructSignature],
) -> Vec<TypeDiagnostic> {
    check_selected_functions(
        functions.iter(),
        declarations,
        strict_calls,
        external_signatures,
        structs,
        true,
    )
}

/// Check borrowed body references against the complete declaration set. This
/// lets incremental queries select changed bodies without cloning their ASTs.
pub fn check_function_refs_with_declarations_and_structs(
    functions: &[&FunctionItem],
    declarations: &[FunctionItem],
    strict_calls: bool,
    external_signatures: &[ExternalFunctionSignature],
    structs: &[StructSignature],
) -> Vec<TypeDiagnostic> {
    check_selected_functions(
        functions.iter().copied(),
        declarations,
        strict_calls,
        external_signatures,
        structs,
        false,
    )
}

fn check_selected_functions<'a>(
    functions: impl IntoIterator<Item = &'a FunctionItem>,
    declarations: &[FunctionItem],
    strict_calls: bool,
    external_signatures: &[ExternalFunctionSignature],
    struct_signatures: &[StructSignature],
    parallel_functions: bool,
) -> Vec<TypeDiagnostic> {
    let external_functions: Vec<_> = external_signatures
        .iter()
        .map(|signature| FunctionItem {
            name: signature.path.clone(),
            scope: Vec::new(),
            calls: Vec::new(),
            qualified_calls: Vec::new(),
            name_span: 0..0,
            span: 0..0,
            body_span: None,
            parameters: signature
                .parameter_types
                .iter()
                .map(|type_text| quill_syntax::ParameterItem {
                    name: None,
                    type_text: type_text.clone(),
                    span: 0..0,
                })
                .collect(),
            return_type: signature.return_type.clone(),
            body: None,
            is_public: true,
            is_generic: !signature.generic_parameters.is_empty(),
            generic_parameters: signature.generic_parameters.clone(),
            generic_copy_parameters: signature.generic_copy_parameters.clone(),
            generic_as_ref_str_parameters: signature.generic_as_ref_str_parameters.clone(),
            generic_add_parameters: signature.generic_add_parameters.clone(),
            generic_trait_bounds: Vec::new(),
            generic_has_bounds: signature.generic_has_bounds,
            is_async: false,
            is_const: false,
            is_unsafe: false,
            has_abi: false,
        })
        .collect();
    let signatures: SignatureIndex<'_> = {
        let mut exact = HashMap::<&str, Vec<&FunctionItem>>::new();
        let mut suffix = HashMap::<&str, Vec<&FunctionItem>>::new();
        let mut methods = HashMap::<&str, Vec<&FunctionItem>>::new();
        let mut trait_methods = HashMap::<&str, Vec<&FunctionItem>>::new();
        let mut trait_impl_methods = HashMap::<(&str, &str), Vec<&FunctionItem>>::new();
        let needs_trait_index = declarations
            .iter()
            .chain(&external_functions)
            .any(|function| !function.generic_trait_bounds.is_empty());
        for function in declarations {
            add_signature_suffixes(&mut exact, &mut suffix, function);
            if method_owner(function).is_some()
                && let Some(method) = function.name.rsplit("::").next()
            {
                methods.entry(method).or_default().push(function);
            }
            if needs_trait_index {
                index_trait_method(
                    &mut trait_methods,
                    &mut trait_impl_methods,
                    function,
                );
            }
            // Root-module items remain directly addressable by their source
            // name even when stored under a canonical crate-qualified path.
            if function.scope.is_empty()
                && let Some(simple_name) = function.name.rsplit("::").next()
                && simple_name != function.name
            {
                exact.entry(simple_name).or_default().push(function);
            }
        }
        for function in &external_functions {
            add_signature_suffixes(&mut exact, &mut suffix, function);
            if method_owner(function).is_some()
                && let Some(method) = function.name.rsplit("::").next()
            {
                methods.entry(method).or_default().push(function);
            }
            if needs_trait_index {
                index_trait_method(
                    &mut trait_methods,
                    &mut trait_impl_methods,
                    function,
                );
            }
        }
        SignatureIndex {
            exact,
            suffix,
            methods,
            trait_methods,
            trait_impl_methods,
        }
    };
    let functions: Vec<_> = functions
        .into_iter()
        .filter(|function| function.body.is_some())
        .collect();
    // Body checking is independent once the whole-crate signature and struct
    // tables are built. Large crates use bounded Rayon work stealing; small
    // crates stay sequential to avoid paying pool startup costs.
    let checked = if !parallel_functions || functions.len() < 8 {
        functions
            .iter()
            .map(|function| {
                check_one_function(function, &signatures, struct_signatures, strict_calls)
            })
            .collect::<Vec<_>>()
    } else if let Some(pool) = WorkPool::shared_typeck() {
        pool.map(&functions, |function| {
            check_one_function(function, &signatures, struct_signatures, strict_calls)
        })
    } else {
        functions
            .iter()
            .map(|function| {
                check_one_function(function, &signatures, struct_signatures, strict_calls)
            })
            .collect::<Vec<_>>()
    };
    checked.into_iter().flatten().collect()
}

fn add_signature_suffixes<'a>(
    exact: &mut HashMap<&'a str, Vec<&'a FunctionItem>>,
    suffixes: &mut HashMap<&'a str, Vec<&'a FunctionItem>>,
    function: &'a FunctionItem,
) {
    let name = function.name.as_str();
    exact.entry(name).or_default().push(function);
    for (offset, _) in name.match_indices("::") {
        let candidates = suffixes.entry(&name[offset + 2..]).or_default();
        if !candidates.contains(&function) {
            candidates.push(function);
        }
    }
}

fn check_one_function(
    function: &FunctionItem,
    signatures: &SignatureIndex<'_>,
    struct_signatures: &[StructSignature],
    strict_calls: bool,
) -> Vec<TypeDiagnostic> {
    let Some(body) = &function.body else {
        return Vec::new();
    };
    let mut diagnostics = Vec::new();
    let function_structs = if let Some(self_type) = function
        .scope
        .iter()
        .find_map(|scope| scope.strip_prefix("impl:"))
    {
        let mut function_structs = struct_signatures.to_vec();
        if let Some(structure) = find_struct_signature(&function_structs, self_type).cloned() {
            function_structs.push(StructSignature {
                path: "Self".to_owned(),
                generic_parameters: structure.generic_parameters,
                derived_traits: structure.derived_traits,
                fields: structure.fields,
                is_enum: false,
                enum_variants: Vec::new(),
            });
        }
        Cow::Owned(function_structs)
    } else {
        Cow::Borrowed(struct_signatures)
    };
    let function_structs = function_structs.as_ref();
    let mut locals = HashMap::new();
    for parameter in &function.parameters {
        if let Some(name) = &parameter.name {
            locals.insert(
                name.clone(),
                type_from_function_syntax(&parameter.type_text, function, function_structs),
            );
        }
    }
    let mut mutable_locals: HashMap<String, bool> = function
        .parameters
        .iter()
        .filter_map(|parameter| {
            parameter.name.as_ref().map(|name| {
                (
                    name.clone(),
                    parameter.type_text.trim_start().starts_with("&mut ")
                        || parameter.type_text.trim_start().starts_with("mut "),
                )
            })
        })
        .collect();
    let expected = function.return_type.as_deref().map_or(Ty::Unit, |text| {
        type_from_function_syntax(text, function, function_structs)
    });
    let actual = infer_with_structs(
        body,
        &mut locals,
        signatures,
        function_structs,
        &expected,
        &mut diagnostics,
    );
    if actual != Ty::Never && !types_compatible(&expected, &actual) {
        mismatch(
            &mut diagnostics,
            body.span.clone(),
            &expected,
            &actual,
            "function body",
        );
    }
    check_move_uses(
        function,
        body,
        signatures,
        function_structs,
        &mut diagnostics,
    );
    check_local_borrow_conflicts(body, &mut diagnostics);
    check_mutability(body, &mut mutable_locals, &mut diagnostics);
    check_loop_control(body, 0, &mut diagnostics);
    if strict_calls {
        let mut names: HashSet<String> = function
            .parameters
            .iter()
            .filter_map(|parameter| parameter.name.clone())
            .collect();
        check_call_targets(body, &mut names, signatures, &mut diagnostics);
    }
    diagnostics
}

/// Diagnose use of a local after a move along straight-line control flow.
/// This is deliberately a separate pass from type inference: move state is
/// flow-sensitive, while the current type environment is reused by many
/// recursive inference paths.
fn check_move_uses(
    function: &FunctionItem,
    body: &BodyExpr,
    signatures: &SignatureIndex<'_>,
    struct_signatures: &[StructSignature],
    diagnostics: &mut Vec<TypeDiagnostic>,
) {
    let mut locals = HashMap::new();
    for parameter in &function.parameters {
        if let Some(name) = &parameter.name {
            locals.insert(
                name.clone(),
                type_from_syntax(&parameter.type_text, struct_signatures),
            );
        }
    }
    let mut moved = HashSet::new();
    check_move_expression(
        body,
        true,
        signatures,
        struct_signatures,
        &mut locals,
        &mut moved,
        diagnostics,
    );
}

/// Diagnose overlapping references held by named locals in a block.
/// Liveness is approximated by whether the first reference is used after the
/// second borrow; this preserves Rust's non-lexical end of borrow for simple
/// straight-line bodies.
fn check_local_borrow_conflicts(body: &BodyExpr, diagnostics: &mut Vec<TypeDiagnostic>) {
    check_local_borrow_conflicts_in_expression(body, diagnostics);
}

fn check_local_borrow_conflicts_in_expression(
    expression: &BodyExpr,
    diagnostics: &mut Vec<TypeDiagnostic>,
) {
    match &expression.kind {
        BodyExprKind::Block { statements, tail } => {
            let mut borrows = Vec::<(String, String, bool)>::new();
            for (index, statement) in statements.iter().enumerate() {
                if let BodyStatement::Let {
                    name: Some(name),
                    initializer: Some(initializer),
                    ..
                } = statement
                    && let Some((place, is_mutable)) = borrowed_local_path(initializer)
                {
                    for (borrower, borrowed_place, previous_is_mutable) in &borrows {
                        if borrowed_place == place
                            && (*previous_is_mutable || is_mutable)
                            && local_used_after(
                                borrower,
                                &statements[index + 1..],
                                tail.as_deref(),
                            )
                        {
                            let message = match (*previous_is_mutable, is_mutable) {
                                (true, true) => format!(
                                    "error[E0499]: cannot borrow `{place}` as mutable more than once at a time"
                                ),
                                (false, true) => format!(
                                    "error[E0502]: cannot borrow `{place}` as mutable because it is also borrowed as immutable"
                                ),
                                (true, false) => format!(
                                    "error[E0502]: cannot borrow `{place}` as immutable because it is also borrowed as mutable"
                                ),
                                (false, false) => unreachable!("shared borrows may overlap"),
                            };
                            diagnostics.push(TypeDiagnostic {
                                span: initializer.span.clone(),
                                message,
                            });
                            break;
                        }
                    }
                    borrows.push((name.clone(), place.to_owned(), is_mutable));
                }
                check_local_borrow_conflicts_in_statement(statement, diagnostics);
            }
            if let Some(tail) = tail {
                check_local_borrow_conflicts_in_expression(tail, diagnostics);
            }
        }
        BodyExprKind::Reference { expression, .. }
        | BodyExprKind::Cast { expression, .. }
        | BodyExprKind::Negate(expression)
        | BodyExprKind::Not(expression)
        | BodyExprKind::Try(expression)
        | BodyExprKind::Parenthesized(expression)
        | BodyExprKind::Dereference(expression) => {
            check_local_borrow_conflicts_in_expression(expression, diagnostics)
        }
        BodyExprKind::Call { arguments, .. }
        | BodyExprKind::Tuple(arguments)
        | BodyExprKind::ArrayLiteral(arguments)
        | BodyExprKind::FixedArrayLiteral(arguments) => {
            for argument in arguments {
                check_local_borrow_conflicts_in_expression(argument, diagnostics);
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            arguments,
            ..
        } => {
            check_local_borrow_conflicts_in_expression(receiver, diagnostics);
            for argument in arguments {
                check_local_borrow_conflicts_in_expression(argument, diagnostics);
            }
        }
        BodyExprKind::Closure { body, .. } => {
            check_local_borrow_conflicts_in_expression(body, diagnostics)
        }
        BodyExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                check_local_borrow_conflicts_in_expression(value, diagnostics);
            }
        }
        BodyExprKind::FieldAccess { base, .. } => {
            check_local_borrow_conflicts_in_expression(base, diagnostics)
        }
        BodyExprKind::Index { base, index, .. } => {
            check_local_borrow_conflicts_in_expression(base, diagnostics);
            check_local_borrow_conflicts_in_expression(index, diagnostics);
        }
        BodyExprKind::Binary { left, right, .. } => {
            check_local_borrow_conflicts_in_expression(left, diagnostics);
            check_local_borrow_conflicts_in_expression(right, diagnostics);
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            check_local_borrow_conflicts_in_expression(condition, diagnostics);
            check_local_borrow_conflicts_in_expression(then_branch, diagnostics);
            if let Some(else_branch) = else_branch {
                check_local_borrow_conflicts_in_expression(else_branch, diagnostics);
            }
        }
        BodyExprKind::Match { scrutinee, arms } => {
            check_local_borrow_conflicts_in_expression(scrutinee, diagnostics);
            for arm in arms {
                check_local_borrow_conflicts_in_expression(&arm.body, diagnostics);
            }
        }
        BodyExprKind::MacroInvocation(macro_expression) => {
            for argument in &macro_expression.arguments {
                check_local_borrow_conflicts_in_expression(argument, diagnostics);
            }
        }
        BodyExprKind::Return(Some(value)) => {
            check_local_borrow_conflicts_in_expression(value, diagnostics)
        }
        BodyExprKind::Unit
        | BodyExprKind::Literal(_)
        | BodyExprKind::EmptySliceReference
        | BodyExprKind::Path(_)
        | BodyExprKind::Return(None)
        | BodyExprKind::Unsupported => {}
    }
}

fn check_local_borrow_conflicts_in_statement(
    statement: &BodyStatement,
    diagnostics: &mut Vec<TypeDiagnostic>,
) {
    match statement {
        BodyStatement::Let { initializer, .. } => {
            if let Some(initializer) = initializer {
                check_local_borrow_conflicts_in_expression(initializer, diagnostics);
            }
        }
        BodyStatement::LetTuple { initializer, .. }
        | BodyStatement::Assign {
            value: initializer, ..
        }
        | BodyStatement::Expression {
            expression: initializer,
            ..
        } => check_local_borrow_conflicts_in_expression(initializer, diagnostics),
        BodyStatement::While {
            condition, body, ..
        } => {
            check_local_borrow_conflicts_in_expression(condition, diagnostics);
            check_local_borrow_conflicts_in_expression(body, diagnostics);
        }
        BodyStatement::Loop { body, .. } => {
            check_local_borrow_conflicts_in_expression(body, diagnostics)
        }
        BodyStatement::ForRange {
            start, end, body, ..
        } => {
            check_local_borrow_conflicts_in_expression(start, diagnostics);
            check_local_borrow_conflicts_in_expression(end, diagnostics);
            check_local_borrow_conflicts_in_expression(body, diagnostics);
        }
        BodyStatement::ForEach { iterator, body, .. } => {
            check_local_borrow_conflicts_in_expression(iterator, diagnostics);
            check_local_borrow_conflicts_in_expression(body, diagnostics);
        }
        BodyStatement::MacroInvocation {
            arguments,
            ..
        } => {
            for argument in arguments {
                check_local_borrow_conflicts_in_expression(argument, diagnostics);
            }
        }
        BodyStatement::Break { .. }
        | BodyStatement::Continue { .. }
        | BodyStatement::Unsupported => {}
    }
}

fn borrowed_local_path(expression: &BodyExpr) -> Option<(&str, bool)> {
    match &expression.kind {
        BodyExprKind::Reference {
            mutable,
            expression,
        } => match &expression.kind {
            BodyExprKind::Path(name) => Some((name, *mutable)),
            _ => None,
        },
        BodyExprKind::Parenthesized(inner) => borrowed_local_path(inner),
        _ => None,
    }
}

fn local_used_after(
    name: &str,
    statements: &[BodyStatement],
    tail: Option<&BodyExpr>,
) -> bool {
    statements
        .iter()
        .any(|statement| statement_uses_local(statement, name))
        || tail.is_some_and(|tail| expression_uses_local(tail, name))
}

fn statement_uses_local(statement: &BodyStatement, name: &str) -> bool {
    match statement {
        BodyStatement::Let { initializer, .. } => initializer
            .as_ref()
            .is_some_and(|initializer| expression_uses_local(initializer, name)),
        BodyStatement::LetTuple { initializer, .. } => expression_uses_local(initializer, name),
        BodyStatement::Assign {
            name: assigned,
            value,
            ..
        } => assigned == name || expression_uses_local(value, name),
        BodyStatement::While {
            condition, body, ..
        } => expression_uses_local(condition, name) || expression_uses_local(body, name),
        BodyStatement::Loop { body, .. } => expression_uses_local(body, name),
        BodyStatement::ForRange {
            start, end, body, ..
        } => {
            expression_uses_local(start, name)
                || expression_uses_local(end, name)
                || expression_uses_local(body, name)
        }
        BodyStatement::ForEach { iterator, body, .. } => {
            expression_uses_local(iterator, name) || expression_uses_local(body, name)
        }
        BodyStatement::MacroInvocation {
            arguments,
            format_string,
            ..
        } => {
            arguments
                .iter()
                .any(|argument| expression_uses_local(argument, name))
                || format_string
                    .as_deref()
                    .is_some_and(|format| format_uses_capture(format, name))
        }
        BodyStatement::Expression { expression, .. } => expression_uses_local(expression, name),
        BodyStatement::Break { .. }
        | BodyStatement::Continue { .. }
        | BodyStatement::Unsupported => false,
    }
}

fn expression_uses_local(expression: &BodyExpr, name: &str) -> bool {
    match &expression.kind {
        BodyExprKind::Path(path) => path == name,
        BodyExprKind::Reference { expression, .. }
        | BodyExprKind::Cast { expression, .. }
        | BodyExprKind::Negate(expression)
        | BodyExprKind::Not(expression)
        | BodyExprKind::Try(expression)
        | BodyExprKind::Parenthesized(expression)
        | BodyExprKind::Dereference(expression) => expression_uses_local(expression, name),
        BodyExprKind::Call { arguments, .. }
        | BodyExprKind::Tuple(arguments)
        | BodyExprKind::ArrayLiteral(arguments)
        | BodyExprKind::FixedArrayLiteral(arguments) => arguments
            .iter()
            .any(|argument| expression_uses_local(argument, name)),
        BodyExprKind::MethodCall {
            receiver,
            arguments,
            ..
        } => {
            expression_uses_local(receiver, name)
                || arguments
                    .iter()
                    .any(|argument| expression_uses_local(argument, name))
        }
        BodyExprKind::Closure { body, .. } => expression_uses_local(body, name),
        BodyExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .any(|(_, value)| expression_uses_local(value, name)),
        BodyExprKind::FieldAccess { base, .. } => expression_uses_local(base, name),
        BodyExprKind::Index { base, index, .. } => {
            expression_uses_local(base, name) || expression_uses_local(index, name)
        }
        BodyExprKind::Binary { left, right, .. } => {
            expression_uses_local(left, name) || expression_uses_local(right, name)
        }
        BodyExprKind::Block { statements, tail } => {
            local_used_after(name, statements, tail.as_deref())
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expression_uses_local(condition, name)
                || expression_uses_local(then_branch, name)
                || else_branch
                    .as_deref()
                    .is_some_and(|branch| expression_uses_local(branch, name))
        }
        BodyExprKind::Match { scrutinee, arms } => {
            expression_uses_local(scrutinee, name)
                || arms
                    .iter()
                    .any(|arm| expression_uses_local(&arm.body, name))
        }
        BodyExprKind::MacroInvocation(macro_expression) => {
            macro_expression
                .arguments
                .iter()
                .any(|argument| expression_uses_local(argument, name))
                || macro_expression
                    .format_string
                    .as_deref()
                    .is_some_and(|format| format_uses_capture(format, name))
        }
        BodyExprKind::Return(Some(value)) => expression_uses_local(value, name),
        BodyExprKind::Unit
        | BodyExprKind::Literal(_)
        | BodyExprKind::EmptySliceReference
        | BodyExprKind::Return(None)
        | BodyExprKind::Unsupported => false,
    }
}

fn format_uses_capture(format: &str, name: &str) -> bool {
    format.split('{').skip(1).any(|field| {
        field
            .split('}')
            .next()
            .is_some_and(|field| field.split(':').next().unwrap_or(field) == name)
    })
}

fn check_move_expression(
    expression: &BodyExpr,
    consume: bool,
    signatures: &SignatureIndex<'_>,
    struct_signatures: &[StructSignature],
    locals: &mut HashMap<String, Ty>,
    moved: &mut HashSet<String>,
    diagnostics: &mut Vec<TypeDiagnostic>,
) {
    match &expression.kind {
        BodyExprKind::Path(name) => {
            let Some(ty) = locals.get(name) else {
                return;
            };
            if moved.contains(name) {
                diagnostics.push(TypeDiagnostic {
                    span: expression.span.clone(),
                    message: format!(
                        "error[E0382]: {} of moved value: `{name}`",
                        if consume { "use" } else { "borrow" }
                    ),
                });
            } else if consume && !is_copy_type(ty, struct_signatures) {
                moved.insert(name.clone());
            }
        }
        BodyExprKind::Reference { expression, .. } => check_move_expression(
            expression,
            false,
            signatures,
            struct_signatures,
            locals,
            moved,
            diagnostics,
        ),
        BodyExprKind::Parenthesized(inner) => check_move_expression(
            inner,
            consume,
            signatures,
            struct_signatures,
            locals,
            moved,
            diagnostics,
        ),
        BodyExprKind::Block { statements, tail } => {
            let original_locals = locals.clone();
            for statement in statements {
                match statement {
                    BodyStatement::Let {
                        name,
                        type_text,
                        initializer,
                        ..
                    } => {
                        let binding_type = initializer.as_ref().map(|initializer| {
                            check_move_expression(
                                initializer,
                                true,
                                signatures,
                                struct_signatures,
                                locals,
                                moved,
                                diagnostics,
                            );
                            let mut ignored = Vec::new();
                            infer_with_structs(
                                initializer,
                                locals,
                                signatures,
                                struct_signatures,
                                &Ty::Unknown,
                                &mut ignored,
                            )
                        });
                        if let Some(name) = name {
                            let ty = type_text
                                .as_deref()
                                .map(|text| type_from_syntax(text, struct_signatures))
                                .or(binding_type)
                                .unwrap_or(Ty::Unknown);
                            locals.insert(name.clone(), ty);
                            moved.remove(name);
                        }
                    }
                    BodyStatement::LetTuple {
                        initializer, names, ..
                    } => {
                        check_move_expression(
                            initializer,
                            true,
                            signatures,
                            struct_signatures,
                            locals,
                            moved,
                            diagnostics,
                        );
                        let mut ignored = Vec::new();
                        let ty = infer_with_structs(
                            initializer,
                            locals,
                            signatures,
                            struct_signatures,
                            &Ty::Unknown,
                            &mut ignored,
                        );
                        if let Ty::Tuple(types) = ty {
                            locals.extend(names.iter().cloned().zip(types));
                        }
                    }
                    BodyStatement::Assign { name, value, .. } => {
                        check_move_expression(
                            value,
                            true,
                            signatures,
                            struct_signatures,
                            locals,
                            moved,
                            diagnostics,
                        );
                        moved.remove(name);
                    }
                    BodyStatement::Expression { expression, .. } => check_move_expression(
                        expression,
                        true,
                        signatures,
                        struct_signatures,
                        locals,
                        moved,
                        diagnostics,
                    ),
                    BodyStatement::MacroInvocation {
                        path,
                        tokens,
                        format_string,
                        span,
                        ..
                    } if path == "println" => {
                        check_format_capture_moves(
                            path,
                            tokens,
                            format_string.as_deref(),
                            span,
                            locals,
                            moved,
                            struct_signatures,
                            diagnostics,
                        );
                    }
                    BodyStatement::While {
                        condition, body, ..
                    } => {
                        check_move_expression(
                            condition,
                            false,
                            signatures,
                            struct_signatures,
                            locals,
                            moved,
                            diagnostics,
                        );
                        let mut loop_locals = locals.clone();
                        let mut loop_moved = moved.clone();
                        check_move_expression(
                            body,
                            false,
                            signatures,
                            struct_signatures,
                            &mut loop_locals,
                            &mut loop_moved,
                            diagnostics,
                        );
                        moved.extend(loop_moved);
                    }
                    BodyStatement::ForRange {
                        start, end, body, ..
                    } => {
                        for value in [start, end, body] {
                            check_move_expression(
                                value,
                                false,
                                signatures,
                                struct_signatures,
                                locals,
                                moved,
                                diagnostics,
                            );
                        }
                    }
                    BodyStatement::ForEach { iterator, body, .. } => {
                        check_move_expression(
                            iterator,
                            true,
                            signatures,
                            struct_signatures,
                            locals,
                            moved,
                            diagnostics,
                        );
                        check_move_expression(
                            body,
                            false,
                            signatures,
                            struct_signatures,
                            locals,
                            moved,
                            diagnostics,
                        );
                    }
                    _ => {}
                }
            }
            if let Some(tail) = tail {
                check_move_expression(
                    tail,
                    consume,
                    signatures,
                    struct_signatures,
                    locals,
                    moved,
                    diagnostics,
                );
            }
            *locals = original_locals;
        }
        BodyExprKind::Call { arguments, .. } => {
            for argument in arguments {
                check_move_expression(
                    argument,
                    true,
                    signatures,
                    struct_signatures,
                    locals,
                    moved,
                    diagnostics,
                );
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } => {
            check_move_expression(
                receiver,
                matches!(
                    method.as_str(),
                    "into_iter" | "into_bytes" | "unwrap" | "expect"
                ),
                signatures,
                struct_signatures,
                locals,
                moved,
                diagnostics,
            );
            for argument in arguments {
                check_move_expression(
                    argument,
                    true,
                    signatures,
                    struct_signatures,
                    locals,
                    moved,
                    diagnostics,
                );
            }
        }
        BodyExprKind::Tuple(elements)
        | BodyExprKind::ArrayLiteral(elements)
        | BodyExprKind::FixedArrayLiteral(elements) => {
            for element in elements {
                check_move_expression(
                    element,
                    true,
                    signatures,
                    struct_signatures,
                    locals,
                    moved,
                    diagnostics,
                );
            }
        }
        BodyExprKind::Binary { left, right, .. } => {
            check_move_expression(
                left,
                true,
                signatures,
                struct_signatures,
                locals,
                moved,
                diagnostics,
            );
            check_move_expression(
                right,
                true,
                signatures,
                struct_signatures,
                locals,
                moved,
                diagnostics,
            );
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            check_move_expression(
                condition,
                false,
                signatures,
                struct_signatures,
                locals,
                moved,
                diagnostics,
            );
            let mut then_locals = locals.clone();
            let mut then_moved = moved.clone();
            check_move_expression(
                then_branch,
                consume,
                signatures,
                struct_signatures,
                &mut then_locals,
                &mut then_moved,
                diagnostics,
            );
            let mut else_locals = locals.clone();
            let mut else_moved = moved.clone();
            if let Some(else_branch) = else_branch {
                check_move_expression(
                    else_branch,
                    consume,
                    signatures,
                    struct_signatures,
                    &mut else_locals,
                    &mut else_moved,
                    diagnostics,
                );
            }
            moved.extend(then_moved);
            moved.extend(else_moved);
        }
        BodyExprKind::Match { scrutinee, arms } => {
            let binds_scrutinee = arms
                .iter()
                .any(|arm| match_pattern_binds_by_value(&arm.pattern));
            let scrutinee_type = if binds_scrutinee {
                let mut ignored = Vec::new();
                Some(infer_with_structs(
                    scrutinee,
                    locals,
                    signatures,
                    struct_signatures,
                    &Ty::Unknown,
                    &mut ignored,
                ))
            } else {
                None
            };
            if !matches!(&scrutinee.kind, BodyExprKind::Path(_)) {
                check_move_expression(
                    scrutinee,
                    binds_scrutinee,
                    signatures,
                    struct_signatures,
                    locals,
                    moved,
                    diagnostics,
                );
            }
            let mut merged = moved.clone();
            for arm in arms {
                let mut arm_locals = locals.clone();
                let mut arm_moved = moved.clone();
                if match_pattern_binds_by_value(&arm.pattern)
                    && matches!(&scrutinee.kind, BodyExprKind::Path(_))
                {
                    check_move_expression(
                        scrutinee,
                        true,
                        signatures,
                        struct_signatures,
                        &mut arm_locals,
                        &mut arm_moved,
                        diagnostics,
                    );
                }
                if let Some(scrutinee_type) = &scrutinee_type {
                    let mut bindings = Vec::new();
                    collect_match_pattern_bindings(&arm.pattern, &mut bindings);
                    for name in bindings {
                        arm_locals.insert(name.clone(), scrutinee_type.clone());
                        arm_moved.remove(&name);
                    }
                }
                check_move_expression(
                    &arm.body,
                    consume,
                    signatures,
                    struct_signatures,
                    &mut arm_locals,
                    &mut arm_moved,
                    diagnostics,
                );
                merged.extend(arm_moved);
            }
            *moved = merged;
        }
        BodyExprKind::Return(Some(value)) => check_move_expression(
            value,
            true,
            signatures,
            struct_signatures,
            locals,
            moved,
            diagnostics,
        ),
        _ => {}
    }
}

fn match_pattern_binds_by_value(pattern: &MatchPattern) -> bool {
    match pattern {
        MatchPattern::Binding(_) => true,
        MatchPattern::Or(patterns) => patterns.iter().any(match_pattern_binds_by_value),
        _ => false,
    }
}

fn collect_match_pattern_bindings(pattern: &MatchPattern, bindings: &mut Vec<String>) {
    match pattern {
        MatchPattern::Binding(name) => bindings.push(name.clone()),
        MatchPattern::Or(patterns) => {
            for pattern in patterns {
                collect_match_pattern_bindings(pattern, bindings);
            }
        }
        _ => {}
    }
}

fn check_format_capture_moves(
    path: &str,
    tokens: &str,
    format: Option<&str>,
    macro_span: &Range<usize>,
    locals: &HashMap<String, Ty>,
    moved: &HashSet<String>,
    struct_signatures: &[StructSignature],
    diagnostics: &mut Vec<TypeDiagnostic>,
) {
    let Some(format) = format else {
        return;
    };
    let mut cursor = 0;
    while let Some(open) = format[cursor..].find('{') {
        let open = cursor + open;
        let Some(close_relative) = format[open + 1..].find('}') else {
            break;
        };
        let close = open + 1 + close_relative;
        let name = &format[open + 1..close];
        if !name.is_empty()
            && name
                .chars()
                .all(|character| character == '_' || character.is_alphanumeric())
            && moved.contains(name)
            && locals
                .get(name)
                .is_some_and(|ty| !is_copy_type(ty, struct_signatures))
        {
            let placeholder = format!("{{{name}}}");
            if let Some(token_offset) = tokens.find(&placeholder) {
                let capture_start = macro_span.start + path.len() + 2 + token_offset + 1;
                diagnostics.push(TypeDiagnostic {
                    span: capture_start..capture_start + name.len(),
                    message: format!("error[E0382]: borrow of moved value: `{name}`"),
                });
            }
        }
        cursor = close + 1;
    }
}

fn check_call_targets(
    expression: &BodyExpr,
    locals: &mut HashSet<String>,
    signatures: &SignatureIndex<'_>,
    diagnostics: &mut Vec<TypeDiagnostic>,
) {
    match &expression.kind {
        BodyExprKind::Call {
            callee, arguments, ..
        } => {
            if !matches!(
                callee.as_str(),
                "std::process::exit"
                    | "Ok"
                    | "Err"
                    | "Some"
                    | "Vec::new"
                    | "std::vec::Vec::new"
                    | "BTreeMap::new"
                    | "std::collections::BTreeMap::new"
                    | "String::new"
                    | "String::from"
                    | "String::with_capacity"
                    | "std::string::String::with_capacity"
                    | "u64::from"
            ) && !is_option_some_path(callee)
                && !locals.contains(callee)
            {
                let candidates = matching_signatures(signatures, callee);
                match candidates.as_slice() {
                    [] => diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!("unresolved function `{callee}`"),
                    }),
                    [_, _, ..] => diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "ambiguous function call `{callee}` (candidates: {})",
                            candidates
                                .iter()
                                .map(|candidate| candidate.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    }),
                    [_] => {}
                }
            }
            for argument in arguments {
                check_call_targets(argument, locals, signatures, diagnostics);
            }
        }
        BodyExprKind::MacroInvocation(macro_expression) => {
            for argument in &macro_expression.arguments {
                check_call_targets(argument, locals, signatures, diagnostics);
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            arguments,
            ..
        } => {
            check_call_targets(receiver, locals, signatures, diagnostics);
            for argument in arguments {
                check_call_targets(argument, locals, signatures, diagnostics);
            }
        }
        BodyExprKind::Closure { parameters, body } => {
            let mut scoped = locals.clone();
            for parameter in parameters {
                match parameter {
                    ClosureParameter::Identifier(name) => {
                        scoped.insert(name.clone());
                    }
                    ClosureParameter::Tuple(names) => scoped.extend(names.iter().cloned()),
                }
            }
            check_call_targets(body, &mut scoped, signatures, diagnostics);
        }
        BodyExprKind::Tuple(elements)
        | BodyExprKind::ArrayLiteral(elements)
        | BodyExprKind::FixedArrayLiteral(elements) => {
            for element in elements {
                check_call_targets(element, locals, signatures, diagnostics);
            }
        }
        BodyExprKind::Index { base, index, .. } => {
            check_call_targets(base, locals, signatures, diagnostics);
            check_call_targets(index, locals, signatures, diagnostics);
        }
        BodyExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                check_call_targets(value, locals, signatures, diagnostics);
            }
        }
        BodyExprKind::FieldAccess { base, .. } => {
            check_call_targets(base, locals, signatures, diagnostics);
        }
        BodyExprKind::Reference { expression, .. } | BodyExprKind::Dereference(expression) => {
            check_call_targets(expression, locals, signatures, diagnostics);
        }
        BodyExprKind::Cast { expression, .. } => {
            check_call_targets(expression, locals, signatures, diagnostics);
        }
        BodyExprKind::Binary { left, right, .. } => {
            check_call_targets(left, locals, signatures, diagnostics);
            check_call_targets(right, locals, signatures, diagnostics);
        }
        BodyExprKind::Negate(value)
        | BodyExprKind::Not(value)
        | BodyExprKind::Try(value)
        | BodyExprKind::Parenthesized(value) => {
            check_call_targets(value, locals, signatures, diagnostics);
        }
        BodyExprKind::Block { statements, tail } => {
            let mut scoped = locals.clone();
            for statement in statements {
                match statement {
                    BodyStatement::Let {
                        name, initializer, ..
                    } => {
                        if let Some(initializer) = initializer {
                            check_call_targets(initializer, &mut scoped, signatures, diagnostics);
                        }
                        if let Some(name) = name {
                            scoped.insert(name.clone());
                        }
                    }
                    BodyStatement::LetTuple {
                        names, initializer, ..
                    } => {
                        check_call_targets(initializer, &mut scoped, signatures, diagnostics);
                        scoped.extend(names.iter().cloned());
                    }
                    BodyStatement::Assign { value, .. } => {
                        check_call_targets(value, &mut scoped, signatures, diagnostics);
                    }
                    BodyStatement::While {
                        condition, body, ..
                    } => {
                        check_call_targets(condition, &mut scoped, signatures, diagnostics);
                        check_call_targets(body, &mut scoped.clone(), signatures, diagnostics);
                    }
                    BodyStatement::Loop { body, .. } => {
                        check_call_targets(body, &mut scoped.clone(), signatures, diagnostics);
                    }
                    BodyStatement::ForRange {
                        name,
                        start,
                        end,
                        body,
                        ..
                    } => {
                        check_call_targets(start, &mut scoped, signatures, diagnostics);
                        check_call_targets(end, &mut scoped, signatures, diagnostics);
                        let mut loop_locals = scoped.clone();
                        loop_locals.insert(name.clone());
                        check_call_targets(body, &mut loop_locals, signatures, diagnostics);
                    }
                    BodyStatement::ForEach {
                        names,
                        iterator,
                        body,
                        ..
                    } => {
                        check_call_targets(iterator, &mut scoped, signatures, diagnostics);
                        let mut loop_locals = scoped.clone();
                        loop_locals.extend(names.iter().cloned());
                        check_call_targets(body, &mut loop_locals, signatures, diagnostics);
                    }
                    BodyStatement::Expression { expression, .. } => {
                        check_call_targets(expression, &mut scoped, signatures, diagnostics);
                    }
                    BodyStatement::Break { .. }
                    | BodyStatement::Continue { .. }
                    | BodyStatement::MacroInvocation { .. }
                    | BodyStatement::Unsupported => {}
                }
            }
            if let Some(tail) = tail {
                check_call_targets(tail, &mut scoped, signatures, diagnostics);
            }
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            check_call_targets(condition, locals, signatures, diagnostics);
            check_call_targets(then_branch, &mut locals.clone(), signatures, diagnostics);
            if let Some(branch) = else_branch {
                check_call_targets(branch, &mut locals.clone(), signatures, diagnostics);
            }
        }
        BodyExprKind::Match { scrutinee, arms } => {
            check_call_targets(scrutinee, locals, signatures, diagnostics);
            for arm in arms {
                check_call_targets(&arm.body, &mut locals.clone(), signatures, diagnostics);
            }
        }
        BodyExprKind::Return(Some(value)) => {
            check_call_targets(value, locals, signatures, diagnostics);
        }
        BodyExprKind::Unit
        | BodyExprKind::Literal(_)
        | BodyExprKind::EmptySliceReference
        | BodyExprKind::Path(_)
        | BodyExprKind::Return(None)
        | BodyExprKind::Unsupported => {}
    }
}

fn check_loop_control(expression: &BodyExpr, depth: usize, diagnostics: &mut Vec<TypeDiagnostic>) {
    match &expression.kind {
        BodyExprKind::Block { statements, tail } => {
            for statement in statements {
                match statement {
                    BodyStatement::Break { span } | BodyStatement::Continue { span }
                        if depth == 0 =>
                    {
                        diagnostics.push(TypeDiagnostic {
                            span: span.clone(),
                            message: "loop control outside of a loop".to_owned(),
                        })
                    }
                    BodyStatement::While {
                        condition, body, ..
                    } => {
                        check_loop_control(condition, depth, diagnostics);
                        check_loop_control(body, depth + 1, diagnostics);
                    }
                    BodyStatement::Loop { body, .. } => {
                        check_loop_control(body, depth + 1, diagnostics)
                    }
                    BodyStatement::ForRange {
                        start, end, body, ..
                    } => {
                        check_loop_control(start, depth, diagnostics);
                        check_loop_control(end, depth, diagnostics);
                        check_loop_control(body, depth + 1, diagnostics);
                    }
                    BodyStatement::ForEach { iterator, body, .. } => {
                        check_loop_control(iterator, depth, diagnostics);
                        check_loop_control(body, depth + 1, diagnostics);
                    }
                    BodyStatement::Let {
                        initializer: Some(value),
                        ..
                    }
                    | BodyStatement::Assign { value, .. } => {
                        check_loop_control(value, depth, diagnostics)
                    }
                    BodyStatement::Expression { expression, .. } => {
                        check_loop_control(expression, depth, diagnostics)
                    }
                    _ => {}
                }
            }
            if let Some(tail) = tail {
                check_loop_control(tail, depth, diagnostics);
            }
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            check_loop_control(condition, depth, diagnostics);
            check_loop_control(then_branch, depth, diagnostics);
            if let Some(branch) = else_branch {
                check_loop_control(branch, depth, diagnostics);
            }
        }
        BodyExprKind::Match { scrutinee, arms } => {
            check_loop_control(scrutinee, depth, diagnostics);
            for arm in arms {
                check_loop_control(&arm.body, depth, diagnostics);
            }
        }
        BodyExprKind::Call { arguments, .. } => {
            for argument in arguments {
                check_loop_control(argument, depth, diagnostics);
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            arguments,
            ..
        } => {
            check_loop_control(receiver, depth, diagnostics);
            for argument in arguments {
                check_loop_control(argument, depth, diagnostics);
            }
        }
        BodyExprKind::Closure { body, .. } => check_loop_control(body, depth, diagnostics),
        BodyExprKind::Tuple(elements)
        | BodyExprKind::ArrayLiteral(elements)
        | BodyExprKind::FixedArrayLiteral(elements) => {
            for element in elements {
                check_loop_control(element, depth, diagnostics);
            }
        }
        BodyExprKind::Index { base, index, .. } => {
            check_loop_control(base, depth, diagnostics);
            check_loop_control(index, depth, diagnostics);
        }
        BodyExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                check_loop_control(value, depth, diagnostics);
            }
        }
        BodyExprKind::FieldAccess { base, .. }
        | BodyExprKind::Reference {
            expression: base, ..
        }
        | BodyExprKind::Dereference(base)
        | BodyExprKind::Cast {
            expression: base, ..
        } => check_loop_control(base, depth, diagnostics),
        BodyExprKind::Binary { left, right, .. } => {
            check_loop_control(left, depth, diagnostics);
            check_loop_control(right, depth, diagnostics);
        }
        BodyExprKind::Negate(value)
        | BodyExprKind::Not(value)
        | BodyExprKind::Try(value)
        | BodyExprKind::Parenthesized(value) => check_loop_control(value, depth, diagnostics),
        BodyExprKind::Return(Some(value)) => check_loop_control(value, depth, diagnostics),
        _ => {}
    }
}

fn check_mutability(
    expression: &BodyExpr,
    locals: &mut HashMap<String, bool>,
    diagnostics: &mut Vec<TypeDiagnostic>,
) {
    match &expression.kind {
        BodyExprKind::Call { arguments, .. } => {
            for argument in arguments {
                check_mutability(argument, locals, diagnostics);
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            arguments,
            ..
        } => {
            check_mutability(receiver, locals, diagnostics);
            for argument in arguments {
                check_mutability(argument, locals, diagnostics);
            }
        }
        BodyExprKind::Closure { parameters, body } => {
            let mut scoped = locals.clone();
            for parameter in parameters {
                match parameter {
                    ClosureParameter::Identifier(name) => {
                        scoped.insert(name.clone(), false);
                    }
                    ClosureParameter::Tuple(names) => {
                        scoped.extend(names.iter().map(|name| (name.clone(), false)));
                    }
                }
            }
            check_mutability(body, &mut scoped, diagnostics);
        }
        BodyExprKind::Tuple(elements)
        | BodyExprKind::ArrayLiteral(elements)
        | BodyExprKind::FixedArrayLiteral(elements) => {
            for element in elements {
                check_mutability(element, locals, diagnostics);
            }
        }
        BodyExprKind::Index { base, index, .. } => {
            check_mutability(base, locals, diagnostics);
            check_mutability(index, locals, diagnostics);
        }
        BodyExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                check_mutability(value, locals, diagnostics);
            }
        }
        BodyExprKind::FieldAccess { base, .. }
        | BodyExprKind::Reference {
            expression: base, ..
        }
        | BodyExprKind::Dereference(base)
        | BodyExprKind::Cast {
            expression: base, ..
        } => check_mutability(base, locals, diagnostics),
        BodyExprKind::Binary { left, right, .. } => {
            check_mutability(left, locals, diagnostics);
            check_mutability(right, locals, diagnostics);
        }
        BodyExprKind::Negate(operand)
        | BodyExprKind::Not(operand)
        | BodyExprKind::Try(operand)
        | BodyExprKind::Parenthesized(operand) => check_mutability(operand, locals, diagnostics),
        BodyExprKind::Block { statements, tail } => {
            let mut scoped = locals.clone();
            for statement in statements {
                match statement {
                    BodyStatement::Let {
                        name,
                        mutable,
                        initializer,
                        ..
                    } => {
                        if let Some(initializer) = initializer {
                            check_mutability(initializer, &mut scoped, diagnostics);
                        }
                        if let Some(name) = name {
                            scoped.insert(name.clone(), *mutable);
                        }
                    }
                    BodyStatement::LetTuple {
                        names, initializer, ..
                    } => {
                        check_mutability(initializer, &mut scoped, diagnostics);
                        scoped.extend(names.iter().cloned().map(|name| (name, false)));
                    }
                    BodyStatement::Assign { name, value, span } => {
                        check_mutability(value, &mut scoped, diagnostics);
                        let binding = name.split_once('.').map_or(name.as_str(), |(base, _)| base);
                        if scoped.get(binding) == Some(&false) {
                            diagnostics.push(TypeDiagnostic {
                                span: span.clone(),
                                message: format!(
                                    "cannot assign twice to immutable variable `{name}`"
                                ),
                            });
                        }
                    }
                    BodyStatement::While {
                        condition, body, ..
                    } => {
                        check_mutability(condition, &mut scoped, diagnostics);
                        check_mutability(body, &mut scoped.clone(), diagnostics);
                    }
                    BodyStatement::Loop { body, .. } => {
                        check_mutability(body, &mut scoped.clone(), diagnostics)
                    }
                    BodyStatement::Expression { expression, .. } => {
                        check_mutability(expression, &mut scoped, diagnostics);
                    }
                    BodyStatement::Break { .. } | BodyStatement::Continue { .. } => {}
                    BodyStatement::ForRange {
                        name,
                        start,
                        end,
                        body,
                        ..
                    } => {
                        check_mutability(start, &mut scoped, diagnostics);
                        check_mutability(end, &mut scoped, diagnostics);
                        let mut loop_locals = scoped.clone();
                        loop_locals.insert(name.clone(), false);
                        check_mutability(body, &mut loop_locals, diagnostics);
                    }
                    BodyStatement::ForEach {
                        names,
                        iterator,
                        body,
                        ..
                    } => {
                        check_mutability(iterator, &mut scoped, diagnostics);
                        let mut loop_locals = scoped.clone();
                        loop_locals.extend(names.iter().cloned().map(|name| (name, false)));
                        check_mutability(body, &mut loop_locals, diagnostics);
                    }
                    BodyStatement::MacroInvocation { .. } | BodyStatement::Unsupported => {}
                }
            }
            if let Some(tail) = tail {
                check_mutability(tail, &mut scoped, diagnostics);
            }
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            check_mutability(condition, locals, diagnostics);
            check_mutability(then_branch, &mut locals.clone(), diagnostics);
            if let Some(else_branch) = else_branch {
                check_mutability(else_branch, &mut locals.clone(), diagnostics);
            }
        }
        BodyExprKind::Match { scrutinee, arms } => {
            check_mutability(scrutinee, locals, diagnostics);
            for arm in arms {
                check_mutability(&arm.body, &mut locals.clone(), diagnostics);
            }
        }
        BodyExprKind::Return(value) => {
            if let Some(value) = value {
                check_mutability(value, locals, diagnostics);
            }
        }
        BodyExprKind::MacroInvocation(macro_expression) => {
            for argument in &macro_expression.arguments {
                check_mutability(argument, locals, diagnostics);
            }
        }
        BodyExprKind::Unit
        | BodyExprKind::Literal(_)
        | BodyExprKind::EmptySliceReference
        | BodyExprKind::Path(_)
        | BodyExprKind::Unsupported => {}
    }
}

fn infer_with_structs(
    expression: &BodyExpr,
    locals: &mut HashMap<String, Ty>,
    signatures: &SignatureIndex<'_>,
    struct_signatures: &[StructSignature],
    expected_return: &Ty,
    diagnostics: &mut Vec<TypeDiagnostic>,
) -> Ty {
    match &expression.kind {
        BodyExprKind::Unit => Ty::Unit,
        BodyExprKind::Literal(literal) => literal_type(literal),
        BodyExprKind::Path(path) => locals
            .get(path.rsplit("::").next().unwrap_or(path))
            .cloned()
            .or_else(|| {
                (is_option_none_path(path) && matches!(expected_return, Ty::Option(_)))
                    .then(|| expected_return.clone())
            })
            .or_else(|| {
                find_enum_variant(struct_signatures, path)
                    .map(|(enumeration, _)| Ty::Named(enumeration.path.clone()))
            })
            .or_else(|| {
                find_struct_signature(struct_signatures, path)
                    .filter(|structure| {
                        structure.fields.is_empty() && structure.generic_parameters.is_empty()
                    })
                    .map(|structure| Ty::Named(structure.path.clone()))
            })
            .unwrap_or(Ty::Unknown),
        BodyExprKind::Reference { expression, .. } => Ty::Reference(Box::new(infer_with_structs(
            expression,
            locals,
            signatures,
            struct_signatures,
            expected_return,
            diagnostics,
        ))),
        BodyExprKind::Dereference(expression) => {
            let ty = infer_with_structs(
                expression,
                locals,
                signatures,
                struct_signatures,
                expected_return,
                diagnostics,
            );
            match ty {
                Ty::Reference(inner) => *inner,
                Ty::Unknown => Ty::Unknown,
                other => {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!("cannot dereference `{}`", other.display_name()),
                    });
                    Ty::Unknown
                }
            }
        }
        BodyExprKind::Cast {
            expression: inner,
            type_name,
        } => {
            let source = infer_with_structs(
                inner,
                locals,
                signatures,
                struct_signatures,
                expected_return,
                diagnostics,
            );
            let target = type_from_syntax(type_name, struct_signatures);
            if source.is_numeric() && target.is_numeric() {
                target
            } else {
                diagnostics.push(TypeDiagnostic {
                    span: expression.span.clone(),
                    message: format!(
                        "cast requires numeric source and target types, found `{}` as `{type_name}`",
                        source.display_name()
                    ),
                });
                Ty::Unknown
            }
        }
        BodyExprKind::StructLiteral { path, fields } => {
            let Some(structure) = find_struct_signature(struct_signatures, path) else {
                diagnostics.push(TypeDiagnostic {
                    span: expression.span.clone(),
                    message: format!("unknown or ambiguous struct `{path}`"),
                });
                return Ty::Unknown;
            };
            let mut seen = HashSet::new();
            for (name, value) in fields {
                if !seen.insert(name.as_str()) {
                    diagnostics.push(TypeDiagnostic {
                        span: value.span.clone(),
                        message: format!("duplicate struct field `{name}`"),
                    });
                    continue;
                }
                let Some(field) = structure.fields.iter().find(|field| field.name == *name) else {
                    diagnostics.push(TypeDiagnostic {
                        span: value.span.clone(),
                        message: format!("struct `{path}` has no field `{name}`"),
                    });
                    infer_with_structs(
                        value,
                        locals,
                        signatures,
                        struct_signatures,
                        expected_return,
                        diagnostics,
                    );
                    continue;
                };
                let expected = type_from_syntax(&field.type_text, struct_signatures);
                let actual = infer_with_structs(
                    value,
                    locals,
                    signatures,
                    struct_signatures,
                    &expected,
                    diagnostics,
                );
                require_type(
                    diagnostics,
                    value.span.clone(),
                    &expected,
                    &actual,
                    "struct field initializer",
                );
            }
            for field in &structure.fields {
                if !seen.contains(field.name.as_str()) {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!("missing field `{}` in `{path}` literal", field.name),
                    });
                }
            }
            Ty::Named(structure.path.clone())
        }
        BodyExprKind::FieldAccess { base, field } => {
            let mut base_type = infer_with_structs(
                base,
                locals,
                signatures,
                struct_signatures,
                expected_return,
                diagnostics,
            );
            // Rust field lookup performs autoderef through shared and mutable
            // references before resolving a named struct field.
            while let Ty::Reference(referent) = base_type {
                base_type = *referent;
            }
            match base_type {
                Ty::Named(name) => {
                    let Some(structure) = find_struct_signature(struct_signatures, &name) else {
                        return Ty::Unknown;
                    };
                    match structure
                        .fields
                        .iter()
                        .find(|candidate| candidate.name == *field)
                    {
                        Some(field) => type_from_syntax(&field.type_text, struct_signatures),
                        None => {
                            diagnostics.push(TypeDiagnostic {
                                span: expression.span.clone(),
                                message: format!("struct `{name}` has no field `{field}`"),
                            });
                            Ty::Unknown
                        }
                    }
                }
                Ty::Tuple(fields) => match field.parse::<usize>() {
                    Ok(index) => fields.get(index).cloned().unwrap_or_else(|| {
                        diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!(
                                "tuple index `{index}` is outside a tuple with {} fields",
                                fields.len()
                            ),
                        });
                        Ty::Unknown
                    }),
                    Err(_) => {
                        diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!("tuple field `{field}` is not a numeric index"),
                        });
                        Ty::Unknown
                    }
                },
                Ty::Unknown => Ty::Unknown,
                actual => {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "cannot access field `{field}` on `{}`",
                            actual.display_name()
                        ),
                    });
                    Ty::Unknown
                }
            }
        }
        BodyExprKind::Index { base, index, .. } => {
            let base_type = infer_with_structs(
                base,
                locals,
                signatures,
                struct_signatures,
                expected_return,
                diagnostics,
            );
            let index_type = infer_with_structs(
                index,
                locals,
                signatures,
                struct_signatures,
                &Ty::Integer("usize".to_owned()),
                diagnostics,
            );
            if !index_type.is_integer() && index_type != Ty::Unknown {
                diagnostics.push(TypeDiagnostic {
                    span: index.span.clone(),
                    message: format!(
                        "array index must be an integer, found `{}`",
                        index_type.display_name()
                    ),
                });
            }
            match without_references(&base_type) {
                Ty::Array(element, _) | Ty::Slice(element) | Ty::Vector(element) => {
                    element.as_ref().clone()
                }
                _ => {
                    diagnostics.push(TypeDiagnostic {
                        span: base.span.clone(),
                        message: format!(
                            "cannot index value of type `{}`",
                            base_type.display_name()
                        ),
                    });
                    Ty::Unknown
                }
            }
        }
        BodyExprKind::Call {
            callee,
            generic_arguments,
            arguments,
        } => {
            if callee == "std::process::exit" {
                if arguments.len() != 1 {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "function `std::process::exit` expects 1 argument but received {}",
                            arguments.len()
                        ),
                    });
                } else {
                    let actual = infer_with_structs(
                        &arguments[0],
                        locals,
                        signatures,
                        struct_signatures,
                        &Ty::Integer("i32".to_owned()),
                        diagnostics,
                    );
                    require_type(
                        diagnostics,
                        arguments[0].span.clone(),
                        &Ty::Integer("i32".to_owned()),
                        &actual,
                        "function argument",
                    );
                }
                return Ty::Never;
            }
            if callee == "u64::from" {
                if arguments.len() != 1 {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "function `u64::from` expects 1 argument but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                let actual = infer_with_structs(
                    &arguments[0],
                    locals,
                    signatures,
                    struct_signatures,
                    &Ty::Unknown,
                    diagnostics,
                );
                if !matches!(&actual, Ty::Integer(name) if matches!(name.as_str(), "u8" | "u16" | "u32"))
                    && actual != Ty::InferInteger
                    && actual != Ty::Unknown
                {
                    diagnostics.push(TypeDiagnostic {
                        span: arguments[0].span.clone(),
                        message: format!("`u64::from` cannot convert `{}`", actual.display_name()),
                    });
                }
                return Ty::Integer("u64".to_owned());
            }
            if matches!(
                callee.as_str(),
                "String::from" | "std::string::String::from"
            ) {
                if arguments.len() != 1 {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "constructor `{callee}` expects 1 argument but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                let actual = infer_with_structs(
                    &arguments[0],
                    locals,
                    signatures,
                    struct_signatures,
                    &Ty::StringSlice,
                    diagnostics,
                );
                let string_compatible = matches!(actual, Ty::String | Ty::StringSlice)
                    || matches!(&actual, Ty::Reference(inner) if **inner == Ty::StringSlice);
                if !string_compatible && actual != Ty::Unknown {
                    diagnostics.push(TypeDiagnostic {
                        span: arguments[0].span.clone(),
                        message: format!(
                            "`{callee}` cannot construct a String from `{}`",
                            actual.display_name()
                        ),
                    });
                }
                return Ty::String;
            }
            let constructor_type = match callee.as_str() {
                "Vec::new" | "std::vec::Vec::new" => Some(Ty::Vector(Box::new(Ty::Unknown))),
                "BTreeMap::new" | "std::collections::BTreeMap::new" => {
                    Some(Ty::Map(Box::new(Ty::Unknown), Box::new(Ty::Unknown)))
                }
                "String::new" | "String::with_capacity" | "std::string::String::with_capacity" => {
                    Some(Ty::String)
                }
                _ => None,
            };
            if let Some(constructor_type) = constructor_type {
                let capacity_constructor = callee.ends_with("with_capacity");
                let expected_count = usize::from(capacity_constructor);
                if arguments.len() != expected_count {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!("constructor `{callee}` expects {expected_count} arguments but received {}", arguments.len()),
                    });
                    return Ty::Unknown;
                }
                if capacity_constructor {
                    let capacity_ty = Ty::Integer("usize".to_owned());
                    let actual = infer_with_structs(
                        &arguments[0],
                        locals,
                        signatures,
                        struct_signatures,
                        &capacity_ty,
                        diagnostics,
                    );
                    require_type(
                        diagnostics,
                        arguments[0].span.clone(),
                        &capacity_ty,
                        &actual,
                        "capacity argument",
                    );
                }
                if matches!(constructor_type, Ty::Vector(_) | Ty::Map(_, _)) {
                    return match expected_return {
                        Ty::Vector(element) if matches!(constructor_type, Ty::Vector(_)) => {
                            Ty::Vector(element.clone())
                        }
                        Ty::Map(key, value) if matches!(constructor_type, Ty::Map(_, _)) => {
                            Ty::Map(key.clone(), value.clone())
                        }
                        _ => constructor_type,
                    };
                }
                return constructor_type;
            }
            if is_option_some_path(callee) {
                if arguments.len() != 1 {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "variant `Some` expects 1 argument but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                if let Ty::Option(payload_type) = expected_return {
                    let actual = infer_with_structs(
                        &arguments[0],
                        locals,
                        signatures,
                        struct_signatures,
                        payload_type,
                        diagnostics,
                    );
                    require_type(
                        diagnostics,
                        arguments[0].span.clone(),
                        payload_type,
                        &actual,
                        "Option payload",
                    );
                    return expected_return.clone();
                }
                let payload_type = infer_with_structs(
                    &arguments[0],
                    locals,
                    signatures,
                    struct_signatures,
                    expected_return,
                    diagnostics,
                );
                return Ty::Option(Box::new(payload_type));
            }
            if matches!(callee.as_str(), "Ok" | "Err") {
                if arguments.len() != 1 {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "variant `{callee}` expects 1 argument but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                let Ty::Result(ok_type, err_type) = expected_return else {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!("cannot infer `{callee}` without an expected Result type"),
                    });
                    return Ty::Unknown;
                };
                let payload_type = if callee == "Ok" { ok_type } else { err_type };
                let actual = infer_with_structs(
                    &arguments[0],
                    locals,
                    signatures,
                    struct_signatures,
                    payload_type,
                    diagnostics,
                );
                require_type(
                    diagnostics,
                    arguments[0].span.clone(),
                    payload_type,
                    &actual,
                    "Result payload",
                );
                return expected_return.clone();
            }
            let argument_types: Vec<_> = arguments
                .iter()
                .map(|argument| {
                    infer_with_structs(
                        argument,
                        locals,
                        signatures,
                        struct_signatures,
                        expected_return,
                        diagnostics,
                    )
                })
                .collect();
            if locals.contains_key(callee) {
                return Ty::Unknown;
            }
            let candidates = matching_signatures(signatures, callee);
            if candidates.is_empty() {
                return Ty::Unknown;
            }
            if candidates.len() != 1 {
                return Ty::Unknown;
            }
            let function = candidates[0];
            if function.is_generic {
                let return_generic = function.return_type.as_deref().map(str::trim);
                let valid_shape = (1..=2).contains(&function.generic_parameters.len())
                    && (1..=2).contains(&function.parameters.len())
                    && function
                        .parameters
                        .iter()
                        .all(|parameter| parameter.name.is_some())
                    && function.generic_parameters.iter().all(|generic| {
                        function.parameters.iter().any(|parameter| {
                            type_mentions_identifier(&parameter.type_text, generic)
                        })
                    })
                    && function.return_type.as_deref().is_some();
                if function.generic_has_bounds || !valid_shape {
                    return Ty::Unknown;
                }
                if function.parameters.len() != argument_types.len() {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "generic function `{callee}` expects {} arguments but received {}",
                            function.parameters.len(),
                            argument_types.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                let mut explicit_substitutions = HashMap::new();
                if !generic_arguments.is_empty() {
                    if generic_arguments.len() != function.generic_parameters.len() {
                        diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!(
                                "generic function `{callee}` expects {} explicit type arguments but received {}",
                                function.generic_parameters.len(),
                                generic_arguments.len()
                            ),
                        });
                        return Ty::Unknown;
                    }
                    for (generic, type_text) in
                        function.generic_parameters.iter().zip(generic_arguments)
                    {
                        let ty = type_from_syntax(type_text, struct_signatures);
                        if !matches!(&ty, Ty::Integer(name) if matches!(name.as_str(), "i32" | "u64" | "usize"))
                            && !matches!(&ty, Ty::Named(_))
                        {
                            diagnostics.push(TypeDiagnostic {
                                span: expression.span.clone(),
                                message: format!(
                                    "explicit generic type `{type_text}` is outside the checked scalar subset"
                                ),
                            });
                            return Ty::Unknown;
                        }
                        explicit_substitutions.insert(generic.as_str(), ty);
                    }
                }
                let mut substitutions = HashMap::new();
                for (parameter, actual) in function.parameters.iter().zip(argument_types) {
                    for generic in &function.generic_parameters {
                        let Some(mut inferred) =
                            infer_generic_type(&parameter.type_text, &actual, generic)
                        else {
                            continue;
                        };
                        if inferred == Ty::InferInteger {
                            inferred = explicit_substitutions
                                .get(generic.as_str())
                                .cloned()
                                .or_else(|| {
                                    (Some(generic.as_str()) == return_generic
                                        && matches!(expected_return, Ty::Integer(_)))
                                    .then(|| expected_return.clone())
                                })
                                .unwrap_or_else(|| Ty::Integer("i32".to_owned()));
                        }
                        if let Some(explicit) = explicit_substitutions.get(generic.as_str())
                            && !types_compatible(explicit, &inferred)
                        {
                            mismatch(
                                diagnostics,
                                expression.span.clone(),
                                explicit,
                                &inferred,
                                "explicit generic argument",
                            );
                        }
                        if let Some(previous) = substitutions.get(generic.as_str()) {
                            if !types_compatible(previous, &inferred) {
                                mismatch(
                                    diagnostics,
                                    expression.span.clone(),
                                    previous,
                                    &inferred,
                                    "generic argument",
                                );
                            }
                        } else {
                            substitutions.insert(generic.as_str(), inferred);
                        }
                    }
                }
                for generic in &function.generic_copy_parameters {
                    match substitutions.get(generic.as_str()) {
                        Some(ty) if is_copy_type(ty, struct_signatures) => {}
                        // Generic parameter bodies are currently represented
                        // as Unknown here. Their bounds are checked when the
                        // outer function is instantiated; avoid a cascading
                        // false diagnostic for a forwarded type parameter.
                        Some(Ty::Unknown) => {}
                        Some(ty) => diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!(
                                "generic argument `{}` does not satisfy the `Copy` bound for `{generic}`",
                                ty.display_name()
                            ),
                        }),
                        None => diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!(
                                "cannot infer a type satisfying the `Copy` bound for `{generic}`"
                            ),
                        }),
                    }
                }
                for generic in &function.generic_as_ref_str_parameters {
                    match substitutions.get(generic.as_str()) {
                        Some(ty) if is_as_ref_str_type(ty, struct_signatures) => {}
                        Some(Ty::Unknown) => {}
                        Some(ty) => diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!(
                                "generic argument `{}` does not satisfy `AsRef<str>` for `{generic}`",
                                ty.display_name()
                            ),
                        }),
                        None => diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!(
                                "cannot infer a type satisfying `AsRef<str>` for `{generic}`"
                            ),
                        }),
                    }
                }
                for generic in &function.generic_add_parameters {
                    match substitutions.get(generic.as_str()) {
                        Some(ty) if ty.is_numeric() => {}
                        Some(ty @ Ty::Named(name))
                            if name.starts_with("\0quill_generic:") && is_add_type(ty) => {}
                        Some(Ty::Unknown) => {}
                        Some(ty) => diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!(
                                "generic argument `{}` does not satisfy the `Add<Output = T>` bound for `{generic}`",
                                ty.display_name()
                            ),
                        }),
                        None => diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!(
                                "cannot infer a type satisfying `Add<Output = T>` for `{generic}`"
                            ),
                        }),
                    }
                }
                for (generic, trait_name) in &function.generic_trait_bounds {
                    match substitutions.get(generic.as_str()) {
                        Some(Ty::Named(name)) if name.starts_with("\0quill_generic:") => {}
                        Some(Ty::Named(name))
                            if trait_is_implemented_for_type(signatures, trait_name, name) => {}
                        Some(Ty::Unknown) => {}
                        Some(ty) => diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!(
                                "generic argument `{}` does not implement `{trait_name}` for `{generic}`",
                                ty.display_name()
                            ),
                        }),
                        None => diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!(
                                "cannot infer a type satisfying `{trait_name}` for `{generic}`"
                            ),
                        }),
                    }
                }
                return return_generic
                    .and_then(|generic| substitutions.remove(generic))
                    .unwrap_or_else(|| {
                        type_from_syntax(
                            function.return_type.as_deref().unwrap_or(""),
                            struct_signatures,
                        )
                    });
            }
            if function.parameters.len() != argument_types.len() {
                diagnostics.push(TypeDiagnostic {
                    span: expression.span.clone(),
                    message: format!(
                        "function `{callee}` expects {} arguments but received {}",
                        function.parameters.len(),
                        argument_types.len()
                    ),
                });
            } else {
                for (parameter, actual) in function.parameters.iter().zip(argument_types) {
                    let expected = type_from_syntax(&parameter.type_text, struct_signatures);
                    if !types_compatible(&expected, &actual) {
                        mismatch(
                            diagnostics,
                            expression.span.clone(),
                            &expected,
                            &actual,
                            "function argument",
                        );
                    }
                }
            }
            function.return_type.as_deref().map_or(Ty::Unit, |text| {
                if text.trim() == "Self" {
                    method_owner(function)
                        .map(|owner| type_from_syntax(owner, struct_signatures))
                        .unwrap_or(Ty::Unknown)
                } else {
                    type_from_syntax(text, struct_signatures)
                }
            })
        }
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } => {
            let receiver_type = infer_with_structs(
                receiver,
                locals,
                signatures,
                struct_signatures,
                expected_return,
                diagnostics,
            );
            let receiver_base = without_references(&receiver_type);
            if method == "clone" {
                if !arguments.is_empty() {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "method `clone` expects 0 arguments but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                if is_cloneable(receiver_base, struct_signatures) {
                    return receiver_base.clone();
                }
                diagnostics.push(TypeDiagnostic {
                    span: receiver.span.clone(),
                    message: format!(
                        "type `{}` does not implement `Clone`",
                        receiver_base.display_name()
                    ),
                });
                return Ty::Unknown;
            }
            if method == "into"
                && arguments.is_empty()
                && let Ty::Into(target) = receiver_base
            {
                return *target.clone();
            }
            if method == "as_ref"
                && arguments.is_empty()
                && (*receiver_base == Ty::Unknown
                    || is_as_ref_str_type(receiver_base, struct_signatures))
            {
                return Ty::Reference(Box::new(Ty::StringSlice));
            }
            if matches!(method.as_str(), "len" | "is_empty")
                && matches!(
                    receiver_base,
                    Ty::String | Ty::StringSlice | Ty::Vector(_) | Ty::Slice(_) | Ty::Map(_, _)
                )
            {
                if !arguments.is_empty() {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "method `{method}` expects 0 arguments but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                return if method == "len" {
                    Ty::Integer("usize".to_owned())
                } else {
                    Ty::Bool
                };
            }
            if method == "iter" {
                let item = match receiver_base {
                    Ty::Vector(item) | Ty::Slice(item) => Ty::Reference(item.clone()),
                    Ty::String => Ty::Reference(Box::new(Ty::Integer("u8".to_owned()))),
                    Ty::StringSlice => Ty::Reference(Box::new(Ty::Integer("u8".to_owned()))),
                    Ty::Map(_, _) => Ty::Unknown,
                    _ => Ty::Unknown,
                };
                if item != Ty::Unknown && arguments.is_empty() {
                    return Ty::Iterator(Box::new(item));
                }
            }
            if method == "bytes"
                && matches!(receiver_base, Ty::String | Ty::StringSlice)
                && arguments.is_empty()
            {
                return Ty::Iterator(Box::new(Ty::Integer("u8".to_owned())));
            }
            if method == "trim"
                && matches!(receiver_base, Ty::String | Ty::StringSlice)
                && arguments.is_empty()
            {
                return Ty::Reference(Box::new(Ty::StringSlice));
            }
            if method == "any"
                && let Ty::Iterator(item_type) = receiver_base
            {
                if arguments.len() != 1 {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "method `Iterator::any` expects 1 argument but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                let BodyExprKind::Closure { parameters, body } = &arguments[0].kind else {
                    diagnostics.push(TypeDiagnostic {
                        span: arguments[0].span.clone(),
                        message: "`Iterator::any` requires a one-argument closure".to_owned(),
                    });
                    return Ty::Unknown;
                };
                if parameters.len() != 1 {
                    diagnostics.push(TypeDiagnostic {
                        span: arguments[0].span.clone(),
                        message: format!(
                            "`Iterator::any` closure expects 1 parameter but received {}",
                            parameters.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                let mut closure_locals = locals.clone();
                let ClosureParameter::Identifier(parameter) = &parameters[0] else {
                    diagnostics.push(TypeDiagnostic {
                        span: arguments[0].span.clone(),
                        message: "`Iterator::any` closure parameter must be an identifier"
                            .to_owned(),
                    });
                    return Ty::Unknown;
                };
                closure_locals.insert(parameter.clone(), *item_type.clone());
                let closure_result = infer_with_structs(
                    body,
                    &mut closure_locals,
                    signatures,
                    struct_signatures,
                    &Ty::Bool,
                    diagnostics,
                );
                require_type(
                    diagnostics,
                    body.span.clone(),
                    &Ty::Bool,
                    &closure_result,
                    "Iterator::any closure result",
                );
                return Ty::Bool;
            }
            if method == "fold"
                && let Ty::Iterator(item_type) = receiver_base
            {
                if arguments.len() != 2 {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "method `Iterator::fold` expects 2 arguments but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                let accumulator_type = infer_with_structs(
                    &arguments[0],
                    locals,
                    signatures,
                    struct_signatures,
                    expected_return,
                    diagnostics,
                );
                let BodyExprKind::Closure { parameters, body } = &arguments[1].kind else {
                    diagnostics.push(TypeDiagnostic {
                        span: arguments[1].span.clone(),
                        message: "`Iterator::fold` requires a two-argument closure".to_owned(),
                    });
                    return Ty::Unknown;
                };
                if parameters.len() != 2 {
                    diagnostics.push(TypeDiagnostic {
                        span: arguments[1].span.clone(),
                        message: format!(
                            "`Iterator::fold` closure expects 2 parameters but received {}",
                            parameters.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                let mut closure_locals = locals.clone();
                match (&parameters[0], &accumulator_type) {
                    (ClosureParameter::Identifier(name), ty) => {
                        closure_locals.insert(name.clone(), ty.clone());
                    }
                    (ClosureParameter::Tuple(names), Ty::Tuple(types))
                        if names.len() == types.len() =>
                    {
                        closure_locals.extend(names.iter().cloned().zip(types.iter().cloned()));
                    }
                    _ => {
                        diagnostics.push(TypeDiagnostic {
                            span: arguments[1].span.clone(),
                            message:
                                "`Iterator::fold` accumulator pattern does not match its seed type"
                                    .to_owned(),
                        });
                        return Ty::Unknown;
                    }
                }
                let ClosureParameter::Identifier(item_name) = &parameters[1] else {
                    diagnostics.push(TypeDiagnostic {
                        span: arguments[1].span.clone(),
                        message: "`Iterator::fold` item parameter must be an identifier".to_owned(),
                    });
                    return Ty::Unknown;
                };
                closure_locals.insert(item_name.clone(), *item_type.clone());
                let closure_result = infer_with_structs(
                    body,
                    &mut closure_locals,
                    signatures,
                    struct_signatures,
                    &accumulator_type,
                    diagnostics,
                );
                require_type(
                    diagnostics,
                    body.span.clone(),
                    &accumulator_type,
                    &closure_result,
                    "Iterator::fold closure result",
                );
                return accumulator_type;
            }
            let generic_candidates = trait_methods_for_generic(signatures, &receiver_type, method);
            let candidates = if generic_candidates.is_empty() {
                method_signatures(&signatures.methods, &receiver_type, method)
            } else {
                generic_candidates
            };
            if candidates.len() > 1 {
                diagnostics.push(TypeDiagnostic {
                    span: expression.span.clone(),
                    message: format!(
                        "ambiguous method call `{method}`: {}",
                        candidates
                            .iter()
                            .map(|candidate| candidate.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
                return Ty::Unknown;
            }
            if let Some(function) = candidates.first() {
                let expected_arguments = function.parameters.len().saturating_sub(1);
                if arguments.len() != expected_arguments {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!("method `{method}` expects {expected_arguments} arguments but received {}", arguments.len()),
                    });
                    return Ty::Unknown;
                }
                for (argument, parameter) in
                    arguments.iter().zip(function.parameters.iter().skip(1))
                {
                    let expected = type_from_syntax(&parameter.type_text, struct_signatures);
                    let actual = infer_with_structs(
                        argument,
                        locals,
                        signatures,
                        struct_signatures,
                        &expected,
                        diagnostics,
                    );
                    if !types_compatible(&expected, &actual) {
                        mismatch(
                            diagnostics,
                            argument.span.clone(),
                            &expected,
                            &actual,
                            "method argument",
                        );
                    }
                }
                return function.return_type.as_deref().map_or(Ty::Unit, |text| {
                    if text.trim() == "Self" {
                        method_owner(function)
                            .map(|owner| type_from_syntax(owner, struct_signatures))
                            .unwrap_or(Ty::Unknown)
                    } else {
                        type_from_syntax(text, struct_signatures)
                    }
                });
            }
            if method == "chars"
                && matches!(receiver_base, Ty::String | Ty::StringSlice)
                && arguments.is_empty()
            {
                return Ty::Iterator(Box::new(Ty::Char));
            }
            if method == "to_lowercase" && receiver_base == &Ty::Char && arguments.is_empty() {
                return Ty::Iterator(Box::new(Ty::Char));
            }
            if method == "is_ascii_alphanumeric"
                && receiver_base == &Ty::Char
                && arguments.is_empty()
            {
                return Ty::Bool;
            }
            if method == "push" && receiver_base == &Ty::String {
                if arguments.len() != 1 {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "method `String::push` expects 1 argument but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                let expected = Ty::Char;
                let actual = infer_with_structs(
                    &arguments[0],
                    locals,
                    signatures,
                    struct_signatures,
                    &expected,
                    diagnostics,
                );
                require_type(
                    diagnostics,
                    arguments[0].span.clone(),
                    &expected,
                    &actual,
                    "String::push argument",
                );
                return Ty::Unit;
            }
            if method == "extend"
                && (receiver_base == &Ty::String || matches!(receiver_base, Ty::Vector(_)))
            {
                if arguments.len() != 1 {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "method `{method}` expects 1 argument but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                let expected_item = match receiver_base {
                    Ty::String => Ty::Char,
                    Ty::Vector(item) => *item.clone(),
                    _ => unreachable!(),
                };
                let iterator_type = infer_with_structs(
                    &arguments[0],
                    locals,
                    signatures,
                    struct_signatures,
                    expected_return,
                    diagnostics,
                );
                let Ty::Iterator(actual_item) = without_references(&iterator_type) else {
                    if iterator_type != Ty::Unknown {
                        diagnostics.push(TypeDiagnostic {
                            span: arguments[0].span.clone(),
                            message: "`extend` argument must be an iterator".to_owned(),
                        });
                    }
                    return Ty::Unknown;
                };
                require_type(
                    diagnostics,
                    arguments[0].span.clone(),
                    &expected_item,
                    actual_item,
                    "extend item",
                );
                return Ty::Unit;
            }
            if method == "push"
                && let Ty::Vector(element) = receiver_base
            {
                if arguments.len() != 1 {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "method `Vec::push` expects 1 argument but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                let actual = infer_with_structs(
                    &arguments[0],
                    locals,
                    signatures,
                    struct_signatures,
                    element,
                    diagnostics,
                );
                require_type(
                    diagnostics,
                    arguments[0].span.clone(),
                    element,
                    &actual,
                    "Vec::push argument",
                );
                return Ty::Unit;
            }
            if method == "insert"
                && let Ty::Map(key_type, value_type) = receiver_base
            {
                if arguments.len() != 2 {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "method `BTreeMap::insert` expects 2 arguments but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                for (argument, expected) in arguments.iter().zip([key_type, value_type]) {
                    let actual = infer_with_structs(
                        argument,
                        locals,
                        signatures,
                        struct_signatures,
                        expected,
                        diagnostics,
                    );
                    require_type(
                        diagnostics,
                        argument.span.clone(),
                        expected,
                        &actual,
                        "BTreeMap::insert argument",
                    );
                }
                return Ty::Unknown;
            }
            if method == "get"
                && let Ty::Map(key_type, value_type) = receiver_base
            {
                if arguments.len() != 1 {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "method `BTreeMap::get` expects 1 argument but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                let actual = infer_with_structs(
                    &arguments[0], locals, signatures, struct_signatures, key_type, diagnostics,
                );
                if !(**key_type == Ty::String
                    && matches!(&actual, Ty::Reference(inner) if **inner == Ty::StringSlice))
                {
                    require_type(
                        diagnostics,
                        arguments[0].span.clone(),
                        key_type,
                        &actual,
                        "BTreeMap::get key",
                    );
                }
                return Ty::Option(Box::new(Ty::Reference(value_type.clone())));
            }
            if method == "is_some" {
                if !matches!(receiver_type, Ty::Option(_)) && receiver_type != Ty::Unknown {
                    diagnostics.push(TypeDiagnostic {
                        span: receiver.span.clone(),
                        message: "method `is_some` requires an Option receiver".to_owned(),
                    });
                    return Ty::Unknown;
                }
                if !arguments.is_empty() {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "method `Option::is_some` expects 0 arguments but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                return Ty::Bool;
            }
            if method == "unwrap" {
                if !arguments.is_empty() {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!("method `unwrap` expects 0 arguments but received {}", arguments.len()),
                    });
                    return Ty::Unknown;
                }
                return match receiver_type {
                    Ty::Option(payload) | Ty::Result(payload, _) => *payload,
                    Ty::Unknown => Ty::Unknown,
                    other => {
                        diagnostics.push(TypeDiagnostic {
                            span: receiver.span.clone(),
                            message: format!("method `unwrap` requires an Option or Result receiver, found `{}`", other.display_name()),
                        });
                        Ty::Unknown
                    }
                };
            }
            if method == "unwrap_or" {
                let payload_type = match receiver_type {
                    Ty::Option(payload_type) | Ty::Result(payload_type, _) => payload_type,
                    _ => {
                        if receiver_type != Ty::Unknown {
                            diagnostics.push(TypeDiagnostic {
                                span: receiver.span.clone(),
                                message: "method `unwrap_or` requires an Option or Result receiver"
                                    .to_owned(),
                            });
                        }
                        return Ty::Unknown;
                    }
                };
                if arguments.len() != 1 {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "method `unwrap_or` expects 1 argument but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                let actual = infer_with_structs(
                    &arguments[0],
                    locals,
                    signatures,
                    struct_signatures,
                    &payload_type,
                    diagnostics,
                );
                require_type(
                    diagnostics,
                    arguments[0].span.clone(),
                    &payload_type,
                    &actual,
                    "unwrap_or fallback",
                );
                return *payload_type;
            }
            if matches!(method.as_str(), "is_ok" | "is_err") {
                let Ty::Result(_, _) = receiver_type else {
                    if receiver_type != Ty::Unknown {
                        diagnostics.push(TypeDiagnostic {
                            span: receiver.span.clone(),
                            message: format!("method `{method}` requires a Result receiver"),
                        });
                    }
                    return Ty::Unknown;
                };
                if !arguments.is_empty() {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "method `Result::{method}` expects 0 arguments but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                return Ty::Bool;
            }
            let expected_u64 = Ty::Integer("u64".to_owned());
            let receiver_type = infer_with_structs(
                receiver,
                locals,
                signatures,
                struct_signatures,
                &expected_u64,
                diagnostics,
            );
            let argument_types: Vec<_> = arguments
                .iter()
                .map(|argument| {
                    infer_with_structs(
                        argument,
                        locals,
                        signatures,
                        struct_signatures,
                        &expected_u64,
                        diagnostics,
                    )
                })
                .collect();
            if !matches!(method.as_str(), "wrapping_add" | "wrapping_mul") {
                diagnostics.push(TypeDiagnostic {
                    span: expression.span.clone(),
                    message: format!(
                        "method `{method}` is outside the supported type-checking subset"
                    ),
                });
                return Ty::Unknown;
            }
            require_type(
                diagnostics,
                receiver.span.clone(),
                &expected_u64,
                &receiver_type,
                "wrapping arithmetic receiver",
            );
            if arguments.len() != 1 {
                diagnostics.push(TypeDiagnostic {
                    span: expression.span.clone(),
                    message: format!(
                        "method `u64::{method}` expects 1 argument but received {}",
                        arguments.len()
                    ),
                });
                return Ty::Unknown;
            }
            require_type(
                diagnostics,
                arguments[0].span.clone(),
                &expected_u64,
                &argument_types[0],
                "wrapping arithmetic argument",
            );
            expected_u64
        }
        BodyExprKind::Binary {
            operator,
            left,
            right,
        } => {
            let left_ty = infer_with_structs(
                left,
                locals,
                signatures,
                struct_signatures,
                expected_return,
                diagnostics,
            );
            let right_ty = infer_with_structs(
                right,
                locals,
                signatures,
                struct_signatures,
                expected_return,
                diagnostics,
            );
            match operator {
                BinaryOperator::And | BinaryOperator::Or => {
                    require_type(
                        diagnostics,
                        left.span.clone(),
                        &Ty::Bool,
                        &left_ty,
                        "boolean operator",
                    );
                    require_type(
                        diagnostics,
                        right.span.clone(),
                        &Ty::Bool,
                        &right_ty,
                        "boolean operator",
                    );
                    Ty::Bool
                }
                BinaryOperator::Equal
                | BinaryOperator::PatternEqual
                | BinaryOperator::NotEqual
                | BinaryOperator::Less
                | BinaryOperator::LessEqual
                | BinaryOperator::Greater
                | BinaryOperator::GreaterEqual => {
                    if matches!(operator, BinaryOperator::Equal | BinaryOperator::NotEqual)
                        && let Ty::Named(name) = &left_ty
                        && let Some(structure) = find_nominal_signature(struct_signatures, name)
                        && !structure
                            .derived_traits
                            .iter()
                            .any(|name| name == "PartialEq")
                    {
                        diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!("type `{name}` does not implement `PartialEq`"),
                        });
                    }
                    if left_ty != Ty::Unknown
                        && right_ty != Ty::Unknown
                        && !types_compatible(&left_ty, &right_ty)
                    {
                        mismatch(
                            diagnostics,
                            expression.span.clone(),
                            &left_ty,
                            &right_ty,
                            "comparison",
                        );
                    }
                    Ty::Bool
                }
                BinaryOperator::Add
                | BinaryOperator::Subtract
                | BinaryOperator::Multiply
                | BinaryOperator::Divide
                | BinaryOperator::Remainder
                | BinaryOperator::BitXor => {
                    if *operator == BinaryOperator::Add
                        && left_ty == right_ty
                        && is_add_type(&left_ty)
                    {
                        left_ty.clone()
                    } else if left_ty != Ty::Unknown && right_ty != Ty::Unknown {
                        // Rust's arithmetic operator implementations accept
                        // shared references to numeric operands (for example,
                        // `u64 + &u64`), which is common in iterator folds.
                        let left_value_ty = without_references(&left_ty);
                        let right_value_ty = without_references(&right_ty);
                        if !left_value_ty.is_numeric() || !right_value_ty.is_numeric() {
                            diagnostics.push(TypeDiagnostic {
                                span: expression.span.clone(),
                                message: format!(
                                    "arithmetic requires numeric operands, found `{}` and `{}`",
                                    left_ty.display_name(),
                                    right_ty.display_name()
                                ),
                            });
                            Ty::Unknown
                        } else if !types_compatible(left_value_ty, right_value_ty) {
                            mismatch(
                                diagnostics,
                                expression.span.clone(),
                                left_value_ty,
                                right_value_ty,
                                "arithmetic",
                            );
                            Ty::Unknown
                        } else {
                            join_numeric_types(left_value_ty.clone(), right_value_ty.clone())
                        }
                    } else {
                        Ty::Unknown
                    }
                }
                BinaryOperator::Unsupported => Ty::Unknown,
            }
        }
        BodyExprKind::Negate(operand) => {
            let ty = infer_with_structs(
                operand,
                locals,
                signatures,
                struct_signatures,
                expected_return,
                diagnostics,
            );
            if ty != Ty::Unknown && !ty.is_numeric() {
                diagnostics.push(TypeDiagnostic {
                    span: expression.span.clone(),
                    message: format!(
                        "unary negation requires a numeric operand, found `{}`",
                        ty.display_name()
                    ),
                });
                Ty::Unknown
            } else {
                ty
            }
        }
        BodyExprKind::Try(operand) => {
            let operand_ty = infer_with_structs(
                operand,
                locals,
                signatures,
                struct_signatures,
                expected_return,
                diagnostics,
            );
            match (&operand_ty, expected_return) {
                (Ty::Option(value_type), Ty::Option(_)) => (**value_type).clone(),
                (Ty::Option(_), _) => {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: "`?` requires a function returning an Option type".to_owned(),
                    });
                    Ty::Unknown
                }
                (Ty::Result(ok_type, err_type), Ty::Result(_, expected_err))
                    if types_compatible(expected_err, err_type) =>
                {
                    (**ok_type).clone()
                }
                (Ty::Result(_, _), _) => {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: "`?` requires a function returning a compatible Result type"
                            .to_owned(),
                    });
                    Ty::Unknown
                }
                _ => {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: "`?` operand must have a Result type".to_owned(),
                    });
                    Ty::Unknown
                }
            }
        }
        BodyExprKind::Not(operand) => {
            let ty = infer_with_structs(
                operand,
                locals,
                signatures,
                struct_signatures,
                &Ty::Bool,
                diagnostics,
            );
            if ty != Ty::Bool && ty != Ty::Unknown {
                diagnostics.push(TypeDiagnostic {
                    span: expression.span.clone(),
                    message: format!(
                        "logical negation requires bool, found `{}`",
                        ty.display_name()
                    ),
                });
                Ty::Unknown
            } else {
                Ty::Bool
            }
        }
        BodyExprKind::Block { statements, tail } => {
            let original_locals = locals.clone();
            let mut diverges = false;
            for statement in statements {
                match statement {
                    BodyStatement::Let {
                        name,
                        type_text,
                        mutable: _,
                        initializer,
                        span,
                    } => {
                        let declared = type_text
                            .as_deref()
                            .map(|text| type_from_syntax(text, struct_signatures));
                        let inferred = initializer.as_ref().map(|initializer| {
                            infer_with_structs(
                                initializer,
                                locals,
                                signatures,
                                struct_signatures,
                                declared.as_ref().unwrap_or(expected_return),
                                diagnostics,
                            )
                        });
                        diverges |= inferred == Some(Ty::Never);
                        let binding_type = match (declared, inferred) {
                            (Some(declared), Some(actual)) => {
                                if actual != Ty::Never && !types_compatible(&declared, &actual) {
                                    mismatch(
                                        diagnostics,
                                        initializer.as_ref().map_or_else(
                                            || span.clone(),
                                            |value| value.span.clone(),
                                        ),
                                        &declared,
                                        &actual,
                                        "local binding",
                                    );
                                }
                                declared
                            }
                            (Some(declared), None) => declared,
                            (None, Some(actual)) => actual,
                            (None, None) => Ty::Unknown,
                        };
                        if let Some(name) = name {
                            locals.insert(name.clone(), binding_type);
                        }
                    }
                    BodyStatement::LetTuple {
                        names,
                        initializer,
                        span,
                    } => {
                        let inferred = infer_with_structs(
                            initializer,
                            locals,
                            signatures,
                            struct_signatures,
                            expected_return,
                            diagnostics,
                        );
                        match inferred {
                            Ty::Tuple(types) if types.len() == names.len() => {
                                locals.extend(names.iter().cloned().zip(types));
                            }
                            Ty::Tuple(types) => diagnostics.push(TypeDiagnostic {
                                span: span.clone(),
                                message: format!(
                                    "tuple pattern has {} names but initializer has {} elements",
                                    names.len(),
                                    types.len()
                                ),
                            }),
                            Ty::Unknown => {
                                locals
                                    .extend(names.iter().cloned().map(|name| (name, Ty::Unknown)));
                            }
                            actual => diagnostics.push(TypeDiagnostic {
                                span: span.clone(),
                                message: format!(
                                    "tuple pattern requires a tuple initializer, found `{}`",
                                    actual.display_name()
                                ),
                            }),
                        }
                    }
                    BodyStatement::Expression { expression, .. } => {
                        let actual = infer_with_structs(
                            expression,
                            locals,
                            signatures,
                            struct_signatures,
                            expected_return,
                            diagnostics,
                        );
                        diverges |= actual == Ty::Never;
                    }
                    BodyStatement::Break { .. } | BodyStatement::Continue { .. } => {}
                    BodyStatement::Assign { name, value, span } => {
                        let actual = infer_with_structs(
                            value,
                            locals,
                            signatures,
                            struct_signatures,
                            expected_return,
                            diagnostics,
                        );
                        let expected_field = name.split_once('.').and_then(|(base, field)| {
                            let mut base_type = locals.get(base)?.clone();
                            while let Ty::Reference(referent) = base_type {
                                base_type = *referent;
                            }
                            let Ty::Named(structure_name) = base_type else {
                                return None;
                            };
                            find_struct_signature(struct_signatures, &structure_name)?
                                .fields
                                .iter()
                                .find(|candidate| candidate.name == field)
                                .map(|candidate| {
                                    type_from_syntax(&candidate.type_text, struct_signatures)
                                })
                        });
                        if let Some(expected) = expected_field.as_ref().or_else(|| locals.get(name))
                        {
                            if !types_compatible(expected, &actual) {
                                mismatch(
                                    diagnostics,
                                    span.clone(),
                                    expected,
                                    &actual,
                                    "assignment",
                                );
                            }
                        } else {
                            diagnostics.push(TypeDiagnostic {
                                span: span.clone(),
                                message: format!("cannot assign to unresolved local `{name}`"),
                            });
                        }
                    }
                    BodyStatement::While {
                        condition, body, ..
                    } => {
                        let actual = infer_with_structs(
                            condition,
                            locals,
                            signatures,
                            struct_signatures,
                            expected_return,
                            diagnostics,
                        );
                        require_type(
                            diagnostics,
                            condition.span.clone(),
                            &Ty::Bool,
                            &actual,
                            "while condition",
                        );
                        let body_type = infer_with_structs(
                            body,
                            locals,
                            signatures,
                            struct_signatures,
                            expected_return,
                            diagnostics,
                        );
                        if body_type != Ty::Unit
                            && body_type != Ty::Unknown
                            && body_type != Ty::Never
                        {
                            mismatch(
                                diagnostics,
                                body.span.clone(),
                                &Ty::Unit,
                                &body_type,
                                "while body",
                            );
                        }
                    }
                    BodyStatement::Loop { body, .. } => {
                        let body_type = infer_with_structs(
                            body,
                            locals,
                            signatures,
                            struct_signatures,
                            expected_return,
                            diagnostics,
                        );
                        if body_type != Ty::Unit
                            && body_type != Ty::Unknown
                            && body_type != Ty::Never
                        {
                            mismatch(
                                diagnostics,
                                body.span.clone(),
                                &Ty::Unit,
                                &body_type,
                                "loop body",
                            );
                        }
                    }
                    BodyStatement::ForRange {
                        name,
                        start,
                        end,
                        body,
                        span,
                        ..
                    } => {
                        let start_type = infer_with_structs(
                            start,
                            locals,
                            signatures,
                            struct_signatures,
                            expected_return,
                            diagnostics,
                        );
                        let end_type = infer_with_structs(
                            end,
                            locals,
                            signatures,
                            struct_signatures,
                            expected_return,
                            diagnostics,
                        );
                        if start_type != Ty::Unknown && end_type != Ty::Unknown {
                            if !start_type.is_integer() || !end_type.is_integer() {
                                diagnostics.push(TypeDiagnostic {
                                    span: span.clone(),
                                    message: "range bounds must have integer types".to_owned(),
                                });
                            } else if !types_compatible(&start_type, &end_type) {
                                mismatch(
                                    diagnostics,
                                    span.clone(),
                                    &start_type,
                                    &end_type,
                                    "range bounds",
                                );
                            }
                        }
                        let mut loop_locals = locals.clone();
                        let iteration_type = if start_type == Ty::InferInteger {
                            end_type.clone()
                        } else {
                            start_type.clone()
                        };
                        loop_locals.insert(name.clone(), iteration_type);
                        let body_type = infer_with_structs(
                            body,
                            &mut loop_locals,
                            signatures,
                            struct_signatures,
                            expected_return,
                            diagnostics,
                        );
                        if body_type != Ty::Unit
                            && body_type != Ty::Unknown
                            && body_type != Ty::Never
                        {
                            mismatch(
                                diagnostics,
                                body.span.clone(),
                                &Ty::Unit,
                                &body_type,
                                "for body",
                            );
                        }
                    }
                    BodyStatement::ForEach {
                        names,
                        iterator,
                        body,
                        span,
                    } => {
                        let iterator_type = infer_with_structs(
                            iterator,
                            locals,
                            signatures,
                            struct_signatures,
                            expected_return,
                            diagnostics,
                        );
                        let (element_type, borrowed_item) = match &iterator_type {
                            Ty::Reference(inner) => match inner.as_ref() {
                                Ty::Slice(element) | Ty::Vector(element) => {
                                    (Some(element.as_ref()), true)
                                }
                                _ => (None, false),
                            },
                            Ty::Slice(element) | Ty::Vector(element) => {
                                (Some(element.as_ref()), false)
                            }
                            Ty::Iterator(element) => (Some(element.as_ref()), false),
                            _ => (None, false),
                        };
                        let mut loop_locals = locals.clone();
                        if let Some(element_type) = element_type {
                            let binding_type = |ty: &Ty| {
                                if borrowed_item {
                                    Ty::Reference(Box::new(ty.clone()))
                                } else {
                                    ty.clone()
                                }
                            };
                            match element_type {
                                Ty::Tuple(elements) if elements.len() == names.len() => {
                                    loop_locals.extend(
                                        names
                                            .iter()
                                            .cloned()
                                            .zip(elements.iter().map(binding_type)),
                                    );
                                }
                                element if names.len() == 1 => {
                                    loop_locals.insert(names[0].clone(), binding_type(element));
                                }
                                _ => diagnostics.push(TypeDiagnostic {
                                    span: span.clone(),
                                    message: "for pattern does not match the iterator item type"
                                        .to_owned(),
                                }),
                            }
                        } else if iterator_type != Ty::Unknown {
                            diagnostics.push(TypeDiagnostic {
                                span: span.clone(),
                                message: "for expression must be an iterable slice or vector"
                                    .to_owned(),
                            });
                        }
                        let body_type = infer_with_structs(
                            body,
                            &mut loop_locals,
                            signatures,
                            struct_signatures,
                            expected_return,
                            diagnostics,
                        );
                        if body_type != Ty::Unit
                            && body_type != Ty::Unknown
                            && body_type != Ty::Never
                        {
                            mismatch(
                                diagnostics,
                                body.span.clone(),
                                &Ty::Unit,
                                &body_type,
                                "for body",
                            );
                        }
                    }
                    BodyStatement::MacroInvocation { .. } | BodyStatement::Unsupported => {}
                }
            }
            let block_type = if let Some(tail) = tail {
                infer_with_structs(
                    tail,
                    locals,
                    signatures,
                    struct_signatures,
                    expected_return,
                    diagnostics,
                )
            } else if diverges {
                Ty::Never
            } else {
                Ty::Unit
            };
            *locals = original_locals;
            block_type
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let condition_ty = infer_with_structs(
                condition,
                locals,
                signatures,
                struct_signatures,
                expected_return,
                diagnostics,
            );
            require_type(
                diagnostics,
                condition.span.clone(),
                &Ty::Bool,
                &condition_ty,
                "if condition",
            );
            let then_ty = infer_with_structs(
                then_branch,
                locals,
                signatures,
                struct_signatures,
                expected_return,
                diagnostics,
            );
            let else_ty = else_branch.as_ref().map(|branch| {
                infer_with_structs(
                    branch,
                    locals,
                    signatures,
                    struct_signatures,
                    expected_return,
                    diagnostics,
                )
            });
            match else_ty {
                Some(else_ty) if then_ty == Ty::Never => else_ty,
                Some(Ty::Never) => then_ty,
                Some(else_ty) if !types_compatible(&then_ty, &else_ty) => {
                    mismatch(
                        diagnostics,
                        expression.span.clone(),
                        &then_ty,
                        &else_ty,
                        "if branches",
                    );
                    Ty::Unknown
                }
                Some(_) => then_ty,
                None => {
                    if then_ty != Ty::Never && then_ty != Ty::Unit && then_ty != Ty::Unknown {
                        mismatch(
                            diagnostics,
                            then_branch.span.clone(),
                            &Ty::Unit,
                            &then_ty,
                            "if without else",
                        );
                    }
                    Ty::Unit
                }
            }
        }
        BodyExprKind::Match { scrutinee, arms } => {
            let scrutinee_ty = infer_with_structs(
                scrutinee,
                locals,
                signatures,
                struct_signatures,
                expected_return,
                diagnostics,
            );
            let mut result_ty = Ty::Unknown;
            let mut has_wildcard = false;
            let mut bool_true = false;
            let mut bool_false = false;
            let mut enum_patterns = HashMap::<String, HashSet<String>>::new();
            let mut enum_variant_counts = HashMap::<String, usize>::new();
            let mut seen_literals = HashSet::new();
            for arm in arms {
                if has_wildcard {
                    diagnostics.push(TypeDiagnostic {
                        span: arm.span.clone(),
                        message: "unreachable match arm after wildcard".to_owned(),
                    });
                }
                for pattern in match_pattern_alternatives(&arm.pattern) {
                    match pattern {
                        MatchPattern::Wildcard => {
                            if has_wildcard {
                                diagnostics.push(TypeDiagnostic {
                                    span: arm.span.clone(),
                                    message: "duplicate wildcard match arm".to_owned(),
                                });
                            }
                            has_wildcard = true;
                        }
                        MatchPattern::Literal(literal) => {
                            let pattern_ty = literal_type(literal);
                            if !types_compatible(&scrutinee_ty, &pattern_ty) {
                                mismatch(
                                    diagnostics,
                                    arm.span.clone(),
                                    &scrutinee_ty,
                                    &pattern_ty,
                                    "match pattern",
                                );
                            }
                            match literal {
                                LiteralKind::Boolean(true) => bool_true = true,
                                LiteralKind::Boolean(false) => bool_false = true,
                                _ => {}
                            }
                            let key = format!("{literal:?}");
                            if !seen_literals.insert(key) {
                                diagnostics.push(TypeDiagnostic {
                                    span: arm.span.clone(),
                                    message: "duplicate match pattern".to_owned(),
                                });
                            }
                        }
                        MatchPattern::Binding(_) => {
                            has_wildcard = true;
                        }
                        MatchPattern::Range { start, end, .. } => {
                            for bound in [start, end] {
                                let bound_ty = literal_type(bound);
                                if !types_compatible(&scrutinee_ty, &bound_ty) {
                                    mismatch(
                                        diagnostics,
                                        arm.span.clone(),
                                        &scrutinee_ty,
                                        &bound_ty,
                                        "match range pattern",
                                    );
                                }
                            }
                            if literal_pattern_ordering(start, end)
                                .is_some_and(|ordering| ordering.is_gt())
                            {
                                diagnostics.push(TypeDiagnostic {
                                    span: arm.span.clone(),
                                    message: "match range pattern starts after its end".to_owned(),
                                });
                            }
                        }
                        MatchPattern::Path(path) => {
                            if let Some((enumeration, variant)) =
                                find_enum_variant(struct_signatures, path)
                            {
                                let variants =
                                    enum_patterns.entry(enumeration.path.clone()).or_default();
                                if !variants.insert(variant.name.clone()) {
                                    diagnostics.push(TypeDiagnostic {
                                        span: arm.span.clone(),
                                        message: "duplicate enum variant match pattern".to_owned(),
                                    });
                                }
                                enum_variant_counts.insert(
                                    enumeration.path.clone(),
                                    enumeration.enum_variants.len(),
                                );
                                let expected = Ty::Named(enumeration.path.clone());
                                if !types_compatible(&scrutinee_ty, &expected) {
                                    mismatch(
                                        diagnostics,
                                        arm.span.clone(),
                                        &scrutinee_ty,
                                        &expected,
                                        "enum match pattern",
                                    );
                                }
                            } else {
                                diagnostics.push(TypeDiagnostic {
                                    span: arm.span.clone(),
                                    message: format!("unknown enum variant pattern `{path}`"),
                                });
                            }
                        }
                        MatchPattern::Or(_) => {
                            unreachable!("or-pattern alternatives are flattened")
                        }
                        MatchPattern::EnumVariant {
                            path,
                            variant_count,
                            ..
                        } => {
                            let Some((enumeration, variant)) =
                                find_enum_variant(struct_signatures, path)
                            else {
                                diagnostics.push(TypeDiagnostic {
                                    span: arm.span.clone(),
                                    message: format!("unknown enum variant pattern `{path}`"),
                                });
                                continue;
                            };
                            let variants =
                                enum_patterns.entry(enumeration.path.clone()).or_default();
                            if !variants.insert(variant.name.clone()) {
                                diagnostics.push(TypeDiagnostic {
                                    span: arm.span.clone(),
                                    message: "duplicate enum variant match pattern".to_owned(),
                                });
                            }
                            enum_variant_counts.insert(
                                enumeration.path.clone(),
                                (*variant_count).min(enumeration.enum_variants.len()),
                            );
                            let expected = Ty::Named(enumeration.path.clone());
                            if !types_compatible(&scrutinee_ty, &expected) {
                                mismatch(
                                    diagnostics,
                                    arm.span.clone(),
                                    &scrutinee_ty,
                                    &expected,
                                    "enum match pattern",
                                );
                            }
                        }
                        MatchPattern::Unsupported => diagnostics.push(TypeDiagnostic {
                            span: arm.span.clone(),
                            message: "unsupported match pattern".to_owned(),
                        }),
                    }
                }
                let mut arm_locals = locals.clone();
                if let MatchPattern::Binding(name) = &arm.pattern {
                    arm_locals.insert(name.clone(), scrutinee_ty.clone());
                }
                let arm_ty = infer_with_structs(
                    &arm.body,
                    &mut arm_locals,
                    signatures,
                    struct_signatures,
                    expected_return,
                    diagnostics,
                );
                if result_ty == Ty::Unknown {
                    result_ty = arm_ty;
                } else if arm_ty != Ty::Never
                    && result_ty != Ty::Never
                    && !types_compatible(&result_ty, &arm_ty)
                {
                    mismatch(
                        diagnostics,
                        arm.span.clone(),
                        &result_ty,
                        &arm_ty,
                        "match arms",
                    );
                    result_ty = Ty::Unknown;
                } else if result_ty == Ty::Never {
                    result_ty = arm_ty;
                }
            }
            let exhaustive_enum = enum_patterns.iter().any(|(enum_path, variants)| {
                enum_variant_counts
                    .get(enum_path)
                    .is_some_and(|count| variants.len() == *count)
            });
            if !has_wildcard
                && !exhaustive_enum
                && !(scrutinee_ty == Ty::Bool && bool_true && bool_false)
            {
                diagnostics.push(TypeDiagnostic {
                    span: expression.span.clone(),
                    message: "non-exhaustive match; add a wildcard arm".to_owned(),
                });
            }
            result_ty
        }
        BodyExprKind::Return(value) => {
            let actual = value.as_ref().map_or(Ty::Unit, |value| {
                infer_with_structs(
                    value,
                    locals,
                    signatures,
                    struct_signatures,
                    expected_return,
                    diagnostics,
                )
            });
            if !types_compatible(expected_return, &actual) {
                mismatch(
                    diagnostics,
                    expression.span.clone(),
                    expected_return,
                    &actual,
                    "return",
                );
            }
            Ty::Never
        }
        BodyExprKind::Parenthesized(inner) => infer_with_structs(
            inner,
            locals,
            signatures,
            struct_signatures,
            expected_return,
            diagnostics,
        ),
        BodyExprKind::Closure { .. } => Ty::Unknown,
        BodyExprKind::MacroInvocation(macro_expression) => {
            for argument in &macro_expression.arguments {
                infer_with_structs(
                    argument,
                    locals,
                    signatures,
                    struct_signatures,
                    expected_return,
                    diagnostics,
                );
            }
            match macro_expression.path.as_str() {
                "format" => Ty::String,
                "concat" | "env" | "stringify" => Ty::StringSlice,
                "panic" | "todo" | "unimplemented" => Ty::Never,
                _ => Ty::Unknown,
            }
        }
        BodyExprKind::Tuple(elements) => {
            let expected_elements = match expected_return {
                Ty::Tuple(types) if types.len() == elements.len() => Some(types.as_slice()),
                _ => None,
            };
            Ty::Tuple(
                elements
                    .iter()
                    .enumerate()
                    .map(|(index, element)| {
                        let expected = expected_elements
                            .and_then(|types| types.get(index))
                            .unwrap_or(expected_return);
                        infer_with_structs(
                            element,
                            locals,
                            signatures,
                            struct_signatures,
                            expected,
                            diagnostics,
                        )
                    })
                    .collect(),
            )
        }
        BodyExprKind::ArrayLiteral(elements) => {
            let expected_element = match expected_return {
                Ty::Reference(inner) => match inner.as_ref() {
                    Ty::Slice(element) => Some(element.as_ref()),
                    _ => None,
                },
                Ty::Slice(element) => Some(element.as_ref()),
                _ => None,
            };
            let mut element_type = expected_element.cloned().unwrap_or(Ty::Unknown);
            for element in elements {
                let actual = infer_with_structs(
                    element,
                    locals,
                    signatures,
                    struct_signatures,
                    expected_element.unwrap_or(expected_return),
                    diagnostics,
                );
                if element_type == Ty::Unknown {
                    element_type = actual;
                } else if actual != Ty::Unknown && !types_compatible(&element_type, &actual) {
                    mismatch(
                        diagnostics,
                        element.span.clone(),
                        &element_type,
                        &actual,
                        "array element",
                    );
                }
            }
            Ty::Reference(Box::new(Ty::Slice(Box::new(element_type))))
        }
        BodyExprKind::FixedArrayLiteral(elements) => {
            let expected = match expected_return {
                Ty::Array(element, length) if *length == elements.len() => Some(element.as_ref()),
                Ty::Array(element, _) => Some(element.as_ref()),
                _ => None,
            };
            let mut element_type = expected.cloned().unwrap_or(Ty::Unknown);
            for element in elements {
                let actual = infer_with_structs(
                    element,
                    locals,
                    signatures,
                    struct_signatures,
                    expected.unwrap_or(expected_return),
                    diagnostics,
                );
                if element_type == Ty::Unknown {
                    element_type = actual;
                } else if actual != Ty::Unknown && !types_compatible(&element_type, &actual) {
                    mismatch(
                        diagnostics,
                        element.span.clone(),
                        &element_type,
                        &actual,
                        "array element",
                    );
                }
            }
            Ty::Array(Box::new(element_type), elements.len())
        }
        BodyExprKind::EmptySliceReference | BodyExprKind::Unsupported => Ty::Unknown,
    }
}

fn match_pattern_alternatives(pattern: &MatchPattern) -> Vec<&MatchPattern> {
    match pattern {
        MatchPattern::Or(patterns) => patterns
            .iter()
            .flat_map(match_pattern_alternatives)
            .collect(),
        pattern => vec![pattern],
    }
}

fn literal_type(literal: &LiteralKind) -> Ty {
    match literal {
        LiteralKind::Integer { suffix, .. } => {
            if suffix.is_empty() {
                Ty::InferInteger
            } else {
                Ty::from_syntax(suffix)
            }
        }
        LiteralKind::Float { suffix, .. } => {
            if suffix.is_empty() {
                Ty::InferFloat
            } else {
                Ty::from_syntax(suffix)
            }
        }
        LiteralKind::Boolean(_) => Ty::Bool,
        LiteralKind::Character(_) => Ty::Char,
        LiteralKind::String { .. } => Ty::Reference(Box::new(Ty::StringSlice)),
    }
}

fn literal_pattern_ordering(left: &LiteralKind, right: &LiteralKind) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (LiteralKind::Integer { text: left, .. }, LiteralKind::Integer { text: right, .. }) => {
            Some(left.parse::<i128>().ok()?.cmp(&right.parse::<i128>().ok()?))
        }
        (LiteralKind::Character(left), LiteralKind::Character(right)) => Some(left.cmp(right)),
        _ => None,
    }
}

fn require_type(
    diagnostics: &mut Vec<TypeDiagnostic>,
    span: Range<usize>,
    expected: &Ty,
    actual: &Ty,
    context: &str,
) {
    if actual != &Ty::Unknown && !types_compatible(expected, actual) {
        mismatch(diagnostics, span, expected, actual, context);
    }
}

fn types_compatible(expected: &Ty, actual: &Ty) -> bool {
    expected == actual
        || matches!(expected, Ty::Into(target) if types_compatible(target, actual)
            || matches!((&**target, actual), (Ty::String, Ty::Reference(inner)) if **inner == Ty::StringSlice))
        || matches!((expected, actual), (Ty::Tuple(expected), Ty::Tuple(actual))
            if expected.len() == actual.len()
                && expected.iter().zip(actual).all(|(expected, actual)| types_compatible(expected, actual)))
        || matches!((expected, actual), (Ty::Array(expected, expected_len), Ty::Array(actual, actual_len))
            if expected_len == actual_len && types_compatible(expected, actual))
        || matches!((expected, actual), (Ty::Slice(expected), Ty::Vector(actual)) if types_compatible(expected, actual))
        || matches!((expected, actual), (Ty::Reference(expected), Ty::Reference(actual))
            if matches!((&**expected, &**actual), (Ty::Slice(expected), Ty::Array(actual, _) | Ty::Vector(actual))
                if types_compatible(expected, actual)))
        || matches!((expected, actual), (Ty::Reference(expected), Ty::Reference(actual)) | (Ty::Slice(expected), Ty::Slice(actual)) if types_compatible(expected, actual))
        || *expected == Ty::Unknown
        || *actual == Ty::Unknown
        || matches!((expected, actual), (Ty::Integer(_), Ty::InferInteger))
        || matches!((expected, actual), (Ty::Float(_), Ty::InferFloat))
        || matches!((expected, actual), (Ty::InferInteger, Ty::Integer(_)))
        || matches!((expected, actual), (Ty::InferFloat, Ty::Float(_)))
}

fn is_copy_type(ty: &Ty, structs: &[StructSignature]) -> bool {
    match ty {
        Ty::Bool
        | Ty::Char
        | Ty::Integer(_)
        | Ty::InferInteger
        | Ty::Float(_)
        | Ty::InferFloat
        | Ty::Reference(_)
        | Ty::Never => true,
        Ty::Tuple(fields) => fields.iter().all(|field| is_copy_type(field, structs)),
        Ty::Array(element, _) => is_copy_type(element, structs),
        Ty::Named(name) if name.starts_with("\0quill_generic:") => {
            name.split(':').nth_back(2) == Some("1")
        }
        Ty::Named(name) => find_nominal_signature(structs, name).is_some_and(|structure| {
            structure
                .derived_traits
                .iter()
                .any(|trait_name| trait_name == "Copy")
        }),
        _ => false,
    }
}

fn is_as_ref_str_type(ty: &Ty, structs: &[StructSignature]) -> bool {
    match ty {
        Ty::String => true,
        Ty::Reference(inner) => matches!(inner.as_ref(), Ty::String | Ty::StringSlice),
        Ty::Named(name) if name.starts_with("\0quill_generic:") => {
            name.split(':').nth_back(1) == Some("1")
        }
        Ty::Named(name) => find_nominal_signature(structs, name).is_some_and(|structure| {
            structure
                .derived_traits
                .iter()
                .any(|trait_name| trait_name == "AsRef<str>")
        }),
        _ => false,
    }
}

fn is_add_type(ty: &Ty) -> bool {
    matches!(ty, Ty::Named(name)
        if name.starts_with("\0quill_generic:")
            && name.split(':').next_back() == Some("1"))
}

fn join_numeric_types(left: Ty, right: Ty) -> Ty {
    match (&left, &right) {
        (Ty::InferInteger, Ty::InferInteger) => Ty::InferInteger,
        (Ty::InferFloat, Ty::InferFloat) => Ty::InferFloat,
        (Ty::InferInteger | Ty::InferFloat, _) => right,
        (_, Ty::InferInteger | Ty::InferFloat) => left,
        _ => left,
    }
}

fn mismatch(
    diagnostics: &mut Vec<TypeDiagnostic>,
    span: Range<usize>,
    expected: &Ty,
    actual: &Ty,
    context: &str,
) {
    diagnostics.push(TypeDiagnostic {
        span,
        message: format!(
            "{context} has type `{}`, expected `{}` ({actual:?} vs {expected:?})",
            actual.display_name(),
            expected.display_name()
        ),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use quill_source::SourceFile;
    use quill_syntax::parse;

    #[test]
    fn infers_primitive_function_calls_and_binary_expressions() {
        let source = SourceFile::from_text(
            "valid.rs",
            "fn add(left: i32, right: i32) -> i32 { left + right } fn main() { let result: i32 = add(20, 22); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn checks_scalar_match_patterns_and_exhaustiveness() {
        let source = SourceFile::from_text(
            "match.rs",
            "fn choose(value: i32) -> i32 { match value { 0 | 1 => 42, -2..=2 => 10, _ => 7 } } fn flag(value: bool) -> i32 { match value { true => 1, false => 0 } }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions_strict(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let non_exhaustive = SourceFile::from_text(
            "non-exhaustive-match.rs",
            "fn flag(value: bool) -> i32 { match value { true => 1 } }",
        );
        let parsed = parse(&non_exhaustive);
        let diagnostics = check_functions_strict(&parsed.functions);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("non-exhaustive match")),
            "{diagnostics:?}"
        );

        let mismatched = SourceFile::from_text(
            "mismatched-match.rs",
            "fn flag(value: bool) -> i32 { match value { 0 => 1, _ => 0 } }",
        );
        let parsed = parse(&mismatched);
        let diagnostics = check_functions_strict(&parsed.functions);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("match pattern")),
            "{diagnostics:?}"
        );

        let reversed_range = SourceFile::from_text(
            "reversed-range.rs",
            "fn bad(value: i32) -> i32 { match value { 2..=1 => 1, _ => 0 } }",
        );
        let parsed = parse(&reversed_range);
        let diagnostics = check_functions_strict(&parsed.functions);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("starts after its end")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn checks_enum_identity_variant_paths_and_exhaustiveness() {
        let source = SourceFile::from_text(
            "enum-match.rs",
            "fn valid(value: State) -> i32 { match value { State::Ready => 1, State::Done => 2 } } fn wrong_type(value: State) -> i32 { match value { Other::Ready => 1, _ => 2 } } fn missing(value: State) -> i32 { match value { State::Ready => 1 } }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let enum_signature = |path: &str| StructSignature {
            path: path.to_owned(),
            derived_traits: vec!["Copy".to_owned(), "PartialEq".to_owned()],
            is_enum: true,
            enum_variants: vec![
                EnumVariantSignature {
                    name: "Ready".to_owned(),
                    discriminant: 0,
                },
                EnumVariantSignature {
                    name: "Done".to_owned(),
                    discriminant: 1,
                },
            ],
            ..StructSignature::default()
        };
        let signatures = [enum_signature("State"), enum_signature("Other")];
        let diagnostics = check_functions_with_structs(&parsed.functions, &signatures);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("enum match pattern")),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("non-exhaustive match")),
            "{diagnostics:?}"
        );
    }

    fn pair_struct_signature() -> StructSignature {
        StructSignature {
            path: "Pair".to_owned(),
            generic_parameters: Vec::new(),
            derived_traits: vec!["PartialEq".to_owned()],
            fields: vec![
                StructFieldSignature {
                    name: "left".to_owned(),
                    type_text: "i32".to_owned(),
                },
                StructFieldSignature {
                    name: "right".to_owned(),
                    type_text: "i32".to_owned(),
                },
            ],
            ..StructSignature::default()
        }
    }

    #[test]
    fn checks_named_struct_literals_and_field_reads() {
        let source = SourceFile::from_text(
            "valid.rs",
            "struct Pair { left: i32, right: i32 } fn main() { let pair = Pair { left: 40, right: 2 }; std::process::exit(pair.right); }",
        );
        let parsed = parse(&source);
        assert!(
            check_functions_with_structs(&parsed.functions, &[pair_struct_signature()]).is_empty()
        );
    }

    #[test]
    fn clone_method_requires_clone_metadata_for_named_types() {
        let source = SourceFile::from_text(
            "clone-metadata.rs",
            "struct HasClone {} struct NoClone {} fn clone_has(value: HasClone) -> HasClone { value.clone() } fn clone_none(value: NoClone) -> NoClone { value.clone() }",
        );
        let parsed = parse(&source);
        let signatures = [
            StructSignature {
                path: "HasClone".to_owned(),
                generic_parameters: Vec::new(),
                derived_traits: vec!["Clone".to_owned()],
                fields: Vec::new(),
                ..StructSignature::default()
            },
            StructSignature {
                path: "NoClone".to_owned(),
                generic_parameters: Vec::new(),
                derived_traits: Vec::new(),
                fields: Vec::new(),
                ..StructSignature::default()
            },
        ];
        let diagnostics = check_functions_with_structs(&parsed.functions, &signatures);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(
            diagnostics[0]
                .message
                .contains("does not implement `Clone`")
        );
    }

    #[test]
    fn resolves_struct_fields_through_references() {
        let source = SourceFile::from_text(
            "borrowed-field.rs",
            "struct Pair { left: i32, right: i32 } fn right(pair: &Pair) -> i32 { pair.right } fn main() {}",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        assert!(
            check_functions_with_structs(&parsed.functions, &[pair_struct_signature()]).is_empty()
        );

        let mismatch = SourceFile::from_text(
            "borrowed-field-mismatch.rs",
            "struct Pair { left: i32, right: i32 } fn wrong(pair: &Pair) -> bool { pair.right }",
        );
        let parsed = parse(&mismatch);
        let diagnostics =
            check_functions_with_structs(&parsed.functions, &[pair_struct_signature()]);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("expected `bool`")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn checks_basic_string_vector_and_map_operations() {
        let source = SourceFile::from_text(
            "collections.rs",
            "struct Data { values: Vec<u64>, names: BTreeMap<String, u64> } fn build() -> Data { Data { values: Vec::new(), names: BTreeMap::new() } } fn mutate(data: &mut Data) { data.values.push(7); data.names.insert(String::new(), 9); } fn empty(data: &Data) -> bool { data.values.is_empty() } fn text() -> String { let mut out = String::with_capacity(4); out.push('x'); out }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let structure = StructSignature {
            path: "Data".to_owned(),
            generic_parameters: Vec::new(),
            derived_traits: Vec::new(),
            fields: vec![
                StructFieldSignature {
                    name: "values".to_owned(),
                    type_text: "Vec<u64>".to_owned(),
                },
                StructFieldSignature {
                    name: "names".to_owned(),
                    type_text: "BTreeMap<String, u64>".to_owned(),
                },
            ],
            ..StructSignature::default()
        };
        let diagnostics = check_functions_strict_with_structs(&parsed.functions, &[structure]);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn checks_nonempty_integer_array_borrowed_as_a_slice() {
        let source = SourceFile::from_text(
            "array-slice.rs",
            "fn total(values: &[i32]) -> i32 { let mut sum = 0; for value in values { sum = sum + value; } sum } fn main() { std::process::exit(total(&[11, 13, 18])); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let named_array = SourceFile::from_text(
            "named-array-slice.rs",
            "fn total(values: &[u64]) -> u64 { let mut sum = 0u64; for value in values { sum = sum + *value; } sum } fn main() { let values: [u64; 3] = [11, 13, 18]; let result = total(&values); }",
        );
        let parsed = parse(&named_array);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn checks_opaque_into_string_parameters_and_conversions() {
        let source = SourceFile::from_text(
            "into-string.rs",
            "fn own(value: impl Into<String>) -> String { value.into() } fn from_slice() -> String { own(\"ready\") } fn from_owned() -> String { own(String::new()) }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions_strict(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn parallel_body_checking_preserves_source_order_for_diagnostics() {
        let mut text = String::new();
        for index in 0..64 {
            text.push_str(&format!("fn bad_{index}() -> bool {{ {index} }}\n"));
        }
        let source = SourceFile::from_text("parallel-diagnostics.rs", text);
        let parsed = parse(&source);
        let diagnostics = check_functions_strict(&parsed.functions);
        assert_eq!(diagnostics.len(), 64);
        assert!(
            diagnostics
                .windows(2)
                .all(|pair| pair[0].span.start < pair[1].span.start)
        );
    }

    #[test]
    fn checks_iterator_any_and_fold_closure_bodies() {
        let source = SourceFile::from_text(
            "iterator-closures.rs",
            "fn has_empty(labels: &Vec<String>) -> bool { labels.iter().any(|label| label.trim().is_empty()) } fn count_bytes(input: &str) -> u64 { input.bytes().fold(0u64, |state, byte| state.wrapping_add(1)) }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions_strict(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn checks_tuple_destructuring_fold_and_impl_method_calls() {
        let source = SourceFile::from_text(
            "tuple-fold.rs",
            "struct Pair { value: u64 } impl Pair { fn plus(&self, amount: u64) -> u64 { self.value.wrapping_add(amount) } } fn combine(pair: &Pair, input: &str) -> (usize, u64) { let value = pair.plus(1); input.bytes().fold((0usize, value), |(count, hash), byte| (count + 1, hash ^ u64::from(byte))) }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions_strict_with_structs(
            &parsed.functions,
            &[StructSignature {
                path: "Pair".to_owned(),
                generic_parameters: Vec::new(),
                derived_traits: Vec::new(),
                fields: vec![StructFieldSignature {
                    name: "value".to_owned(),
                    type_text: "u64".to_owned(),
                }],
                ..StructSignature::default()
            }],
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn checks_types_bound_by_local_tuple_destructuring() {
        let source = SourceFile::from_text(
            "tuple-local.rs",
            "fn combine() -> u64 { let (count, digest) = (1u64, 2u64); count.wrapping_add(digest) }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions_strict(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn rejects_tuple_patterns_with_the_wrong_number_of_elements() {
        let source = SourceFile::from_text(
            "tuple-arity.rs",
            "fn combine() { let (left, right) = (1u64,); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions_strict(&parsed.functions);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.message.contains("tuple pattern has 2 names") })
        );
    }

    #[test]
    fn resolves_unqualified_root_calls_with_canonical_storage_paths() {
        let source = SourceFile::from_text(
            "canonical-root.rs",
            "fn digest() -> u64 { 7 } fn run() -> u64 { digest() }",
        );
        let parsed = parse(&source);
        let mut functions = parsed.functions;
        for function in &mut functions {
            function.name = format!("sample_crate::{}", function.name);
        }
        assert!(check_functions_strict(&functions).is_empty());
    }

    #[test]
    fn resolves_external_self_returning_builder_methods() {
        let source = SourceFile::from_text(
            "external-builder.rs",
            "fn run() { let record = Record::new(\"item\", 1u64).label(\"ready\"); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let external = [
            ExternalFunctionSignature {
                path: "catalog::Record::new".to_owned(),
                parameter_types: vec!["impl Into<String>".to_owned(), "u64".to_owned()],
                return_type: Some("Self".to_owned()),
                generic_parameters: Vec::new(),
                generic_copy_parameters: Vec::new(),
                generic_as_ref_str_parameters: Vec::new(),
                generic_add_parameters: Vec::new(),
                generic_has_bounds: false,
            },
            ExternalFunctionSignature {
                path: "catalog::Record::label".to_owned(),
                parameter_types: vec!["Self".to_owned(), "impl Into<String>".to_owned()],
                return_type: Some("Self".to_owned()),
                generic_parameters: Vec::new(),
                generic_copy_parameters: Vec::new(),
                generic_as_ref_str_parameters: Vec::new(),
                generic_add_parameters: Vec::new(),
                generic_has_bounds: false,
            },
        ];
        let diagnostics = check_functions_with_external_signatures_and_structs(
            &parsed.functions,
            &external,
            &[StructSignature {
                path: "catalog::Record".to_owned(),
                generic_parameters: Vec::new(),
                derived_traits: Vec::new(),
                fields: Vec::new(),
                ..StructSignature::default()
            }],
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn diagnoses_struct_field_type_name_and_presence_errors() {
        let source = SourceFile::from_text(
            "invalid.rs",
            "struct Pair { left: i32, right: i32 } fn main() { let pair = Pair { left: true, extra: 2 }; std::process::exit(pair.missing); }",
        );
        let parsed = parse(&source);
        let diagnostics =
            check_functions_with_structs(&parsed.functions, &[pair_struct_signature()]);
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.message.contains("struct field initializer")
                    && diagnostic.message.contains("expected `i32`")
            }),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("has no field `extra`"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("has no field `missing`"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("missing field `right`"))
        );
    }

    #[test]
    fn requires_partialeq_metadata_for_named_value_comparisons() {
        let source = SourceFile::from_text(
            "model.rs",
            "fn same(left: Pair, right: Pair) -> bool { left == right }",
        );
        let parsed = parse(&source);
        let mut pair = pair_struct_signature();
        pair.derived_traits.clear();
        let diagnostics = check_functions_with_structs(&parsed.functions, &[pair.clone()]);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("does not implement `PartialEq`")
        }));

        pair.derived_traits.push("PartialEq".to_owned());
        assert!(check_functions_with_structs(&parsed.functions, &[pair]).is_empty());
    }

    #[test]
    fn checks_u64_wrapping_methods_and_rejects_wrong_receivers() {
        let valid = SourceFile::from_text(
            "wrapping.rs",
            "fn mix(state: u64, byte: u64) -> u64 { state.wrapping_mul(33).wrapping_add(byte) }",
        );
        let parsed = parse(&valid);
        assert!(
            check_functions_with_structs(&parsed.functions, &[]).is_empty(),
            "valid wrapping arithmetic should type-check"
        );

        let invalid = SourceFile::from_text(
            "wrapping.rs",
            "fn mix(state: u32) -> u64 { state.wrapping_add(1) }",
        );
        let parsed = parse(&invalid);
        let diagnostics = check_functions_with_structs(&parsed.functions, &[]);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.message.contains("wrapping arithmetic receiver") }),
            "wrong receiver should be rejected: {diagnostics:?}"
        );
    }

    #[test]
    fn resolves_self_struct_literals_and_types_within_impl_methods() {
        let source = SourceFile::from_text(
            "impl.rs",
            "struct Pair { value: i32 } impl Pair { fn make() -> Self { Self { value: 42 } } }",
        );
        let parsed = parse(&source);
        let diagnostics = check_functions_with_structs(
            &parsed.functions,
            &[StructSignature {
                path: "Pair".to_owned(),
                generic_parameters: Vec::new(),
                derived_traits: Vec::new(),
                fields: vec![StructFieldSignature {
                    name: "value".to_owned(),
                    type_text: "i32".to_owned(),
                }],
                ..StructSignature::default()
            }],
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn checks_arguments_and_return_types_from_an_external_crate_signature() {
        let source = SourceFile::from_text(
            "caller.rs",
            "fn caller() -> i32 { model_alias::api::count(true) }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let external = ExternalFunctionSignature {
            path: "model_alias::api::count".to_owned(),
            parameter_types: vec!["u64".to_owned()],
            return_type: Some("bool".to_owned()),
            generic_parameters: Vec::new(),
            generic_copy_parameters: Vec::new(),
            generic_as_ref_str_parameters: Vec::new(),
            generic_add_parameters: Vec::new(),
            generic_has_bounds: false,
        };
        let diagnostics = check_functions_with_external_signatures(&parsed.functions, &[external]);
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic
                    .message
                    .contains("has type `bool`, expected `u64`")
            }),
            "{diagnostics:?}"
        );
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("function body")
                && diagnostic
                    .message
                    .contains("has type `bool`, expected `i32`")
        }));
    }

    #[test]
    fn checks_assignment_mutability_and_while_loop_types() {
        let valid = SourceFile::from_text(
            "loop.rs",
            "fn sum(n: i32) -> i32 { let mut i = 0; let mut total = 0; while i < n { total = total + i; i = i + 1; } total }",
        );
        let parsed = parse(&valid);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let immutable = SourceFile::from_text(
            "immutable.rs",
            "fn bad() -> i32 { let value = 0; value = 1; value }",
        );
        let parsed = parse(&immutable);
        let diagnostics = check_functions(&parsed.functions);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.message.contains("immutable variable `value`") })
        );
    }

    #[test]
    fn reports_borrow_of_moved_string_in_format_capture() {
        let source = SourceFile::from_text(
            "move.rs",
            "fn main() { let value = String::from(\"owned\"); let moved = value; println!(\"{value}\"); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "error[E0382]: borrow of moved value: `value`"
        );
        let capture = source.text().find("{value}").unwrap() + 1;
        assert_eq!(diagnostics[0].span, capture..capture + "value".len());
    }

    #[test]
    fn contextual_integer_literals_adapt_to_their_expected_integer_type() {
        let source = SourceFile::from_text(
            "numeric.rs",
            "fn small(value: u8) -> u8 { value + 1 } fn literal() -> u8 { 1 + 2 } fn main() { let value: u8 = small(4); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        assert!(check_functions(&parsed.functions).is_empty());
    }

    #[test]
    fn diagnoses_incompatible_local_and_function_body_types() {
        let source = SourceFile::from_text(
            "invalid.rs",
            "fn broken() -> i32 { let count: bool = 7; count }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.len() >= 2, "{diagnostics:?}");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("local binding"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("function body"))
        );
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.span.end <= source.text().len())
        );
    }

    #[test]
    fn checks_arity_and_primitive_argument_types_for_unique_calls() {
        let source = SourceFile::from_text(
            "calls.rs",
            "fn accept(value: u8) {} fn bad() { accept(true); accept(1, 2); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("function argument"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("expects 1 arguments"))
        );
    }

    #[test]
    fn strict_mode_reports_unresolved_calls_in_dead_bodies_but_allows_callable_locals() {
        let source = SourceFile::from_text(
            "unresolved.rs",
            "fn main() {} fn unused() -> i32 { missing() }",
        );
        let parsed = parse(&source);
        assert!(check_functions(&parsed.functions).is_empty());
        let diagnostics = check_functions_strict(&parsed.functions);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message == "unresolved function `missing`")
        );

        let source = SourceFile::from_text(
            "callable.rs",
            "fn apply(callback: fn() -> i32) -> i32 { callback() } fn main() {}",
        );
        let parsed = parse(&source);
        assert!(check_functions_strict(&parsed.functions).is_empty());
    }

    #[test]
    fn checks_the_process_exit_builtin_signature_and_never_type() {
        let valid = SourceFile::from_text("exit.rs", "fn main() { std::process::exit(17); }");
        let parsed = parse(&valid);
        assert!(check_functions(&parsed.functions).is_empty());

        let invalid =
            SourceFile::from_text("bad-exit.rs", "fn main() { std::process::exit(true); }");
        let parsed = parse(&invalid);
        let diagnostics = check_functions(&parsed.functions);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("expected `i32`"));
    }

    #[test]
    fn checks_result_constructors_payloads_and_status_methods() {
        let valid = SourceFile::from_text(
            "result.rs",
            "fn parse() -> Result<(), &'static str> { if true { Ok(()) } else { Err(\"bad\") } } fn has_error() -> bool { parse().is_err() }",
        );
        let parsed = parse(&valid);
        assert!(!parsed.had_error);
        assert!(check_functions_strict(&parsed.functions).is_empty());

        let invalid = SourceFile::from_text(
            "bad-result.rs",
            "fn bad() -> Result<(), &'static str> { Ok(\"wrong payload\") } fn status() -> bool { bad().is_ok(true) }",
        );
        let parsed = parse(&invalid);
        let diagnostics = check_functions_strict(&parsed.functions);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("Result payload")),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("expects 0 arguments")),
            "{diagnostics:?}"
        );

        let incompatible_residual = SourceFile::from_text(
            "bad-residual.rs",
            "fn inner() -> Result<(), &'static str> { Err(\"bad\") } fn outer() -> Result<(), i32> { inner()?; Ok(()) }",
        );
        let parsed = parse(&incompatible_residual);
        let diagnostics = check_functions_strict(&parsed.functions);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("compatible Result type")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn infers_i32_for_the_supported_identity_generic_instantiation() {
        let valid = SourceFile::from_text(
            "generic.rs",
            "fn identity<T>(value: T) -> T { value } fn main() { std::process::exit(identity(42)); }",
        );
        let parsed = parse(&valid);
        assert!(check_functions(&parsed.functions).is_empty());

        let invalid = SourceFile::from_text(
            "generic-bool.rs",
            "fn identity<T>(value: T) -> T { value } fn main() { std::process::exit(identity(true)); }",
        );
        let parsed = parse(&invalid);
        let diagnostics = check_functions(&parsed.functions);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("expected `i32`")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn checks_explicit_generic_arguments_and_contextual_integer_types() {
        let valid = SourceFile::from_text(
            "generic-explicit.rs",
            "fn identity<T>(value: T) -> T { value } fn main() { let _word: u64 = identity::<u64>(4294967296); std::process::exit(identity::<i32>(42)); }",
        );
        let parsed = parse(&valid);
        assert!(check_functions(&parsed.functions).is_empty());

        let invalid = SourceFile::from_text(
            "generic-explicit-mismatch.rs",
            "fn identity<T>(value: T) -> T { value } fn main() { std::process::exit(identity::<u64>(42i32)); }",
        );
        let parsed = parse(&invalid);
        let diagnostics = check_functions(&parsed.functions);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("explicit generic argument")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn checks_the_supported_copy_bound_for_scalar_instances() {
        let valid = SourceFile::from_text(
            "generic-copy.rs",
            "fn identity<T: Copy>(value: T) -> T { value } fn main() { std::process::exit(identity(42)); }",
        );
        let parsed = parse(&valid);
        assert!(check_functions(&parsed.functions).is_empty());

        let as_ref = SourceFile::from_text(
            "generic-as-ref.rs",
            "fn identity<T: AsRef<str>>(value: T) -> T { value } fn main() {}",
        );
        let parsed = parse(&as_ref);
        assert!(!parsed.functions[0].generic_has_bounds);
        assert_eq!(parsed.functions[0].generic_as_ref_str_parameters, ["T"]);

        let unsupported = SourceFile::from_text(
            "generic-display.rs",
            "fn identity<T: Display>(value: T) -> T { value } fn main() {}",
        );
        let parsed = parse(&unsupported);
        assert!(parsed.functions[0].generic_has_bounds);
    }

    #[test]
    fn rejects_a_generic_trait_bound_without_a_matching_impl() {
        let source = SourceFile::from_text(
            "generic-trait-missing-impl.rs",
            "trait Value { fn value(&self) -> u64; } struct Number; fn read<T: Value>(item: &T) -> u64 { item.value() } fn main() { let _result = read(&Number); }",
        );
        let parsed = parse(&source);
        let structs = [StructSignature {
            path: "Number".to_owned(),
            ..StructSignature::default()
        }];
        let diagnostics = check_functions_with_declarations_and_structs(
            &parsed.functions,
            &parsed.functions,
            false,
            &[],
            &structs,
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("does not implement `Value`")),
            "{diagnostics:?}"
        );

        let wrong_signature = SourceFile::from_text(
            "generic-trait-wrong-signature.rs",
            "trait Value { fn value(&self) -> u64; } struct Number; impl Value for Number { fn value(&self) -> i32 { 42 } } fn read<T: Value>(item: &T) -> u64 { item.value() } fn main() { let _result = read(&Number); }",
        );
        let parsed = parse(&wrong_signature);
        let structs = [StructSignature {
            path: "Number".to_owned(),
            ..StructSignature::default()
        }];
        let diagnostics = check_functions_with_declarations_and_structs(
            &parsed.functions,
            &parsed.functions,
            false,
            &[],
            &structs,
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("does not implement `Value`")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn infers_copy_generic_types_inside_tuple_slice_parameters() {
        let source = SourceFile::from_text(
            "generic-tuple-slice.rs",
            "fn count_pairs<T: Copy>(pairs: &[(T, T)]) -> usize { let mut count = 0usize; for (_left, _right) in pairs { count = count + 1usize; } count } fn main() { let count: usize = count_pairs(&[(10u64, 11u64), (10u64, 11u64)]); }",
        );
        let parsed = parse(&source);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn rejects_non_copy_types_for_copy_bounded_generics() {
        let source = SourceFile::from_text(
            "generic-copy-reject.rs",
            "fn identity<T: Copy>(value: T) -> T { value } fn forward(text: String) -> String { identity(text) }",
        );
        let parsed = parse(&source);
        let diagnostics = check_functions(&parsed.functions);
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("does not satisfy the `Copy` bound")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn rejects_forwarding_an_unbounded_parameter_to_a_copy_generic() {
        let source = SourceFile::from_text(
            "generic-copy-forward.rs",
            "fn identity<T: Copy>(value: T) -> T { value } fn forward<T>(value: T) -> T { identity(value) }",
        );
        let parsed = parse(&source);
        let diagnostics = check_functions(&parsed.functions);
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("does not satisfy the `Copy` bound")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn infers_each_type_in_a_two_parameter_generic_call() {
        let valid = SourceFile::from_text(
            "generic-pair.rs",
            "fn second<T, U>(first: T, value: U) -> U { value } fn main() { std::process::exit(second(17, 42)); }",
        );
        let parsed = parse(&valid);
        assert!(check_functions(&parsed.functions).is_empty());
    }

    #[test]
    fn checks_add_output_bounds_on_generic_bodies_and_call_sites() {
        let valid = SourceFile::from_text(
            "generic-add.rs",
            "fn plus<T: std::ops::Add<Output = T>>(left: T, right: T) -> T { left + right } fn main() { let _value = plus(20i32, 22i32); }",
        );
        let parsed = parse(&valid);
        assert!(check_functions(&parsed.functions).is_empty());

        let invalid = SourceFile::from_text(
            "generic-add-reject.rs",
            "struct Value {} fn plus<T: std::ops::Add<Output = T>>(left: T, right: T) -> T { left + right } fn main() { let _value = plus(Value {}, Value {}); }",
        );
        let parsed = parse(&invalid);
        let value = StructSignature {
            path: "Value".to_owned(),
            ..StructSignature::default()
        };
        let diagnostics = check_functions_with_structs(&parsed.functions, &[value]);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("does not satisfy the `Add")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn checks_tuple_bindings_in_slice_for_each_loops() {
        let source = SourceFile::from_text(
            "for-each.rs",
            "fn sum_pairs(fields: &[(u64, u64)]) { for (key, value) in fields { let _sum = key + value; } }",
        );
        let parsed = parse(&source);
        assert!(
            check_functions(&parsed.functions).is_empty(),
            "{:?}",
            check_functions(&parsed.functions)
        );
    }

    #[test]
    fn type_checks_as_ref_bounds_inside_tuple_slice_loops() {
        let source = SourceFile::from_text(
            "as-ref-loop.rs",
            "struct Record {} impl Record { fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {} } fn ingest<T: AsRef<str>>(record: &mut Record, fields: &[(T, T)]) { for (key, value) in fields { record.insert(key.as_ref(), value.as_ref()); } }",
        );
        let parsed = parse(&source);
        let record = StructSignature {
            path: "Record".to_owned(),
            generic_parameters: Vec::new(),
            derived_traits: Vec::new(),
            fields: Vec::new(),
            ..StructSignature::default()
        };
        let diagnostics = check_functions_with_structs(&parsed.functions, &[record]);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn checks_as_ref_str_generic_instantiations() {
        let valid = SourceFile::from_text(
            "as-ref-instantiation.rs",
            "fn length<T: AsRef<str>>(value: T) -> usize { value.as_ref().len() } fn caller(value: &str) -> usize { length(value) }",
        );
        let parsed = parse(&valid);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let invalid = SourceFile::from_text(
            "as-ref-invalid.rs",
            "fn length<T: AsRef<str>>(value: T) -> usize { value.as_ref().len() } fn caller() -> usize { length(42u64) }",
        );
        let parsed = parse(&invalid);
        let diagnostics = check_functions(&parsed.functions);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("does not satisfy `AsRef<str>`")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn type_checks_string_character_iteration_and_lowercase_extension() {
        let source = SourceFile::from_text(
            "char-iteration.rs",
            "fn normalize(input: &str) -> String { let mut output = String::new(); for ch in input.chars() { if ch.is_ascii_alphanumeric() { output.extend(ch.to_lowercase()); } } output }",
        );
        let parsed = parse(&source);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn format_macro_expressions_infer_owned_string_results() {
        let source = SourceFile::from_text(
            "partial.rs",
            "fn run(revision: u64) -> String { format!(\"item-{revision}\") }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        assert!(check_functions(&parsed.functions).is_empty());
    }

    #[test]
    fn local_callable_bindings_are_not_mistaken_for_function_items() {
        let source = SourceFile::from_text(
            "closure.rs",
            "fn target(value: u8) {} fn caller() { let target = |_| (); target(); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        assert!(check_functions(&parsed.functions).is_empty());
    }
}
