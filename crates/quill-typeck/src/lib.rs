use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

use quill_parallel::WorkPool;
use quill_syntax::{
    BinaryOperator, BodyExpr, BodyExprKind, BodyStatement, ClosureParameter, FunctionItem,
    LiteralKind, MatchPattern,
};
use serde::{Deserialize, Serialize};

mod borrowck;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct TypeDiagnostic {
    pub span: Range<usize>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ExternalFunctionSignature {
    /// Fully qualified call spelling as it appears in the caller's source.
    pub path: String,
    pub scope: Vec<String>,
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
    Reference(Box<Ty>, bool),
    RawPointer(Box<Ty>, bool),
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
            Self::Reference(_, _) => "reference",
            Self::RawPointer(_, _) => "raw pointer",
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
        let matches_enum = if enum_path == "Self" {
            item.path != "Self"
        } else {
            item.path == enum_path
                || item.path.ends_with(&format!("::{enum_path}"))
                || item.path.rsplit("::").next() == Some(requested_enum_name)
        };
        if !item.is_enum || !matches_enum {
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
    if text == "Self" {
        return method_owner(function).map_or(Ty::Unknown, |owner| {
            let resolved = type_from_syntax(owner, structs);
            if resolved == Ty::Unknown {
                Ty::Named(owner.to_owned())
            } else {
                resolved
            }
        });
    }
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
            reference
        };
        let mutable = referent.starts_with("mut ");
        let referent = referent.strip_prefix("mut ").unwrap_or(referent);
        return Ty::Reference(
            Box::new(type_from_function_syntax(referent, function, structs)),
            mutable,
        );
    }
    if let Some(value) = text.strip_prefix("mut ") {
        return type_from_function_syntax(value, function, structs);
    }
    if let Some((ok_type, err_type)) = generic_pair(text, "Result")
        .or_else(|| generic_pair(text, "std::result::Result"))
        .or_else(|| generic_pair(text, "core::result::Result"))
    {
        return Ty::Result(
            Box::new(type_from_function_syntax(ok_type, function, structs)),
            Box::new(type_from_function_syntax(err_type, function, structs)),
        );
    }
    if let Some(value_type) = generic_single(text, "Option")
        .or_else(|| generic_single(text, "std::option::Option"))
        .or_else(|| generic_single(text, "core::option::Option"))
    {
        return Ty::Option(Box::new(type_from_function_syntax(
            value_type, function, structs,
        )));
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

fn function_has_trait_owner(function: &FunctionItem, owner: &str) -> bool {
    function.scope.iter().any(|scope| {
        scope
            .strip_prefix("trait:")
            .is_some_and(|candidate| {
                candidate == owner
                    || candidate.rsplit("::").next() == Some(owner.rsplit("::").next().unwrap_or(owner))
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
    let candidates: Vec<_> = traits
        .iter()
        .filter_map(|trait_name| signatures.trait_methods.get(trait_name))
        .flatten()
        .filter(|function| function.name.rsplit("::").next() == Some(method))
        .copied()
        .collect();
    candidates
}

fn trait_methods_for_concrete<'a>(
    signatures: &'a SignatureIndex<'a>,
    receiver_type: &Ty,
    method: &str,
) -> Vec<&'a FunctionItem> {
    let Ty::Named(receiver_name) = without_references(receiver_type) else {
        return Vec::new();
    };
    let receiver_basename = receiver_name.rsplit("::").next().unwrap_or(receiver_name);
    let traits: HashSet<_> = signatures
        .trait_impl_methods
        .keys()
        .filter_map(|(trait_name, implementor)| {
            (*implementor == receiver_basename).then_some(*trait_name)
        })
        .collect();
    let mut candidates: Vec<_> = traits
        .into_iter()
        .filter_map(|trait_name| signatures.trait_methods.get(trait_name))
        .flatten()
        .filter(|function| function.name.rsplit("::").next() == Some(method))
        .copied()
        .collect();
    candidates.sort_by(|left, right| left.name.cmp(&right.name));
    candidates.dedup_by(|left, right| left.name == right.name);
    candidates
}

fn trait_methods_for_context<'a>(
    signatures: &'a SignatureIndex<'a>,
    context: &Ty,
    receiver: &BodyExpr,
    receiver_type: &Ty,
    method: &str,
) -> Vec<&'a FunctionItem> {
    let Ty::Named(owner) = context else {
        return Vec::new();
    };
    let owner = owner.rsplit("::").next().unwrap_or(owner);
    let receiver_is_self = move_place(receiver).as_deref() == Some("self")
        || matches!(without_references(receiver_type), Ty::Named(name) if name.rsplit("::").next() == Some(owner));
    if !receiver_is_self {
        return Vec::new();
    }
    signatures
        .trait_methods
        .get(owner)
        .into_iter()
        .flatten()
        .filter(|function| function.name.rsplit("::").next() == Some(method))
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

fn error_type_converts_to(
    signatures: &SignatureIndex<'_>,
    source: &Ty,
    target: &Ty,
) -> bool {
    if types_compatible(target, source) {
        return true;
    }
    let (Ty::Named(source), Ty::Named(target)) = (source, target) else {
        return false;
    };
    let source = source.rsplit("::").next().unwrap_or(source);
    let target = target.rsplit("::").next().unwrap_or(target);
    signatures
        .trait_impl_methods
        .get(&("From", target))
        .into_iter()
        .flatten()
        .any(|implementation| {
            implementation.name.rsplit("::").next() == Some("from")
                && implementation.parameters.get(1).is_some_and(|parameter| {
                    parameter
                        .type_text
                        .trim_start_matches('&')
                        .trim_start_matches("mut ")
                        .trim()
                        .rsplit("::")
                        .next()
                        == Some(source)
                })
        })
}

fn expression_always_diverges(expression: &BodyExpr) -> bool {
    match &expression.kind {
        BodyExprKind::Return(_) => true,
        BodyExprKind::Block { statements, tail } => tail
            .as_deref()
            .is_some_and(expression_always_diverges)
            || statements.last().is_some_and(|statement| {
                matches!(statement, BodyStatement::Expression { expression, .. }
                    if expression_always_diverges(expression))
            }),
        BodyExprKind::If {
            then_branch,
            else_branch: Some(else_branch),
            ..
        } => expression_always_diverges(then_branch)
            && expression_always_diverges(else_branch),
        BodyExprKind::Match { arms, .. } => {
            !arms.is_empty() && arms.iter().all(|arm| expression_always_diverges(&arm.body))
        }
        _ => false,
    }
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

fn call_argument_is_borrowed(
    signatures: &SignatureIndex<'_>,
    callee: &str,
    index: usize,
) -> bool {
    let candidates = signatures
        .exact
        .get(callee)
        .or_else(|| signatures.suffix.get(callee.rsplit("::").next().unwrap_or(callee)));
    let Some(candidates) = candidates.filter(|candidates| !candidates.is_empty()) else {
        return false;
    };
    candidates.iter().all(|function| {
        function
            .parameters
            .get(index)
            .is_some_and(|parameter| parameter.type_text.trim_start().starts_with('&'))
    })
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
            reference
        };
        let mutable = referent.starts_with("mut ");
        let referent = referent.strip_prefix("mut ").unwrap_or(referent);
        return Ty::Reference(Box::new(type_from_syntax(referent, structs)), mutable);
    }
    if let Some(pointer) = text.strip_prefix('*') {
        let pointer = pointer.trim_start();
        let mutable = pointer.starts_with("mut ");
        let referent = pointer
            .strip_prefix("mut ")
            .or_else(|| pointer.strip_prefix("const "))
            .unwrap_or(pointer);
        return Ty::RawPointer(Box::new(type_from_syntax(referent, structs)), mutable);
    }
    if matches!(text, "fmt::Result" | "std::fmt::Result" | "core::fmt::Result") {
        return Ty::Result(
            Box::new(Ty::Unit),
            Box::new(Ty::Named("core::fmt::Error".to_owned())),
        );
    }
    let nominal_head = text.split('<').next().unwrap_or(text).trim();
    if nominal_head.rsplit("::").next() == Some("Formatter") {
        return Ty::Named("core::fmt::Formatter".to_owned());
    }
    if let Some((ok_type, err_type)) = generic_pair(text, "Result") {
        return Ty::Result(
            Box::new(type_from_syntax(ok_type, structs)),
            Box::new(type_from_syntax(err_type, structs)),
        );
    }
    // A crate-local `type Result<T> = std::result::Result<T, Error>` alias is
    // common in I/O libraries. Its error type is unavailable until aliases
    // are part of HIR, but preserving the Result shape still allows `?` and
    // `Ok` payloads to be checked without inventing an error type.
    if let Some(ok_type) = generic_single(text, "Result") {
        return Ty::Result(
            Box::new(type_from_syntax(ok_type, structs)),
            Box::new(Ty::Unknown),
        );
    }
    // std::io::Result<T> and io::Result<T> are aliases for
    // Result<T, std::io::Error>. Keeping the error type explicit lets `?`
    // propagate through ordinary Read/Write helper methods.
    if let Some(ok_type) = generic_single(text, "std::io::Result")
        .or_else(|| generic_single(text, "io::Result"))
    {
        return Ty::Result(
            Box::new(type_from_syntax(ok_type, structs)),
            Box::new(Ty::Named("std::io::Error".to_owned())),
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
    if let Some(trait_bounds) = text
        .strip_prefix("impl ")
        .or_else(|| text.strip_prefix("dyn "))
    {
        let traits = trait_bounds
            .split('+')
            .map(str::trim)
            .filter(|bound| !bound.is_empty() && !bound.contains("="))
            .map(|bound| bound.rsplit("::").next().unwrap_or(bound))
            .collect::<Vec<_>>()
            .join("+");
        if !traits.is_empty() {
            return Ty::Named(format!("\0quill_generic:_opaque:traits={traits}:0:0:0"));
        }
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
        if let Some((element, length)) = element.split_once(';') {
            return Ty::Array(
                Box::new(type_from_syntax(element, structs)),
                length.trim().parse::<usize>().unwrap_or(0),
            );
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
        let Ty::Reference(actual_referent, actual_mutable) = actual else {
            return None;
        };
        let referent = referent.trim_start();
        if referent.starts_with("mut ") && !*actual_mutable {
            return None;
        }
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
    while let Ty::Reference(inner, _) = ty {
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
        | Ty::Unknown => true,
        Ty::Reference(_, mutable) => !*mutable,
        Ty::RawPointer(_, _) => true,
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
            scope: signature.scope.clone(),
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
                    mutable: false,
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
        for function in declarations {
            add_signature_suffixes(&mut exact, &mut suffix, function);
            if method_owner(function).is_some()
                && let Some(method) = function.name.rsplit("::").next()
            {
                methods.entry(method).or_default().push(function);
            }
        index_trait_method(
            &mut trait_methods,
            &mut trait_impl_methods,
            function,
        );
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
        index_trait_method(
            &mut trait_methods,
            &mut trait_impl_methods,
            function,
        );
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
        if let Some(structure) = find_nominal_signature(&function_structs, self_type).cloned() {
            function_structs.push(StructSignature {
                path: "Self".to_owned(),
                generic_parameters: structure.generic_parameters,
                derived_traits: structure.derived_traits,
                fields: structure.fields,
                is_enum: structure.is_enum,
                enum_variants: structure.enum_variants,
            });
        }
        Cow::Owned(function_structs)
    } else {
        Cow::Borrowed(struct_signatures)
    };
    let function_structs = function_structs.as_ref();
    let mut locals = HashMap::new();
    if let Some(trait_owner) = function.scope.iter().find_map(|scope| {
        scope
            .strip_prefix("trait:")
            .or_else(|| scope.strip_prefix("traitimpl:"))
    }) {
        locals.insert(
            "\0quill_trait_context".to_owned(),
            Ty::Named(trait_owner.to_owned()),
        );
    }
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
                    parameter.mutable,
                )
            })
        })
        .collect();
    for parameter in &function.parameters {
        if parameter.name.as_deref() == Some("self") && parameter.type_text.starts_with("&mut ") {
            mutable_locals.insert("\0mutable_ref:self".to_owned(), true);
        }
    }
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
    diagnostics.extend(
        borrowck::check(function, body, signatures, function_structs, &locals)
            .into_iter()
            .map(|diagnostic| TypeDiagnostic {
                span: diagnostic.span,
                message: diagnostic.message,
            }),
    );
    check_mutability(body, &mut mutable_locals, &mut diagnostics);
    check_loop_control(body, &mut Vec::new(), &mut diagnostics);
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

/// Propagate moves around a loop until the header state includes every move
/// that can flow back from its body. Move state only grows over a finite set
/// of places, so the union reaches a fixed point without an iteration limit.
#[derive(Clone, Debug, PartialEq, Eq)]
enum MoveLoopExit {
    BackEdge,
    Break(Option<String>),
    Continue(Option<String>),
    Return,
}

fn move_loop_fixed_point(
    incoming: &HashSet<String>,
    mut transfer: impl FnMut(&mut HashSet<String>, &mut HashSet<String>) -> MoveLoopExit,
) -> (HashSet<String>, HashSet<String>) {
    let mut header = incoming.clone();
    let mut break_exits = HashSet::new();
    loop {
        let mut back_edge = header.clone();
        let mut side_back_edges = HashSet::new();
        match transfer(&mut back_edge, &mut side_back_edges) {
            MoveLoopExit::Break(_) => {
                break_exits.extend(back_edge.iter().cloned());
                back_edge.clone_from(&header);
            }
            MoveLoopExit::Continue(_) => {}
            MoveLoopExit::Return => back_edge.clone_from(&header),
            MoveLoopExit::BackEdge => {}
        }
        let mut next = incoming.clone();
        next.extend(back_edge);
        next.extend(side_back_edges);
        if next == header {
            return (header, break_exits);
        }
        header = next;
    }
}

fn push_move_diagnostic(
    diagnostics: &mut Vec<TypeDiagnostic>,
    span: Range<usize>,
    message: impl Into<String>,
) {
    let diagnostic = TypeDiagnostic {
        span,
        message: message.into(),
    };
    if !diagnostics.contains(&diagnostic) {
        diagnostics.push(diagnostic);
    }
}

fn move_match_bindings(pattern: &MatchPattern) -> Vec<String> {
    match pattern {
        MatchPattern::Binding(name) => vec![name.clone()],
        MatchPattern::Destructure { bindings, .. } => bindings.clone(),
        MatchPattern::Or(patterns) => patterns
            .iter()
            .flat_map(move_match_bindings)
            .collect(),
        _ => Vec::new(),
    }
}

#[derive(Clone, Copy)]
enum LabeledLoopControl {
    Break,
    Continue,
}

fn contains_labeled_control(
    expression: &BodyExpr,
    label: &str,
    control: LabeledLoopControl,
) -> bool {
    fn in_statement(
        statement: &BodyStatement,
        label: &str,
        control: LabeledLoopControl,
    ) -> bool {
        match statement {
            BodyStatement::Break {
                label: Some(target), ..
            } if matches!(control, LabeledLoopControl::Break) => target == label,
            BodyStatement::Continue {
                label: Some(target), ..
            } if matches!(control, LabeledLoopControl::Continue) => target == label,
            BodyStatement::While {
                label: nested_label,
                condition,
                body,
                ..
            } => {
                nested_label.as_deref() != Some(label)
                    && (contains_labeled_control(condition, label, control)
                        || contains_labeled_control(body, label, control))
            }
            BodyStatement::Loop {
                label: nested_label,
                body,
                ..
            } => {
                nested_label.as_deref() != Some(label)
                    && contains_labeled_control(body, label, control)
            }
            BodyStatement::ForRange {
                label: nested_label,
                start,
                end,
                body,
                ..
            } => {
                nested_label.as_deref() != Some(label)
                    && (contains_labeled_control(start, label, control)
                        || contains_labeled_control(end, label, control)
                        || contains_labeled_control(body, label, control))
            }
            BodyStatement::ForEach {
                label: nested_label,
                iterator,
                body,
                ..
            } => {
                nested_label.as_deref() != Some(label)
                    && (contains_labeled_control(iterator, label, control)
                        || contains_labeled_control(body, label, control))
            }
            BodyStatement::Let {
                initializer: Some(value),
                ..
            }
            | BodyStatement::LetTuple {
                initializer: value,
                ..
            }
            | BodyStatement::Assign { value, .. } => {
                contains_labeled_control(value, label, control)
            }
            BodyStatement::Expression { expression, .. } => {
                contains_labeled_control(expression, label, control)
            }
            BodyStatement::MacroInvocation { arguments, .. } => arguments
                .iter()
                .any(|argument| contains_labeled_control(argument, label, control)),
            _ => false,
        }
    }
    match &expression.kind {
        BodyExprKind::Block { statements, tail } => {
            statements
                .iter()
                .any(|statement| in_statement(statement, label, control))
                || tail
                    .as_deref()
                    .is_some_and(|tail| contains_labeled_control(tail, label, control))
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            contains_labeled_control(condition, label, control)
                || contains_labeled_control(then_branch, label, control)
                || else_branch
                    .as_deref()
                    .is_some_and(|branch| contains_labeled_control(branch, label, control))
        }
        BodyExprKind::Match { scrutinee, arms } => {
            contains_labeled_control(scrutinee, label, control)
                || arms
                    .iter()
                    .any(|arm| contains_labeled_control(&arm.body, label, control))
        }
        BodyExprKind::Call { arguments, .. }
        | BodyExprKind::Tuple(arguments)
        | BodyExprKind::ArrayLiteral(arguments)
        | BodyExprKind::FixedArrayLiteral(arguments) => arguments
            .iter()
            .any(|argument| contains_labeled_control(argument, label, control)),
        BodyExprKind::MethodCall {
            receiver,
            arguments,
            ..
        } => {
            contains_labeled_control(receiver, label, control)
                || arguments
                    .iter()
                    .any(|argument| contains_labeled_control(argument, label, control))
        }
        BodyExprKind::Return(Some(value))
        | BodyExprKind::Try(value)
        | BodyExprKind::Parenthesized(value)
        | BodyExprKind::Reference {
            expression: value, ..
        }
        | BodyExprKind::Dereference(value)
        | BodyExprKind::Negate(value)
        | BodyExprKind::Not(value)
        | BodyExprKind::Cast {
            expression: value, ..
        } => contains_labeled_control(value, label, control),
        BodyExprKind::Binary { left, right, .. } => {
            contains_labeled_control(left, label, control)
                || contains_labeled_control(right, label, control)
        }
        BodyExprKind::FieldAccess { base, .. } => {
            contains_labeled_control(base, label, control)
        }
        BodyExprKind::Index { base, index } => {
            contains_labeled_control(base, label, control)
                || contains_labeled_control(index, label, control)
        }
        BodyExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .any(|(_, value)| contains_labeled_control(value, label, control)),
        BodyExprKind::Closure { .. } => false,
        _ => false,
    }
}

/// Analyze loop-body paths through direct exits and top-level `if`/`match`
/// splits. Terminating branches contribute exit states; only continuing paths
/// flow into the loop back edge.
fn check_move_loop_transfer(
    expression: &BodyExpr,
    current_label: Option<&str>,
    parent_label: Option<&str>,
    signatures: &SignatureIndex<'_>,
    struct_signatures: &[StructSignature],
    locals: &mut HashMap<String, Ty>,
    moved: &mut HashSet<String>,
    diagnostics: &mut Vec<TypeDiagnostic>,
    branch_breaks: &mut Vec<HashSet<String>>,
    outer_back_edges: &mut HashSet<String>,
) -> MoveLoopExit {
    let BodyExprKind::Block {
        statements: block_statements,
        tail,
    } = &expression.kind
    else {
        check_move_expression(
            expression,
            false,
            signatures,
            struct_signatures,
            locals,
            moved,
            diagnostics,
        );
        return MoveLoopExit::BackEdge;
    };
    let expanded_statements;
    let statements: &[BodyStatement] = if let Some(tail) = tail {
        expanded_statements = {
            let mut statements = block_statements.clone();
            statements.push(BodyStatement::Expression {
                expression: tail.as_ref().clone(),
                has_semicolon: false,
            });
            statements
        };
        &expanded_statements
    } else {
        block_statements
    };
    if let Some(current_label) = current_label {
        let nested_loop = statements.iter().enumerate().find_map(|(index, statement)| {
            let BodyStatement::Loop {
                label: nested_label,
                body,
                ..
            } = statement
            else {
                return None;
            };
            let prefix_is_scope_safe = statements[..index].iter().all(|statement| {
                !matches!(
                    statement,
                    BodyStatement::Let { .. }
                        | BodyStatement::LetTuple { .. }
                        | BodyStatement::ForRange { .. }
                        | BodyStatement::ForEach { .. }
                        | BodyStatement::Break { .. }
                        | BodyStatement::Continue { .. }
                )
            });
            (prefix_is_scope_safe
                && (contains_labeled_control(
                    body,
                    current_label,
                    LabeledLoopControl::Break,
                ) || contains_labeled_control(
                    body,
                    current_label,
                    LabeledLoopControl::Continue,
                )))
                .then_some((index, nested_label.as_deref(), body))
        });
        if let Some((nested_index, nested_label, nested_body)) = nested_loop {
            if nested_index > 0 {
                let prefix = BodyExpr {
                    kind: BodyExprKind::Block {
                        statements: statements[..nested_index].to_vec(),
                        tail: None,
                    },
                    span: expression.span.clone(),
                };
                check_move_expression(
                    &prefix,
                    false,
                    signatures,
                    struct_signatures,
                    locals,
                    moved,
                    diagnostics,
                );
            }
            let first_exit = branch_breaks.len();
            let outcome = check_move_loop_transfer(
                nested_body,
                nested_label,
                Some(current_label),
                signatures,
                struct_signatures,
                locals,
                moved,
                diagnostics,
                branch_breaks,
                outer_back_edges,
            );
            if matches!(
                &outcome,
                MoveLoopExit::Break(Some(target)) | MoveLoopExit::Continue(Some(target))
                    if target == current_label
            ) {
                for exit in &branch_breaks[first_exit..] {
                    moved.extend(exit.iter().cloned());
                }
                return outcome;
            }
        }
    }
    let branch_if = statements.iter().enumerate().find_map(|(index, statement)| {
        if statements[..index].iter().any(|prior| {
            matches!(
                prior,
                BodyStatement::Expression { expression, .. }
                    if matches!(&expression.kind, BodyExprKind::Match { .. })
            )
        }) {
            return None;
        }
        let BodyStatement::Expression {
            expression:
                BodyExpr {
                    kind:
                        BodyExprKind::If {
                            condition,
                            then_branch,
                            else_branch,
                        },
                    ..
                },
            ..
        } = statement
        else {
            return None;
        };
        let prefix_is_scope_safe = statements[..index].iter().all(|statement| {
            !matches!(
                statement,
                BodyStatement::Let { .. }
                    | BodyStatement::LetTuple { .. }
                    | BodyStatement::ForRange { .. }
                    | BodyStatement::ForEach { .. }
                    | BodyStatement::Break { .. }
                    | BodyStatement::Continue { .. }
            ) && !matches!(
                statement,
                BodyStatement::Expression {
                    expression,
                    ..
                } if matches!(&expression.kind, BodyExprKind::Return(_))
            )
        });
        prefix_is_scope_safe.then_some((index, condition, then_branch, else_branch))
    });
    if let Some((if_index, condition, then_branch, else_branch)) = branch_if {
        if if_index > 0 {
            let prefix = BodyExpr {
                kind: BodyExprKind::Block {
                    statements: statements[..if_index].to_vec(),
                    tail: None,
                },
                span: expression.span.clone(),
            };
            check_move_expression(
                &prefix,
                false,
                signatures,
                struct_signatures,
                locals,
                moved,
                diagnostics,
            );
        }
        check_move_expression(
            condition,
            false,
            signatures,
            struct_signatures,
            locals,
            moved,
            diagnostics,
        );
        let incoming = moved.clone();
        let mut continuing = Vec::new();
        let mut branch_exits = Vec::new();
        let mut propagated_exits = Vec::new();
        for branch in [Some(then_branch.as_ref()), else_branch.as_deref()]
            .into_iter()
            .flatten()
        {
            let mut branch_locals = locals.clone();
            let mut branch_moved = incoming.clone();
            match check_move_loop_transfer(
                branch,
                current_label,
                parent_label,
                signatures,
                struct_signatures,
                &mut branch_locals,
                &mut branch_moved,
                diagnostics,
                branch_breaks,
                outer_back_edges,
            ) {
                MoveLoopExit::BackEdge => continuing.push(branch_moved),
                MoveLoopExit::Break(target)
                    if target.as_deref().is_none_or(|target| Some(target) == current_label) =>
                {
                    branch_breaks.push(branch_moved.clone());
                    branch_exits.push(branch_moved);
                }
                MoveLoopExit::Continue(target)
                    if target.as_deref().is_none_or(|target| Some(target) == current_label) =>
                {
                    continuing.push(branch_moved)
                }
                MoveLoopExit::Break(target) => {
                    if parent_label.is_some() && target.as_deref() == parent_label {
                        branch_breaks.push(branch_moved.clone());
                    }
                    propagated_exits.push((MoveLoopExit::Break(target), branch_moved));
                }
                MoveLoopExit::Continue(target) => {
                    if parent_label.is_some() && target.as_deref() == parent_label {
                        outer_back_edges.extend(branch_moved.iter().cloned());
                    }
                    propagated_exits.push((MoveLoopExit::Continue(target), branch_moved));
                }
                MoveLoopExit::Return => {}
            }
        }
        if else_branch.is_none() {
            continuing.push(incoming);
        }
        if continuing.is_empty() {
            if branch_exits.is_empty() {
                if let Some((exit, _)) = propagated_exits.first().cloned()
                    && propagated_exits
                        .iter()
                        .all(|(other_exit, _)| other_exit == &exit)
                {
                    moved.clear();
                    for (_, state) in propagated_exits {
                        moved.extend(state);
                    }
                    return exit;
                }
                return MoveLoopExit::Return;
            }
            moved.clear();
            for exit in branch_exits {
                moved.extend(exit);
            }
            return MoveLoopExit::Break(current_label.map(str::to_owned));
        }
        // A labeled continue reaches the enclosing loop's next iteration,
        // while a labeled break exits it. Only continues belong in this
        // loop's back-edge state; joining a break here leaks moves past the
        // enclosing loop exit.
        continuing.extend(propagated_exits.into_iter().filter_map(|(exit, state)| {
            matches!(exit, MoveLoopExit::Continue(_)).then_some(state)
        }));
        moved.clear();
        for state in continuing {
            moved.extend(state);
        }
        if if_index + 1 == statements.len() {
            return MoveLoopExit::BackEdge;
        }
        let suffix = BodyExpr {
            kind: BodyExprKind::Block {
                statements: statements[if_index + 1..].to_vec(),
                tail: None,
            },
            span: expression.span.clone(),
        };
        return check_move_loop_transfer(
            &suffix,
            current_label,
            parent_label,
            signatures,
            struct_signatures,
            locals,
            moved,
            diagnostics,
            branch_breaks,
            outer_back_edges,
        );
    }
    let branch_match = statements.iter().enumerate().find_map(|(index, statement)| {
        let BodyStatement::Expression {
            expression:
                BodyExpr {
                    kind: BodyExprKind::Match { scrutinee, arms },
                    ..
                },
            ..
        } = statement
        else {
            return None;
        };
        let prefix_is_scope_safe = statements[..index].iter().all(|statement| {
            !matches!(
                statement,
                BodyStatement::Let { .. }
                    | BodyStatement::LetTuple { .. }
                    | BodyStatement::ForRange { .. }
                    | BodyStatement::ForEach { .. }
                    | BodyStatement::Break { .. }
                    | BodyStatement::Continue { .. }
            ) && !matches!(
                statement,
                BodyStatement::Expression { expression, .. }
                    if matches!(&expression.kind, BodyExprKind::Return(_))
            )
        });
        prefix_is_scope_safe.then_some((index, scrutinee, arms))
    });
    if let Some((match_index, scrutinee, arms)) = branch_match {
        if match_index > 0 {
            let prefix = BodyExpr {
                kind: BodyExprKind::Block {
                    statements: statements[..match_index].to_vec(),
                    tail: None,
                },
                span: expression.span.clone(),
            };
            check_move_expression(
                &prefix,
                false,
                signatures,
                struct_signatures,
                locals,
                moved,
                diagnostics,
            );
        }
        let consumes_scrutinee = arms
            .iter()
            .any(|arm| !move_match_bindings(&arm.pattern).is_empty());
        check_move_expression(
            scrutinee,
            consumes_scrutinee,
            signatures,
            struct_signatures,
            locals,
            moved,
            diagnostics,
        );
        let incoming = moved.clone();
        let mut continuing = Vec::new();
        let mut exits = Vec::new();
        let mut propagated_exits = Vec::new();
        for arm in arms {
            let mut arm_locals = locals.clone();
            let mut arm_moved = incoming.clone();
            let bindings = move_match_bindings(&arm.pattern);
            for name in &bindings {
                arm_locals.insert(name.clone(), Ty::Unknown);
                arm_moved.remove(name);
            }
            let outcome = check_move_loop_transfer(
                &arm.body,
                current_label,
                parent_label,
                signatures,
                struct_signatures,
                &mut arm_locals,
                &mut arm_moved,
                diagnostics,
                branch_breaks,
                outer_back_edges,
            );
            for name in &bindings {
                arm_moved.remove(name);
                if incoming.contains(name) {
                    arm_moved.insert(name.clone());
                }
            }
            match outcome {
                MoveLoopExit::BackEdge => continuing.push(arm_moved),
                MoveLoopExit::Break(target)
                    if target.as_deref().is_none_or(|target| Some(target) == current_label) =>
                {
                    branch_breaks.push(arm_moved.clone());
                    exits.push(arm_moved);
                }
                MoveLoopExit::Continue(target)
                    if target.as_deref().is_none_or(|target| Some(target) == current_label) =>
                {
                    continuing.push(arm_moved)
                }
                MoveLoopExit::Break(target) => {
                    if parent_label.is_some() && target.as_deref() == parent_label {
                        branch_breaks.push(arm_moved.clone());
                    }
                    propagated_exits.push((MoveLoopExit::Break(target), arm_moved));
                }
                exit @ MoveLoopExit::Continue(_) => {
                    if let MoveLoopExit::Continue(target) = &exit
                        && parent_label.is_some()
                        && target.as_deref() == parent_label
                    {
                        outer_back_edges.extend(arm_moved.iter().cloned());
                    }
                    propagated_exits.push((exit, arm_moved));
                }
                MoveLoopExit::Return => {}
            }
        }
        if continuing.is_empty() {
            if exits.is_empty() {
                if let Some((exit, _)) = propagated_exits.first().cloned()
                    && propagated_exits
                        .iter()
                        .all(|(other_exit, _)| other_exit == &exit)
                {
                    moved.clear();
                    for (_, state) in propagated_exits {
                        moved.extend(state);
                    }
                    return exit;
                }
                return MoveLoopExit::Return;
            }
            moved.clear();
            for exit in exits {
                moved.extend(exit);
            }
            return MoveLoopExit::Break(current_label.map(str::to_owned));
        }
        // Outer breaks are exit states, not back edges of the current loop.
        continuing.extend(propagated_exits.into_iter().filter_map(|(exit, state)| {
            matches!(exit, MoveLoopExit::Continue(_)).then_some(state)
        }));
        moved.clear();
        for state in continuing {
            moved.extend(state);
        }
        if match_index + 1 == statements.len() {
            return MoveLoopExit::BackEdge;
        }
        let suffix = BodyExpr {
            kind: BodyExprKind::Block {
                statements: statements[match_index + 1..].to_vec(),
                tail: None,
            },
            span: expression.span.clone(),
        };
        return check_move_loop_transfer(
            &suffix,
            current_label,
            parent_label,
            signatures,
            struct_signatures,
            locals,
            moved,
            diagnostics,
            branch_breaks,
            outer_back_edges,
        );
    }
    let terminal = statements.iter().position(|statement| match statement {
        BodyStatement::Break { .. } | BodyStatement::Continue { .. } => true,
        BodyStatement::Expression { expression, .. } => {
            matches!(&expression.kind, BodyExprKind::Return(_))
        }
        _ => false,
    });
    let Some(terminal) = terminal else {
        check_move_expression(
            expression,
            false,
            signatures,
            struct_signatures,
            locals,
            moved,
            diagnostics,
        );
        return MoveLoopExit::BackEdge;
    };
    let reachable_prefix = BodyExpr {
        kind: BodyExprKind::Block {
            statements: statements[..terminal].to_vec(),
            tail: None,
        },
        span: expression.span.clone(),
    };
    check_move_expression(
        &reachable_prefix,
        false,
        signatures,
        struct_signatures,
        locals,
        moved,
        diagnostics,
    );
    match &statements[terminal] {
        BodyStatement::Break { label, .. } => MoveLoopExit::Break(label.clone()),
        BodyStatement::Continue { label, .. } => MoveLoopExit::Continue(label.clone()),
        BodyStatement::Expression { expression, .. } => {
            check_move_expression(
                expression,
                false,
                signatures,
                struct_signatures,
                locals,
                moved,
                diagnostics,
            );
            MoveLoopExit::Return
        }
        _ => MoveLoopExit::BackEdge,
    }
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
            if is_uninitialized(name, moved) {
                push_move_diagnostic(
                    diagnostics,
                    expression.span.clone(),
                    format!(
                        "error[E0381]: use of possibly-uninitialized variable: `{name}`"
                    ),
                );
            } else if move_state_contains(name, moved) {
                push_move_diagnostic(
                    diagnostics,
                    expression.span.clone(),
                    format!(
                        "error[E0382]: {} of moved value: `{name}`",
                        if consume { "use" } else { "borrow" }
                    ),
                );
            } else if consume && *ty != Ty::Unknown && !is_copy_type(ty, struct_signatures) {
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
            let original_names: HashSet<String> = original_locals.keys().cloned().collect();
            let original_moved = moved.clone();
            let shadowed_names: HashSet<String> = statements
                .iter()
                .flat_map(|statement| match statement {
                    BodyStatement::Let {
                        name: Some(name), ..
                    } => vec![name.clone()],
                    BodyStatement::LetTuple { names, .. }
                    | BodyStatement::ForEach { names, .. } => names.clone(),
                    BodyStatement::ForRange { name, .. } => vec![name.clone()],
                    _ => Vec::new(),
                })
                .collect();
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
                            clear_move_state(name, moved);
                            if initializer.is_none() {
                                moved.insert(uninitialized_marker(name));
                            }
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
                        clear_move_state(name, moved);
                        mark_assigned_place(name, locals, struct_signatures, moved);
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
                        arguments,
                        span,
                        ..
                    } if path == "println" => {
                        for argument in arguments {
                            check_move_expression(
                                argument,
                                false,
                                signatures,
                                struct_signatures,
                                locals,
                                moved,
                                diagnostics,
                            );
                        }
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
                        label,
                        condition, body, ..
                    } => {
                        let incoming = moved.clone();
                        let mut branch_breaks = Vec::new();
                        let (header, break_exits) = move_loop_fixed_point(&incoming, |loop_moved, outer_back_edges| {
                            let mut loop_locals = locals.clone();
                            check_move_expression(
                                condition,
                                false,
                                signatures,
                                struct_signatures,
                                &mut loop_locals,
                                loop_moved,
                                diagnostics,
                            );
                            check_move_loop_transfer(
                                body,
                                label.as_deref(),
                                None,
                                signatures,
                                struct_signatures,
                                &mut loop_locals,
                                loop_moved,
                                diagnostics,
                                &mut branch_breaks,
                                outer_back_edges,
                            )
                        });
                        *moved = header;
                        moved.extend(break_exits);
                        for exit in branch_breaks {
                            moved.extend(exit);
                        }
                    }
                    BodyStatement::Loop { label, body, .. } => {
                        let incoming = moved.clone();
                        let mut branch_breaks = Vec::new();
                        let (header, break_exits) = move_loop_fixed_point(&incoming, |loop_moved, outer_back_edges| {
                            let mut loop_locals = locals.clone();
                            check_move_loop_transfer(
                                body,
                                label.as_deref(),
                                None,
                                signatures,
                                struct_signatures,
                                &mut loop_locals,
                                loop_moved,
                                diagnostics,
                                &mut branch_breaks,
                                outer_back_edges,
                            )
                        });
                        let mut all_breaks = break_exits;
                        for exit in branch_breaks {
                            all_breaks.extend(exit);
                        }
                        *moved = if all_breaks.is_empty() {
                            header
                        } else {
                            all_breaks
                        };
                    }
                    BodyStatement::ForRange {
                        label,
                        name,
                        start,
                        end,
                        body,
                        ..
                    } => {
                        let mut ignored = Vec::new();
                        let start_type = infer_with_structs(
                            start,
                            locals,
                            signatures,
                            struct_signatures,
                            &Ty::Unknown,
                            &mut ignored,
                        );
                        let end_type = infer_with_structs(
                            end,
                            locals,
                            signatures,
                            struct_signatures,
                            &Ty::Unknown,
                            &mut ignored,
                        );
                        let iteration_type = if start_type == Ty::InferInteger {
                            end_type
                        } else {
                            start_type
                        };
                        check_move_expression(
                            start,
                            true,
                            signatures,
                            struct_signatures,
                            locals,
                            moved,
                            diagnostics,
                        );
                        check_move_expression(
                            end,
                            true,
                            signatures,
                            struct_signatures,
                            locals,
                            moved,
                            diagnostics,
                        );
                        let incoming = moved.clone();
                        let mut branch_breaks = Vec::new();
                        let (header, break_exits) = move_loop_fixed_point(&incoming, |loop_moved, outer_back_edges| {
                            let mut loop_locals = locals.clone();
                            loop_locals.insert(name.clone(), iteration_type.clone());
                            check_move_loop_transfer(
                                body,
                                label.as_deref(),
                                None,
                                signatures,
                                struct_signatures,
                                &mut loop_locals,
                                loop_moved,
                                diagnostics,
                                &mut branch_breaks,
                                outer_back_edges,
                            )
                        });
                        *moved = header;
                        moved.extend(break_exits);
                        for exit in branch_breaks {
                            moved.extend(exit);
                        }
                    }
                    BodyStatement::ForEach {
                        label,
                        iterator,
                        body,
                        ..
                    } => {
                        check_move_expression(
                            iterator,
                            true,
                            signatures,
                            struct_signatures,
                            locals,
                            moved,
                            diagnostics,
                        );
                        let incoming = moved.clone();
                        let mut branch_breaks = Vec::new();
                        let (header, break_exits) = move_loop_fixed_point(&incoming, |loop_moved, outer_back_edges| {
                            let mut loop_locals = locals.clone();
                            check_move_loop_transfer(
                                body,
                                label.as_deref(),
                                None,
                                signatures,
                                struct_signatures,
                                &mut loop_locals,
                                loop_moved,
                                diagnostics,
                                &mut branch_breaks,
                                outer_back_edges,
                            )
                        });
                        *moved = header;
                        moved.extend(break_exits);
                        for exit in branch_breaks {
                            moved.extend(exit);
                        }
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
            // Bindings introduced in this block cease to exist at its closing
            // brace. A moved inner binding must not poison an outer binding
            // with the same spelling after the block.
            moved.retain(|place| original_names.contains(move_place_root(place)));
            for name in shadowed_names.intersection(&original_names) {
                moved.retain(|place| move_place_root(place) != name);
                moved.extend(
                    original_moved
                        .iter()
                        .filter(|place| move_place_root(place) == name)
                        .cloned(),
                );
            }
            *locals = original_locals;
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let then_diverges = expression_always_diverges(then_branch);
            let else_diverges = else_branch
                .as_deref()
                .is_some_and(expression_always_diverges);
            check_move_expression(
                condition,
                false,
                signatures,
                struct_signatures,
                locals,
                moved,
                diagnostics,
            );
            let incoming = moved.clone();
            let mut then_moved = incoming.clone();
            let mut then_locals = locals.clone();
            check_move_expression(
                then_branch,
                consume,
                signatures,
                struct_signatures,
                &mut then_locals,
                &mut then_moved,
                diagnostics,
            );
            let mut else_moved = incoming.clone();
            if let Some(else_branch) = else_branch {
                let mut else_locals = locals.clone();
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
            // A value is considered moved if any reachable branch consumes
            // it; this is the conservative join used by definite-initialization
            // analysis at the following program point.
            *moved = match (then_diverges, else_diverges) {
                (true, true) => incoming,
                (true, false) => else_moved,
                (false, true) => then_moved,
                (false, false) => {
                    then_moved.extend(else_moved);
                    then_moved
                }
            };
        }
        BodyExprKind::Match { scrutinee, arms } => {
            // A match scrutinee is inspected as a place. It is consumed only
            // when a by-value binding takes ownership of the matched value.
            let consumes_scrutinee = arms
                .iter()
                .any(|arm| {
                    matches!(
                        &arm.pattern,
                        MatchPattern::Binding(_) | MatchPattern::Destructure { .. }
                    )
                });
            check_move_expression(
                scrutinee,
                consumes_scrutinee,
                signatures,
                struct_signatures,
                locals,
                moved,
                diagnostics,
            );
            let incoming = moved.clone();
            let mut joined = HashSet::new();
            let mut has_reachable_arm = false;
            for arm in arms {
                let mut arm_moved = incoming.clone();
                let mut arm_locals = locals.clone();
                if let MatchPattern::Binding(name) = &arm.pattern {
                    arm_locals.insert(name.clone(), Ty::Unknown);
                    arm_moved.remove(name);
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
                if !expression_always_diverges(&arm.body) {
                    joined.extend(arm_moved);
                    has_reachable_arm = true;
                }
            }
            *moved = if has_reachable_arm { joined } else { incoming };
        }
        BodyExprKind::Call {
            callee,
            arguments,
            ..
        } => {
            for (index, argument) in arguments.iter().enumerate() {
                check_move_expression(
                    argument,
                    !call_argument_is_borrowed(signatures, callee, index),
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
        BodyExprKind::Binary {
            operator: BinaryOperator::PatternEqual,
            left,
            right,
        } => {
            // The parser lowers simple `if let` literal/path patterns to a
            // non-consuming equality predicate. The scrutinee is inspected
            // in place, as it is for the pattern-match lowering.
            check_move_expression(
                left,
                false,
                signatures,
                struct_signatures,
                locals,
                moved,
                diagnostics,
            );
            check_move_expression(
                right,
                false,
                signatures,
                struct_signatures,
                locals,
                moved,
                diagnostics,
            );
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
        BodyExprKind::Closure {
            parameters,
            body,
            move_capture,
        } => {
            let mut captures = borrowck::captures(body, parameters)
                .into_iter()
                .filter(|name| locals.contains_key(name))
                .collect::<Vec<_>>();
            captures.sort();
            let mut closure_locals = locals.clone();
            let mut closure_moved = moved.clone();
            if *move_capture {
                for name in &captures {
                    let captured = BodyExpr {
                        kind: BodyExprKind::Path(name.clone()),
                        span: expression.span.clone(),
                    };
                    check_move_expression(
                        &captured,
                        true,
                        signatures,
                        struct_signatures,
                        locals,
                        moved,
                        diagnostics,
                    );
                    closure_moved.remove(name);
                }
            }
            for parameter in parameters {
                match parameter {
                    ClosureParameter::Identifier(name) => {
                        closure_locals.insert(name.clone(), Ty::Unknown);
                        closure_moved.remove(name);
                    }
                    ClosureParameter::Tuple(names) => {
                        for name in names {
                            closure_locals.insert(name.clone(), Ty::Unknown);
                            closure_moved.remove(name);
                        }
                    }
                }
            }
            check_move_expression(
                body,
                false,
                signatures,
                struct_signatures,
                &mut closure_locals,
                &mut closure_moved,
                diagnostics,
            );
        }
        BodyExprKind::FieldAccess { base, .. } => {
            if let Some(place) = move_place(expression) {
                let root = place.split(['.', '[']).next().unwrap_or(&place);
                if !locals.contains_key(root) {
                    return;
                }
                if is_uninitialized(&place, moved) {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "error[E0381]: use of possibly-uninitialized variable: `{}`",
                            place
                        ),
                    });
                } else if move_state_contains(&place, moved) {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "error[E0382]: {} of moved value: `{place}`",
                            if consume { "use" } else { "borrow" }
                        ),
                    });
                } else if consume {
                    let mut ignored = Vec::new();
                    let ty = infer_with_structs(
                        expression,
                        locals,
                        signatures,
                        struct_signatures,
                        &Ty::Unknown,
                        &mut ignored,
                    );
                    let base_type = infer_with_structs(
                        base,
                        locals,
                        signatures,
                        struct_signatures,
                        &Ty::Unknown,
                        &mut ignored,
                    );
                    if matches!(base_type, Ty::Reference(_, _))
                        && ty != Ty::Unknown
                        && !is_copy_type(&ty, struct_signatures)
                    {
                        diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!(
                                "error[E0507]: cannot move out of borrowed content `{place}`"
                            ),
                        });
                    } else if ty != Ty::Unknown && !is_copy_type(&ty, struct_signatures) {
                        moved.insert(place);
                    }
                }
            }
            else {
                check_move_expression(
                    base,
                    false,
                    signatures,
                    struct_signatures,
                    locals,
                    moved,
                    diagnostics,
                );
            }
        }
        BodyExprKind::Index { base, index } => {
            // Indexing borrows its base place; the index expression itself is
            // evaluated by value.
            if let Some(place) = move_place(expression) {
                let root = move_place_root(&place);
                if locals.contains_key(root) {
                    if is_uninitialized(&place, moved) {
                        push_move_diagnostic(
                            diagnostics,
                            expression.span.clone(),
                            format!(
                                "error[E0381]: use of possibly-uninitialized variable: `{place}`"
                            ),
                        );
                    } else if move_state_contains(&place, moved) {
                        push_move_diagnostic(
                            diagnostics,
                            expression.span.clone(),
                            format!(
                                "error[E0382]: {} of moved value: `{place}`",
                                if consume { "use" } else { "borrow" }
                            ),
                        );
                    }
                }
            } else {
                check_move_expression(
                    base,
                    false,
                    signatures,
                    struct_signatures,
                    locals,
                    moved,
                    diagnostics,
                );
            }
            check_move_expression(
                index,
                true,
                signatures,
                struct_signatures,
                locals,
                moved,
                diagnostics,
            );
            if consume {
                let mut ignored = Vec::new();
                let ty = infer_with_structs(
                    expression,
                    locals,
                    signatures,
                    struct_signatures,
                    &Ty::Unknown,
                    &mut ignored,
                );
                if !is_copy_type(&ty, struct_signatures) && ty != Ty::Unknown {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: "error[E0507]: cannot move out of indexed content".to_owned(),
                    });
                }
            }
        }
        BodyExprKind::Dereference(inner) => {
            check_move_expression(
                inner,
                false,
                signatures,
                struct_signatures,
                locals,
                moved,
                diagnostics,
            );
            if consume {
                let mut ignored = Vec::new();
                let ty = infer_with_structs(
                    expression,
                    locals,
                    signatures,
                    struct_signatures,
                    &Ty::Unknown,
                    &mut ignored,
                );
                if !is_copy_type(&ty, struct_signatures) && ty != Ty::Unknown {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: "error[E0507]: cannot move out of borrowed content".to_owned(),
                    });
                }
            }
        }
        BodyExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                check_move_expression(
                    value,
                    true,
                    signatures,
                    struct_signatures,
                    locals,
                    moved,
                    diagnostics,
                );
            }
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

