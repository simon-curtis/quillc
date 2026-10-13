use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::ops::Range;

use quill_syntax::{FunctionItem, ImportItem, ModuleItem, Parse, StructItem};
use serde::{Deserialize, Serialize};

/// Canonical source-level identity; whitespace and unrelated declarations do not affect it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DefPath {
    pub crate_name: String,
    pub scope: Vec<String>,
    pub name: String,
    pub disambiguator: u32,
}

impl fmt::Display for DefPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.crate_name)?;
        for component in &self.scope {
            write!(formatter, "::{component}")?;
        }
        write!(formatter, "::{}", self.name)?;
        if self.disambiguator != 0 {
            write!(formatter, "#{}", self.disambiguator)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    pub def_path: DefPath,
    pub source_span: Range<usize>,
    pub name_span: Range<usize>,
    pub body_span: Option<Range<usize>>,
    pub externally_visible: bool,
    pub generic: bool,
    pub calls: Vec<String>,
    pub qualified_calls: Vec<String>,
    pub signature: FunctionSignature,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionSignature {
    pub parameter_types: Vec<String>,
    pub return_type: Option<String>,
    pub generic_parameters: Vec<String>,
    pub generic_copy_parameters: Vec<String>,
    pub generic_as_ref_str_parameters: Vec<String>,
    pub generic_add_parameters: Vec<String>,
    pub generic_has_bounds: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructDef {
    pub def_path: DefPath,
    pub source_span: Range<usize>,
    pub externally_visible: bool,
    pub generic_parameters: Vec<String>,
    pub derived_traits: Vec<String>,
    pub repr_c: bool,
    pub repr_layout_supported: bool,
    pub fields: Vec<StructField>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructField {
    pub name: Option<String>,
    pub type_text: String,
    pub externally_visible: bool,
    pub source_span: Range<usize>,
}

/// A description of a Rust aggregate's field placement for Quill's current
/// native target. The layout engine models x86_64-unknown-linux-gnu and is
/// deliberately separate from
/// machine-code emission so type checking and codegen can share one answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructLayout {
    pub size: u64,
    pub align: u64,
    pub field_offsets: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutError {
    UnknownType(String),
    RecursiveByValue(String),
    GenericType(String),
    UnsupportedRepresentation(String),
}

/// Compute a concrete struct layout for the native x86_64 target.
/// Rust-owned standard containers use their current three-word descriptor
/// representation in this ABI model; this describes placement only and does
/// not claim their runtime operations are implemented.
pub fn layout_of_struct(hir: &HirCrate, definition: &DefPath) -> Result<StructLayout, LayoutError> {
    fn layout_type(
        hir: &HirCrate,
        ty: &str,
        active: &mut Vec<DefPath>,
    ) -> Result<(u64, u64), LayoutError> {
        let ty = ty.trim();
        let primitive = match ty {
            "()" | "!" => Some((0, 1)),
            "bool" | "i8" | "u8" => Some((1, 1)),
            "i16" | "u16" => Some((2, 2)),
            "i32" | "u32" | "f32" | "char" => Some((4, 4)),
            "i64" | "u64" | "isize" | "usize" | "f64" => Some((8, 8)),
            "String" | "Vec" | "BTreeMap" | "HashMap" => Some((24, 8)),
            _ => None,
        };
        if let Some(layout) = primitive {
            return Ok(layout);
        }
        if ["Vec<", "BTreeMap<", "HashMap<"]
            .iter()
            .any(|prefix| ty.starts_with(prefix))
        {
            return Ok((24, 8));
        }
        if ty.starts_with('&') || ty.starts_with("*const ") || ty.starts_with("*mut ") {
            let pointee = ty.trim_start_matches('&').trim_start_matches("mut ").trim();
            return Ok(if pointee.starts_with('[') || pointee == "str" {
                (16, 8)
            } else {
                (8, 8)
            });
        }
        if ty.starts_with('T') || ty.contains("impl ") || ty.contains('<') {
            return Err(LayoutError::GenericType(ty.to_owned()));
        }
        let simple_name = ty.rsplit("::").next().unwrap_or(ty);
        let candidates: Vec<_> = hir
            .structs
            .iter()
            .filter(|item| item.def_path.name == simple_name || item.def_path.to_string() == ty)
            .collect();
        let Some(item) = candidates.first().copied() else {
            return Err(LayoutError::UnknownType(ty.to_owned()));
        };
        if candidates.len() > 1 && !ty.contains("::") {
            return Err(LayoutError::UnknownType(ty.to_owned()));
        }
        if active.contains(&item.def_path) {
            return Err(LayoutError::RecursiveByValue(ty.to_owned()));
        }
        let nested = layout_struct(hir, item, active)?;
        Ok((nested.size, nested.align))
    }

    fn layout_struct(
        hir: &HirCrate,
        definition: &StructDef,
        active: &mut Vec<DefPath>,
    ) -> Result<StructLayout, LayoutError> {
        if !definition.generic_parameters.is_empty() {
            return Err(LayoutError::GenericType(definition.def_path.to_string()));
        }
        if !definition.repr_layout_supported {
            return Err(LayoutError::UnsupportedRepresentation(
                definition.def_path.to_string(),
            ));
        }
        active.push(definition.def_path.clone());
        let mut physical_fields = definition
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| {
                layout_type(hir, &field.type_text, active).map(|(size, align)| (index, size, align))
            })
            .collect::<Result<Vec<_>, _>>()?;
        // rustc's repr(Rust) layout groups fields by decreasing alignment,
        // then decreasing size. Keep equal-sized fields in declaration order.
        if !definition.repr_c {
            physical_fields.sort_by_key(|(index, size, align)| {
                (std::cmp::Reverse(*align), std::cmp::Reverse(*size), *index)
            });
        }
        let mut offset: u64 = 0;
        let mut alignment = 1;
        let mut field_offsets = vec![0; definition.fields.len()];
        for (index, size, align) in physical_fields {
            alignment = alignment.max(align);
            offset = offset.div_ceil(align) * align;
            field_offsets[index] = offset;
            offset += size;
        }
        active.pop();
        Ok(StructLayout {
            size: offset.div_ceil(alignment) * alignment,
            align: alignment,
            field_offsets,
        })
    }

    let item = hir
        .structs
        .iter()
        .find(|item| &item.def_path == definition)
        .ok_or_else(|| LayoutError::UnknownType(definition.to_string()))?;
    layout_struct(hir, item, &mut Vec::new())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedFunction {
    /// Module-qualified path relative to this crate root.
    pub path: String,
    /// Stable ELF symbol used by separately compiled Quill crate units.
    pub symbol: String,
    pub signature: FunctionSignature,
}

/// Versioned, serializable callable interface consumed by dependent crates.
/// It intentionally describes only exported functions; struct layouts,
/// trait implementations, and macro exports need separate metadata records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrateInterface {
    pub format_version: u32,
    pub crate_name: String,
    pub functions: Vec<ExportedFunction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HirCrate {
    pub name: String,
    pub functions: Vec<Function>,
    pub structs: Vec<StructDef>,
    pub modules: Vec<Module>,
    pub imports: Vec<Import>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Module {
    pub path: Vec<String>,
    pub source_span: Range<usize>,
    pub external: bool,
    pub path_override: Option<String>,
    pub externally_visible: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    pub scope: Vec<String>,
    pub path: Vec<String>,
    pub alias: Option<String>,
    pub glob: bool,
    pub externally_visible: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallResolution {
    pub caller: DefPath,
    pub spelling: String,
    pub target: CallTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallTarget {
    Unique(DefPath),
    Ambiguous(Vec<DefPath>),
    Unresolved,
}

/// Structural type identity used to key requested generic instances.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TypeKey {
    Unit,
    Bool,
    Integer {
        bits: u16,
        signed: bool,
    },
    Float {
        bits: u16,
    },
    Tuple(Vec<TypeKey>),
    Named {
        definition: DefPath,
        arguments: Vec<TypeKey>,
    },
    Reference {
        mutable: bool,
        target: Box<TypeKey>,
    },
    Parameter(u32),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConstKey {
    Integer(i128),
    Bool(bool),
    Character(char),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MonoInstanceKey {
    pub definition: DefPath,
    pub type_arguments: Vec<TypeKey>,
    pub const_arguments: Vec<ConstKey>,
}

/// Deduplicates instantiation requests by the shared generic definition and
/// its exact canonical arguments.
#[derive(Clone, Debug, Default)]
pub struct MonomorphizationQueue {
    requested: BTreeSet<MonoInstanceKey>,
    pending: VecDeque<MonoInstanceKey>,
}

impl MonomorphizationQueue {
    pub fn request(&mut self, instance: MonoInstanceKey) -> bool {
        if self.requested.insert(instance.clone()) {
            self.pending.push_back(instance);
            true
        } else {
            false
        }
    }

    pub fn pop(&mut self) -> Option<MonoInstanceKey> {
        self.pending.pop_front()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

pub fn lower_functions(crate_name: &str, items: &[FunctionItem]) -> HirCrate {
    lower_items(crate_name, items, &[], &[], &[])
}

/// Lower all parsed source files in a crate together so item identities and
/// later resolution operate over one crate-wide index. Parsing remains file
/// parallel; this stage only joins immutable parse results.
pub fn lower_parses(crate_name: &str, parses: &[Parse]) -> HirCrate {
    let files: Vec<_> = parses
        .iter()
        .map(|parse| (Vec::new(), parse.clone()))
        .collect();
    lower_module_files(crate_name, &files)
}

/// Lower parse results with the module path of each source file. For example,
/// `src/net/client.rs` is passed `["net", "client"]`. This keeps sibling
/// files distinct and gives callers a deterministic place to attach module
/// discovery and file loading.
pub fn lower_module_files(crate_name: &str, files: &[(Vec<String>, Parse)]) -> HirCrate {
    let functions: Vec<_> = files
        .iter()
        .flat_map(|(file_scope, parse)| {
            parse.functions.iter().cloned().map(move |mut item| {
                let mut scope = file_scope
                    .iter()
                    .map(|name| format!("mod:{name}"))
                    .collect::<Vec<_>>();
                scope.extend(item.scope);
                item.scope = scope;
                item
            })
        })
        .collect();
    let modules: Vec<_> = files
        .iter()
        .flat_map(|(file_scope, parse)| {
            parse.modules.iter().cloned().map(move |mut item| {
                let mut scope = file_scope
                    .iter()
                    .map(|name| format!("mod:{name}"))
                    .collect::<Vec<_>>();
                scope.extend(item.parent_scope);
                item.parent_scope = scope;
                item
            })
        })
        .collect();
    let imports: Vec<_> = files
        .iter()
        .flat_map(|(file_scope, parse)| {
            parse.imports.iter().cloned().map(move |mut item| {
                let mut scope = file_scope
                    .iter()
                    .map(|name| format!("mod:{name}"))
                    .collect::<Vec<_>>();
                scope.extend(item.scope);
                item.scope = scope;
                item
            })
        })
        .collect();
    let structs: Vec<_> = files
        .iter()
        .flat_map(|(file_scope, parse)| {
            parse.structs.iter().cloned().map(move |mut item| {
                let mut scope = file_scope
                    .iter()
                    .map(|name| format!("mod:{name}"))
                    .collect::<Vec<_>>();
                scope.extend(item.scope);
                item.scope = scope;
                item
            })
        })
        .collect();
    lower_items(crate_name, &functions, &structs, &modules, &imports)
}

fn lower_items(
    crate_name: &str,
    items: &[FunctionItem],
    struct_items: &[StructItem],
    module_items: &[ModuleItem],
    import_items: &[ImportItem],
) -> HirCrate {
    let mut counts: HashMap<(Vec<String>, String), u32> = HashMap::new();
    let functions = items
        .iter()
        .map(|item| {
            let key = (item.scope.clone(), item.name.clone());
            let disambiguator = counts.entry(key).or_default();
            let current = *disambiguator;
            *disambiguator += 1;
            Function {
                def_path: DefPath {
                    crate_name: crate_name.to_owned(),
                    scope: item.scope.clone(),
                    name: item.name.clone(),
                    disambiguator: current,
                },
                source_span: item.span.clone(),
                name_span: item.name_span.clone(),
                body_span: item.body_span.clone(),
                externally_visible: item.is_public,
                generic: item.is_generic,
                calls: item.calls.clone(),
                qualified_calls: item.qualified_calls.clone(),
                signature: FunctionSignature {
                    parameter_types: item
                        .parameters
                        .iter()
                        .map(|parameter| parameter.type_text.clone())
                        .collect(),
                    return_type: item.return_type.clone(),
                    generic_parameters: item.generic_parameters.clone(),
                    generic_copy_parameters: item.generic_copy_parameters.clone(),
                    generic_as_ref_str_parameters: item.generic_as_ref_str_parameters.clone(),
                    generic_add_parameters: item.generic_add_parameters.clone(),
                    generic_has_bounds: item.generic_has_bounds,
                },
            }
        })
        .collect();
    let modules = module_items
        .iter()
        .map(|item| {
            let mut path = item
                .parent_scope
                .iter()
                .filter_map(|component| component.strip_prefix("mod:").map(str::to_owned))
                .collect::<Vec<_>>();
            path.push(item.name.clone());
            Module {
                path,
                source_span: item.span.clone(),
                external: item.body_span.is_none(),
                path_override: item.path_override.clone(),
                externally_visible: item.is_public,
            }
        })
        .collect();
    let mut struct_counts: HashMap<(Vec<String>, String), u32> = HashMap::new();
    let structs = struct_items
        .iter()
        .map(|item| {
            let key = (item.scope.clone(), item.name.clone());
            let disambiguator = struct_counts.entry(key).or_default();
            let current = *disambiguator;
            *disambiguator += 1;
            StructDef {
                def_path: DefPath {
                    crate_name: crate_name.to_owned(),
                    scope: item.scope.clone(),
                    name: item.name.clone(),
                    disambiguator: current,
                },
                source_span: item.span.clone(),
                externally_visible: item.is_public,
                generic_parameters: item.generic_parameters.clone(),
                derived_traits: item.derived_traits.clone(),
                repr_c: item.repr_c,
                repr_layout_supported: item.repr_layout_supported,
                fields: item
                    .fields
                    .iter()
                    .map(|field| StructField {
                        name: field.name.clone(),
                        type_text: field.type_text.clone(),
                        externally_visible: field.is_public,
                        source_span: field.span.clone(),
                    })
                    .collect(),
            }
        })
        .collect();
    let imports = import_items
        .iter()
        .map(|item| Import {
            scope: item.scope.clone(),
            path: item.path.clone(),
            alias: item.alias.clone(),
            glob: item.is_glob,
            externally_visible: item.is_public,
        })
        .collect();
    HirCrate {
        name: crate_name.to_owned(),
        functions,
        structs,
        modules,
        imports,
    }
}

/// Build the callable portion of a crate interface for downstream queries.
/// This is intentionally not Rust metadata: types remain source spellings and
/// reexports, traits, and visibility through macro expansion are not modeled.
pub fn exported_functions(hir: &HirCrate) -> Vec<ExportedFunction> {
    let mut exports: Vec<_> = hir
        .functions
        .iter()
        .filter(|function| exported_function(hir, function))
        .map(|function| {
            let mut components = module_scope(&function.def_path.scope);
            components.extend(
                function
                    .def_path
                    .scope
                    .iter()
                    .filter_map(|scope| scope.strip_prefix("impl:").map(str::to_owned)),
            );
            components.push(function.def_path.name.clone());
            let path = components.join("::");
            ExportedFunction {
                symbol: exported_symbol(&format!("{}::{path}", hir.name)),
                path,
                signature: function.signature.clone(),
            }
        })
        .collect();
    exports.sort_by(|left, right| left.path.cmp(&right.path));
    exports
}

pub fn exported_interface(hir: &HirCrate) -> CrateInterface {
    CrateInterface {
        format_version: 1,
        crate_name: hir.name.clone(),
        functions: exported_functions(hir),
    }
}

/// Deterministic symbol encoding shared by exported interface metadata and
/// ELF code generation. Hex encoding keeps arbitrary Rust path bytes valid in
/// common object-tool output without relying on linker-specific quoting.
pub fn exported_symbol(definition_path: &str) -> String {
    let mut symbol = String::with_capacity("__quill_export_".len() + definition_path.len() * 2);
    symbol.push_str("__quill_export_");
    for byte in definition_path.bytes() {
        use std::fmt::Write as _;
        write!(&mut symbol, "{byte:02x}").expect("writing to String cannot fail");
    }
    symbol
}

/// Resolve the unqualified function calls represented by the syntax slice.
/// Qualified paths, method dispatch, macros, and external crate exports remain
/// unresolved until their respective namespaces are modeled.
pub fn resolve_function_calls(hir: &HirCrate) -> Vec<CallResolution> {
    hir.functions
        .iter()
        .flat_map(|caller| {
            caller.calls.iter().map(move |spelling| {
                let target = resolve_call(hir, caller, spelling);
                CallResolution {
                    caller: caller.def_path.clone(),
                    spelling: spelling.clone(),
                    target,
                }
            })
        })
        .collect()
}

/// Resolve qualified calls within a crate or through its Cargo dependency
/// aliases. `dependency_aliases` maps caller package name -> Rust crate alias
/// -> dependency package name. External crates without supplied HIR remain
/// unresolved, allowing a later metadata/interface query to fill them in.
pub fn resolve_workspace_calls(
    crates: &[HirCrate],
    dependency_aliases: &HashMap<String, HashMap<String, String>>,
) -> Vec<CallResolution> {
    let crates_by_name: HashMap<_, _> = crates.iter().map(|hir| (hir.name.as_str(), hir)).collect();
    let mut resolutions = Vec::new();
    for hir in crates {
        let aliases = dependency_aliases.get(&hir.name);
        for caller in &hir.functions {
            for spelling in &caller.qualified_calls {
                let target =
                    resolve_workspace_call(hir, caller, spelling, aliases, &crates_by_name);
                resolutions.push(CallResolution {
                    caller: caller.def_path.clone(),
                    spelling: spelling.clone(),
                    target,
                });
            }
        }
    }
    resolutions
}

fn resolve_workspace_call(
    hir: &HirCrate,
    caller: &Function,
    spelling: &str,
    aliases: Option<&HashMap<String, String>>,
    crates_by_name: &HashMap<&str, &HirCrate>,
) -> CallTarget {
    let parts: Vec<_> = spelling.split("::").collect();
    let Some((first, rest)) = parts.split_first() else {
        return CallTarget::Unresolved;
    };
    if let Some(dependency_name) = aliases.and_then(|aliases| aliases.get(*first)) {
        let Some(dependency) = crates_by_name.get(dependency_name.as_str()) else {
            return CallTarget::Unresolved;
        };
        let path: Vec<_> = rest.iter().map(|part| (*part).to_owned()).collect();
        return resolve_path_in_crate(dependency, &path, true);
    }
    let normalized = match *first {
        "crate" => rest.to_vec(),
        "self" => {
            let mut base = module_scope(&caller.def_path.scope);
            base.extend(rest.iter().map(|part| (*part).to_owned()));
            return resolve_path_in_crate(hir, &base, false);
        }
        "super" => {
            let mut base = module_scope(&caller.def_path.scope);
            let mut remaining = parts.as_slice();
            while remaining.first() == Some(&"super") {
                base.pop();
                remaining = &remaining[1..];
            }
            base.extend(remaining.iter().map(|part| (*part).to_owned()));
            return resolve_path_in_crate(hir, &base, false);
        }
        _ => parts,
    };
    let path: Vec<_> = normalized.into_iter().map(str::to_owned).collect();
    resolve_path_in_crate(hir, &path, false)
}

fn resolve_path_in_crate(hir: &HirCrate, path: &[String], require_public: bool) -> CallTarget {
    let Some((name, module_path)) = path.split_last() else {
        return CallTarget::Unresolved;
    };
    let candidates: BTreeSet<_> = hir
        .functions
        .iter()
        .filter(|function| {
            function.def_path.name == *name
                && module_scope(&function.def_path.scope) == module_path
                && (!require_public || exported_function(hir, function))
        })
        .map(|function| function.def_path.clone())
        .collect();
    into_call_target(candidates)
}

fn exported_function(hir: &HirCrate, function: &Function) -> bool {
    if !function.externally_visible {
        return false;
    }
    let scope = module_scope(&function.def_path.scope);
    (1..=scope.len()).all(|length| {
        hir.modules
            .iter()
            .any(|module| module.path == scope[..length] && module.externally_visible)
    })
}

fn resolve_call(hir: &HirCrate, caller: &Function, spelling: &str) -> CallTarget {
    let active_imports: Vec<_> = hir
        .imports
        .iter()
        .filter(|import| {
            scope_starts_with(&caller.def_path.scope, &import.scope)
                && module_scope(&import.scope) == module_scope(&caller.def_path.scope)
        })
        .collect();
    let named_imports: Vec<_> = active_imports
        .iter()
        .copied()
        .filter(|import| !import.glob && import_binding_name(import) == spelling)
        .collect();
    if !named_imports.is_empty() {
        let mut candidates = BTreeSet::new();
        for import in named_imports {
            if let Some((module_path, name)) = normalize_import_path(&import.scope, &import.path) {
                candidates.extend(
                    hir.functions
                        .iter()
                        .filter(|function| {
                            function.def_path.name == name
                                && module_scope(&function.def_path.scope) == module_path
                        })
                        .map(|function| function.def_path.clone()),
                );
            }
        }
        return into_call_target(candidates);
    }

    // A glob may introduce this identifier from another crate or module. Keep
    // it unresolved rather than guessing that a same-named local item wins.
    if active_imports.iter().any(|import| import.glob) {
        return CallTarget::Unresolved;
    }

    let mut current_module = module_scope(&caller.def_path.scope);
    loop {
        let candidates: BTreeSet<_> = hir
            .functions
            .iter()
            .filter(|function| {
                function.def_path.name == spelling
                    && module_scope(&function.def_path.scope) == current_module
                    && function
                        .def_path
                        .scope
                        .iter()
                        .all(|part| part.starts_with("mod:"))
            })
            .map(|function| function.def_path.clone())
            .collect();
        if !candidates.is_empty() {
            return into_call_target(candidates);
        }
        if current_module.pop().is_none() {
            return CallTarget::Unresolved;
        }
    }
}

fn scope_starts_with(scope: &[String], prefix: &[String]) -> bool {
    scope.starts_with(prefix)
}

fn module_scope(scope: &[String]) -> Vec<String> {
    scope
        .iter()
        .filter_map(|part| part.strip_prefix("mod:").map(str::to_owned))
        .collect()
}

fn import_binding_name(import: &Import) -> &str {
    import
        .alias
        .as_deref()
        .or_else(|| import.path.last().map(String::as_str))
        .unwrap_or("")
}

fn normalize_import_path(scope: &[String], path: &[String]) -> Option<(Vec<String>, String)> {
    let (first, rest) = path.split_first()?;
    let module_path = match first.as_str() {
        "crate" => rest[..rest.len().saturating_sub(1)].to_vec(),
        "self" => {
            let mut base = module_scope(scope);
            base.extend_from_slice(&rest[..rest.len().saturating_sub(1)]);
            base
        }
        "super" => {
            let mut base = module_scope(scope);
            let mut remaining = rest;
            base.pop()?;
            while remaining.first().is_some_and(|part| part == "super") {
                base.pop()?;
                remaining = &remaining[1..];
            }
            base.extend_from_slice(&remaining[..remaining.len().saturating_sub(1)]);
            base
        }
        _ => path[..path.len().saturating_sub(1)].to_vec(),
    };
    Some((module_path, path.last()?.clone()))
}

fn into_call_target(candidates: BTreeSet<DefPath>) -> CallTarget {
    let mut candidates = candidates.into_iter();
    let Some(first) = candidates.next() else {
        return CallTarget::Unresolved;
    };
    let Some(second) = candidates.next() else {
        return CallTarget::Unique(first);
    };
    CallTarget::Ambiguous(
        std::iter::once(first)
            .chain([second])
            .chain(candidates)
            .collect(),
    )
}

/// Prototype reachability for recognized same-crate calls. It is not safe to
/// omit emitted functions yet: name resolution, function pointers, macro
/// expansion, and complete trait dispatch are not modeled. Any eventual use
/// must happen after type and borrow checking has visited every body.
pub fn codegen_reachable(hir: &HirCrate, explicit_roots: &[DefPath]) -> BTreeSet<DefPath> {
    let by_path: HashMap<_, _> = hir
        .functions
        .iter()
        .map(|function| (function.def_path.clone(), function))
        .collect();
    let by_name: HashMap<&str, Vec<&Function>> = {
        let mut index: HashMap<&str, Vec<&Function>> = HashMap::new();
        for function in &hir.functions {
            index
                .entry(function.def_path.name.as_str())
                .or_default()
                .push(function);
        }
        index
    };
    let mut reachable = BTreeSet::new();
    let mut pending = VecDeque::new();
    for function in hir
        .functions
        .iter()
        .filter(|function| {
            function.externally_visible
                || function
                    .def_path
                    .scope
                    .iter()
                    .any(|scope| scope.starts_with("impl:") || scope.starts_with("trait:"))
        })
        .chain(
            explicit_roots
                .iter()
                .filter_map(|root| by_path.get(root).copied()),
        )
    {
        if reachable.insert(function.def_path.clone()) {
            pending.push_back(function.def_path.clone());
        }
    }
    while let Some(path) = pending.pop_front() {
        let Some(function) = by_path.get(&path) else {
            continue;
        };
        for called_name in &function.calls {
            if let Some(candidates) = by_name.get(called_name.as_str()) {
                for candidate in candidates {
                    if reachable.insert(candidate.def_path.clone()) {
                        pending.push_back(candidate.def_path.clone());
                    }
                }
            }
        }
    }
    reachable
}

#[cfg(test)]
mod tests {
    use super::*;
    use quill_source::SourceFile;
    use quill_syntax::parse;

    fn lowered(source: &str) -> HirCrate {
        let file = SourceFile::from_text("sample.rs", source);
        let syntax = parse(&file);
        lower_functions("sample", &syntax.functions)
    }

    #[test]
    fn unrelated_items_do_not_change_function_identity() {
        let before = lowered("fn target() {}\n");
        let after = lowered("fn unrelated() {}\nfn target() {}\n");
        assert_eq!(before.functions[0].def_path, after.functions[1].def_path);
    }

    #[test]
    fn scope_and_generic_state_are_part_of_hir() {
        let hir = lowered("mod api { pub fn map<T>(x: T) { } } impl Thing { fn run() {} }");
        let map = hir
            .functions
            .iter()
            .find(|f| f.def_path.name == "map")
            .unwrap();
        assert_eq!(map.def_path.scope, ["mod:api"]);
        assert!(map.externally_visible);
        assert!(map.generic);
        let run = hir
            .functions
            .iter()
            .find(|f| f.def_path.name == "run")
            .unwrap();
        assert_eq!(run.def_path.scope, ["impl:Thing"]);
    }

    #[test]
    fn lowers_struct_definitions_fields_and_associated_methods() {
        let file = SourceFile::from_text(
            "sample.rs",
            "pub mod model { pub struct Pair<T> { pub first: T, second: i32 } impl<T> Pair<T> { pub fn first(&self) -> &T { &self.first } } }",
        );
        let syntax = parse(&file);
        let hir = lower_parses("sample", &[syntax]);
        let pair = hir
            .structs
            .iter()
            .find(|item| item.def_path.name == "Pair")
            .unwrap();
        assert_eq!(pair.def_path.scope, ["mod:model"]);
        assert!(pair.externally_visible);
        assert_eq!(pair.generic_parameters, ["T"]);
        assert_eq!(pair.fields.len(), 2);
        assert_eq!(pair.fields[0].name.as_deref(), Some("first"));
        assert!(pair.fields[0].externally_visible);
        assert_eq!(pair.fields[1].type_text, "i32");
        assert!(!pair.fields[1].externally_visible);
        let method = hir
            .functions
            .iter()
            .find(|function| function.def_path.name == "first")
            .unwrap();
        assert_eq!(method.def_path.scope, ["mod:model", "impl:Pair"]);
        assert!(method.generic);
        assert_eq!(method.signature.generic_parameters, ["T"]);
    }

    #[test]
    fn computes_nested_native_struct_field_offsets() {
        let source = SourceFile::from_text(
            "layout.rs",
            "struct Inner { tag: u8, value: u64 } struct Outer { prefix: u32, inner: Inner, text: String, tail: u8 }",
        );
        let hir = lower_parses("layout", &[parse(&source)]);
        let outer = hir
            .structs
            .iter()
            .find(|item| item.def_path.name == "Outer")
            .unwrap();
        let layout = layout_of_struct(&hir, &outer.def_path).unwrap();
        // rustc +nightly-2026-08-23 -Zprint-type-sizes reports `text` at 0,
        // `inner` at 24, `prefix` at 40, and `tail` at 44 for this repr(Rust)
        // type. Offsets remain indexed by source field order in HIR.
        assert_eq!(layout.field_offsets, [40, 24, 0, 44]);
        assert_eq!(layout.align, 8);
        assert_eq!(layout.size, 48);
        let inner = hir
            .structs
            .iter()
            .find(|item| item.def_path.name == "Inner")
            .unwrap();
        let inner_layout = layout_of_struct(&hir, &inner.def_path).unwrap();
        assert_eq!(inner_layout.field_offsets, [8, 0]);
        assert_eq!(inner_layout.size, 16);
    }

    #[test]
    fn rejects_recursive_by_value_layouts() {
        let source = SourceFile::from_text("recursive.rs", "struct Node { next: Node }");
        let hir = lower_parses("recursive", &[parse(&source)]);
        let node = &hir.structs[0];
        assert_eq!(
            layout_of_struct(&hir, &node.def_path),
            Err(LayoutError::RecursiveByValue("Node".to_owned()))
        );
    }

    #[test]
    fn repr_c_layout_preserves_field_order() {
        let source = SourceFile::from_text(
            "repr_c.rs",
            "#[repr(C)] struct C { tag: u8, value: u64, tail: u8 }",
        );
        let hir = lower_parses("repr_c", &[parse(&source)]);
        let structure = &hir.structs[0];
        assert!(structure.repr_c);
        let layout = layout_of_struct(&hir, &structure.def_path).unwrap();
        assert_eq!(layout.field_offsets, [0, 8, 16]);
        assert_eq!(layout.size, 24);
    }

    #[test]
    fn rejects_unimplemented_explicit_representations() {
        let source =
            SourceFile::from_text("packed.rs", "#[repr(packed)] struct Packed { value: u64 }");
        let hir = lower_parses("packed", &[parse(&source)]);
        assert_eq!(
            layout_of_struct(&hir, &hir.structs[0].def_path),
            Err(LayoutError::UnsupportedRepresentation(
                "packed::Packed".to_owned()
            ))
        );
    }

    #[test]
    fn crate_lowering_joins_module_and_import_metadata_across_files() {
        let root = parse(&SourceFile::from_text(
            "lib.rs",
            "pub mod api; use api::serve as run;",
        ));
        let api = parse(&SourceFile::from_text(
            "api.rs",
            "pub fn serve() {} fn helper() {}",
        ));
        let hir = lower_parses("sample", &[root, api]);
        assert_eq!(hir.modules.len(), 1);
        assert_eq!(hir.modules[0].path, ["api"]);
        assert!(hir.modules[0].external);
        assert_eq!(hir.imports.len(), 1);
        assert_eq!(hir.imports[0].path, ["api", "serve"]);
        assert_eq!(hir.imports[0].alias.as_deref(), Some("run"));
        assert_eq!(hir.functions.len(), 2);
        assert!(
            hir.functions
                .iter()
                .all(|function| function.def_path.crate_name == "sample")
        );
    }

    #[test]
    fn module_file_paths_keep_sibling_definitions_distinct() {
        let first = parse(&SourceFile::from_text("left.rs", "pub fn run() {}"));
        let second = parse(&SourceFile::from_text("right.rs", "pub fn run() {}"));
        let hir = lower_module_files(
            "sample",
            &[(vec!["left".into()], first), (vec!["right".into()], second)],
        );
        assert_eq!(hir.functions[0].def_path.scope, ["mod:left"]);
        assert_eq!(hir.functions[1].def_path.scope, ["mod:right"]);
        assert_ne!(hir.functions[0].def_path, hir.functions[1].def_path);
    }

    #[test]
    fn call_resolution_respects_import_aliases_and_module_scope() {
        let source = SourceFile::from_text(
            "lib.rs",
            "mod api { pub fn serve() {} } use api::serve as run; fn entry() { run(); missing(); }",
        );
        let syntax = parse(&source);
        let hir = lower_parses("sample", &[syntax]);
        let entry = hir
            .functions
            .iter()
            .find(|function| function.def_path.name == "entry")
            .unwrap();
        let calls: Vec<_> = resolve_function_calls(&hir)
            .into_iter()
            .filter(|call| call.caller == entry.def_path)
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].spelling, "run");
        assert!(
            matches!(calls[0].target, CallTarget::Unique(ref target) if target.name == "serve")
        );
        assert_eq!(calls[1].spelling, "missing");
        assert_eq!(calls[1].target, CallTarget::Unresolved);
    }

    #[test]
    fn call_resolution_prefers_the_nearest_module_definition() {
        let source = SourceFile::from_text(
            "lib.rs",
            "fn pick() {} mod child { fn pick() {} fn use_child() { pick(); } }",
        );
        let hir = lower_parses("sample", &[parse(&source)]);
        let caller = hir
            .functions
            .iter()
            .find(|function| function.def_path.name == "use_child")
            .unwrap();
        let resolution = resolve_function_calls(&hir)
            .into_iter()
            .find(|call| call.caller == caller.def_path)
            .unwrap();
        assert!(
            matches!(resolution.target, CallTarget::Unique(ref target) if target.scope == ["mod:child"])
        );
    }

    #[test]
    fn workspace_calls_follow_cargo_aliases_and_require_exported_items() {
        let model_root = parse(&SourceFile::from_text("model.rs", "pub mod api;"));
        let model_api = parse(&SourceFile::from_text(
            "api.rs",
            "pub fn serve(value: i32) -> bool { true } fn private() {}",
        ));
        let model = lower_module_files(
            "catalog-model",
            &[(Vec::new(), model_root), (vec!["api".into()], model_api)],
        );
        let app_source = SourceFile::from_text(
            "app.rs",
            "fn entry() { model_alias::api::serve(); model_alias::api::private(); }",
        );
        let app = lower_parses("catalog-app", &[parse(&app_source)]);
        let crates = [app, model];
        let aliases = HashMap::from([(
            "catalog-app".to_owned(),
            HashMap::from([("model_alias".to_owned(), "catalog-model".to_owned())]),
        )]);
        let calls = resolve_workspace_calls(&crates, &aliases);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].spelling, "model_alias::api::serve");
        assert!(
            matches!(calls[0].target, CallTarget::Unique(ref target) if target.name == "serve")
        );
        assert_eq!(calls[1].spelling, "model_alias::api::private");
        assert_eq!(calls[1].target, CallTarget::Unresolved);
        let interface = exported_functions(&crates[1]);
        let serve = interface
            .iter()
            .find(|function| function.path == "api::serve")
            .unwrap();
        assert_eq!(serve.signature.parameter_types, ["i32"]);
        assert_eq!(serve.signature.return_type.as_deref(), Some("bool"));
        assert_eq!(serve.symbol, exported_symbol("catalog-model::api::serve"));
        assert!(
            !interface
                .iter()
                .any(|function| function.path == "api::private")
        );
        let metadata = exported_interface(&crates[1]);
        assert_eq!(metadata.format_version, 1);
        let encoded = serde_json::to_vec(&metadata).unwrap();
        assert_eq!(
            serde_json::from_slice::<CrateInterface>(&encoded).unwrap(),
            metadata
        );
    }

    #[test]
    fn crate_interfaces_include_public_inherent_methods_with_owner_paths() {
        let source = SourceFile::from_text(
            "record.rs",
            "pub struct Record { pub revision: u64 } impl Record { pub fn new(revision: u64) -> Self { Self { revision } } pub fn label(self, value: &str) -> Self { self } fn private(self) {} }",
        );
        let hir = lower_parses("catalog", &[parse(&source)]);
        let exports = exported_functions(&hir);
        assert!(exports.iter().any(|item| item.path == "Record::new"));
        assert!(exports.iter().any(|item| item.path == "Record::label"));
        assert!(!exports.iter().any(|item| item.path == "Record::private"));
    }

    #[test]
    fn private_dead_functions_are_omitted_only_from_codegen_reachability() {
        let hir = lowered("fn main() { if condition() { live(); } } fn live() {} fn dead() {}");
        assert_eq!(
            hir.functions.len(),
            3,
            "all bodies remain in HIR for checking"
        );
        let main = hir
            .functions
            .iter()
            .find(|function| function.def_path.name == "main")
            .unwrap();
        let reachable = codegen_reachable(&hir, std::slice::from_ref(&main.def_path));
        let names: Vec<_> = reachable.iter().map(|path| path.name.as_str()).collect();
        assert_eq!(names, ["live", "main"]);
        assert!(!reachable.iter().any(|path| path.name == "dead"));
    }

    #[test]
    fn monomorphization_requests_share_definitions_and_deduplicate_instances() {
        let definition = DefPath {
            crate_name: "sample".to_owned(),
            scope: vec![],
            name: "identity".to_owned(),
            disambiguator: 0,
        };
        let int_instance = MonoInstanceKey {
            definition: definition.clone(),
            type_arguments: vec![TypeKey::Integer {
                bits: 32,
                signed: true,
            }],
            const_arguments: vec![],
        };
        let bool_instance = MonoInstanceKey {
            definition,
            type_arguments: vec![TypeKey::Bool],
            const_arguments: vec![],
        };
        let mut queue = MonomorphizationQueue::default();
        assert!(queue.request(int_instance.clone()));
        assert!(!queue.request(int_instance));
        assert!(queue.request(bool_instance));
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn trait_and_impl_methods_are_kept_as_conservative_roots() {
        let hir =
            lowered("fn dead() {} impl Thing { fn method() {} } trait Api { fn provided() {} }");
        let reachable = codegen_reachable(&hir, &[]);
        assert!(reachable.iter().any(|path| path.name == "method"));
        assert!(reachable.iter().any(|path| path.name == "provided"));
        assert!(!reachable.iter().any(|path| path.name == "dead"));
    }
}