fn move_state_contains(place: &str, moved: &HashSet<String>) -> bool {
    moved.iter().any(|moved_place| {
        marker_place(moved_place).is_none()
            && move_places_overlap(place, moved_place)
            && !(move_place_is_strict_prefix(moved_place, place)
                && is_initialized(place, moved))
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MoveProjection {
    Field(String),
    Index(Option<usize>),
}

fn move_places_overlap(left: &str, right: &str) -> bool {
    let (Some((left_root, left_projection)), Some((right_root, right_projection))) =
        (parse_move_place(left), parse_move_place(right))
    else {
        return left == right;
    };
    left_root == right_root
        && left_projection
            .iter()
            .zip(&right_projection)
            .all(|(left, right)| match (left, right) {
                (MoveProjection::Index(None), MoveProjection::Index(_))
                | (MoveProjection::Index(_), MoveProjection::Index(None)) => true,
                _ => left == right,
            })
}

fn move_place_is_strict_prefix(prefix: &str, place: &str) -> bool {
    let (Some((prefix_root, prefix_projection)), Some((place_root, place_projection))) =
        (parse_move_place(prefix), parse_move_place(place))
    else {
        return false;
    };
    prefix_root == place_root
        && prefix_projection.len() < place_projection.len()
        && prefix_projection
            .iter()
            .zip(&place_projection)
            .all(|(prefix, place)| match (prefix, place) {
                (MoveProjection::Index(None), MoveProjection::Index(_))
                | (MoveProjection::Index(_), MoveProjection::Index(None)) => true,
                _ => prefix == place,
            })
}

fn parse_move_place(place: &str) -> Option<(&str, Vec<MoveProjection>)> {
    let root_end = place
        .char_indices()
        .take_while(|(_, character)| character.is_ascii_alphanumeric() || *character == '_')
        .map(|(index, character)| index + character.len_utf8())
        .last()?;
    let root = &place[..root_end];
    let mut rest = &place[root_end..];
    let mut projections = Vec::new();
    while !rest.is_empty() {
        if let Some(after_dot) = rest.strip_prefix('.') {
            let field_end = after_dot
                .char_indices()
                .take_while(|(_, character)| {
                    character.is_ascii_alphanumeric() || *character == '_'
                })
                .map(|(index, character)| index + character.len_utf8())
                .last()?;
            projections.push(MoveProjection::Field(after_dot[..field_end].to_owned()));
            rest = &after_dot[field_end..];
        } else if let Some(after_open) = rest.strip_prefix('[') {
            let close = after_open.find(']')?;
            let index = parse_move_index(&after_open[..close]);
            projections.push(MoveProjection::Index(index));
            rest = &after_open[close + 1..];
        } else {
            return None;
        }
    }
    Some((root, projections))
}

fn parse_move_index(text: &str) -> Option<usize> {
    let text = text.replace('_', "");
    if text == "*" {
        return None;
    }
    if let Some(hex) = text.strip_prefix("0x") {
        usize::from_str_radix(hex, 16).ok()
    } else if let Some(octal) = text.strip_prefix("0o") {
        usize::from_str_radix(octal, 8).ok()
    } else if let Some(binary) = text.strip_prefix("0b") {
        usize::from_str_radix(binary, 2).ok()
    } else {
        text.parse().ok()
    }
}

fn aggregate_field_names(ty: &Ty, structs: &[StructSignature]) -> Option<Vec<String>> {
    match ty {
        Ty::Named(name) => find_struct_signature(structs, name)
            .map(|structure| structure.fields.iter().map(|field| field.name.clone()).collect()),
        Ty::Tuple(fields) => Some((0..fields.len()).map(|index| index.to_string()).collect()),
        _ => None,
    }
}

fn aggregate_field_type(ty: &Ty, field: &str, structs: &[StructSignature]) -> Option<Ty> {
    match ty {
        Ty::Named(name) => find_struct_signature(structs, name)
            .and_then(|structure| structure.fields.iter().find(|item| item.name == field))
            .map(|item| type_from_syntax(&item.type_text, structs)),
        Ty::Tuple(fields) => field
            .parse::<usize>()
            .ok()
            .and_then(|index| fields.get(index).cloned()),
        _ => None,
    }
}

fn aggregate_index_type(ty: &Ty, index: usize) -> Option<Ty> {
    match ty {
        Ty::Array(element, length) if index < *length => Some((**element).clone()),
        Ty::Tuple(fields) => fields.get(index).cloned(),
        _ => None,
    }
}

fn aggregate_fully_initialized(
    place: &str,
    ty: &Ty,
    structs: &[StructSignature],
    state: &HashSet<String>,
) -> bool {
    if is_initialized(place, state) {
        return true;
    }
    if let Some(fields) = aggregate_field_names(ty, structs) {
        return !fields.is_empty()
            && fields
                .iter()
                .all(|field| is_initialized(&format!("{place}.{field}"), state));
    }
    let Ty::Array(_, length) = ty else {
        return false;
    };
    initialized_array_indices(place, state)
        .iter()
        .filter(|index| **index < *length)
        .count()
        == *length
}

fn initialized_array_indices(place: &str, state: &HashSet<String>) -> HashSet<usize> {
    let Some((root, parent_projection)) = parse_move_place(place) else {
        return HashSet::new();
    };
    state
        .iter()
        .filter_map(|marker| {
            let (marked_place, initialized) = marker_place(marker)?;
            initialized.then_some(marked_place)
        })
        .filter_map(|marked_place| parse_move_place(marked_place))
        .filter_map(|(marked_root, projection)| {
            if marked_root != root
                || projection.len() != parent_projection.len() + 1
                || !parent_projection.iter().zip(&projection).all(|(parent, child)| {
                    match (parent, child) {
                        (MoveProjection::Index(None), MoveProjection::Index(_))
                        | (MoveProjection::Index(_), MoveProjection::Index(None)) => true,
                        _ => parent == child,
                    }
                })
            {
                return None;
            }
            match projection.last()? {
                MoveProjection::Index(Some(index)) => Some(*index),
                _ => None,
            }
        })
        .collect()
}

fn mark_assigned_place(
    place: &str,
    locals: &HashMap<String, Ty>,
    structs: &[StructSignature],
    state: &mut HashSet<String>,
) {
    let Some((root, projections)) = parse_move_place(place) else {
        return;
    };
    if projections.is_empty()
        || projections
            .iter()
            .any(|projection| matches!(projection, MoveProjection::Index(None)))
    {
        return;
    }
    state.insert(initialized_marker(place));
    let Some(mut ty) = locals.get(root).cloned() else {
        return;
    };
    let mut ancestors = vec![(root.to_owned(), ty.clone())];
    let mut projected = root.to_owned();
    for projection in projections {
        let (field_type, projection_text) = match projection {
            MoveProjection::Field(field) => (
                aggregate_field_type(&ty, &field, structs),
                format!(".{field}"),
            ),
            MoveProjection::Index(Some(index)) => {
                (aggregate_index_type(&ty, index), format!("[{index}]"))
            }
            MoveProjection::Index(None) => unreachable!("dynamic indices were filtered above"),
        };
        let Some(field_type) = field_type else {
            return;
        };
        projected.push_str(&projection_text);
        ty = field_type;
        ancestors.push((projected.clone(), ty.clone()));
    }
    for (parent, parent_type) in ancestors.into_iter().rev().skip(1) {
        if !aggregate_fully_initialized(&parent, &parent_type, structs, state) {
            continue;
        }
        state.remove(&uninitialized_marker(&parent));
        state.remove(&parent);
        state.insert(initialized_marker(&parent));
    }
}

fn uninitialized_marker(place: &str) -> String {
    format!("{place}.\0uninitialized")
}

fn initialized_marker(place: &str) -> String {
    format!("{place}.\0initialized")
}

fn is_uninitialized(place: &str, state: &HashSet<String>) -> bool {
    place_initialization(place, state) == Some(false)
}

fn is_initialized(place: &str, state: &HashSet<String>) -> bool {
    place_initialization(place, state) == Some(true)
}

fn place_initialization(place: &str, state: &HashSet<String>) -> Option<bool> {
    state
        .iter()
        .filter_map(|marker| marker_place(marker))
        .filter(|(marked_place, _)| {
            place == *marked_place
                || place
                    .strip_prefix(*marked_place)
                    .is_some_and(|suffix| suffix.starts_with('.') || suffix.starts_with('['))
        })
        .max_by_key(|(marked_place, _)| marked_place.len())
        .map(|(_, initialized)| initialized)
}

fn marker_place(marker: &str) -> Option<(&str, bool)> {
    marker
        .strip_suffix(".\0uninitialized")
        .map(|place| (place, false))
        .or_else(|| {
            marker
                .strip_suffix(".\0initialized")
                .map(|place| (place, true))
        })
}

fn move_place_root(place: &str) -> &str {
    place.split(['.', '[']).next().unwrap_or(place)
}

fn clear_move_state(place: &str, moved: &mut HashSet<String>) {
    moved.retain(|moved_place| {
        if let Some((marked_place, _)) = marker_place(moved_place) {
            marked_place != place
                && !marked_place
                    .strip_prefix(place)
                    .is_some_and(|suffix| suffix.starts_with('.') || suffix.starts_with('['))
        } else {
            moved_place != place
                && !moved_place
                    .strip_prefix(place)
                    .is_some_and(|suffix| suffix.starts_with('.') || suffix.starts_with('['))
        }
    });
}

fn move_place(expression: &BodyExpr) -> Option<String> {
    match &expression.kind {
        BodyExprKind::Path(name) => Some(name.clone()),
        BodyExprKind::Parenthesized(inner) => move_place(inner),
        BodyExprKind::FieldAccess { base, field } => {
            Some(format!("{}.{field}", move_place(base)?))
        }
        BodyExprKind::Index { base, index } => {
            let index = match &index.kind {
                BodyExprKind::Literal(LiteralKind::Integer { text, .. }) => {
                    parse_move_index(text).map_or_else(|| "*".to_owned(), |index| index.to_string())
                }
                _ => "*".to_owned(),
            };
            Some(format!("{}[{index}]", move_place(base)?))
        }
        _ => None,
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
        let field = &format[open + 1..close];
        let name = field.split_once(':').map_or(field, |(name, _)| name);
        if !name.is_empty()
            && name
                .chars()
                .all(|character| character == '_' || character.is_alphanumeric())
            && (is_uninitialized(name, moved)
                || (move_state_contains(name, moved)
                    && locals
                        .get(name)
                        .is_some_and(|ty| !is_copy_type(ty, struct_signatures))))
        {
            let placeholder = format!("{{{name}");
            if let Some(token_offset) = tokens.find(&placeholder) {
                let capture_start = macro_span.start + path.len() + 2 + token_offset + 1;
                diagnostics.push(TypeDiagnostic {
                    span: capture_start..capture_start + name.len(),
                    message: if is_uninitialized(name, moved) {
                        format!("error[E0381]: use of possibly-uninitialized variable: `{name}`")
                    } else {
                        format!("error[E0382]: borrow of moved value: `{name}`")
                    },
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
            ) && !callee.starts_with("Self::")
                && !is_option_some_path(callee)
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
        BodyExprKind::Closure { parameters, body, .. } => {
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
        BodyExprKind::ArrayRepeat { element, .. } => {
            check_call_targets(element, locals, signatures, diagnostics);
        }
        BodyExprKind::Index { base, index } => {
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

fn check_loop_control(
    expression: &BodyExpr,
    loop_labels: &mut Vec<Option<String>>,
    diagnostics: &mut Vec<TypeDiagnostic>,
) {
    match &expression.kind {
        BodyExprKind::Block { statements, tail } => {
            for statement in statements {
                match statement {
                    BodyStatement::Break { label, span }
                    | BodyStatement::Continue { label, span } => {
                        let target_exists = label.as_ref().map_or_else(
                            || !loop_labels.is_empty(),
                            |label| loop_labels.iter().rev().any(|active| active.as_ref() == Some(label)),
                        );
                        if !target_exists {
                            diagnostics.push(TypeDiagnostic {
                                span: span.clone(),
                                message: if let Some(label) = label {
                                    format!("error[E0426]: use of undeclared label `{label}`")
                                } else {
                                    "loop control outside of a loop".to_owned()
                                },
                            });
                        }
                    }
                    BodyStatement::While {
                        label,
                        condition,
                        body,
                        ..
                    } => {
                        check_loop_control(condition, loop_labels, diagnostics);
                        loop_labels.push(label.clone());
                        check_loop_control(body, loop_labels, diagnostics);
                        loop_labels.pop();
                    }
                    BodyStatement::Loop { label, body, .. } => {
                        loop_labels.push(label.clone());
                        check_loop_control(body, loop_labels, diagnostics);
                        loop_labels.pop();
                    }
                    BodyStatement::ForRange {
                        label,
                        start, end, body, ..
                    } => {
                        check_loop_control(start, loop_labels, diagnostics);
                        check_loop_control(end, loop_labels, diagnostics);
                        loop_labels.push(label.clone());
                        check_loop_control(body, loop_labels, diagnostics);
                        loop_labels.pop();
                    }
                    BodyStatement::ForEach {
                        label,
                        iterator,
                        body,
                        ..
                    } => {
                        check_loop_control(iterator, loop_labels, diagnostics);
                        loop_labels.push(label.clone());
                        check_loop_control(body, loop_labels, diagnostics);
                        loop_labels.pop();
                    }
                    BodyStatement::Let {
                        initializer: Some(value),
                        ..
                    }
                    | BodyStatement::Assign { value, .. } => {
                        check_loop_control(value, loop_labels, diagnostics)
                    }
                    BodyStatement::Expression { expression, .. } => {
                        check_loop_control(expression, loop_labels, diagnostics)
                    }
                    _ => {}
                }
            }
            if let Some(tail) = tail {
                check_loop_control(tail, loop_labels, diagnostics);
            }
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            check_loop_control(condition, loop_labels, diagnostics);
            check_loop_control(then_branch, loop_labels, diagnostics);
            if let Some(branch) = else_branch {
                check_loop_control(branch, loop_labels, diagnostics);
            }
        }
        BodyExprKind::Match { scrutinee, arms } => {
            check_loop_control(scrutinee, loop_labels, diagnostics);
            for arm in arms {
                check_loop_control(&arm.body, loop_labels, diagnostics);
            }
        }
        BodyExprKind::Call { arguments, .. } => {
            for argument in arguments {
                check_loop_control(argument, loop_labels, diagnostics);
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            arguments,
            ..
        } => {
            check_loop_control(receiver, loop_labels, diagnostics);
            for argument in arguments {
                check_loop_control(argument, loop_labels, diagnostics);
            }
        }
        BodyExprKind::Closure { body, .. } => {
            check_loop_control(body, &mut Vec::new(), diagnostics)
        }
        BodyExprKind::Tuple(elements)
        | BodyExprKind::ArrayLiteral(elements)
        | BodyExprKind::FixedArrayLiteral(elements) => {
            for element in elements {
                check_loop_control(element, loop_labels, diagnostics);
            }
        }
        BodyExprKind::Index { base, index } => {
            check_loop_control(base, loop_labels, diagnostics);
            check_loop_control(index, loop_labels, diagnostics);
        }
        BodyExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                check_loop_control(value, loop_labels, diagnostics);
            }
        }
        BodyExprKind::FieldAccess { base, .. }
        | BodyExprKind::Reference {
            expression: base, ..
        }
        | BodyExprKind::Dereference(base)
        | BodyExprKind::Cast {
            expression: base, ..
        } => check_loop_control(base, loop_labels, diagnostics),
        BodyExprKind::Binary { left, right, .. } => {
            check_loop_control(left, loop_labels, diagnostics);
            check_loop_control(right, loop_labels, diagnostics);
        }
        BodyExprKind::Negate(value)
        | BodyExprKind::Not(value)
        | BodyExprKind::Try(value)
        | BodyExprKind::Parenthesized(value) => {
            check_loop_control(value, loop_labels, diagnostics)
        }
        BodyExprKind::Return(Some(value)) => check_loop_control(value, loop_labels, diagnostics),
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
        BodyExprKind::Closure { parameters, body, .. } => {
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
        BodyExprKind::ArrayRepeat { element, .. } => {
            check_mutability(element, locals, diagnostics);
        }
        BodyExprKind::Index { base, index } => {
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
                        let mutable_reference_field = name != binding
                            && binding == "self"
                            && scoped.get("\0mutable_ref:self") == Some(&true);
                        if scoped.get(binding) == Some(&false) && !mutable_reference_field {
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
                (path == "Self" && expected_return != &Ty::Unknown)
                    .then(|| expected_return.clone())
            })
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
        BodyExprKind::Reference { mutable, expression } => Ty::Reference(
            Box::new(infer_with_structs(
                expression,
                locals,
                signatures,
                struct_signatures,
                expected_return,
                diagnostics,
            )),
            *mutable,
        ),
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
                Ty::Reference(inner, _) => *inner,
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
            if (source.is_numeric() && target.is_numeric())
                || (source == Ty::Bool && target.is_integer())
                || (source == Ty::Char && (target.is_numeric() || target == Ty::Char))
                || (source.is_numeric() && target == Ty::Char)
                || matches!((&source, &target), (Ty::RawPointer(_, _), Ty::RawPointer(_, _)))
                || matches!((&source, &target), (Ty::RawPointer(_, _), Ty::Integer(_)))
                || matches!((&source, &target), (Ty::Integer(_), Ty::RawPointer(_, _)))
                || source == Ty::Unknown
            {
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
            while let Ty::Reference(referent, _) = base_type {
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
        BodyExprKind::Index { base, index } => {
            let base_type = infer_with_structs(
                base,
                locals,
                signatures,
                struct_signatures,
                expected_return,
                diagnostics,
            );
            if matches!(index.kind, BodyExprKind::Unsupported) {
                let element = match without_references(&base_type) {
                    Ty::Array(element, _) | Ty::Slice(element) | Ty::Vector(element) => {
                        Some(element.as_ref().clone())
                    }
                    _ => None,
                };
                if let Some(element) = element {
                    return Ty::Slice(Box::new(element));
                }
            }
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
            if let Some((integer_name, method)) = callee.rsplit_once("::")
                && matches!(method, "from_le" | "from_be" | "from_ne")
            {
                let integer = type_from_syntax(integer_name, struct_signatures);
                if integer.is_integer() {
                    if arguments.len() != 1 {
                        diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!(
                                "integer constructor `{callee}` expects 1 argument but received {}",
                                arguments.len()
                            ),
                        });
                        return Ty::Unknown;
                    }
                    let actual = infer_with_structs(
                        &arguments[0], locals, signatures, struct_signatures, &integer,
                        diagnostics,
                    );
                    require_type(
                        diagnostics,
                        arguments[0].span.clone(),
                        &integer,
                        &actual,
                        "integer byte-order conversion",
                    );
                    return integer;
                }
            }
            if let Some((integer_name, method)) = callee.rsplit_once("::")
                && matches!(method, "from_le_bytes" | "from_be_bytes" | "from_ne_bytes")
            {
                let integer = type_from_syntax(integer_name, struct_signatures);
                let width = match &integer {
                    Ty::Integer(name) => match name.as_str() {
                        "i8" | "u8" => Some(1),
                        "i16" | "u16" => Some(2),
                        "i32" | "u32" => Some(4),
                        "i64" | "u64" | "isize" | "usize" => Some(8),
                        "i128" | "u128" => Some(16),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(width) = width {
                    if arguments.len() != 1 {
                        diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!(
                                "integer constructor `{callee}` expects 1 argument but received {}",
                                arguments.len()
                            ),
                        });
                        return Ty::Unknown;
                    }
                    let expected = Ty::Array(
                        Box::new(Ty::Integer("u8".to_owned())),
                        width,
                    );
                    let actual = infer_with_structs(
                        &arguments[0], locals, signatures, struct_signatures, &expected,
                        diagnostics,
                    );
                    require_type(
                        diagnostics,
                        arguments[0].span.clone(),
                        &expected,
                        &actual,
                        "integer byte array",
                    );
                    return integer;
                }
            }
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
                    || matches!(&actual, Ty::Reference(inner, _) if **inner == Ty::StringSlice);
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
            if let Some(structure) = find_struct_signature(struct_signatures, callee)
                .filter(|structure| {
                    !structure.fields.is_empty()
                        && structure
                            .fields
                            .iter()
                            .enumerate()
                            .all(|(index, field)| field.name == index.to_string())
                })
            {
                if arguments.len() != structure.fields.len() {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "tuple struct `{callee}` expects {} fields but received {}",
                            structure.fields.len(),
                            arguments.len()
                        ),
                    });
                }
                for (field, (argument, actual)) in
                    structure.fields.iter().zip(arguments.iter().zip(argument_types.iter()))
                {
                    let expected = type_from_syntax(&field.type_text, struct_signatures);
                    require_type(
                        diagnostics,
                        argument.span.clone(),
                        &expected,
                        actual,
                        "tuple struct field initializer",
                    );
                }
                return Ty::Named(structure.path.clone());
            }
            let mut candidates = callee
                .strip_prefix("Self::")
                .map_or_else(|| matching_signatures(signatures, callee), |method| {
                    matching_signatures(signatures, method)
                });
            if let Some(Ty::Named(trait_owner)) = locals.get("\0quill_trait_context")
                && callee.starts_with("Self::")
            {
                candidates.retain(|candidate| function_has_trait_owner(candidate, trait_owner));
            }
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
                type_from_function_syntax(text, function, struct_signatures)
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
            if let Ty::Slice(element) | Ty::Array(element, _) | Ty::Vector(element) = receiver_base
                && matches!(method.as_str(), "as_ptr" | "as_mut_ptr")
                && arguments.is_empty()
            {
                let mutable = method == "as_mut_ptr"
                    && matches!(receiver_type, Ty::Reference(_, true));
                return Ty::RawPointer(element.clone(), mutable);
            }
            if matches!(receiver_base, Ty::String | Ty::StringSlice)
                && method == "as_bytes"
                && arguments.is_empty()
            {
                return Ty::Reference(
                    Box::new(Ty::Slice(Box::new(Ty::Integer("u8".to_owned())))),
                    false,
                );
            }
            if matches!(receiver_base, Ty::String | Ty::StringSlice)
                && matches!(method.as_str(), "as_ptr" | "as_mut_ptr")
                && arguments.is_empty()
            {
                return Ty::RawPointer(
                    Box::new(Ty::Integer("u8".to_owned())),
                    method == "as_mut_ptr" && matches!(receiver_type, Ty::Reference(_, true)),
                );
            }
            if let Ty::RawPointer(element, mutable) = receiver_base {
                match method.as_str() {
                    "as_usize" | "addr" if arguments.is_empty() => {
                        return Ty::Integer("usize".to_owned());
                    }
                    "cast" if arguments.is_empty() => {
                        if matches!(expected_return, Ty::RawPointer(_, _)) {
                            return expected_return.clone();
                        }
                        return Ty::RawPointer(Box::new(Ty::Unknown), *mutable);
                    }
                    "add" | "sub" | "offset" if arguments.len() == 1 => {
                        let expected = if method == "offset" {
                            Ty::Integer("isize".to_owned())
                        } else {
                            Ty::Integer("usize".to_owned())
                        };
                        let actual = infer_with_structs(
                            &arguments[0], locals, signatures, struct_signatures, &expected,
                            diagnostics,
                        );
                        require_type(diagnostics, arguments[0].span.clone(), &expected, &actual,
                            "pointer offset");
                        return receiver_base.clone();
                    }
                    "read" | "read_unaligned" | "read_volatile" if arguments.is_empty() => {
                        return element.as_ref().clone();
                    }
                    "write" | "write_unaligned" | "write_volatile" if arguments.len() == 1 => {
                        let actual = infer_with_structs(
                            &arguments[0], locals, signatures, struct_signatures, element,
                            diagnostics,
                        );
                        require_type(diagnostics, arguments[0].span.clone(), element, &actual,
                            "pointer write value");
                        return Ty::Unit;
                    }
                    _ => {}
                }
            }
            if matches!(method.as_str(), "read_exact" | "write_all")
                && generic_receiver_has_trait(
                    &receiver_type,
                    if method == "read_exact" { "Read" } else { "Write" },
                )
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
                // The standard Read and Write methods both take a byte slice
                // and return std::io::Result<()>. The generic parameter's
                // trait bound keeps this built-in from masking unrelated
                // methods with the same spelling.
                let byte_slice = Ty::Reference(
                    Box::new(Ty::Slice(Box::new(Ty::Integer("u8".to_owned())))),
                    false,
                );
                let actual = infer_with_structs(
                    &arguments[0],
                    locals,
                    signatures,
                    struct_signatures,
                    &byte_slice,
                    diagnostics,
                );
                require_type(
                    diagnostics,
                    arguments[0].span.clone(),
                    &byte_slice,
                    &actual,
                    "I/O byte buffer",
                );
                return Ty::Result(
                    Box::new(Ty::Unit),
                    Box::new(Ty::Named("std::io::Error".to_owned())),
                );
            }
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
            if method == "try_into" && arguments.is_empty() {
                let target = match expected_return {
                    Ty::Result(ok, _) => ok.as_ref().clone(),
                    other => other.clone(),
                };
                return Ty::Result(
                    Box::new(target),
                    Box::new(Ty::Named("core::array::TryFromSliceError".to_owned())),
                );
            }
            if method == "as_ref"
                && arguments.is_empty()
                && (*receiver_base == Ty::Unknown
                    || is_as_ref_str_type(receiver_base, struct_signatures))
            {
                return Ty::Reference(Box::new(Ty::StringSlice), false);
            }
            if matches!(method.as_str(), "len" | "is_empty")
                && matches!(
                    receiver_base,
                    Ty::String
                        | Ty::StringSlice
                        | Ty::Vector(_)
                        | Ty::Array(_, _)
                        | Ty::Slice(_)
                        | Ty::Map(_, _)
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
            if method == "copy_from_slice"
                && let Some(element) = match receiver_base {
                    Ty::Slice(element) | Ty::Array(element, _) | Ty::Vector(element) => {
                        Some(element.as_ref())
                    }
                    _ => None,
                }
            {
                if arguments.len() != 1 {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "method `copy_from_slice` expects 1 argument but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                let expected = Ty::Reference(Box::new(Ty::Slice(Box::new(element.clone()))), false);
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
                    "copy_from_slice source",
                );
                return Ty::Unit;
            }
            if arguments.is_empty()
                && matches!(method.as_str(), "to_be_bytes" | "to_le_bytes" | "to_ne_bytes")
            {
                let width = match receiver_base {
                    Ty::Integer(name) => match name.as_str() {
                        "i8" | "u8" => Some(1),
                        "i16" | "u16" => Some(2),
                        "i32" | "u32" => Some(4),
                        "i64" | "u64" | "isize" | "usize" => Some(8),
                        "i128" | "u128" => Some(16),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(width) = width {
                    return Ty::Array(Box::new(Ty::Integer("u8".to_owned())), width);
                }
            }
            if method == "to_bits"
                && arguments.is_empty()
                && let Ty::Float(name) = receiver_base
            {
                return Ty::Integer(
                    (if name == "f32" { "u32" } else { "u64" }).to_owned(),
                );
            }
            if method == "iter" {
                let item = match receiver_base {
                    Ty::Vector(item) | Ty::Slice(item) => Ty::Reference(item.clone(), false),
                    Ty::String => Ty::Reference(Box::new(Ty::Integer("u8".to_owned())), false),
                    Ty::StringSlice => Ty::Reference(Box::new(Ty::Integer("u8".to_owned())), false),
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
                return Ty::Reference(Box::new(Ty::StringSlice), false);
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
                let BodyExprKind::Closure { parameters, body, .. } = &arguments[0].kind else {
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
                let BodyExprKind::Closure { parameters, body, .. } = &arguments[1].kind else {
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
            if matches!(receiver_base, Ty::Named(name) if name.split('<').next().unwrap_or(name).rsplit("::").next() == Some("Formatter"))
                && matches!(method.as_str(), "debug_struct" | "debug_tuple" | "debug_list" | "debug_map" | "debug_set")
            {
                let expected_arguments = usize::from(matches!(method.as_str(), "debug_struct" | "debug_tuple"));
                if arguments.len() != expected_arguments {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!("method `Formatter::{method}` expects {expected_arguments} arguments but received {}", arguments.len()),
                    });
                    return Ty::Unknown;
                }
                if let Some(argument) = arguments.first() {
                    let expected_name = Ty::Reference(Box::new(Ty::StringSlice), false);
                    let actual = infer_with_structs(
                        argument,
                        locals,
                        signatures,
                        struct_signatures,
                        &expected_name,
                        diagnostics,
                    );
                    require_type(
                        diagnostics,
                        argument.span.clone(),
                        &expected_name,
                        &actual,
                        "Formatter builder name",
                    );
                }
                return Ty::Named(format!("core::fmt::{}", match method.as_str() {
                    "debug_struct" => "DebugStruct",
                    "debug_tuple" => "DebugTuple",
                    "debug_list" => "DebugList",
                    "debug_map" => "DebugMap",
                    _ => "DebugSet",
                }));
            }
            if let Ty::Named(builder) = receiver_base
                && let Some(builder_kind) = builder.rsplit("::").next()
                && matches!(builder_kind, "DebugStruct" | "DebugTuple" | "DebugList" | "DebugMap" | "DebugSet")
                && matches!(method.as_str(), "field" | "entry" | "key" | "value" | "finish" | "finish_non_exhaustive")
            {
                let expected_arguments = match (builder_kind, method.as_str()) {
                    ("DebugStruct", "field") | ("DebugMap", "entry") => 2,
                    ("DebugTuple", "field") | ("DebugList" | "DebugSet", "entry")
                    | ("DebugMap", "key" | "value") => 1,
                    (_, "finish" | "finish_non_exhaustive") => 0,
                    _ => {
                        diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: format!("method `{builder_kind}::{method}` is not available"),
                        });
                        return Ty::Unknown;
                    }
                };
                if arguments.len() != expected_arguments {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!("method `{builder_kind}::{method}` expects {expected_arguments} arguments but received {}", arguments.len()),
                    });
                    return Ty::Unknown;
                }
                if method == "finish" || method == "finish_non_exhaustive" {
                    return Ty::Result(
                        Box::new(Ty::Unit),
                        Box::new(Ty::Named("core::fmt::Error".to_owned())),
                    );
                }
                if matches!(method.as_str(), "field" | "key")
                    && let Some(argument) = arguments.first()
                {
                    let expected_name = Ty::Reference(Box::new(Ty::StringSlice), false);
                    let actual = infer_with_structs(
                        argument,
                        locals,
                        signatures,
                        struct_signatures,
                        &expected_name,
                        diagnostics,
                    );
                    require_type(
                        diagnostics,
                        argument.span.clone(),
                        &expected_name,
                        &actual,
                        "format builder field name",
                    );
                }
                return Ty::Reference(Box::new(receiver_base.clone()), true);
            }
            let contextual_candidates = locals
                .get("\0quill_trait_context")
                .map_or_else(Vec::new, |context| {
                    trait_methods_for_context(
                        signatures,
                        context,
                        receiver,
                        &receiver_type,
                        method,
                    )
                });
            let generic_candidates = trait_methods_for_generic(signatures, &receiver_type, method);
            let candidates = if !contextual_candidates.is_empty() {
                contextual_candidates
            } else if !generic_candidates.is_empty() {
                generic_candidates
            } else {
                let inherent = method_signatures(&signatures.methods, &receiver_type, method);
                if inherent.is_empty() {
                    trait_methods_for_concrete(signatures, &receiver_type, method)
                } else {
                    inherent
                }
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
                    type_from_function_syntax(text, function, struct_signatures)
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
                    && matches!(&actual, Ty::Reference(inner, _) if **inner == Ty::StringSlice))
                {
                    require_type(
                        diagnostics,
                        arguments[0].span.clone(),
                        key_type,
                        &actual,
                        "BTreeMap::get key",
                    );
                }
                return Ty::Option(Box::new(Ty::Reference(value_type.clone(), false)));
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
            let receiver_type = infer_with_structs(
                receiver,
                locals,
                signatures,
                struct_signatures,
                expected_return,
                diagnostics,
            );
            let receiver_base = without_references(&receiver_type);
            let is_integer_receiver = matches!(receiver_base, Ty::Integer(_) | Ty::InferInteger);
            let integer_methods = [
                "wrapping_add", "wrapping_sub", "wrapping_mul", "wrapping_div",
                "wrapping_rem", "to_le", "to_be", "to_ne", "swap_bytes",
                "reverse_bits", "leading_zeros", "trailing_zeros", "count_ones",
                "count_zeros", "rotate_left", "rotate_right",
            ];
            if is_integer_receiver && integer_methods.contains(&method.as_str()) {
                let unary = matches!(
                    method.as_str(),
                    "to_le" | "to_be" | "to_ne" | "swap_bytes" | "reverse_bits"
                        | "leading_zeros" | "trailing_zeros" | "count_ones" | "count_zeros"
                );
                let expected_arguments = usize::from(!unary);
                if arguments.len() != expected_arguments {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "integer method `{method}` expects {expected_arguments} arguments but received {}",
                            arguments.len()
                        ),
                    });
                    return Ty::Unknown;
                }
                if !unary {
                    let actual = infer_with_structs(
                        &arguments[0],
                        locals,
                        signatures,
                        struct_signatures,
                        receiver_base,
                        diagnostics,
                    );
                    require_type(
                        diagnostics,
                        arguments[0].span.clone(),
                        receiver_base,
                        &actual,
                        "integer method argument",
                    );
                }
                return if matches!(
                    method.as_str(),
                    "leading_zeros" | "trailing_zeros" | "count_ones" | "count_zeros"
                ) {
                    Ty::Integer("u32".to_owned())
                } else {
                    receiver_base.clone()
                };
            }
            let expected_u64 = Ty::Integer("u64".to_owned());
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
                    if left_ty == Ty::Unknown
                        && right_ty.is_numeric()
                    {
                        right_ty.clone()
                    } else if right_ty == Ty::Unknown && left_ty.is_numeric() {
                        left_ty.clone()
                    } else if *operator == BinaryOperator::Add
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
                BinaryOperator::ShiftLeft | BinaryOperator::ShiftRight => {
                    let left_value_ty = without_references(&left_ty);
                    let right_value_ty = without_references(&right_ty);
                    if !left_value_ty.is_integer() && !right_value_ty.is_integer() {
                        diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: "shift operands must be integers".to_owned(),
                        });
                        Ty::Unknown
                    } else if left_value_ty == &Ty::Unknown {
                        right_value_ty.clone()
                    } else {
                        left_value_ty.clone()
                    }
                }
                BinaryOperator::BitAnd | BinaryOperator::BitOr => {
                    let left_value_ty = without_references(&left_ty);
                    let right_value_ty = without_references(&right_ty);
                    if !left_value_ty.is_integer() && !right_value_ty.is_integer() {
                        diagnostics.push(TypeDiagnostic {
                            span: expression.span.clone(),
                            message: "bitwise operands must be integers".to_owned(),
                        });
                        Ty::Unknown
                    } else if left_value_ty == &Ty::Unknown {
                        right_value_ty.clone()
                    } else if right_value_ty == &Ty::Unknown {
                        left_value_ty.clone()
                    } else if !types_compatible(left_value_ty, right_value_ty) {
                        mismatch(
                            diagnostics,
                            expression.span.clone(),
                            left_value_ty,
                            right_value_ty,
                            "bitwise operation",
                        );
                        Ty::Unknown
                    } else {
                        join_numeric_types(left_value_ty.clone(), right_value_ty.clone())
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
                    if error_type_converts_to(signatures, err_type, expected_err) =>
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
                &Ty::Unknown,
                diagnostics,
            );
            match ty {
                Ty::Bool | Ty::Unknown => ty,
                integer if integer.is_integer() => integer,
                other => {
                    diagnostics.push(TypeDiagnostic {
                        span: expression.span.clone(),
                        message: format!(
                            "negation requires bool or integer, found `{}`",
                            other.display_name()
                        ),
                    });
                    Ty::Unknown
                }
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
                        let normalized_name = name.trim();
                        let dereferences = normalized_name
                            .chars()
                            .take_while(|character| *character == '*')
                            .count();
                        let target = normalized_name.trim_start_matches('*').trim();
                        let root_end = target.find(['.', '[']).unwrap_or(target.len());
                        let root = &target[..root_end];
                        let mut base_type = locals.get(root).cloned();
                        for _ in 0..dereferences {
                            base_type = match base_type {
                                Some(Ty::Reference(referent, _)) => Some(*referent),
                                _ => None,
                            };
                        }
                        let expected_target = base_type.as_ref().and_then(|base_type| {
                            let mut base_type = base_type.clone();
                            if dereferences > 0 || target.contains('.') || target.contains('[') {
                                while let Ty::Reference(referent, _) = base_type {
                                    base_type = *referent;
                                }
                            }
                            if let Some((_, field)) = target.split_once('.') {
                                let Ty::Named(structure_name) = &base_type else { return None };
                                find_struct_signature(struct_signatures, structure_name)?
                                .fields
                                .iter()
                                .find(|candidate| candidate.name == field)
                                .map(|candidate| {
                                    type_from_syntax(&candidate.type_text, struct_signatures)
                                })
                            } else if target.contains('[') {
                                match &base_type {
                                    Ty::Array(element, _) | Ty::Slice(element) | Ty::Vector(element) => Some((**element).clone()),
                                    _ => None,
                                }
                            } else {
                                Some(base_type)
                            }
                        });
                        if let Some(expected) = expected_target.as_ref()
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
                        } else if !normalized_name.contains('*')
                            && !target.contains('.')
                            && !target.contains('[')
                        {
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
                        ..
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
                            Ty::Reference(inner, _) => match inner.as_ref() {
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
                                    Ty::Reference(Box::new(ty.clone()), false)
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
                        MatchPattern::Destructure { path, .. } => {
                            if let Some((enumeration, variant)) =
                                find_enum_variant(struct_signatures, path)
                            {
                                let variants =
                                    enum_patterns.entry(enumeration.path.clone()).or_default();
                                variants.insert(variant.name.clone());
                                enum_variant_counts.insert(
                                    enumeration.path.clone(),
                                    enumeration.enum_variants.len(),
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
                if let MatchPattern::Destructure { bindings, .. } = &arm.pattern {
                    arm_locals.extend(bindings.iter().cloned().map(|name| (name, Ty::Unknown)));
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
                Ty::Reference(inner, _) => match inner.as_ref() {
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
            Ty::Reference(Box::new(Ty::Slice(Box::new(element_type))), false)
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
        BodyExprKind::ArrayRepeat { element, length } => {
            let (expected_element, expected_length) = match expected_return {
                Ty::Array(expected, expected_length) => {
                    (Some(expected.as_ref()), Some(*expected_length))
                }
                _ => (None, None),
            };
            if let (Some(actual_length), Some(expected_length)) = (length, expected_length)
                && actual_length != &expected_length
            {
                diagnostics.push(TypeDiagnostic {
                    span: expression.span.clone(),
                    message: format!(
                        "array repeat length is {actual_length}, expected {expected_length}"
                    ),
                });
            }
            let element_type = infer_with_structs(
                element,
                locals,
                signatures,
                struct_signatures,
                expected_element.unwrap_or(expected_return),
                diagnostics,
            );
            Ty::Array(
                Box::new(element_type),
                length.or(expected_length).unwrap_or(0),
            )
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
        LiteralKind::String { .. } => Ty::Reference(Box::new(Ty::StringSlice), false),
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
            || matches!((&**target, actual), (Ty::String, Ty::Reference(inner, _)) if **inner == Ty::StringSlice))
        || matches!((expected, actual), (Ty::Tuple(expected), Ty::Tuple(actual))
            if expected.len() == actual.len()
                && expected.iter().zip(actual).all(|(expected, actual)| types_compatible(expected, actual)))
        || matches!((expected, actual), (Ty::Option(expected), Ty::Option(actual))
            if types_compatible(expected, actual))
        || matches!((expected, actual), (Ty::Result(expected_ok, expected_err), Ty::Result(actual_ok, actual_err))
            if types_compatible(expected_ok, actual_ok)
                && types_compatible(expected_err, actual_err))
        || matches!((expected, actual), (Ty::Vector(expected), Ty::Vector(actual))
            if types_compatible(expected, actual))
        || matches!((expected, actual), (Ty::Array(expected, expected_len), Ty::Array(actual, actual_len))
            if (expected_len == actual_len || *expected_len == 0 || *actual_len == 0)
                && types_compatible(expected, actual))
        || matches!((expected, actual), (Ty::Slice(expected), Ty::Vector(actual)) if types_compatible(expected, actual))
        || matches!((expected, actual), (Ty::Reference(expected, expected_mut), Ty::Reference(actual, actual_mut))
            if (!*expected_mut || *actual_mut)
                && matches!((&**expected, &**actual), (Ty::Slice(expected), Ty::Array(actual, _) | Ty::Vector(actual))
                    if types_compatible(expected, actual)))
        || matches!((expected, actual), (Ty::Reference(expected, expected_mut), Ty::Reference(actual, actual_mut))
            if (!*expected_mut || *actual_mut) && types_compatible(expected, actual))
        || matches!((expected, actual), (Ty::RawPointer(expected, expected_mut), Ty::RawPointer(actual, actual_mut))
            if (!*expected_mut || *actual_mut) && types_compatible(expected, actual))
        || matches!((expected, actual), (Ty::Slice(expected), Ty::Slice(actual)) if types_compatible(expected, actual))
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
        | Ty::Never => true,
        Ty::RawPointer(_, _) => true,
        Ty::Reference(_, mutable) => !*mutable,
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

fn generic_receiver_has_trait(ty: &Ty, requested_trait: &str) -> bool {
    let Ty::Named(name) = without_references(ty) else {
        return false;
    };
    // These extension traits inherit the standard I/O traits. Within a trait
    // default method, `Self` is represented by the extension-trait owner
    // rather than by a function generic marker.
    if (requested_trait == "Read" && name.ends_with("ReadBytesExt"))
        || (requested_trait == "Write" && name.ends_with("WriteBytesExt"))
    {
        return true;
    }
    let Some(traits) = name
        .strip_prefix("\0quill_generic:")
        .and_then(|marker| marker.split_once(":traits=").map(|(_, traits)| traits))
        .map(|traits| traits.split(':').next().unwrap_or_default())
    else {
        return false;
    };
    traits
        .split('+')
        .any(|trait_name| trait_name.rsplit("::").next() == Some(requested_trait))
}

fn is_as_ref_str_type(ty: &Ty, structs: &[StructSignature]) -> bool {
    match ty {
        Ty::String => true,
        Ty::Reference(inner, mutable) => !*mutable && matches!(inner.as_ref(), Ty::String | Ty::StringSlice),
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

    #[test]
    fn resolves_static_calls_in_trait_default_methods_to_trait_declarations() {
        let source = SourceFile::from_text(
            "trait-default-self.rs",
            "trait Value { fn value() -> u64; fn copy_value() -> u64 { Self::value() } }",
        );
        let parsed = parse(&source);
        let diagnostics = check_functions_strict(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }
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
                scope: Vec::new(),
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
                scope: Vec::new(),
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
    fn checks_integer_wrapping_methods_and_rejects_non_integer_receivers() {
        let valid = SourceFile::from_text(
            "wrapping.rs",
            "fn mix(state: u64, byte: u64) -> u64 { state.wrapping_mul(33).wrapping_add(byte) }",
        );
        let parsed = parse(&valid);
        assert!(
            check_functions_with_structs(&parsed.functions, &[]).is_empty(),
            "valid wrapping arithmetic should type-check"
        );

        let valid_u32 = SourceFile::from_text(
            "wrapping.rs",
            "fn mix(state: u32) -> u32 { state.wrapping_add(1) }",
        );
        let parsed = parse(&valid_u32);
        assert!(
            check_functions_with_structs(&parsed.functions, &[]).is_empty(),
            "u32 wrapping arithmetic should type-check"
        );

        let invalid = SourceFile::from_text(
            "wrapping.rs",
            "fn mix(state: bool) -> bool { state.wrapping_add(true) }",
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
            scope: Vec::new(),
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
    fn passing_a_mutable_reference_to_a_borrowing_function_reborrows_it() {
        let source = SourceFile::from_text(
            "mutable-reference-call-reborrow.rs",
            "fn touch(value: &mut i32) { *value = 1; } fn caller() { let mut value = 0; let reference = &mut value; touch(reference); *reference = 2; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(!diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("error[E0382]")
                || diagnostic.message.contains("error[E0505]")
        }), "{diagnostics:?}");
    }

    #[test]
    fn checks_move_state_on_later_loop_iterations() {
        let valid = SourceFile::from_text(
            "move-loop-reinitialize.rs",
            "fn consume(value: String) {} fn valid(mut again: bool) { let mut value = String::from(\"owned\"); while again { consume(value); value = String::from(\"again\"); again = false; } }",
        );
        let parsed = parse(&valid);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let source = SourceFile::from_text(
            "move-loop.rs",
            "fn consume(value: String) {} fn invalid(mut again: bool) { let value = String::from(\"owned\"); while again { println!(\"{value}\"); consume(value); again = false; } }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("error[E0382]")
                && diagnostic.message.contains("moved value: `value`")
        }), "{diagnostics:?}");
    }

    #[test]
    fn integer_range_bindings_are_copy_during_move_analysis() {
        let source = SourceFile::from_text(
            "copy-range-binding.rs",
            "fn sum(start: i32, end: i32) -> i32 { let mut total = 0; for value in start..end { total = total + value; } total } fn literal_range() -> i32 { let mut total = 0; for value in 0..8 { total = total + value; } total }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn resolves_labeled_loop_control_and_rejects_unknown_labels() {
        let source = SourceFile::from_text(
            "loop-labels.rs",
            "fn valid(condition: bool) { 'outer: while condition { loop { break 'outer; } } } fn invalid() { loop { break 'missing; } }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0]
            .message
            .contains("error[E0426]: use of undeclared label `'missing`"));
    }

    #[test]
    fn does_not_flow_moves_past_a_nested_labeled_break() {
        let source = SourceFile::from_text(
            "nested-labeled-break.rs",
            "fn valid(condition: bool) { let value = String::from(\"owned\"); 'outer: loop { loop { if condition { drop(value); break 'outer; } println!(\"{value}\"); break 'outer; } println!(\"{value}\"); } } fn invalid(condition: bool) { let value = String::from(\"owned\"); 'outer: loop { loop { if condition { drop(value); break 'outer; } println!(\"{value}\"); break 'outer; } println!(\"{value}\"); } println!(\"{value}\"); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("error[E0382]")
            && diagnostics[0].message.contains("moved value: `value`"));
        let invalid_read = source.text().rfind("{value}").unwrap() + 1;
        assert_eq!(diagnostics[0].span, invalid_read..invalid_read + "value".len());
    }

    #[test]
    fn keeps_mixed_outer_break_and_inner_break_paths_separate() {
        let source = SourceFile::from_text(
            "mixed-labeled-loop-exits.rs",
            "fn valid(condition: bool) { let value = String::from(\"owned\"); 'outer: loop { loop { if condition { drop(value); break 'outer; } break; } println!(\"{value}\"); break 'outer; } } fn invalid(condition: bool) { let value = String::from(\"owned\"); 'outer: loop { loop { if condition { drop(value); break 'outer; } break; } println!(\"{value}\"); break 'outer; } println!(\"{value}\"); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("error[E0382]"));
        let final_use = source.text().rfind("{value}").unwrap() + 1;
        assert_eq!(
            diagnostics[0].span,
            final_use..final_use + "value".len()
        );
    }

    #[test]
    fn preserves_outer_break_moves_from_match_arms() {
        let source = SourceFile::from_text(
            "match-outer-break-move.rs",
            "fn invalid(condition: bool) { let value = String::from(\"owned\"); 'outer: loop { loop { match condition { true => { drop(value); break 'outer; }, false => { break; } } } println!(\"{value}\"); break 'outer; } let later = value; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("error[E0382]")
                && diagnostic.message.contains("moved value: `value`")
        }), "{diagnostics:?}");
        let invalid_use = source.text().rfind("value").unwrap();
        assert_eq!(
            diagnostics
                .iter()
                .find(|diagnostic| diagnostic.message.contains("error[E0382]"))
                .map(|diagnostic| diagnostic.span.clone()),
            Some(invalid_use..invalid_use + "value".len()),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn propagates_outer_continue_moves_from_match_arms() {
        let source = SourceFile::from_text(
            "match-outer-continue-move.rs",
            "fn invalid(mut condition: bool) { let value = String::from(\"owned\"); 'outer: loop { loop { match condition { true => { drop(value); condition = false; continue 'outer; }, false => { break; } } } println!(\"{value}\"); break 'outer; } }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("error[E0382]")
                && diagnostic.message.contains("moved value: `value`")
        }), "{diagnostics:?}");
        let invalid_use = source.text().find("{value}").unwrap() + 1;
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.span == (invalid_use..invalid_use + "value".len())));
    }

    #[test]
    fn propagates_outer_continue_moves_from_if_branches() {
        let source = SourceFile::from_text(
            "if-outer-continue-move.rs",
            "fn invalid(mut condition: bool) { let value = String::from(\"owned\"); 'outer: loop { loop { if condition { drop(value); condition = false; continue 'outer; } else { break; } } println!(\"{value}\"); break 'outer; } }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("error[E0382]")
                && diagnostic.message.contains("moved value: `value`")
        }), "{diagnostics:?}");
        let invalid_use = source.text().find("{value}").unwrap() + 1;
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.span == (invalid_use..invalid_use + "value".len())));
    }

    #[test]
    fn excludes_unreachable_moves_after_direct_loop_breaks() {
        let source = SourceFile::from_text(
            "move-loop-break.rs",
            "fn consume(value: String) {} fn valid() { let value = String::from(\"owned\"); loop { break; consume(value); } println!(\"{value}\"); } fn invalid() { let value = String::from(\"owned\"); loop { consume(value); break; } println!(\"{value}\"); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("error[E0382]")
            && diagnostics[0].message.contains("moved value: `value`"));
        let invalid_read = source.text().rfind("{value}").unwrap() + 1;
        assert_eq!(diagnostics[0].span, invalid_read..invalid_read + "value".len());
    }

    #[test]
    fn excludes_unreachable_moves_after_direct_loop_returns() {
        let source = SourceFile::from_text(
            "move-loop-return.rs",
            "fn consume(value: String) {} fn valid() { let value = String::from(\"owned\"); loop { return; consume(value); } }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn separates_conditional_break_paths_from_loop_back_edges() {
        let source = SourceFile::from_text(
            "move-branch-break.rs",
            "fn valid(condition: bool) { let value = String::from(\"owned\"); loop { if condition { drop(value); break; } println!(\"{value}\"); break; } } fn valid_after_prefix(mut condition: bool) { let value = String::from(\"owned\"); loop { condition = !condition; if condition { drop(value); break; } println!(\"{value}\"); break; } }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn separates_match_arm_break_paths_from_loop_back_edges() {
        let source = SourceFile::from_text(
            "move-match-break.rs",
            "fn valid(condition: bool) { let value = String::from(\"owned\"); loop { match condition { true => { drop(value); break; }, false => { println!(\"{value}\"); break; } } } } fn invalid(condition: bool) { let value = String::from(\"owned\"); loop { match condition { true => { drop(value); break; }, false => { println!(\"{value}\"); break; } } } println!(\"{value}\"); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("error[E0382]")
            && diagnostics[0].message.contains("moved value: `value`"));
        let invalid_read = source.text().rfind("{value}").unwrap() + 1;
        assert_eq!(diagnostics[0].span, invalid_read..invalid_read + "value".len());
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
    fn resolves_methods_on_impl_trait_and_dyn_trait_parameters() {
        let source = SourceFile::from_text(
            "trait-objects.rs",
            "trait Value { fn value(&self) -> u64; } fn read(value: &impl Value) -> u64 { value.value() } fn read_dyn(value: &dyn Value) -> u64 { value.value() }",
        );
        let parsed = parse(&source);
        let diagnostics = check_functions_strict(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
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

    #[test]
    fn allows_replacing_a_mutable_reference_in_a_loop_before_using_it() {
        let source = SourceFile::from_text(
            "loop-reborrow.rs",
            "fn loop_reborrow() { let mut flag = true; let mut value: i32 = 0; let mut reference = &mut value; while flag { reference = &mut value; flag = false; } *reference += 1; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn checks_assignments_through_shared_and_mutable_references() {
        let valid = SourceFile::from_text(
            "mutable-reference-write.rs",
            "fn valid() { let mut value: i32 = 0; let reference = &mut value; *reference = 1; }",
        );
        let parsed = parse(&valid);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let invalid = SourceFile::from_text(
            "shared-reference-write.rs",
            "fn invalid() { let value: i32 = 0; let reference = &value; *reference = 1; }",
        );
        let parsed = parse(&invalid);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("error[E0594]: cannot assign to data in an immutable reference")
        }));
        assert!(diagnostics.iter().all(|diagnostic| {
            !diagnostic.message.contains("cannot assign to unresolved local")
        }));
    }

    #[test]
    fn excludes_returning_if_branches_from_post_branch_loan_state() {
        let source = SourceFile::from_text(
            "returning-branch-loan.rs",
            "fn valid(condition: bool) { let mut value: i32 = 0; let mut other: i32 = 0; let mut reference = &mut other; if condition { reference = &mut value; return; } value = 1; *reference = 2; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn carries_loop_generated_loans_to_the_loop_exit() {
        let source = SourceFile::from_text(
            "loop-loan-exit.rs",
            "fn invalid(flag: bool) { let mut value: i32 = 0; let mut other: i32 = 0; let mut reference = &mut value; while flag { reference = &mut other; } let conflict = &mut other; *reference += 1; *conflict += 1; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("error[E0499]: cannot borrow `other` as mutable more than once")
        }), "{diagnostics:?}");
    }

    #[test]
    fn rejects_a_borrow_of_an_inner_local_stored_in_an_outer_reference() {
        let source = SourceFile::from_text(
            "scope-escape.rs",
            "fn invalid() { let mut reference = &0i32; { let value = 1i32; reference = &value; } println!(\"{reference}\"); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("error[E0597]: `value` does not live long enough")
        }), "{diagnostics:?}");
    }

    #[test]
    fn allows_a_dead_inner_reference_assignment_to_end_with_its_scope() {
        let source = SourceFile::from_text(
            "dead-scope-escape.rs",
            "fn valid() { let mut reference = &0i32; { let value = 1i32; reference = &value; } }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn checks_user_defined_shared_and_mutable_method_receivers() {
        let source = SourceFile::from_text(
            "method-receiver-borrows.rs",
            "struct Buffer { value: i32 } impl Buffer { fn shared(&self, target: &mut i32) {} fn exclusive(&mut self, target: &i32) {} } fn shared_conflict() { let mut item = Buffer { value: 0 }; item.shared(&mut item.value); } fn mutable_conflict() { let mut item = Buffer { value: 0 }; item.exclusive(&item.value); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions_with_structs(
            &parsed.functions,
            &[StructSignature {
                path: "Buffer".to_owned(),
                fields: vec![StructFieldSignature {
                    name: "value".to_owned(),
                    type_text: "i32".to_owned(),
                }],
                ..StructSignature::default()
            }],
        );
        assert!(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.message.contains("error[E0502]"))
                .count()
                >= 2,
            "{diagnostics:?}"
        );
    }

    #[test]
    fn keeps_a_loan_from_a_user_defined_reference_returning_method() {
        let source = SourceFile::from_text(
            "method-returned-loan.rs",
            "struct Buffer { value: i32 } impl Buffer { fn value(&self) -> &i32 { &self.value } } fn invalid() { let mut item = Buffer { value: 0 }; let borrowed = item.value(); item.value = 1; println!(\"{borrowed}\"); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions_with_structs(
            &parsed.functions,
            &[StructSignature {
                path: "Buffer".to_owned(),
                fields: vec![StructFieldSignature {
                    name: "value".to_owned(),
                    type_text: "i32".to_owned(),
                }],
                ..StructSignature::default()
            }],
        );
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("error[E0506]: cannot assign to `item` because it is borrowed")
        }), "{diagnostics:?}");
    }

    #[test]
    fn applies_two_phase_borrows_to_mutable_method_receivers() {
        let source = SourceFile::from_text(
            "two-phase-receiver.rs",
            "struct Buffer { value: i32 } impl Buffer { fn update(&mut self, amount: i32) {} fn shared_arg(&mut self, other: &i32) {} } fn valid() { let mut item = Buffer { value: 1 }; item.update(item.value); } fn invalid() { let mut item = Buffer { value: 1 }; item.shared_arg(&item.value); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions_with_structs(
            &parsed.functions,
            &[StructSignature {
                path: "Buffer".to_owned(),
                fields: vec![StructFieldSignature {
                    name: "value".to_owned(),
                    type_text: "i32".to_owned(),
                }],
                ..StructSignature::default()
            }],
        );
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("error[E0502]: cannot borrow `item` as mutable because it is also borrowed as immutable")
        }), "{diagnostics:?}");
        assert!(!diagnostics.iter().any(|diagnostic| {
            diagnostic.span.start < source.text().find("fn invalid").unwrap()
                && diagnostic.message.contains("error[E050")
        }), "{diagnostics:?}");
    }

    #[test]
    fn propagates_loans_through_reference_returning_function_calls() {
        let source = SourceFile::from_text(
            "call-returned-loan.rs",
            "fn identity(value: &i32) -> &i32 { value } fn invalid() { let mut value: i32 = 0; let borrowed = identity(&value); value = 1; println!(\"{borrowed}\"); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("error[E0506]: cannot assign to `value` because it is borrowed")
        }), "{diagnostics:?}");
    }

    #[test]
    fn models_mutable_reborrows_returned_from_function_calls() {
        let source = SourceFile::from_text(
            "call-reborrow.rs",
            "fn identity(value: &mut i32) -> &mut i32 { value } fn invalid() { let mut value: i32 = 0; let reference = &mut value; let returned = identity(reference); *reference = 1; *returned = 2; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("error[E0503]: cannot use `value` while a reborrow of its mutable reference is active")
        }), "{diagnostics:?}");
    }

    #[test]
    fn follows_named_lifetimes_to_the_correct_call_argument() {
        let source = SourceFile::from_text(
            "named-call-lifetime.rs",
            "fn choose<'a, 'b>(left: &'a i32, right: &'b i32) -> &'b i32 { right } fn check() { let mut first: i32 = 0; let mut second: i32 = 0; let borrowed = choose(&first, &second); first = 1; second = 1; println!(\"{borrowed}\"); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("error[E0506]: cannot assign to `second` because it is borrowed")
        }), "{diagnostics:?}");
        assert!(!diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("error[E0506]: cannot assign to `first` because it is borrowed")
        }), "{diagnostics:?}");
    }

    #[test]
    fn tracks_references_stored_in_and_read_from_aggregate_fields() {
        let source = SourceFile::from_text(
            "aggregate-field-loans.rs",
            "fn invalid() { let mut value: i32 = 0; let holder = (&value, 7); let borrowed = holder.0; value = 1; println!(\"{borrowed}\"); } fn valid_disjoint_field() { let mut value: i32 = 0; let holder = (&value, 7); let scalar = holder.1; value = 1; println!(\"{scalar}\"); } fn invalid_live_aggregate_field() { let mut value: i32 = 0; let holder = (&value, 7); value = 1; println!(\"{}\", holder.1); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("error[E0506]: cannot assign to `value` because it is borrowed")
        }), "{diagnostics:?}");
        assert!(!diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("cannot assign to `value` because it is borrowed")
                && diagnostic.span.start > source.text().find("fn valid_disjoint_field").unwrap()
                && diagnostic.span.start < source.text().find("fn invalid_live_aggregate_field").unwrap()
        }), "{diagnostics:?}");
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("cannot assign to `value` because it is borrowed")
                && diagnostic.span.start > source.text().find("fn invalid_live_aggregate_field").unwrap()
        }), "{diagnostics:?}");
    }

    #[test]
    fn tracks_initialized_fields_of_uninitialized_aggregates() {
        let source = SourceFile::from_text(
            "aggregate-initialization.rs",
            "struct Pair { left: i32, right: i32 } fn valid() { let mut pair: Pair; pair.left = 1; pair.right = 2; let value = pair.left; } fn invalid() { let mut pair: Pair; pair.left = 1; let value = pair.right; } fn invalid_format() { let mut pair: Pair; pair.left = 1; println!(\"{}\", pair.right); } fn invalid_capture() { let mut pair: Pair; pair.left = 1; println!(\"{pair:?}\"); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions_with_structs(
            &parsed.functions,
            &[pair_struct_signature()],
        );
        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.message.contains("error[E0381]"))
                .count(),
            3,
            "{diagnostics:?}"
        );
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("error[E0381]")
                && diagnostic.message.contains("pair.right")
        }), "{diagnostics:?}");
        assert!(!diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("error[E0381]")
                && diagnostic.span.start < source.text().find("fn invalid").unwrap()
        }), "{diagnostics:?}");
    }

    #[test]
    fn tracks_initialized_elements_of_uninitialized_arrays() {
        let source = SourceFile::from_text(
            "array-initialization.rs",
            "fn valid() { let mut values: [i32; 2]; values[0] = 1; values[1] = 2; let first = values[0]; } fn invalid() { let mut values: [i32; 2]; values[0] = 1; let second = values[1]; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("error[E0381]")
            && diagnostics[0].message.contains("values[1]"));
        assert!(diagnostics[0].span.start > source.text().find("fn invalid").unwrap());
    }

    #[test]
    fn reinitializes_moved_fields_without_restoring_siblings() {
        let source = SourceFile::from_text(
            "partial-reinitialization.rs",
            "struct Pair { left: i32, right: i32 } fn valid() { let mut original = Pair { left: 1, right: 2 }; let moved = original; original.left = 3; original.right = 4; let complete = original; } fn invalid() { let mut original = Pair { left: 1, right: 2 }; let moved = original; original.left = 3; let missing = original.right; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions_with_structs(
            &parsed.functions,
            &[pair_struct_signature()],
        );
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("error[E0382]")
            && diagnostics[0].message.contains("original.right"));
        assert!(diagnostics[0].span.start > source.text().find("fn invalid").unwrap());
    }

    #[test]
    fn transfers_mutable_reference_loans_through_tuple_destructuring() {
        let source = SourceFile::from_text(
            "tuple-reference-move.rs",
            "fn valid() { let mut first = 0; let mut second = 0; let pair = (&mut first, &mut second); let (left, right) = pair; *left = 1; *right = 2; } fn invalid() { let mut first = 0; let mut second = 0; let pair = (&mut first, &mut second); let (left, right) = pair; *left = 1; first = 2; *left = 3; } fn moved(value: &mut i32, other: &mut i32) { let pair = (value, other); let (left, right) = pair; let third = pair.0; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("error[E0382]")
                && diagnostic.message.contains("moved value: `pair.0`")
        }), "{diagnostics:?}");
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("error[E0506]")
                && diagnostic.message.contains("cannot assign to `first` because it is borrowed")
        }), "{diagnostics:?}");
        assert!(!diagnostics.iter().any(|diagnostic| {
            diagnostic.span.start < source.text().find("fn invalid").unwrap()
        }), "{diagnostics:?}");
    }

    #[test]
    fn expires_a_shadowed_mutable_reference_before_its_replacement_borrow() {
        let source = SourceFile::from_text(
            "shadowed-reference.rs",
            "fn valid() { let mut value: i32 = 0; let reference = &mut value; let reference = &mut value; *reference += 1; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn rejects_moving_a_non_copy_value_while_a_borrow_is_live() {
        let source = SourceFile::from_text(
            "move-while-borrowed.rs",
            "struct Item { value: i32 } fn consume(item: Item) {} fn invalid() { let item = Item { value: 0 }; let borrowed = &item; consume(item); println!(\"{borrowed:?}\"); }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions_with_structs(
            &parsed.functions,
            &[StructSignature {
                path: "Item".to_owned(),
                fields: vec![StructFieldSignature {
                    name: "value".to_owned(),
                    type_text: "i32".to_owned(),
                }],
                ..StructSignature::default()
            }],
        );
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("error[E0505]: cannot move out of `item` because it is borrowed")
        }), "{diagnostics:?}");
    }

    #[test]
    fn rejects_moving_a_mutable_reference_while_its_reborrow_is_live() {
        let source = SourceFile::from_text(
            "move-reborrowed-reference.rs",
            "fn valid() { let mut value: i32 = 0; let reference = &mut value; let reborrow = &mut *reference; *reborrow = 1; let moved = reference; *moved = 2; } fn invalid() { let mut value: i32 = 0; let reference = &mut value; let reborrow = &mut *reference; let moved = reference; *reborrow = 1; *moved = 2; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("error[E0505]")
                && diagnostic.message.contains("cannot move out of `reference` because it is borrowed")
        }), "{diagnostics:?}");
        assert!(!diagnostics.iter().any(|diagnostic| {
            diagnostic.span.start < source.text().find("fn invalid").unwrap()
        }), "{diagnostics:?}");
    }

    #[test]
    fn tracks_reborrows_of_projected_mutable_reference_holders() {
        let source = SourceFile::from_text(
            "projected-mutable-reborrow.rs",
            "fn invalid() { let mut first = 0; let mut second = 0; let pair = (&mut first, &mut second); let reborrow = &mut *pair.0; let moved = pair.0; *reborrow = 1; *moved = 2; } fn valid_sibling() { let mut first = 0; let mut second = 0; let pair = (&mut first, &mut second); let reborrow = &mut *pair.0; let moved = pair.1; *reborrow = 1; *moved = 2; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("error[E0505]")
                && diagnostic.message.contains("cannot move out of `pair.0` because it is borrowed")
        }), "{diagnostics:?}");
        assert!(!diagnostics.iter().any(|diagnostic| {
            diagnostic.span.start > source.text().find("fn valid_sibling").unwrap()
        }), "{diagnostics:?}");
    }

    #[test]
    fn excludes_unreachable_loan_creation_after_loop_break() {
        let source = SourceFile::from_text(
            "unreachable-loop-loan.rs",
            "fn valid() { let mut value: i32 = 0; let mut other: i32 = 0; let mut reference = &mut other; loop { break; reference = &mut value; } value = 1; *reference = 2; } fn invalid() { let mut value: i32 = 0; let mut other: i32 = 0; let mut reference = &mut other; loop { reference = &mut value; break; } value = 1; *reference = 2; } fn invalid_branch(condition: bool) { let mut value: i32 = 0; let mut other: i32 = 0; let mut reference = &mut other; loop { if condition { reference = &mut value; break; } else { break; } } value = 1; *reference = 2; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().filter(|diagnostic| {
            diagnostic.message.contains("error[E0506]")
                && diagnostic.message.contains("cannot assign to `value` because it is borrowed")
        }).count() >= 2, "{diagnostics:?}");
        assert!(!diagnostics.iter().any(|diagnostic| {
            diagnostic.span.start < source.text().find("fn invalid").unwrap()
        }), "{diagnostics:?}");
    }

    #[test]
    fn distinguishes_borrowing_a_reference_binding_from_its_referent() {
        let source = SourceFile::from_text(
            "reference-binding-loan.rs",
            "fn invalid() { let mut value: i32 = 0; let mut reference = &mut value; let slot = &mut reference; *reference = 1; **slot = 2; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("error[E0506]")
                || diagnostic.message.contains("error[E0503]")
        }), "{diagnostics:?}");
    }

    #[test]
    fn keeps_iterator_borrows_active_through_the_loop_body() {
        let source = SourceFile::from_text(
            "iterator-borrow.rs",
            "fn valid() { let values: Vec<i32> = Vec::new(); let mut total: i32 = 0; for item in values.iter() { total += *item; } } fn invalid() { let mut values: Vec<i32> = Vec::new(); for item in values.iter() { values.push(*item); } }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("error[E0502]")
                || diagnostic.message.contains("error[E0499]")
        }), "{diagnostics:?}");
        assert!(!diagnostics.iter().any(|diagnostic| {
            diagnostic.span.start < source.text().find("fn invalid").unwrap()
                && diagnostic.message.contains("error[E050")
        }), "{diagnostics:?}");
    }

    #[test]
    fn carries_scrutinee_loans_into_match_bindings() {
        let source = SourceFile::from_text(
            "match-binding-loan.rs",
            "fn invalid() { let mut value: i32 = 0; let reference = &value; match reference { captured => { value = 1; println!(\"{captured}\"); } } }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("error[E0506]: cannot assign to `value` because it is borrowed")
        }), "{diagnostics:?}");
    }

    #[test]
    fn tracks_mutability_of_parameter_bindings_separately_from_referents() {
        let source = SourceFile::from_text(
            "parameter-mutability.rs",
            "fn mutable_value(mut value: i32) { value = 1; } fn mutate_referent(value: &mut i32) { *value = 1; } fn cannot_rebind_reference(value: &mut i32, other: &mut i32) { value = other; }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("cannot assign twice to immutable variable `value`")
        }), "{diagnostics:?}");
        assert!(!diagnostics.iter().any(|diagnostic| {
            diagnostic.span.start < source.text().find("fn cannot_rebind_reference").unwrap()
                && diagnostic.message.contains("cannot assign twice")
        }), "{diagnostics:?}");
    }

    #[test]
    fn validates_explicit_function_return_lifetimes() {
        let source = SourceFile::from_text(
            "return-lifetime-mismatch.rs",
            "fn invalid<'a, 'b>(first: &'a i32, second: &'b i32) -> &'b i32 { first } fn valid<'a>(value: &'a i32) -> &'a i32 { value }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("error[E0621]: explicit lifetime required to return this reference")
        }), "{diagnostics:?}");
        assert!(!diagnostics.iter().any(|diagnostic| {
            diagnostic.span.start > source.text().find("fn valid").unwrap()
                && diagnostic.message.contains("explicit lifetime required")
        }), "{diagnostics:?}");
    }

    #[test]
    fn checks_tuple_struct_construction_fields_and_integer_methods() {
        let source = SourceFile::from_text(
            "tuple-struct-integer-method.rs",
            "struct Pair(u32, u8); fn value() -> u32 { let pair = Pair(7, 1); pair.0.to_le() } fn from_bytes(bytes: [u8; 4]) -> u32 { u32::from_be_bytes(bytes) }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions_with_structs(
            &parsed.functions,
            &[StructSignature {
                path: "Pair".to_owned(),
                fields: vec![
                    StructFieldSignature {
                        name: "0".to_owned(),
                        type_text: "u32".to_owned(),
                    },
                    StructFieldSignature {
                        name: "1".to_owned(),
                        type_text: "u8".to_owned(),
                    },
                ],
                ..StructSignature::default()
            }],
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn checks_raw_pointer_types_and_slice_pointer_methods() {
        let source = SourceFile::from_text(
            "raw-pointer-methods.rs",
            "fn raw(bytes: &mut [u8]) -> *mut u8 { bytes.as_mut_ptr() } fn read(pointer: *const u32) -> u32 { pointer.read_unaligned() }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn checks_array_repeats_range_slices_and_try_into() {
        let source = SourceFile::from_text(
            "array-repeat-range-try-into.rs",
            "fn repeated() -> u8 { let bytes = [0; 4]; bytes[0] } fn read(bytes: &[u8]) -> u16 { u16::from_be_bytes(bytes[..2].try_into().unwrap()) }",
        );
        let parsed = parse(&source);
        assert!(!parsed.had_error);
        let diagnostics = check_functions(&parsed.functions);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }
}
