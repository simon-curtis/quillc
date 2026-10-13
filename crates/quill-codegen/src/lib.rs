use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;

use object::write::{Object, Relocation, StandardSection, Symbol, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, RelocationEncoding, RelocationFlags, RelocationKind,
    SymbolFlags, SymbolKind, SymbolScope,
};
use quill_hir::{codegen_reachable, lower_functions};
use quill_syntax::{
    BinaryOperator, BodyExpr, BodyExprKind, BodyStatement, ClosureParameter, FunctionItem,
    LiteralKind, MatchPattern, Parse, StructItem,
};
use serde::Serialize;

use abi::{ResultEncoding, ResultLayout};

pub mod abi;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodegenError {
    ParseErrors,
    UnsupportedTarget,
    UnsupportedProgram(&'static str),
    UnsupportedExpression(String),
    UnsupportedFunction {
        function: String,
        reason: Box<CodegenError>,
    },
    UnsupportedSignature(String),
    UnresolvedCall(String),
    ObjectWrite(String),
}

/// A callable declaration imported from another Quill crate unit. `call_path`
/// is the name used in the importing source after Cargo alias resolution;
/// `symbol` comes from the dependency's exported interface metadata.
#[derive(Clone, Debug, Serialize)]
pub struct ExternalFunctionImport {
    pub declaration: FunctionItem,
    pub symbol: String,
}

impl fmt::Display for CodegenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParseErrors => write!(formatter, "source has syntax errors"),
            Self::UnsupportedTarget => write!(formatter, "target must be x86_64 Linux"),
            Self::UnsupportedProgram(reason) => write!(formatter, "unsupported program: {reason}"),
            Self::UnsupportedExpression(expression) => {
                write!(formatter, "unsupported expression: {expression}")
            }
            Self::UnsupportedFunction { function, reason } => {
                write!(formatter, "in function `{function}`: {reason}")
            }
            Self::UnsupportedSignature(signature) => {
                write!(formatter, "unsupported helper signature: {signature}")
            }
            Self::UnresolvedCall(name) => write!(formatter, "unresolved call target: `{name}`"),
            Self::ObjectWrite(error) => write!(formatter, "could not write ELF object: {error}"),
        }
    }
}
impl std::error::Error for CodegenError {}

#[derive(Clone, Debug)]
enum Value {
    Integer(i32),
    Word(u64),
    TypedWord {
        value: u64,
        type_name: String,
    },
    EmptySlice,
    NicheOk,
    EmptyCollection,
    TextSlice(String),
    StaticArray(Vec<Value>),
    OwnedString(Box<Value>),
    FormatString(Vec<FormatPiece>),
    SliceParameter {
        pointer_slot: usize,
        length_slot: usize,
        element_widths: Vec<usize>,
    },
    SliceLocal {
        pointer_slot: usize,
        length_slot: usize,
        element_widths: Vec<usize>,
    },
    SliceRecord {
        pointer_slot: usize,
        length_slot: usize,
        record_type: String,
    },
    CharIterator {
        pointer_slot: usize,
        length_slot: usize,
    },
    CharIsAsciiAlphanumeric(Box<Value>),
    AnyStringTrimEmpty {
        pointer_slot: usize,
        length_slot: usize,
    },
    SliceElement {
        pointer_slot: usize,
        index_slot: usize,
        element_size: usize,
        element_offset: usize,
        element_stride: usize,
    },
    SliceElementString {
        pointer_slot: usize,
        index_slot: usize,
        element_offset: usize,
        element_stride: usize,
    },
    BTreeMapGetString {
        pointer_slot: usize,
        length_slot: usize,
        key: Box<Value>,
    },
    CollectionLocal {
        pointer_slot: usize,
        length_slot: usize,
        capacity_slot: usize,
        kind: CollectionKind,
        element_type: Option<String>,
        indirect: bool,
    },
    CollectionAddress {
        pointer_slot: usize,
        length_slot: usize,
        capacity_slot: usize,
        indirect: bool,
    },
    Parameter(usize),
    TypedWordParameter {
        slot: usize,
        type_name: String,
    },
    Local(usize),
    WordLocal(usize),
    TypedWordLocal {
        slot: usize,
        type_name: String,
    },
    AddressOfLocal(usize),
    IndirectLocal {
        pointer_slot: usize,
        is_word: bool,
    },
    Tuple(Vec<Value>),
    PairField {
        aggregate: Box<Value>,
        index: usize,
    },
    StructLocal {
        type_name: String,
        fields: HashMap<String, StructLocalField>,
    },
    StructReference {
        type_name: String,
        fields: Vec<Value>,
    },
    SliceIsEmpty(Box<Value>),
    SliceLength(Box<Value>),
    ZeroExtend(Box<Value>),
    SignExtend(Box<Value>),
    SliceFold {
        source: Box<Value>,
        initial: Box<Value>,
        update: Box<Value>,
        element_size: usize,
        pointer_slot: usize,
        length_slot: usize,
        index_slot: usize,
        accumulator_slot: usize,
        item_slot: usize,
    },
    TupleSliceFold {
        source: Box<Value>,
        initial: Vec<Value>,
        updates: Vec<Value>,
        element_size: usize,
        pointer_slot: usize,
        length_slot: usize,
        index_slot: usize,
        accumulator_slots: Vec<usize>,
        update_slots: Vec<usize>,
        item_slot: usize,
        record_fields: Vec<(usize, Value, usize)>,
    },
    Block {
        statements: Vec<Statement>,
        tail: Box<Value>,
    },
    Binary {
        operator: BinaryOperator,
        left: Box<Value>,
        right: Box<Value>,
    },
    WrappingBinary {
        operator: BinaryOperator,
        left: Box<Value>,
        right: Box<Value>,
    },
    Negate(Box<Value>),
    Call {
        name: String,
        arguments: Vec<Value>,
        returns_word: bool,
        word_type: Option<String>,
        returns_pair: bool,
        returns_slice: bool,
        returns_aggregate: bool,
        aggregate_destination: Option<usize>,
    },
    ResultIsOk(Box<Value>),
    OptionIsSome(Box<Value>),
    OptionUnwrapOr {
        option: Box<Value>,
        fallback: Box<Value>,
        success_on_zero: bool,
        payload_is_word: bool,
    },
    If {
        condition: Condition,
        then_value: Box<Value>,
        else_value: Box<Value>,
    },
}

#[derive(Clone, Debug)]
enum StructLocalField {
    Scalar {
        slot: usize,
        is_word: bool,
        type_name: Option<String>,
    },
    IndirectScalar {
        pointer_slot: usize,
        is_word: bool,
    },
    Slice {
        pointer_slot: usize,
        length_slot: usize,
    },
    Collection {
        pointer_slot: usize,
        length_slot: usize,
        capacity_slot: usize,
        kind: CollectionKind,
        element_type: Option<String>,
        indirect: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CollectionKind {
    Vector,
    BTreeMap,
    HashMap,
    StringBuffer,
}

#[derive(Clone, Debug)]
enum Condition {
    Boolean(bool),
    Compare {
        operator: BinaryOperator,
        left: Box<Value>,
        right: Box<Value>,
    },
    And(Box<Condition>, Box<Condition>),
    Or(Box<Condition>, Box<Condition>),
}

#[derive(Clone, Debug)]
enum FormatPiece {
    Text(String),
    Decimal(Value),
    SignedDecimal(Value),
}

#[derive(Clone, Debug)]
enum Statement {
    Evaluate(Value),
    Println(Vec<PrintPart>),
    Let {
        slot: usize,
        initializer: Value,
    },
    LetSlice {
        pointer_slot: usize,
        length_slot: usize,
        initializer: Value,
    },
    LetStruct {
        fields: Vec<(usize, Value)>,
    },
    LetTupleCall {
        fields: Vec<(usize, bool)>,
        initializer: Value,
    },
    Assign {
        slot: usize,
        value: Value,
    },
    AssignIndirect {
        pointer_slot: usize,
        is_word: bool,
        value: Value,
    },
    VecPushString {
        pointer_slot: usize,
        length_slot: usize,
        capacity_slot: usize,
        value: Value,
        indirect: bool,
    },
    VecPushAggregate {
        pointer_slot: usize,
        length_slot: usize,
        capacity_slot: usize,
        fields: Vec<Value>,
        widths: Vec<usize>,
        indirect: bool,
    },
    BTreeMapInsert {
        pointer_slot: usize,
        length_slot: usize,
        capacity_slot: usize,
        key: Value,
        value: Value,
        indirect: bool,
    },
    StringAppendChar {
        pointer_slot: usize,
        length_slot: usize,
        capacity_slot: usize,
        character: Value,
        lowercase: bool,
    },
    If {
        condition: Condition,
        then_body: Vec<Statement>,
        else_body: Vec<Statement>,
    },
    While {
        condition: Condition,
        body: Vec<Statement>,
    },
    ForRange {
        slot: usize,
        end_slot: usize,
        inclusive: bool,
        body: Vec<Statement>,
    },
    ForSlice {
        length_slot: usize,
        index_slot: usize,
        body: Vec<Statement>,
    },
    ForChars {
        pointer_slot: usize,
        length_slot: usize,
        index_slot: usize,
        item_slot: usize,
        body: Vec<Statement>,
    },
    Break,
    Continue,
    TryResult(Value),
    TryOption(Value),
    TryOptionBind {
        result: Value,
        slot: usize,
        success_on_zero: bool,
        payload_is_word: bool,
    },
    Return(Value),
}

#[derive(Clone, Debug)]
enum PrintPart {
    Text(String),
    Panic(String),
    PanicText(String),
    PanicDecimal(Value),
    Slice(Value),
    Decimal(Value),
    Hex16(Value),
}

#[derive(Clone, Debug)]
struct PendingRelocation {
    offset: u64,
    target: String,
}

/// Emit an ELF object for the current compiler-owned x86_64 vertical slice.
/// Functions unreachable from `main` are omitted from this closed executable
/// after all bodies are validated; `pub` does not create an external caller.
pub fn emit_subset_main_object(parse: &Parse) -> Result<Vec<u8>, CodegenError> {
    emit_subset_object(parse, true, &[])
}

pub fn emit_subset_main_object_with_imports(
    parse: &Parse,
    imports: &[ExternalFunctionImport],
) -> Result<Vec<u8>, CodegenError> {
    let combined = parse_with_imports(parse, imports)?;
    emit_subset_object(&combined, true, imports)
}

/// Emit an ELF relocatable object for the public functions in a closed library
/// unit. Public functions receive stable external symbols; private helpers are
/// retained only when reached from those exports.
pub fn emit_subset_library_object(parse: &Parse) -> Result<Vec<u8>, CodegenError> {
    emit_subset_library_object_with_imports(parse, &[])
}

pub fn emit_subset_library_object_with_imports(
    parse: &Parse,
    imports: &[ExternalFunctionImport],
) -> Result<Vec<u8>, CodegenError> {
    let combined = parse_with_imports(parse, imports)?;
    emit_subset_object(&combined, false, imports)
}

fn parse_with_imports(
    parse: &Parse,
    imports: &[ExternalFunctionImport],
) -> Result<Parse, CodegenError> {
    let mut combined = parse.clone();
    for import in imports {
        let is_generic_definition =
            import.declaration.is_generic && import.declaration.body.is_some();
        if (import.declaration.body.is_some() && !is_generic_definition)
            || (import.declaration.is_generic && !is_generic_definition)
        {
            return Err(CodegenError::UnsupportedProgram(
                "external imports must be nongeneric declarations or generic definitions with bodies",
            ));
        }
        if combined
            .functions
            .iter()
            .any(|function| function.name == import.declaration.name)
        {
            return Err(CodegenError::UnsupportedProgram(
                "external function import duplicates a local function path",
            ));
        }
        combined.functions.push(import.declaration.clone());
    }
    Ok(combined)
}

fn emit_subset_object(
    parse: &Parse,
    executable: bool,
    imports: &[ExternalFunctionImport],
) -> Result<Vec<u8>, CodegenError> {
    if cfg!(not(all(target_arch = "x86_64", target_os = "linux"))) {
        return Err(CodegenError::UnsupportedTarget);
    }
    if parse.had_error {
        return Err(CodegenError::ParseErrors);
    }
    if parse.unsupported_item_count != 0 || !parse.modules.is_empty() || !parse.imports.is_empty() {
        return Err(CodegenError::UnsupportedProgram(
            "only top-level function items are supported",
        ));
    }
    if parse.functions.is_empty() {
        return Err(CodegenError::UnsupportedProgram(if executable {
            "expected a `main` function"
        } else {
            "expected at least one public library function"
        }));
    }
    let mut by_name = HashMap::with_capacity(parse.functions.len());
    for (index, function) in parse.functions.iter().enumerate() {
        if by_name.insert(function.name.as_str(), index).is_some() {
            return Err(CodegenError::UnsupportedProgram(
                "duplicate function names are outside this codegen slice",
            ));
        }
        if function.name == "main" {
            validate_function_signature(function, true, &parse.structs)?;
        }
    }
    let main_index = if executable {
        Some(*by_name.get("main").ok_or(CodegenError::UnsupportedProgram(
            "expected a `main` function",
        ))?)
    } else {
        None
    };
    let exit_value = if let Some(main_index) = main_index {
        validate_reachable_struct_expressions(parse, &HashSet::from([main_index]))?;
        let value = main_exit_value(
            &parse.functions[main_index],
            &parse.functions,
            &parse.structs,
        )?;
        validate_value_target(&value, &by_name, &parse.functions, 0)?;
        value
    } else {
        Value::Integer(0)
    };

    let mut hir = lower_functions("quill_input", &parse.functions);
    // This API emits an executable, not a library with externally callable
    // symbols. All semantic bodies were validated above; only `main` and its
    // transitive call graph need backend work.
    for function in &mut hir.functions {
        function.externally_visible = !executable && function.externally_visible;
    }
    let roots = if let Some(main_index) = main_index {
        vec![hir.functions[main_index].def_path.clone()]
    } else {
        hir.functions
            .iter()
            .filter(|function| function.externally_visible)
            .map(|function| function.def_path.clone())
            .collect::<Vec<_>>()
    };
    if roots.is_empty() {
        return Err(CodegenError::UnsupportedProgram(
            "library object requires at least one public function",
        ));
    }
    let reachable = codegen_reachable(&hir, &roots);
    let reachable_indices: HashSet<_> = hir
        .functions
        .iter()
        .enumerate()
        .filter(|(_, function)| reachable.contains(&function.def_path))
        .map(|(index, _)| index)
        .collect();
    // Struct-expression lowering restrictions are backend concerns. Apply
    // them only to functions that will be emitted; semantic checking still
    // belongs before this API and must visit every body, including dead ones.
    let mut reachable_helpers: Vec<_> = reachable_indices
        .iter()
        .copied()
        .filter(|index| Some(*index) != main_index && parse.functions[*index].body.is_some())
        .collect();
    reachable_helpers.sort_unstable();
    for index in &reachable_helpers {
        let function = normalize_impl_function(&parse.functions[*index]);
        validate_function_signature(&function, false, &parse.structs)?;
    }
    let reachable_helper_set: HashSet<_> = reachable_helpers.into_iter().collect();
    validate_reachable_struct_expressions(parse, &reachable_helper_set)?;
    // Semantic checking visits every body before codegen. The backend resolves
    // call targets only for functions retained by the reachability walk.
    for index in &reachable_indices {
        if let Some(body) = &parse.functions[*index].body {
            validate_call_targets(body, &by_name)?;
        }
    }
    let mut helper_indices = Vec::with_capacity(reachable_indices.len());
    for (index, function) in parse.functions.iter().enumerate() {
        if Some(index) == main_index
            || !reachable_indices.contains(&index)
            || function.is_generic
            || function.body.is_none()
        {
            continue;
        }
        helper_indices.push(index);
    }
    let lower_helper = |index: &usize| -> Result<(usize, FunctionItem, Value), CodegenError> {
        let function = normalize_impl_function(&parse.functions[*index]);
        let value =
            helper_return_value(&function, &parse.functions, &parse.structs).map_err(|reason| {
                let name = if function.scope.is_empty() {
                    function.name.clone()
                } else {
                    format!("{}::{}", function.scope.join("::"), function.name)
                };
                CodegenError::UnsupportedFunction {
                    function: name,
                    reason: Box::new(reason),
                }
            })?;
        Ok((*index, function, value))
    };
    let codegen_pool = (helper_indices.len() > 1)
        .then(|| {
            quill_parallel::WorkPool::for_tasks_with_override(
                helper_indices.len(),
                Some("QUILL_CODEGEN_JOBS"),
            )
            .ok()
        })
        .flatten()
        .filter(|pool| pool.worker_count() > 1);
    let lowered_helpers = if let Some(pool) = codegen_pool {
        pool.map(&helper_indices, lower_helper)
    } else {
        helper_indices.iter().map(&lower_helper).collect()
    };
    let mut helper_values = lowered_helpers.into_iter().collect::<Result<Vec<_>, _>>()?;

    // A generic definition is lowered once per concrete call-site type key.
    // The queue is seeded from reachable, already-lowered callers and grows
    // as each instance reveals further generic calls in its body.
    let mut pending_instances = BTreeSet::new();
    collect_generic_instances(&exit_value, &mut pending_instances);
    for (_, _, value) in &helper_values {
        collect_generic_instances(value, &mut pending_instances);
    }
    let mut emitted_instances = BTreeSet::new();
    while let Some(instance_name) = pending_instances.iter().next().cloned() {
        pending_instances.remove(&instance_name);
        if !emitted_instances.insert(instance_name.clone()) {
            continue;
        }
        let (index, type_arguments) = parse_generic_instance_name(&instance_name).ok_or(
            CodegenError::UnsupportedProgram("invalid generic instance key"),
        )?;
        let original = parse
            .functions
            .get(index)
            .ok_or(CodegenError::UnsupportedProgram(
                "generic instance refers to an unknown function",
            ))?;
        if !original.is_generic || type_arguments.len() != original.generic_parameters.len() {
            return Err(CodegenError::UnsupportedProgram(
                "generic instance does not match its definition",
            ));
        }
        let mut instance = original.clone();
        let substitutions: HashMap<_, _> = original
            .generic_parameters
            .iter()
            .zip(&type_arguments)
            .map(|(generic, concrete)| (generic.as_str(), concrete.as_str()))
            .collect();
        for parameter in &mut instance.parameters {
            parameter.type_text = substitute_generic_types(&parameter.type_text, &substitutions);
        }
        if let Some(return_type) = &mut instance.return_type {
            *return_type = substitute_generic_types(return_type, &substitutions);
        }
        if let Some(body) = &mut instance.body {
            substitute_generic_body(body, &substitutions);
        }
        instance.name = instance_name;
        instance.is_generic = false;
        instance.generic_parameters.clear();
        instance.generic_has_bounds = false;
        instance.generic_copy_parameters.clear();
        instance.generic_as_ref_str_parameters.clear();
        instance.generic_add_parameters.clear();
        let value =
            helper_return_value(&instance, &parse.functions, &parse.structs).map_err(|reason| {
                CodegenError::UnsupportedFunction {
                    function: instance.name.clone(),
                    reason: Box::new(reason),
                }
            })?;
        collect_generic_instances(&value, &mut pending_instances);
        helper_values.push((index, instance, value));
    }

    for (_, function, value) in &helper_values {
        validate_value_target(
            value,
            &by_name,
            &parse.functions,
            parameter_slot_count(&function.parameters, &parse.structs),
        )?;
    }

    let mut object = Object::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = object.section_id(StandardSection::Text);
    let data = object.section_id(StandardSection::ReadOnlyData);
    let entry_symbol = executable.then(|| {
        object.add_symbol(Symbol {
            name: b"_start".to_vec(),
            value: 0,
            size: 0,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        })
    });
    let mut helper_symbols = HashMap::new();
    for (index, function, _) in &helper_values {
        let exported = !executable
            && parse
                .functions
                .get(*index)
                .is_some_and(|item| item.is_public);
        let symbol_name = if exported {
            quill_export_symbol(&function.name)
        } else {
            parse_generic_instance_name(&function.name)
                .map(|_| function.name.clone())
                .unwrap_or_else(|| format!("__quill_fn_{index}"))
        };
        let symbol = object.add_symbol(Symbol {
            name: symbol_name.as_bytes().to_vec(),
            value: 0,
            size: 0,
            kind: SymbolKind::Text,
            scope: if exported {
                SymbolScope::Linkage
            } else {
                SymbolScope::Compilation
            },
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        });
        helper_symbols.insert(function.name.clone(), symbol);
    }
    let mut imported_symbols = HashMap::new();
    for import in imports {
        if import.declaration.is_generic {
            continue;
        }
        let symbol = *imported_symbols
            .entry(import.symbol.clone())
            .or_insert_with(|| {
                object.add_symbol(Symbol {
                    name: import.symbol.as_bytes().to_vec(),
                    value: 0,
                    size: 0,
                    kind: SymbolKind::Text,
                    scope: SymbolScope::Linkage,
                    weak: false,
                    section: SymbolSection::Undefined,
                    flags: SymbolFlags::None,
                })
            });
        helper_symbols.insert(import.declaration.name.clone(), symbol);
    }

    let mut print_text = HashSet::new();
    collect_print_text(&exit_value, &mut print_text);
    collect_value_text(&exit_value, &mut print_text);
    let mut static_arrays = HashMap::new();
    collect_static_arrays(&exit_value, &mut static_arrays);
    let mut needs_decimal = has_print_kind(&exit_value, PrintKind::Decimal);
    let mut needs_panic_decimal = has_print_kind(&exit_value, PrintKind::PanicDecimal);
    let mut needs_hex16 = has_print_kind(&exit_value, PrintKind::Hex16);
    let mut needs_format_decimal = contains_format_decimal(&exit_value);
    let mut needs_utf8_chars = value_uses_character_iteration(&exit_value);
    for (_, _, value) in &helper_values {
        collect_print_text(value, &mut print_text);
        collect_value_text(value, &mut print_text);
        collect_static_arrays(value, &mut static_arrays);
        needs_decimal |= has_print_kind(value, PrintKind::Decimal);
        needs_panic_decimal |= has_print_kind(value, PrintKind::PanicDecimal);
        needs_hex16 |= has_print_kind(value, PrintKind::Hex16);
        needs_format_decimal |= contains_format_decimal(value);
        needs_utf8_chars |= value_uses_character_iteration(value);
    }
    if needs_decimal {
        let symbol = add_internal_function_symbol(&mut object, "__quill_print_decimal");
        helper_symbols.insert("__quill_print_decimal".to_owned(), symbol);
    }
    if needs_panic_decimal {
        let symbol = add_internal_function_symbol(&mut object, "__quill_print_decimal_stderr");
        helper_symbols.insert("__quill_print_decimal_stderr".to_owned(), symbol);
    }
    if needs_hex16 {
        let symbol = add_internal_function_symbol(&mut object, "__quill_print_hex16");
        helper_symbols.insert("__quill_print_hex16".to_owned(), symbol);
    }
    if needs_format_decimal {
        let symbol = add_internal_function_symbol(&mut object, "__quill_format_decimal");
        helper_symbols.insert("__quill_format_decimal".to_owned(), symbol);
        let symbol = add_internal_function_symbol(&mut object, "__quill_format_signed_decimal");
        helper_symbols.insert("__quill_format_signed_decimal".to_owned(), symbol);
    }
    if needs_utf8_chars {
        let symbol = add_internal_function_symbol(&mut object, "__quill_utf8_next");
        helper_symbols.insert("__quill_utf8_next".to_owned(), symbol);
    }
    if needs_utf8_chars {
        let whitespace_symbol =
            add_internal_function_symbol(&mut object, "__quill_char_is_whitespace");
        helper_symbols.insert("__quill_char_is_whitespace".to_owned(), whitespace_symbol);
    }
    let mut print_text: Vec<_> = print_text.into_iter().collect();
    print_text.sort();
    for value in print_text {
        let name = text_symbol_name(&value);
        let symbol = object.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: 0,
            size: 0,
            kind: SymbolKind::Data,
            scope: SymbolScope::Compilation,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        });
        if value.is_empty() {
            object.add_symbol_data(symbol, data, &[0], 1);
        } else {
            object.add_symbol_data(symbol, data, value.as_bytes(), 1);
        }
        helper_symbols.insert(name, symbol);
    }
    let mut static_arrays: Vec<_> = static_arrays.into_iter().collect();
    static_arrays.sort_by(|(left, _), (right, _)| left.cmp(right));
    let mut static_array_relocations = Vec::new();
    for (name, values) in static_arrays {
        let symbol = object.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: 0,
            size: 0,
            kind: SymbolKind::Data,
            scope: SymbolScope::Compilation,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        });
        let widths = static_array_element_widths(&values);
        let (offsets, stride) = native_tuple_layout(&widths);
        let mut bytes = vec![0; values.len() * stride];
        for (item_index, value) in values.iter().enumerate() {
            for (field_index, field) in static_array_fields(value)
                .expect("static slice elements were validated as scalars or pairs")
                .into_iter()
                .enumerate()
            {
                let start = item_index * stride + offsets[field_index];
                match field {
                    Value::Integer(number) => {
                        bytes[start..start + 4].copy_from_slice(&number.to_le_bytes());
                    }
                    Value::Word(number) => {
                        bytes[start..start + 8].copy_from_slice(&number.to_le_bytes());
                    }
                    Value::TypedWord { value, .. } => {
                        bytes[start..start + 8].copy_from_slice(&value.to_le_bytes());
                    }
                    Value::TextSlice(value) => {
                        bytes[start + 8..start + 16]
                            .copy_from_slice(&(value.len() as u64).to_le_bytes());
                        static_array_relocations.push((name.clone(), start as u64, value.clone()));
                    }
                    _ => unreachable!("static arrays are validated before object emission"),
                }
            }
        }
        let alignment = widths.iter().copied().max().unwrap_or(4) as u64;
        let data_offset = object.add_symbol_data(symbol, data, &bytes, alignment);
        for (array_name, field_offset, value) in static_array_relocations
            .iter_mut()
            .filter(|(array_name, _, _)| array_name == &name)
        {
            let _ = array_name;
            let target = helper_symbols[&text_symbol_name(value)];
            let rel = Relocation {
                offset: data_offset + *field_offset,
                symbol: target,
                addend: 0,
                flags: RelocationFlags::Generic {
                    kind: RelocationKind::Absolute,
                    encoding: RelocationEncoding::Generic,
                    size: 64,
                },
            };
            object
                .add_relocation(data, rel)
                .map_err(|error| CodegenError::ObjectWrite(error.to_string()))?;
        }
        helper_symbols.insert(name, symbol);
    }

    let mut pending = Vec::new();
    let mut relocations = if let Some(entry_symbol) = entry_symbol {
        let entry_has_frame =
            matches!(&exit_value, Value::Block { statements, .. } if !statements.is_empty());
        let mut entry_code = if entry_has_frame {
            entry_prologue(required_frame_slots(&exit_value, 0))
        } else {
            Vec::new()
        };
        let entry_prefix_len = entry_code.len() as u64;
        entry_code.extend(encode_value(&exit_value, false, &mut pending));
        for relocation in &mut pending {
            relocation.offset += entry_prefix_len;
        }
        if entry_has_frame {
            entry_code.push(0xc9); // leave
        }
        entry_code.extend_from_slice(&[0x89, 0xc7, 0xb8, 60, 0, 0, 0, 0x0f, 0x05, 0x0f, 0x0b]);
        let entry_offset = object.add_symbol_data(entry_symbol, text, &entry_code, 1);
        materialize_relocations(pending, entry_offset, &helper_symbols)?
    } else {
        Vec::new()
    };

    if needs_decimal {
        let symbol = helper_symbols["__quill_print_decimal"];
        let code = print_decimal_helper(1);
        object.add_symbol_data(symbol, text, &code, 1);
    }
    if needs_panic_decimal {
        let symbol = helper_symbols["__quill_print_decimal_stderr"];
        let code = print_decimal_helper(2);
        object.add_symbol_data(symbol, text, &code, 1);
    }
    if needs_hex16 {
        let symbol = helper_symbols["__quill_print_hex16"];
        let code = print_hex16_helper();
        object.add_symbol_data(symbol, text, &code, 1);
    }
    if needs_format_decimal {
        let symbol = helper_symbols["__quill_format_decimal"];
        let code = format_decimal_helper();
        object.add_symbol_data(symbol, text, &code, 1);
        let symbol = helper_symbols["__quill_format_signed_decimal"];
        let code = format_signed_decimal_helper();
        object.add_symbol_data(symbol, text, &code, 1);
    }
    if needs_utf8_chars {
        let symbol = helper_symbols["__quill_utf8_next"];
        let code = utf8_next_character_helper();
        object.add_symbol_data(symbol, text, &code, 1);
    }
    if needs_utf8_chars {
        let symbol = helper_symbols["__quill_char_is_whitespace"];
        let code = char_is_whitespace_helper();
        object.add_symbol_data(symbol, text, &code, 1);
    }

    for (_, function, value) in helper_values {
        let symbol = helper_symbols[function.name.as_str()];
        let mut pending = Vec::new();
        let mut frame_slots = required_frame_slots(
            &value,
            parameter_slot_count(&function.parameters, &parse.structs),
        );
        let return_pair = value_has_pair_return(&value);
        let pair_scratch = return_pair.then_some(frame_slots);
        frame_slots += usize::from(return_pair);
        let return_aggregate = function
            .return_type
            .as_deref()
            .and_then(|return_type| struct_return_field_types(return_type, &parse.structs))
            .is_some_and(|fields| requires_sret(&fields));
        let aggregate_pointer_slot = return_aggregate.then_some(frame_slots);
        frame_slots += usize::from(return_aggregate);
        let mut code = helper_prologue(
            &function.parameters,
            &parse.structs,
            frame_slots,
            return_aggregate,
        );
        append_encoded_helper_return(
            &mut code,
            &value,
            pair_scratch,
            aggregate_pointer_slot,
            &mut pending,
        );
        code.extend_from_slice(&[0xc9, 0xc3]); // leave; ret
        let offset = object.add_symbol_data(symbol, text, &code, 1);
        relocations.extend(materialize_relocations(pending, offset, &helper_symbols)?);
    }
    for relocation in relocations {
        object
            .add_relocation(text, relocation)
            .map_err(|e| CodegenError::ObjectWrite(e.to_string()))?;
    }
    object
        .write()
        .map_err(|e| CodegenError::ObjectWrite(e.to_string()))
}

fn validate_reachable_struct_expressions(
    parse: &Parse,
    reachable_indices: &HashSet<usize>,
) -> Result<(), CodegenError> {
    let mut structs = HashMap::new();
    for structure in &parse.structs {
        if structure.scope.is_empty()
            && structs.insert(structure.name.as_str(), structure).is_some()
        {
            return Err(CodegenError::UnsupportedProgram(
                "duplicate struct names are outside this codegen slice",
            ));
        }
    }
    for (index, function) in parse.functions.iter().enumerate() {
        if !reachable_indices.contains(&index) {
            continue;
        }
        if let Some(body) = &function.body {
            validate_struct_expr(body, &structs, &parse.functions)?;
        }
    }
    Ok(())
}

fn validate_struct_expr(
    expression: &BodyExpr,
    structs: &HashMap<&str, &StructItem>,
    functions: &[FunctionItem],
) -> Result<(), CodegenError> {
    match &expression.kind {
        BodyExprKind::StructLiteral { path, fields } => {
            let structure = structs
                .get(path.as_str())
                .ok_or(CodegenError::UnsupportedProgram(
                    "struct literal must name a declared top-level struct",
                ))?;
            if !structure.generic_parameters.is_empty()
                || !structure.repr_layout_supported
                || structure.fields.iter().any(|field| field.name.is_none())
            {
                return Err(CodegenError::UnsupportedProgram(
                    "aggregate expressions require nongeneric structs with named supported fields",
                ));
            }
            let declared: HashSet<_> = structure
                .fields
                .iter()
                .filter_map(|field| field.name.as_deref())
                .collect();
            let mut provided = HashSet::with_capacity(fields.len());
            for (name, value) in fields {
                if !declared.contains(name.as_str()) || !provided.insert(name.as_str()) {
                    return Err(CodegenError::UnsupportedProgram(
                        "struct literal has a duplicate or unknown field",
                    ));
                }
                let field_type = structure
                    .fields
                    .iter()
                    .find(|field| field.name.as_deref() == Some(name.as_str()))
                    .map(|field| field.type_text.trim());
                let supported = match field_type {
                    Some("i32") => is_supported_i32_expression(value, functions),
                    Some("u64" | "usize") => is_supported_u64_expression(value, functions),
                    Some("String") => is_supported_owned_string_expression(value),
                    Some(field_type) if is_collection_type(field_type) => {
                        is_supported_empty_collection_expression(value)
                    }
                    _ => false,
                };
                if !supported {
                    return Err(CodegenError::UnsupportedProgram(
                        "struct field initializer does not match its supported field type",
                    ));
                }
                validate_struct_expr(value, structs, functions)?;
            }
            if provided.len() != declared.len() {
                return Err(CodegenError::UnsupportedProgram(
                    "struct literal must initialize every field",
                ));
            }
        }
        BodyExprKind::FieldAccess { base, .. }
        | BodyExprKind::Negate(base)
        | BodyExprKind::Not(base)
        | BodyExprKind::Try(base)
        | BodyExprKind::Parenthesized(base)
        | BodyExprKind::Cast {
            expression: base, ..
        } => validate_struct_expr(base, structs, functions)?,
        BodyExprKind::Index { base, index, .. } => {
            validate_struct_expr(base, structs, functions)?;
            validate_struct_expr(index, structs, functions)?;
        }
        BodyExprKind::Call { arguments, .. } => {
            for argument in arguments {
                validate_struct_expr(argument, structs, functions)?;
            }
        }
        BodyExprKind::MacroInvocation(macro_expression) => {
            for argument in &macro_expression.arguments {
                validate_struct_expr(argument, structs, functions)?;
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            arguments,
            ..
        } => {
            validate_struct_expr(receiver, structs, functions)?;
            for argument in arguments {
                validate_struct_expr(argument, structs, functions)?;
            }
        }
        BodyExprKind::Closure { body, .. } => validate_struct_expr(body, structs, functions)?,
        BodyExprKind::Tuple(elements)
        | BodyExprKind::ArrayLiteral(elements)
        | BodyExprKind::FixedArrayLiteral(elements) => {
            for element in elements {
                validate_struct_expr(element, structs, functions)?;
            }
        }
        BodyExprKind::Binary { left, right, .. } => {
            validate_struct_expr(left, structs, functions)?;
            validate_struct_expr(right, structs, functions)?;
        }
        BodyExprKind::Block { statements, tail } => {
            for statement in statements {
                match statement {
                    BodyStatement::Let { initializer, .. } => {
                        if let Some(initializer) = initializer {
                            validate_struct_expr(initializer, structs, functions)?;
                        }
                    }
                    BodyStatement::LetTuple { initializer, .. } => {
                        validate_struct_expr(initializer, structs, functions)?;
                    }
                    BodyStatement::Assign { value, .. }
                    | BodyStatement::Expression {
                        expression: value, ..
                    } => validate_struct_expr(value, structs, functions)?,
                    BodyStatement::While {
                        condition, body, ..
                    } => {
                        validate_struct_expr(condition, structs, functions)?;
                        validate_struct_expr(body, structs, functions)?;
                    }
                    BodyStatement::Loop { body, .. } => {
                        validate_struct_expr(body, structs, functions)?
                    }
                    BodyStatement::ForRange {
                        start, end, body, ..
                    } => {
                        validate_struct_expr(start, structs, functions)?;
                        validate_struct_expr(end, structs, functions)?;
                        validate_struct_expr(body, structs, functions)?;
                    }
                    BodyStatement::ForEach { iterator, body, .. } => {
                        validate_struct_expr(iterator, structs, functions)?;
                        validate_struct_expr(body, structs, functions)?;
                    }
                    BodyStatement::Break { .. }
                    | BodyStatement::Continue { .. }
                    | BodyStatement::MacroInvocation { .. }
                    | BodyStatement::Unsupported => {}
                }
            }
            if let Some(tail) = tail {
                validate_struct_expr(tail, structs, functions)?;
            }
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            validate_struct_expr(condition, structs, functions)?;
            validate_struct_expr(then_branch, structs, functions)?;
            if let Some(branch) = else_branch {
                validate_struct_expr(branch, structs, functions)?;
            }
        }
        BodyExprKind::Match { scrutinee, arms } => {
            validate_struct_expr(scrutinee, structs, functions)?;
            for arm in arms {
                validate_struct_expr(&arm.body, structs, functions)?;
            }
        }
        BodyExprKind::Return(Some(value)) => validate_struct_expr(value, structs, functions)?,
        BodyExprKind::Reference { expression, .. } | BodyExprKind::Dereference(expression) => {
            validate_struct_expr(expression, structs, functions)?
        }
        BodyExprKind::Unit
        | BodyExprKind::Literal(_)
        | BodyExprKind::EmptySliceReference
        | BodyExprKind::Path(_)
        | BodyExprKind::Return(None)
        | BodyExprKind::Unsupported => {}
    }
    Ok(())
}

fn is_supported_owned_string_expression(expression: &BodyExpr) -> bool {
    matches!(
        &expression.kind,
        BodyExprKind::MethodCall {
            method,
            arguments,
            ..
        } if matches!(method.as_str(), "into" | "to_owned") && arguments.is_empty()
    ) || matches!(
        &expression.kind,
        BodyExprKind::Call { callee, arguments, .. }
            if matches!(callee.as_str(), "String::from" | "std::string::String::from")
                && arguments.len() == 1
    )
}

fn is_supported_empty_collection_expression(expression: &BodyExpr) -> bool {
    let BodyExprKind::Call {
        callee, arguments, ..
    } = &expression.kind
    else {
        return false;
    };
    arguments.is_empty()
        && callee.ends_with("::new")
        && ["Vec", "BTreeMap", "HashMap"]
            .iter()
            .any(|name| callee[..callee.len() - "::new".len()].ends_with(name))
}

fn is_supported_string_buffer_expression(expression: &BodyExpr) -> bool {
    let BodyExprKind::Call {
        callee, arguments, ..
    } = &expression.kind
    else {
        return false;
    };
    ((callee == "String::new" || callee == "std::string::String::new") && arguments.is_empty())
        || ((callee == "String::with_capacity" || callee == "std::string::String::with_capacity")
            && arguments.len() == 1)
}

fn is_supported_i32_expression(expression: &BodyExpr, functions: &[FunctionItem]) -> bool {
    match &expression.kind {
        BodyExprKind::Literal(LiteralKind::Integer { text, suffix })
            if suffix.is_empty() || suffix == "i32" =>
        {
            text.parse::<i32>().is_ok()
        }
        BodyExprKind::Parenthesized(inner) | BodyExprKind::Negate(inner) => {
            is_supported_i32_expression(inner, functions)
        }
        BodyExprKind::Binary {
            operator:
                BinaryOperator::Add
                | BinaryOperator::Subtract
                | BinaryOperator::Multiply
                | BinaryOperator::Divide
                | BinaryOperator::Remainder,
            left,
            right,
        } => {
            is_supported_i32_expression(left, functions)
                && is_supported_i32_expression(right, functions)
        }
        BodyExprKind::Call {
            callee, arguments, ..
        } => functions
            .iter()
            .find(|function| function.name == *callee)
            .is_some_and(|function| {
                function.return_type.as_deref() == Some("i32")
                    && function
                        .parameters
                        .iter()
                        .all(|parameter| parameter.type_text.trim() == "i32")
                    && function.parameters.len() == arguments.len()
                    && arguments
                        .iter()
                        .all(|argument| is_supported_i32_expression(argument, functions))
            }),
        _ => false,
    }
}

fn is_supported_u64_expression(expression: &BodyExpr, functions: &[FunctionItem]) -> bool {
    match &expression.kind {
        BodyExprKind::Path(_) => true,
        BodyExprKind::Literal(LiteralKind::Integer { text, suffix })
            if matches!(suffix.as_str(), "" | "u64" | "usize") =>
        {
            text.parse::<u64>().is_ok()
        }
        BodyExprKind::Parenthesized(inner) => is_supported_u64_expression(inner, functions),
        BodyExprKind::Call {
            callee, arguments, ..
        } => functions
            .iter()
            .find(|function| function.name == *callee)
            .is_some_and(|function| {
                function.return_type.as_deref().is_some_and(is_word_type)
                    && function
                        .parameters
                        .iter()
                        .all(|parameter| is_word_type(&parameter.type_text))
                    && function.parameters.len() == arguments.len()
                    && arguments
                        .iter()
                        .all(|argument| is_supported_u64_expression(argument, functions))
            }),
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } if matches!(method.as_str(), "wrapping_add" | "wrapping_mul") && arguments.len() == 1 => {
            is_supported_u64_expression(receiver, functions)
                && is_supported_u64_expression(&arguments[0], functions)
        }
        _ => false,
    }
}

fn validate_call_targets(
    expression: &BodyExpr,
    functions: &HashMap<&str, usize>,
) -> Result<(), CodegenError> {
    match &expression.kind {
        BodyExprKind::Call {
            callee, arguments, ..
        } => {
            if callee != "std::process::exit"
                && !matches!(callee.as_str(), "Ok" | "Err")
                && !is_option_some_path(callee)
                && !functions.contains_key(callee.as_str())
                && !is_associated_item_path(callee)
            {
                return Err(CodegenError::UnresolvedCall(callee.clone()));
            }
            for argument in arguments {
                validate_call_targets(argument, functions)?;
            }
        }
        BodyExprKind::MacroInvocation(macro_expression) => {
            for argument in &macro_expression.arguments {
                validate_call_targets(argument, functions)?;
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            arguments,
            ..
        } => {
            validate_call_targets(receiver, functions)?;
            for argument in arguments {
                validate_call_targets(argument, functions)?;
            }
        }
        BodyExprKind::Closure { body, .. } => validate_call_targets(body, functions)?,
        BodyExprKind::Tuple(elements)
        | BodyExprKind::ArrayLiteral(elements)
        | BodyExprKind::FixedArrayLiteral(elements) => {
            for element in elements {
                validate_call_targets(element, functions)?;
            }
        }
        BodyExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                validate_call_targets(value, functions)?;
            }
        }
        BodyExprKind::FieldAccess { base, .. } => validate_call_targets(base, functions)?,
        BodyExprKind::Index { base, index, .. } => {
            validate_call_targets(base, functions)?;
            validate_call_targets(index, functions)?;
        }
        BodyExprKind::Binary { left, right, .. } => {
            validate_call_targets(left, functions)?;
            validate_call_targets(right, functions)?;
        }
        BodyExprKind::Negate(value)
        | BodyExprKind::Not(value)
        | BodyExprKind::Try(value)
        | BodyExprKind::Parenthesized(value) => {
            validate_call_targets(value, functions)?;
        }
        BodyExprKind::Cast { expression, .. } => validate_call_targets(expression, functions)?,
        BodyExprKind::Block { statements, tail } => {
            for statement in statements {
                match statement {
                    BodyStatement::Let {
                        initializer: Some(value),
                        ..
                    }
                    | BodyStatement::LetTuple {
                        initializer: value, ..
                    }
                    | BodyStatement::Assign { value, .. } => {
                        validate_call_targets(value, functions)?
                    }
                    BodyStatement::While {
                        condition, body, ..
                    } => {
                        validate_call_targets(condition, functions)?;
                        validate_call_targets(body, functions)?;
                    }
                    BodyStatement::Loop { body, .. } => validate_call_targets(body, functions)?,
                    BodyStatement::ForRange {
                        start, end, body, ..
                    } => {
                        validate_call_targets(start, functions)?;
                        validate_call_targets(end, functions)?;
                        validate_call_targets(body, functions)?;
                    }
                    BodyStatement::ForEach { iterator, body, .. } => {
                        validate_call_targets(iterator, functions)?;
                        validate_call_targets(body, functions)?;
                    }
                    BodyStatement::Expression { expression, .. } => {
                        validate_call_targets(expression, functions)?
                    }
                    _ => {}
                }
            }
            if let Some(tail) = tail {
                validate_call_targets(tail, functions)?;
            }
        }
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            validate_call_targets(condition, functions)?;
            validate_call_targets(then_branch, functions)?;
            if let Some(branch) = else_branch {
                validate_call_targets(branch, functions)?;
            }
        }
        BodyExprKind::Match { scrutinee, arms } => {
            validate_call_targets(scrutinee, functions)?;
            for arm in arms {
                validate_call_targets(&arm.body, functions)?;
            }
        }
        BodyExprKind::Return(Some(value)) => validate_call_targets(value, functions)?,
        BodyExprKind::Reference { expression, .. } | BodyExprKind::Dereference(expression) => {
            validate_call_targets(expression, functions)?
        }
        BodyExprKind::Unit
        | BodyExprKind::Literal(_)
        | BodyExprKind::EmptySliceReference
        | BodyExprKind::Path(_)
        | BodyExprKind::Return(None)
        | BodyExprKind::Unsupported => {}
    }
    Ok(())
}

fn is_associated_item_path(path: &str) -> bool {
    if matches!(path, "u64::from" | "usize::from" | "i32::from") {
        return true;
    }
    let mut segments = path.split("::").collect::<Vec<_>>();
    if segments.len() < 2 {
        return false;
    }
    segments.pop();
    segments
        .iter()
        .any(|owner| owner.chars().next().is_some_and(char::is_uppercase))
}

fn generic_instance_name(
    index: usize,
    function: &FunctionItem,
    arguments: &[Value],
    explicit_arguments: &[String],
) -> Option<String> {
    if !explicit_arguments.is_empty()
        && (explicit_arguments.len() != function.generic_parameters.len()
            || explicit_arguments
                .iter()
                .any(|ty| {
                    ty.is_empty()
                        || ty.chars().any(|character| {
                            matches!(character, ' ' | '<' | '>' | ',' | ';')
                        })
                }))
    {
        return None;
    }
    let mut concrete = Vec::with_capacity(function.generic_parameters.len());
    for (generic_index, generic) in function.generic_parameters.iter().enumerate() {
        let mut inferred = None;
        for (parameter, argument) in function.parameters.iter().zip(arguments) {
            let inferred_type = infer_generic_type(&parameter.type_text, generic, argument)
                .or_else(|| {
                    (function.generic_as_ref_str_parameters.contains(generic)
                        && is_slice_value(argument))
                    .then(|| "&str".to_owned())
                });
            let Some(ty) = inferred_type else {
                continue;
            };
            if !explicit_arguments.is_empty() && ty != explicit_arguments[generic_index] {
                return None;
            }
            if inferred.as_ref().is_some_and(|previous| previous != &ty) {
                return None;
            }
            inferred = Some(ty);
        }
        if function.generic_as_ref_str_parameters.contains(generic)
            && inferred.as_deref().is_some_and(|ty| ty != "&str")
        {
            return None;
        }
        concrete.push(if explicit_arguments.is_empty() {
            inferred?
        } else {
            explicit_arguments[generic_index].clone()
        });
    }
    let encoded = concrete
        .iter()
        .map(|ty| match ty.as_str() {
            "&str" => "strref".to_owned(),
            "i32" | "u64" | "usize" => ty.clone(),
            _ => format!("ty{}", encode_type_name(ty)),
        })
        .collect::<Vec<_>>();
    Some(format!("__quill_fn_{index}_{}", encoded.join("_")))
}

fn encode_type_name(type_name: &str) -> String {
    type_name
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn decode_type_name(encoded: &str) -> Option<String> {
    if encoded.is_empty() || encoded.len() % 2 != 0 {
        return None;
    }
    let bytes = (0..encoded.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&encoded[index..index + 2], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    String::from_utf8(bytes).ok()
}

fn substitute_generic_types(type_text: &str, substitutions: &HashMap<&str, &str>) -> String {
    let mut output = String::with_capacity(type_text.len());
    let mut start = 0;
    let bytes = type_text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_alphabetic() || bytes[index] == b'_' {
            let token_start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            let token = &type_text[token_start..index];
            if let Some(replacement) = substitutions.get(token) {
                output.push_str(&type_text[start..token_start]);
                output.push_str(replacement);
                start = index;
            }
        } else {
            index += 1;
        }
    }
    output.push_str(&type_text[start..]);
    output
}

fn substitute_generic_body(body: &mut quill_syntax::BodyExpr, substitutions: &HashMap<&str, &str>) {
    use quill_syntax::{BodyExpr, BodyExprKind, BodyStatement, MatchPattern};

    fn visit_pattern(pattern: &mut MatchPattern, substitutions: &HashMap<&str, &str>) {
        match pattern {
            MatchPattern::Path(path) | MatchPattern::EnumVariant { path, .. } => {
                *path = substitute_generic_types(path, substitutions);
            }
            MatchPattern::Or(patterns) => {
                for pattern in patterns {
                    visit_pattern(pattern, substitutions);
                }
            }
            MatchPattern::Wildcard
            | MatchPattern::Literal(_)
            | MatchPattern::Binding(_)
            | MatchPattern::Range { .. }
            | MatchPattern::Unsupported => {}
        }
    }

    fn visit_statement(statement: &mut BodyStatement, substitutions: &HashMap<&str, &str>) {
        match statement {
            BodyStatement::Let {
                type_text,
                initializer,
                ..
            } => {
                if let Some(type_text) = type_text {
                    *type_text = substitute_generic_types(type_text, substitutions);
                }
                if let Some(initializer) = initializer {
                    visit(initializer, substitutions);
                }
            }
            BodyStatement::LetTuple { initializer, .. }
            | BodyStatement::Assign {
                value: initializer, ..
            }
            | BodyStatement::Expression {
                expression: initializer,
                ..
            } => visit(initializer, substitutions),
            BodyStatement::While {
                condition, body, ..
            } => {
                visit(condition, substitutions);
                visit(body, substitutions);
            }
            BodyStatement::Loop { body, .. } => visit(body, substitutions),
            BodyStatement::ForRange {
                start, end, body, ..
            } => {
                visit(start, substitutions);
                visit(end, substitutions);
                visit(body, substitutions);
            }
            BodyStatement::ForEach { iterator, body, .. } => {
                visit(iterator, substitutions);
                visit(body, substitutions);
            }
            BodyStatement::MacroInvocation { arguments, .. } => {
                for argument in arguments {
                    visit(argument, substitutions);
                }
            }
            BodyStatement::Break { .. }
            | BodyStatement::Continue { .. }
            | BodyStatement::Unsupported => {}
        }
    }

    fn visit(body: &mut BodyExpr, substitutions: &HashMap<&str, &str>) {
        match &mut body.kind {
            BodyExprKind::Cast {
                expression,
                type_name,
            } => {
                *type_name = substitute_generic_types(type_name, substitutions);
                visit(expression, substitutions);
            }
            BodyExprKind::MacroInvocation(macro_expression) => {
                for argument in &mut macro_expression.arguments {
                    visit(argument, substitutions);
                }
            }
            BodyExprKind::Call {
                generic_arguments,
                arguments,
                ..
            } => {
                for argument in generic_arguments {
                    *argument = substitute_generic_types(argument, substitutions);
                }
                for argument in arguments {
                    visit(argument, substitutions);
                }
            }
            BodyExprKind::MethodCall {
                receiver,
                arguments,
                ..
            } => {
                visit(receiver, substitutions);
                for argument in arguments {
                    visit(argument, substitutions);
                }
            }
            BodyExprKind::Closure { body, .. } => visit(body, substitutions),
            BodyExprKind::Tuple(elements)
            | BodyExprKind::ArrayLiteral(elements)
            | BodyExprKind::FixedArrayLiteral(elements) => {
                for element in elements {
                    visit(element, substitutions);
                }
            }
            BodyExprKind::StructLiteral { path, fields } => {
                *path = substitute_generic_types(path, substitutions);
                for (_, value) in fields {
                    visit(value, substitutions);
                }
            }
            BodyExprKind::FieldAccess { base, .. }
            | BodyExprKind::Reference {
                expression: base, ..
            }
            | BodyExprKind::Dereference(base)
            | BodyExprKind::Negate(base)
            | BodyExprKind::Not(base)
            | BodyExprKind::Parenthesized(base)
            | BodyExprKind::Try(base) => visit(base, substitutions),
            BodyExprKind::Index { base, index, .. } => {
                visit(base, substitutions);
                visit(index, substitutions);
            }
            BodyExprKind::Binary { left, right, .. } => {
                visit(left, substitutions);
                visit(right, substitutions);
            }
            BodyExprKind::Block { statements, tail } => {
                for statement in statements {
                    visit_statement(statement, substitutions);
                }
                if let Some(tail) = tail {
                    visit(tail, substitutions);
                }
            }
            BodyExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                visit(condition, substitutions);
                visit(then_branch, substitutions);
                if let Some(branch) = else_branch {
                    visit(branch, substitutions);
                }
            }
            BodyExprKind::Match { scrutinee, arms } => {
                visit(scrutinee, substitutions);
                for arm in arms {
                    visit_pattern(&mut arm.pattern, substitutions);
                    visit(&mut arm.body, substitutions);
                }
            }
            BodyExprKind::Return(value) => {
                if let Some(value) = value {
                    visit(value, substitutions);
                }
            }
            BodyExprKind::Unit
            | BodyExprKind::Literal(_)
            | BodyExprKind::EmptySliceReference
            | BodyExprKind::Path(_)
            | BodyExprKind::Unsupported => {}
        }
    }

    visit(body, substitutions);
}

fn infer_generic_type(parameter_type: &str, generic: &str, value: &Value) -> Option<String> {
    let parameter_type = parameter_type.trim();
    if parameter_type == generic {
        return generic_argument_type(value);
    }
    if let Some(referent) = parameter_type.strip_prefix('&') {
        let referent = referent
            .trim_start()
            .strip_prefix("mut ")
            .unwrap_or(referent.trim_start());
        return infer_generic_type(referent, generic, value);
    }
    if let Some(element_type) = parameter_type
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
    {
        let values = match value {
            Value::StaticArray(values) => values.as_slice(),
            Value::SliceParameter { .. } | Value::SliceLocal { .. } | Value::SliceRecord { .. } => {
                return None;
            }
            _ => return None,
        };
        let mut inferred = None;
        for element in values {
            let ty = infer_generic_type(element_type, generic, element)?;
            if inferred.as_ref().is_some_and(|previous| previous != &ty) {
                return None;
            }
            inferred = Some(ty);
        }
        return inferred;
    }
    if let Some([first_type, second_type]) = pair_type_fields(parameter_type) {
        let elements = match value {
            Value::Tuple(elements) | Value::StaticArray(elements) => elements,
            _ => return None,
        };
        if elements.len() != 2 {
            return None;
        }
        let first = infer_generic_type(first_type, generic, &elements[0]);
        let second = infer_generic_type(second_type, generic, &elements[1]);
        return match (first, second) {
            (Some(first), Some(second)) if first == second => Some(first),
            (Some(ty), None) | (None, Some(ty)) => Some(ty),
            (None, None) => None,
            _ => None,
        };
    }
    None
}

fn generic_argument_type(value: &Value) -> Option<String> {
    match value {
        Value::TextSlice(_) | Value::SliceElementString { .. } => Some("&str".to_owned()),
        Value::TypedWord { type_name, .. }
        | Value::TypedWordParameter { type_name, .. }
        | Value::TypedWordLocal { type_name, .. } => Some(type_name.clone()),
        Value::StructReference { type_name, .. } | Value::StructLocal { type_name, .. } => {
            Some(type_name.clone())
        }
        Value::Word(_) | Value::WordLocal(_) => Some("usize".to_owned()),
        Value::WrappingBinary { .. }
        | Value::SignExtend(_)
        | Value::SliceElement {
            element_size: 8, ..
        } => Some("u64".to_owned()),
        Value::Call {
            word_type: Some(type_name),
            returns_word: true,
            ..
        } => Some(type_name.clone()),
        Value::Call {
            returns_word: true, ..
        } => Some("u64".to_owned()),
        Value::Integer(_)
        | Value::Parameter(_)
        | Value::Local(_)
        | Value::Binary { .. }
        | Value::Negate(_)
        | Value::Call {
            returns_word: false,
            returns_slice: false,
            returns_pair: false,
            returns_aggregate: false,
            ..
        } => Some("i32".to_owned()),
        Value::PairField { .. } if is_word_value(value) => Some("u64".to_owned()),
        Value::PairField { .. } => Some("i32".to_owned()),
        _ => None,
    }
}

fn parse_generic_instance_name(name: &str) -> Option<(usize, Vec<String>)> {
    let remainder = name.strip_prefix("__quill_fn_")?;
    let (index, arguments) = remainder.split_once('_')?;
    let index = index.parse().ok()?;
    let arguments = arguments
        .split('_')
        .map(|argument| match argument {
            "strref" => Some("&str".to_owned()),
            "i32" | "u64" | "usize" => Some(argument.to_owned()),
            _ if argument.starts_with("ty") => decode_type_name(&argument[2..]),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some((index, arguments))
}

fn collect_generic_instances(value: &Value, output: &mut BTreeSet<String>) {
    match value {
        Value::Block { statements, tail } => {
            for statement in statements {
                collect_generic_statement(statement, output);
            }
            collect_generic_instances(tail, output);
        }
        Value::Call {
            name, arguments, ..
        } => {
            if parse_generic_instance_name(name).is_some() {
                output.insert(name.clone());
            }
            for argument in arguments {
                collect_generic_instances(argument, output);
            }
        }
        Value::Tuple(values) | Value::StaticArray(values) => {
            for value in values {
                collect_generic_instances(value, output);
            }
        }
        Value::OptionUnwrapOr {
            option, fallback, ..
        } => {
            collect_generic_instances(option, output);
            collect_generic_instances(fallback, output);
        }
        Value::PairField { aggregate, .. }
        | Value::OwnedString(aggregate)
        | Value::Negate(aggregate)
        | Value::ResultIsOk(aggregate)
        | Value::OptionIsSome(aggregate)
        | Value::SliceIsEmpty(aggregate)
        | Value::SliceLength(aggregate)
        | Value::ZeroExtend(aggregate)
        | Value::SignExtend(aggregate)
        | Value::CharIsAsciiAlphanumeric(aggregate) => collect_generic_instances(aggregate, output),
        Value::SliceFold {
            source,
            initial,
            update,
            ..
        } => {
            collect_generic_instances(source, output);
            collect_generic_instances(initial, output);
            collect_generic_instances(update, output);
        }
        Value::TupleSliceFold {
            source,
            initial,
            updates,
            ..
        } => {
            collect_generic_instances(source, output);
            for value in initial.iter().chain(updates) {
                collect_generic_instances(value, output);
            }
        }
        Value::StructReference { fields, .. } => {
            for field in fields {
                collect_generic_instances(field, output);
            }
        }
        Value::FormatString(pieces) => {
            for piece in pieces {
                if let FormatPiece::Decimal(value) | FormatPiece::SignedDecimal(value) = piece {
                    collect_generic_instances(value, output);
                }
            }
        }
        Value::Binary { left, right, .. } | Value::WrappingBinary { left, right, .. } => {
            collect_generic_instances(left, output);
            collect_generic_instances(right, output);
        }
        Value::If {
            condition,
            then_value,
            else_value,
        } => {
            collect_generic_condition(condition, output);
            collect_generic_instances(then_value, output);
            collect_generic_instances(else_value, output);
        }
        Value::BTreeMapGetString { key, .. } => collect_generic_instances(key, output),
        Value::Integer(_)
        | Value::Word(_)
        | Value::TypedWord { .. }
        | Value::EmptySlice
        | Value::NicheOk
        | Value::EmptyCollection
        | Value::TextSlice(_)
        | Value::SliceElementString { .. }
        | Value::SliceParameter { .. }
        | Value::SliceLocal { .. }
        | Value::SliceRecord { .. }
        | Value::CharIterator { .. }
        | Value::AnyStringTrimEmpty { .. }
        | Value::SliceElement { .. }
        | Value::CollectionLocal { .. }
        | Value::CollectionAddress { .. }
        | Value::Parameter(_)
        | Value::TypedWordParameter { .. }
        | Value::Local(_)
        | Value::WordLocal(_)
        | Value::TypedWordLocal { .. }
        | Value::AddressOfLocal(_)
        | Value::IndirectLocal { .. }
        | Value::StructLocal { .. } => {}
    }
}

fn collect_generic_condition(condition: &Condition, output: &mut BTreeSet<String>) {
    match condition {
        Condition::Compare { left, right, .. } => {
            collect_generic_instances(left, output);
            collect_generic_instances(right, output);
        }
        Condition::And(left, right) | Condition::Or(left, right) => {
            collect_generic_condition(left, output);
            collect_generic_condition(right, output);
        }
        Condition::Boolean(_) => {}
    }
}

fn collect_generic_statement(statement: &Statement, output: &mut BTreeSet<String>) {
    match statement {
        Statement::Evaluate(value)
        | Statement::Let {
            initializer: value, ..
        }
        | Statement::LetSlice {
            initializer: value, ..
        }
        | Statement::Assign { value, .. }
        | Statement::AssignIndirect { value, .. }
        | Statement::TryResult(value)
        | Statement::TryOption(value)
        | Statement::TryOptionBind { result: value, .. }
        | Statement::Return(value) => collect_generic_instances(value, output),
        Statement::VecPushString { value, .. } => collect_generic_instances(value, output),
        Statement::StringAppendChar { character, .. } => {
            collect_generic_instances(character, output)
        }
        Statement::BTreeMapInsert { key, value, .. } => {
            collect_generic_instances(key, output);
            collect_generic_instances(value, output);
        }
        Statement::VecPushAggregate { fields, .. } => {
            for value in fields {
                collect_generic_instances(value, output);
            }
        }
        Statement::LetStruct { fields } => {
            for (_, value) in fields {
                collect_generic_instances(value, output);
            }
        }
        Statement::LetTupleCall { initializer, .. } => {
            collect_generic_instances(initializer, output);
        }
        Statement::If {
            condition,
            then_body,
            else_body,
        } => {
            collect_generic_condition(condition, output);
            for statement in then_body.iter().chain(else_body) {
                collect_generic_statement(statement, output);
            }
        }
        Statement::While { condition, body } => {
            collect_generic_condition(condition, output);
            for statement in body {
                collect_generic_statement(statement, output);
            }
        }
        Statement::ForRange { body, .. }
        | Statement::ForSlice { body, .. }
        | Statement::ForChars { body, .. } => {
            for statement in body {
                collect_generic_statement(statement, output);
            }
        }
        Statement::Println(parts) => {
            for part in parts {
                match part {
                    PrintPart::Slice(value)
                    | PrintPart::Decimal(value)
                    | PrintPart::Hex16(value)
                    | PrintPart::PanicDecimal(value) => collect_generic_instances(value, output),
                    PrintPart::Text(_) | PrintPart::Panic(_) | PrintPart::PanicText(_) => {}
                }
            }
        }
        Statement::Break | Statement::Continue => {}
    }
}

fn normalize_impl_function(function: &FunctionItem) -> FunctionItem {
    let scoped_owner = function
        .scope
        .iter()
        .find_map(|scope| scope.strip_prefix("impl:"));
    let needs_self = function.parameters.iter().any(|parameter| {
        parameter.name.as_deref() == Some("self") || parameter.type_text.contains("Self")
    }) || function
        .return_type
        .as_deref()
        .is_some_and(|ty| ty.contains("Self"));
    let inferred_owner = needs_self
        .then(|| {
            function
                .name
                .rsplit_once("::")
                .and_then(|(prefix, _)| prefix.rsplit("::").next())
        })
        .flatten();
    let Some(owner) = scoped_owner.or(inferred_owner) else {
        return function.clone();
    };
    let mut normalized = function.clone();
    for parameter in &mut normalized.parameters {
        parameter.type_text = parameter.type_text.replace("Self", owner);
    }
    if let Some(return_type) = &mut normalized.return_type {
        *return_type = return_type.replace("Self", owner);
    }
    normalized.scope.clear();
    normalized
}

fn validate_function_signature(
    function: &FunctionItem,
    main: bool,
    structs: &[StructItem],
) -> Result<(), CodegenError> {
    if !function.scope.is_empty()
        || function.is_async
        || function.is_const
        || function.is_unsafe
        || function.has_abi
        || function.body.is_none()
    {
        return Err(CodegenError::UnsupportedProgram(
            "only safe, synchronous, top-level functions with bodies are supported",
        ));
    }
    if main {
        if function.is_generic
            || !function.parameters.is_empty()
            || function
                .return_type
                .as_deref()
                .is_some_and(|t| t.trim() != "()")
        {
            return Err(CodegenError::UnsupportedProgram(
                "`main` must return the unit type",
            ));
        }
    } else if function.is_generic {
        let shape = (1..=2).contains(&function.generic_parameters.len())
            && (1..=2).contains(&function.parameters.len())
            && function
                .parameters
                .iter()
                .all(|parameter| parameter.name.is_some())
            && function.generic_parameters.iter().all(|generic| {
                function
                    .parameters
                    .iter()
                    .any(|parameter| type_mentions_identifier(&parameter.type_text, generic))
            });
        if function.generic_has_bounds
            || !shape
            || function.return_type.as_deref().is_some_and(|return_type| {
                !matches!(return_type.trim(), "()" | "i32" | "u64" | "usize")
                    && !function
                        .generic_parameters
                        .iter()
                        .any(|generic| return_type.trim() == generic)
            })
        {
            return Err(CodegenError::UnsupportedSignature(
                display_function_signature(function),
            ));
        }
    } else if function.return_type.as_deref().is_some_and(|return_type| {
        is_unit_option_type(return_type)
            || is_i32_option_type(return_type)
            || is_native_tagged_result_type(return_type)
    }) {
        if function.parameters.len() > 64
            || function.parameters.iter().any(|parameter| {
                parameter.name.is_none()
                    || !is_codegen_parameter_type(&parameter.type_text, structs)
            })
        {
            return Err(CodegenError::UnsupportedSignature(
                display_function_signature(function),
            ));
        }
    } else if function
        .return_type
        .as_deref()
        .and_then(ResultLayout::parse)
        .is_some_and(|layout| {
            matches!(layout.encoding, ResultEncoding::NullPointerNiche { .. })
                || is_native_tagged_result_type(function.return_type.as_deref().unwrap_or(""))
        })
    {
        if function.parameters.len() > 64
            || function.parameters.iter().any(|parameter| {
                parameter.name.is_none()
                    || !is_codegen_parameter_type(&parameter.type_text, structs)
            })
        {
            return Err(CodegenError::UnsupportedSignature(
                display_function_signature(function),
            ));
        }
    } else if function.return_type.as_deref().is_some_and(is_slice_type) {
        if function.parameters.len() > 64
            || function.parameters.iter().any(|parameter| {
                parameter.name.is_none()
                    || !is_codegen_parameter_type(&parameter.type_text, structs)
            })
        {
            return Err(CodegenError::UnsupportedSignature(
                display_function_signature(function),
            ));
        }
    } else if let Some(fields) = function
        .return_type
        .as_deref()
        .and_then(pair_type_fields)
        .map(|[first, second]| vec![first.to_owned(), second.to_owned()])
        .or_else(|| {
            function
                .return_type
                .as_deref()
                .and_then(|return_type| struct_pair_field_types(return_type, structs))
        })
        .or_else(|| {
            function
                .return_type
                .as_deref()
                .and_then(|return_type| struct_return_field_types(return_type, structs))
                .filter(|fields| requires_sret(fields))
        })
    {
        if fields
            .iter()
            .any(|ty| !is_scalar_codegen_type(ty) && !is_slice_type(ty) && !is_collection_type(ty))
            || function.parameters.len() > 64
            || function.parameters.iter().any(|parameter| {
                parameter.name.is_none()
                    || !is_codegen_parameter_type(&parameter.type_text, structs)
            })
        {
            return Err(CodegenError::UnsupportedSignature(
                display_function_signature(function),
            ));
        }
    } else if function
        .return_type
        .as_deref()
        .is_some_and(|return_type| !matches!(return_type.trim(), "()" | "i32" | "u64" | "usize"))
        || function.parameters.len() > 64
        || function.parameters.iter().any(|parameter| {
            parameter.name.is_none() || !is_codegen_parameter_type(&parameter.type_text, structs)
        })
    {
        return Err(CodegenError::UnsupportedSignature(
            display_function_signature(function),
        ));
    }
    Ok(())
}

fn type_mentions_identifier(type_text: &str, name: &str) -> bool {
    let bytes = type_text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_alphabetic() || bytes[index] == b'_' {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            if &type_text[start..index] == name {
                return true;
            }
        } else {
            index += 1;
        }
    }
    false
}

fn display_function_signature(function: &FunctionItem) -> String {
    let parameters = function
        .parameters
        .iter()
        .map(|parameter| {
            parameter.name.as_ref().map_or_else(
                || parameter.type_text.clone(),
                |name| format!("{name}: {}", parameter.type_text),
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let return_type = function
        .return_type
        .as_deref()
        .map_or(String::from("()"), str::to_owned);
    format!("fn {}({parameters}) -> {return_type}", function.name)
}

fn pair_type_fields(type_text: &str) -> Option<[&str; 2]> {
    let body = type_text
        .trim()
        .strip_prefix('(')?
        .strip_suffix(')')?
        .trim();
    let (first, second) = body.split_once(',')?;
    let second = second.trim().strip_suffix(',').unwrap_or(second.trim());
    (!first.trim().contains(',') && !second.contains(',')).then_some([first.trim(), second.trim()])
}

fn struct_pair_field_types(type_text: &str, structs: &[StructItem]) -> Option<Vec<String>> {
    let structure = find_struct_type(type_text, structs)?;
    if !structure.generic_parameters.is_empty()
        || !structure.repr_layout_supported
        || structure.fields.len() != 2
        || structure.fields.iter().any(|field| field.name.is_none())
    {
        return None;
    }
    let fields = structure
        .fields
        .iter()
        .map(|field| field.type_text.trim().to_owned())
        .collect::<Vec<_>>();
    (fields.iter().all(|ty| is_scalar_codegen_type(ty))).then_some(fields)
}

fn struct_return_field_types(type_text: &str, structs: &[StructItem]) -> Option<Vec<String>> {
    let structure = find_struct_type(type_text, structs)?;
    if !structure.generic_parameters.is_empty()
        || !structure.repr_layout_supported
        || structure.fields.len() < 2
        || structure.fields.iter().any(|field| field.name.is_none())
    {
        return None;
    }
    let fields = structure
        .fields
        .iter()
        .map(|field| field.type_text.trim().to_owned())
        .collect::<Vec<_>>();
    fields
        .iter()
        .all(|ty| aggregate_field_width(ty).is_some())
        .then_some(fields)
}

fn find_struct_type<'a>(type_text: &str, structs: &'a [StructItem]) -> Option<&'a StructItem> {
    let type_text = type_text.trim();
    if let Some(exact) = structs.iter().find(|structure| structure.name == type_text) {
        return Some(exact);
    }
    let short_name = type_text.rsplit("::").next()?;
    let mut matches = structs
        .iter()
        .filter(|structure| structure.name.rsplit("::").next() == Some(short_name));
    let found = matches.next()?;
    matches.next().is_none().then_some(found)
}

fn struct_parameter_type<'a>(type_text: &'a str) -> Option<&'a str> {
    referenced_type_name(type_text).or_else(|| {
        Some(
            type_text
                .trim()
                .strip_prefix("mut ")
                .unwrap_or(type_text.trim()),
        )
    })
}

fn find_struct_parameter<'a>(type_text: &str, structs: &'a [StructItem]) -> Option<&'a StructItem> {
    struct_parameter_type(type_text).and_then(|name| find_struct_type(name, structs))
}

fn referenced_type_name(type_text: &str) -> Option<&str> {
    let referent = type_text.trim().strip_prefix('&')?.trim_start();
    let referent = if referent.starts_with('\'') {
        referent.split_once(char::is_whitespace)?.1.trim_start()
    } else {
        referent
    };
    Some(referent.strip_prefix("mut ").unwrap_or(referent).trim())
}

fn is_codegen_parameter_type(type_text: &str, structs: &[StructItem]) -> bool {
    if is_slice_type(type_text) || is_scalar_codegen_type(type_text) {
        return true;
    }
    find_struct_parameter(type_text, structs).is_some_and(|structure| {
        structure.generic_parameters.is_empty()
            && structure.fields.iter().all(|field| {
                field.name.is_some()
                    && if type_text.trim_start().starts_with("&mut ") {
                        is_scalar_codegen_type(&field.type_text)
                            || is_slice_type(&field.type_text)
                            || is_collection_type(&field.type_text)
                    } else {
                        aggregate_field_width(&field.type_text).is_some()
                    }
            })
    })
}

fn requires_sret(field_types: &[String]) -> bool {
    field_types.len() > 2 || field_types.iter().any(|ty| !is_scalar_codegen_type(ty))
}

fn is_word_type(type_text: &str) -> bool {
    matches!(type_text.trim(), "u64" | "usize")
}

fn is_scalar_codegen_type(type_text: &str) -> bool {
    matches!(type_text.trim(), "i32" | "u64" | "usize")
}

fn is_collection_type(type_text: &str) -> bool {
    let type_text = type_text.trim();
    ["Vec<", "BTreeMap<", "HashMap<"].iter().any(|prefix| {
        type_text.starts_with(prefix)
            || type_text
                .rsplit("::")
                .next()
                .is_some_and(|tail| tail.starts_with(prefix))
    })
}

fn collection_element_type(type_text: &str) -> Option<String> {
    let type_text = type_text.trim();
    let open = type_text.find('<')?;
    let inner = type_text.get(open + 1..)?.strip_suffix('>')?.trim();
    (!inner.is_empty()).then(|| inner.to_owned())
}

fn collection_kind(type_text: &str) -> CollectionKind {
    let tail = type_text
        .trim()
        .rsplit("::")
        .next()
        .unwrap_or(type_text.trim());
    if tail == "String" {
        CollectionKind::StringBuffer
    } else if tail.starts_with("BTreeMap<") {
        CollectionKind::BTreeMap
    } else if tail.starts_with("HashMap<") {
        CollectionKind::HashMap
    } else {
        CollectionKind::Vector
    }
}

fn aggregate_field_width(type_text: &str) -> Option<usize> {
    if is_scalar_codegen_type(type_text) {
        Some(1)
    } else if is_slice_type(type_text) {
        Some(2)
    } else if is_collection_type(type_text) {
        Some(3)
    } else {
        None
    }
}

fn is_slice_type(type_text: &str) -> bool {
    let type_text = type_text.trim();
    (type_text.starts_with("&[") && type_text.ends_with(']'))
        || matches!(type_text, "&str" | "String")
        || type_text.starts_with("impl Into<String>")
}

fn slice_element_widths(type_text: &str) -> Vec<usize> {
    let type_text = type_text.trim();
    let element = type_text
        .strip_prefix("&[")
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or("");
    if let Some([first, second]) = pair_type_fields(element) {
        return [first, second].into_iter().map(slice_field_width).collect();
    }
    vec![slice_field_width(element)]
}

fn slice_record_type(type_text: &str, structs: &[StructItem]) -> Option<String> {
    let element_type = type_text
        .trim()
        .strip_prefix("&[")?
        .strip_suffix(']')?
        .trim();
    find_struct_type(element_type, structs).map(|record| record.name.clone())
}

fn record_field_layout(structure: &StructItem) -> Option<(Vec<(usize, usize)>, usize)> {
    let mut offset = 0usize;
    let mut fields = Vec::with_capacity(structure.fields.len());
    for field in &structure.fields {
        let width = aggregate_field_width(&field.type_text)?;
        fields.push((offset, width));
        offset = offset.checked_add(width.checked_mul(8)?)?;
    }
    Some((fields, offset))
}

fn slice_field_width(element: &str) -> usize {
    if element.trim() == "&str" {
        16
    } else {
        scalar_element_width(element)
    }
}

fn scalar_element_width(element: &str) -> usize {
    match element {
        "i32" => 4,
        "u64" | "usize" => 8,
        _ => 0,
    }
}

fn static_array_element_widths(elements: &[Value]) -> Vec<usize> {
    let Some(first) = elements.first() else {
        return Vec::new();
    };
    match first {
        Value::Tuple(fields) => fields.iter().map(static_array_field_width).collect(),
        scalar => vec![static_array_field_width(scalar)],
    }
}

fn coerce_annotated_integer_values(value: &mut Value, type_text: &str) {
    let type_text = type_text.trim();
    let scalar_type = type_text
        .strip_prefix('[')
        .and_then(|array| array.strip_suffix(']'))
        .and_then(|array| array.split_once(';'))
        .map_or(type_text, |(element, _)| element.trim());
    if !matches!(scalar_type, "u64" | "usize") {
        return;
    }
    let coerce = |value: &mut Value| {
        if let Value::Integer(integer) = value
            && *integer >= 0
        {
            *value = Value::TypedWord {
                value: *integer as u64,
                type_name: scalar_type.to_owned(),
            };
        }
    };
    match value {
        Value::StaticArray(elements) => elements.iter_mut().for_each(coerce),
        value => coerce(value),
    }
}

fn static_array_field_width(value: &Value) -> usize {
    match value {
        Value::TextSlice(_) => 16,
        _ if is_word_value(value) => 8,
        _ => 4,
    }
}

fn native_tuple_layout(widths: &[usize]) -> (Vec<usize>, usize) {
    let mut offset = 0usize;
    let mut alignment = 1usize;
    let mut offsets = Vec::with_capacity(widths.len());
    for width in widths {
        alignment = alignment.max(*width);
        offset = offset.div_ceil(*width) * width;
        offsets.push(offset);
        offset += width;
    }
    (offsets, offset.div_ceil(alignment) * alignment)
}

fn static_array_fields(value: &Value) -> Option<Vec<&Value>> {
    match value {
        Value::Integer(_) | Value::Word(_) | Value::TypedWord { .. } | Value::TextSlice(_) => {
            Some(vec![value])
        }
        Value::Tuple(fields) if fields.len() == 2 => Some(fields.iter().collect()),
        _ => None,
    }
}

fn parameter_slot_count(
    parameters: &[quill_syntax::ParameterItem],
    structs: &[StructItem],
) -> usize {
    parameters
        .iter()
        .map(|parameter| {
            if let Some(structure) = find_struct_parameter(&parameter.type_text, structs)
                .or_else(|| find_struct_type(&parameter.type_text, structs))
            {
                structure
                    .fields
                    .iter()
                    .map(|field| parameter_component_count(&field.type_text))
                    .sum()
            } else {
                usize::from(is_slice_type(&parameter.type_text)) + 1
            }
        })
        .sum()
}

fn parameter_component_count(type_text: &str) -> usize {
    if is_slice_type(type_text) {
        2
    } else if is_collection_type(type_text) {
        3
    } else {
        1
    }
}

fn parameter_component_word_kinds(type_text: &str, structs: &[StructItem]) -> Option<Vec<bool>> {
    if let Some(structure) = find_struct_parameter(type_text, structs) {
        if type_text.trim_start().starts_with("&mut ") {
            return structure
                .fields
                .iter()
                .map(|field| parameter_component_count(&field.type_text))
                .try_fold(Vec::new(), |mut kinds, count| {
                    kinds.extend(std::iter::repeat_n(true, count));
                    Some(kinds)
                });
        }
        let mut fields = structure.fields.iter().collect::<Vec<_>>();
        fields.sort_by(|left, right| left.name.cmp(&right.name));
        let mut words = Vec::new();
        for field in fields {
            if is_slice_type(&field.type_text) {
                words.extend([true, true]);
            } else if is_collection_type(&field.type_text) {
                words.extend([true, true, true]);
            } else if is_scalar_codegen_type(&field.type_text) {
                words.push(is_word_type(&field.type_text));
            } else {
                return None;
            }
        }
        Some(words)
    } else if is_slice_type(type_text) {
        Some(vec![true, true])
    } else if is_scalar_codegen_type(type_text) {
        Some(vec![is_word_type(type_text)])
    } else {
        None
    }
}

fn is_slice_value(value: &Value) -> bool {
    match value {
        Value::EmptySlice
        | Value::NicheOk
        | Value::StaticArray(_)
        | Value::TextSlice(_)
        | Value::SliceElementString { .. }
        | Value::BTreeMapGetString { .. }
        | Value::OwnedString(_)
        | Value::FormatString(_)
        | Value::SliceParameter { .. }
        | Value::SliceLocal { .. }
        | Value::SliceRecord { .. } => true,
        Value::CollectionLocal {
            kind: CollectionKind::StringBuffer,
            ..
        } => true,
        Value::If {
            then_value,
            else_value,
            ..
        } => is_slice_value(then_value) && is_slice_value(else_value),
        Value::Block { tail, .. } => is_slice_value(tail),
        Value::Call { returns_slice, .. } => *returns_slice,
        _ => false,
    }
}

fn main_exit_value(
    function: &FunctionItem,
    functions: &[FunctionItem],
    structs: &[StructItem],
) -> Result<Value, CodegenError> {
    let body = function
        .body
        .as_ref()
        .ok_or(CodegenError::UnsupportedProgram("main has no body"))?;
    let BodyExprKind::Block { statements, tail } = &body.kind else {
        return Err(CodegenError::UnsupportedProgram(
            "main body must be a block",
        ));
    };
    let explicit_exit = if let Some(tail) = tail {
        process_exit_argument(tail).map(|argument| (statements.as_slice(), argument))
    } else {
        statements.last().and_then(|statement| match statement {
            BodyStatement::Expression { expression, .. } => process_exit_argument(expression)
                .map(|argument| (&statements[..statements.len() - 1], argument)),
            _ => None,
        })
    };
    if let Some((prefix, argument)) = explicit_exit {
        let mut environment = HashMap::new();
        let mut mutable_names = HashSet::new();
        let mut next_slot = 0;
        let lowered = lower_statements(
            prefix,
            &mut environment,
            &mut mutable_names,
            &mut next_slot,
            functions,
            structs,
        )?;
        let exit_value = value_from_expression(argument, &environment, functions)?;
        return Ok(if lowered.is_empty() {
            exit_value
        } else {
            Value::Block {
                statements: lowered,
                tail: Box::new(exit_value),
            }
        });
    }
    let mut environment = HashMap::new();
    let mut mutable_names = HashSet::new();
    let mut next_slot = 0;
    let mut main_statements = statements.clone();
    if let Some(tail) = tail
        && !matches!(tail.kind, BodyExprKind::Unit)
    {
        if matches!(tail.kind, BodyExprKind::If { .. }) {
            main_statements.push(BodyStatement::Expression {
                expression: (**tail).clone(),
                has_semicolon: false,
            });
        } else {
            return Err(CodegenError::UnsupportedProgram(
                "main tail expression must be unit or `std::process::exit`",
            ));
        }
    }
    let lowered = lower_statements(
        &main_statements,
        &mut environment,
        &mut mutable_names,
        &mut next_slot,
        functions,
        structs,
    )?;
    Ok(if lowered.is_empty() {
        Value::Integer(0)
    } else {
        Value::Block {
            statements: lowered,
            tail: Box::new(Value::Integer(0)),
        }
    })
}

fn process_exit_argument(expression: &BodyExpr) -> Option<&BodyExpr> {
    let BodyExprKind::Call {
        callee, arguments, ..
    } = &expression.kind
    else {
        return None;
    };
    (callee == "std::process::exit" && arguments.len() == 1).then(|| &arguments[0])
}

fn helper_parameter_environment(
    function: &FunctionItem,
    structs: &[StructItem],
) -> Result<(HashMap<String, Value>, usize), CodegenError> {
    let mut environment = HashMap::with_capacity(function.parameters.len());
    let mut slot = 0;
    for parameter in &function.parameters {
        let name = parameter
            .name
            .as_deref()
            .ok_or(CodegenError::UnsupportedProgram(
                "helper parameter must be named",
            ))?;
        if let Some(structure) = find_struct_parameter(&parameter.type_text, structs) {
            let mutable_reference = parameter.type_text.trim_start().starts_with("&mut ");
            if mutable_reference
                && structure.fields.iter().any(|field| {
                    !is_scalar_codegen_type(&field.type_text)
                        && !is_slice_type(&field.type_text)
                        && !is_collection_type(&field.type_text)
                })
            {
                return Err(CodegenError::UnsupportedProgram(
                    "mutable struct references currently support scalar fields only",
                ));
            }
            let mut fields = HashMap::with_capacity(structure.fields.len());
            let mut ordered_fields = structure.fields.iter().collect::<Vec<_>>();
            ordered_fields.sort_by(|left, right| left.name.cmp(&right.name));
            for field in ordered_fields {
                let field_name = field
                    .name
                    .as_deref()
                    .ok_or(CodegenError::UnsupportedProgram(
                        "borrowed struct fields must be named",
                    ))?;
                let field_value = if mutable_reference
                    && !is_slice_type(&field.type_text)
                    && !is_collection_type(&field.type_text)
                {
                    let value = StructLocalField::IndirectScalar {
                        pointer_slot: slot,
                        is_word: is_word_type(&field.type_text),
                    };
                    slot += 1;
                    value
                } else if is_slice_type(&field.type_text) {
                    let value = StructLocalField::Slice {
                        pointer_slot: slot,
                        length_slot: slot + 1,
                    };
                    slot += 2;
                    value
                } else if is_collection_type(&field.type_text) {
                    let value = StructLocalField::Collection {
                        pointer_slot: slot,
                        length_slot: slot + 1,
                        capacity_slot: slot + 2,
                        kind: collection_kind(&field.type_text),
                        element_type: collection_element_type(&field.type_text),
                        indirect: mutable_reference,
                    };
                    slot += 3;
                    value
                } else {
                    let value = StructLocalField::Scalar {
                        slot,
                        is_word: is_word_type(&field.type_text),
                        type_name: Some(field.type_text.clone()),
                    };
                    slot += 1;
                    value
                };
                fields.insert(field_name.to_owned(), field_value);
            }
            environment.insert(
                name.to_owned(),
                Value::StructLocal {
                    type_name: structure.name.clone(),
                    fields,
                },
            );
            continue;
        }
        let value = if is_slice_type(&parameter.type_text) {
            let value = if let Some(record_type) = slice_record_type(&parameter.type_text, structs)
            {
                Value::SliceRecord {
                    pointer_slot: slot,
                    length_slot: slot + 1,
                    record_type,
                }
            } else {
                Value::SliceParameter {
                    pointer_slot: slot,
                    length_slot: slot + 1,
                    element_widths: slice_element_widths(&parameter.type_text),
                }
            };
            slot += 2;
            value
        } else {
            let value = if is_word_type(&parameter.type_text) {
                Value::TypedWordParameter {
                    slot,
                    type_name: parameter.type_text.clone(),
                }
            } else {
                Value::Parameter(slot)
            };
            slot += 1;
            value
        };
        environment.insert(name.to_owned(), value);
    }
    Ok((environment, slot))
}

fn value_from_struct_local_field(field: &StructLocalField) -> Value {
    match field {
        StructLocalField::Scalar {
            slot,
            is_word: true,
            type_name,
        } => type_name
            .as_ref()
            .map_or(Value::WordLocal(*slot), |type_name| Value::TypedWordLocal {
                slot: *slot,
                type_name: type_name.clone(),
            }),
        StructLocalField::Scalar {
            slot,
            is_word: false,
            ..
        } => Value::Local(*slot),
        StructLocalField::IndirectScalar {
            pointer_slot,
            is_word,
        } => Value::IndirectLocal {
            pointer_slot: *pointer_slot,
            is_word: *is_word,
        },
        StructLocalField::Slice {
            pointer_slot,
            length_slot,
        } => Value::SliceLocal {
            pointer_slot: *pointer_slot,
            length_slot: *length_slot,
            element_widths: vec![4],
        },
        StructLocalField::Collection {
            pointer_slot,
            length_slot,
            capacity_slot,
            kind,
            element_type,
            indirect,
        } => Value::CollectionLocal {
            pointer_slot: *pointer_slot,
            length_slot: *length_slot,
            capacity_slot: *capacity_slot,
            kind: *kind,
            element_type: element_type.clone(),
            indirect: *indirect,
        },
    }
}

fn address_of_struct_local_field(field: &StructLocalField) -> Result<Value, CodegenError> {
    match field {
        StructLocalField::Scalar { slot, .. } => Ok(Value::AddressOfLocal(*slot)),
        StructLocalField::Slice {
            pointer_slot,
            length_slot,
        } => Ok(Value::SliceLocal {
            pointer_slot: *pointer_slot,
            length_slot: *length_slot,
            element_widths: vec![4],
        }),
        StructLocalField::IndirectScalar { pointer_slot, .. } => {
            Ok(Value::WordLocal(*pointer_slot))
        }
        StructLocalField::Collection {
            pointer_slot,
            length_slot,
            capacity_slot,
            indirect,
            ..
        } => Ok(Value::CollectionAddress {
            pointer_slot: *pointer_slot,
            length_slot: *length_slot,
            capacity_slot: *capacity_slot,
            indirect: *indirect,
        }),
    }
}

fn helper_return_value(
    function: &FunctionItem,
    functions: &[FunctionItem],
    structs: &[StructItem],
) -> Result<Value, CodegenError> {
    let body = function
        .body
        .as_ref()
        .ok_or(CodegenError::UnsupportedProgram(
            "helper function has no body",
        ))?;
    match &body.kind {
        BodyExprKind::Block {
            statements,
            tail: Some(expression),
        } => {
            let (mut environment, parameter_slots) =
                helper_parameter_environment(function, structs)?;
            let mut mutable_names = function
                .parameters
                .iter()
                .filter(|parameter| {
                    parameter.type_text.trim_start().starts_with("mut ")
                        || parameter.type_text.trim_start().starts_with("&mut ")
                })
                .filter_map(|parameter| parameter.name.clone())
                .collect();
            let mut next_slot = parameter_slots;
            let lowered_statements = lower_statements(
                statements,
                &mut environment,
                &mut mutable_names,
                &mut next_slot,
                functions,
                structs,
            )?;
            let tail_value = if let Some(field_types) =
                function.return_type.as_deref().and_then(|return_type| {
                    struct_pair_field_types(return_type, structs).or_else(|| {
                        struct_return_field_types(return_type, structs)
                            .filter(|fields| requires_sret(fields))
                    })
                }) {
                let structure =
                    find_struct_type(function.return_type.as_deref().unwrap_or_default(), structs)
                        .expect("record return type was resolved above");
                let returned_local = match &expression.kind {
                    BodyExprKind::Path(name) => match environment.get(name) {
                        Some(Value::StructLocal { type_name, fields })
                            if type_name.rsplit("::").next()
                                == structure.name.rsplit("::").next() =>
                        {
                            Some(fields)
                        }
                        _ => None,
                    },
                    _ => None,
                };
                let literal_fields = match &expression.kind {
                    BodyExprKind::StructLiteral { path, fields }
                        if path.rsplit("::").next() == structure.name.rsplit("::").next() =>
                    {
                        Some(fields)
                    }
                    _ => None,
                };
                if returned_local.is_none() && literal_fields.is_none() {
                    return Err(CodegenError::UnsupportedProgram(
                        "record return must be a matching struct literal or local value",
                    ));
                }
                let mut values = Vec::with_capacity(field_types.len());
                for (field_index, declared_field) in structure.fields.iter().enumerate() {
                    let field_name =
                        declared_field
                            .name
                            .as_deref()
                            .ok_or(CodegenError::UnsupportedProgram(
                                "record return fields must be named",
                            ))?;
                    let mut value = if let Some(fields) = returned_local {
                        value_from_struct_local_field(fields.get(field_name).ok_or(
                            CodegenError::UnsupportedProgram(
                                "returned local is missing a declared record field",
                            ),
                        )?)
                    } else {
                        let field_expression = literal_fields
                            .and_then(|fields| fields.iter().find(|(name, _)| name == field_name))
                            .map(|(_, expression)| expression)
                            .ok_or(CodegenError::UnsupportedProgram(
                                "record return literal must initialize each field",
                            ))?;
                        value_from_expression(field_expression, &environment, functions)?
                    };
                    if is_word_type(&field_types[field_index])
                        && let Value::Integer(integer) = value
                    {
                        if integer < 0 {
                            return Err(CodegenError::UnsupportedProgram(
                                "record u64 field cannot be initialized by a negative integer",
                            ));
                        }
                        value = Value::Word(integer as u64);
                    }
                    values.push(value);
                }
                Value::Tuple(values)
            } else {
                if let Some(fold) = lower_slice_fold_expression(
                    expression,
                    &environment,
                    functions,
                    structs,
                    function
                        .return_type
                        .as_deref()
                        .and_then(pair_type_fields)
                        .map(|fields| [fields[0].to_owned(), fields[1].to_owned()]),
                    &mut next_slot,
                )? {
                    fold
                } else {
                    value_from_expression_with_structs(
                        expression,
                        &environment,
                        functions,
                        structs,
                    )?
                }
            };
            let mut value = if lowered_statements.is_empty() {
                tail_value
            } else {
                Value::Block {
                    statements: lowered_statements,
                    tail: Box::new(tail_value),
                }
            };
            if let Some(field_types) = function.return_type.as_deref().and_then(pair_type_fields) {
                let tuple = match &mut value {
                    Value::Tuple(fields) => Some(fields),
                    Value::Block { tail, .. } => match tail.as_mut() {
                        Value::Tuple(fields) => Some(fields),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(tuple) = tuple {
                    for (field, field_type) in tuple.iter_mut().zip(field_types) {
                        coerce_annotated_integer_values(field, field_type);
                    }
                }
            }
            if function.return_type.as_deref().is_some_and(is_slice_type) {
                if !is_slice_value(&value) {
                    return Err(CodegenError::UnsupportedProgram(
                        "string or slice helper must return a supported pointer/length value",
                    ));
                }
                if function.return_type.as_deref() == Some("String")
                    && !value_has_owned_string_tail(&value)
                {
                    return Err(CodegenError::UnsupportedProgram(
                        "a `String` helper must return an owned string value",
                    ));
                }
                return Ok(value);
            }
            if function
                .return_type
                .as_deref()
                .and_then(ResultLayout::parse)
                .is_some_and(|layout| {
                    matches!(layout.encoding, ResultEncoding::NullPointerNiche { .. })
                })
            {
                if !is_slice_value(&value) {
                    return Err(CodegenError::UnsupportedProgram(
                        "niche-encoded `Result` helper must return a string-slice value",
                    ));
                }
                return Ok(value);
            }
            if function
                .return_type
                .as_deref()
                .is_some_and(is_unit_option_type)
            {
                return Ok(value);
            }
            if function
                .return_type
                .as_deref()
                .is_some_and(is_i32_option_type)
            {
                return normalize_i32_option_return(value);
            }
            if function
                .return_type
                .as_deref()
                .is_some_and(is_native_tagged_result_type)
            {
                let payload_is_word = function
                    .return_type
                    .as_deref()
                    .is_some_and(tagged_result_payload_is_word);
                return normalize_tagged_result_return(value, payload_is_word);
            }
            if function.is_generic {
                let ty = function.return_type.as_deref().map(str::trim).unwrap_or("");
                if !matches!(&value, Value::Parameter(i) if function.parameters[*i].type_text.trim() == ty)
                {
                    return Err(CodegenError::UnsupportedProgram(
                        "generic helper must return a parameter with its declared return type",
                    ));
                }
            }
            if let Some(field_types) = function
                .return_type
                .as_deref()
                .and_then(pair_type_fields)
                .map(|[first, second]| vec![first.to_owned(), second.to_owned()])
                .or_else(|| {
                    function
                        .return_type
                        .as_deref()
                        .and_then(|return_type| struct_pair_field_types(return_type, structs))
                })
                .or_else(|| {
                    function
                        .return_type
                        .as_deref()
                        .and_then(|return_type| struct_return_field_types(return_type, structs))
                        .filter(|fields| requires_sret(fields))
                })
            {
                let tuple = match &value {
                    Value::Tuple(values) => Some(values.as_slice()),
                    Value::TupleSliceFold { initial, .. } => Some(initial.as_slice()),
                    Value::Block { tail, .. } => match tail.as_ref() {
                        Value::Tuple(values) => Some(values.as_slice()),
                        Value::TupleSliceFold { initial, .. } => Some(initial.as_slice()),
                        _ => None,
                    },
                    _ => None,
                }
                .ok_or(CodegenError::UnsupportedProgram(
                    "aggregate-return helper must end in a tuple expression",
                ))?;
                if tuple.len() != field_types.len()
                    || tuple.iter().zip(field_types).any(|(value, field_type)| {
                        is_slice_value(value) != is_slice_type(&field_type)
                            || (!is_slice_type(&field_type)
                                && is_word_value(value) != is_word_type(&field_type))
                    })
                {
                    return Err(CodegenError::UnsupportedProgram(
                        "tuple return values must match the declared field types",
                    ));
                }
            }
            Ok(value)
        }
        BodyExprKind::Block {
            statements,
            tail: None,
        } if function
            .return_type
            .as_deref()
            .is_none_or(|ty| ty.trim() == "()")
            || matches!(statements.last(), Some(BodyStatement::Expression { expression, .. }) if matches!(expression.kind, BodyExprKind::Return(Some(_)))) =>
        {
            let (mut environment, parameter_slots) =
                helper_parameter_environment(function, structs)?;
            let mut mutable_names = function
                .parameters
                .iter()
                .filter(|parameter| {
                    parameter.type_text.trim_start().starts_with("mut ")
                        || parameter.type_text.trim_start().starts_with("&mut ")
                })
                .filter_map(|parameter| parameter.name.clone())
                .collect();
            let mut next_slot = parameter_slots;
            let statements = lower_statements(
                statements,
                &mut environment,
                &mut mutable_names,
                &mut next_slot,
                functions,
                structs,
            )?;
            Ok(Value::Block {
                statements,
                tail: Box::new(Value::Integer(0)),
            })
        }
        _ => Err(CodegenError::UnsupportedProgram(
            "helper bodies must consist of one supported `i32` expression",
        )),
    }
}

fn lower_slice_fold_expression(
    expression: &BodyExpr,
    environment: &HashMap<String, Value>,
    functions: &[FunctionItem],
    structs: &[StructItem],
    expected_tuple_types: Option<[String; 2]>,
    next_slot: &mut usize,
) -> Result<Option<Value>, CodegenError> {
    let BodyExprKind::MethodCall {
        receiver,
        method,
        arguments,
    } = &expression.kind
    else {
        return Ok(None);
    };
    if method != "fold" {
        return Ok(None);
    }
    if arguments.len() != 2 {
        return Err(CodegenError::UnsupportedProgram(
            "`Iterator::fold` requires an initial value and a two-argument closure",
        ));
    }
    let BodyExprKind::MethodCall {
        receiver: source_expression,
        method: iterator_method,
        arguments: iterator_arguments,
    } = &receiver.kind
    else {
        return Ok(None);
    };
    if !matches!(iterator_method.as_str(), "bytes" | "iter") || !iterator_arguments.is_empty() {
        return Ok(None);
    }
    let BodyExprKind::Closure { parameters, body } = &arguments[1].kind else {
        return Err(CodegenError::UnsupportedProgram(
            "slice iterator fold requires a supported two-parameter closure",
        ));
    };
    let source = value_from_expression(source_expression, environment, functions)?;
    let mut initial = value_from_expression(&arguments[0], environment, functions)?;
    if let (Some(expected_types), Value::Tuple(values)) = (&expected_tuple_types, &mut initial) {
        for (value, type_name) in values.iter_mut().zip(expected_types) {
            if matches!(type_name.as_str(), "usize" | "u64")
                && let Value::Integer(integer) = value
                && *integer >= 0
            {
                *value = Value::TypedWord {
                    value: *integer as u64,
                    type_name: type_name.clone(),
                };
            }
        }
    }
    let record_structure = match (&iterator_method[..], &source) {
        ("iter", Value::SliceRecord { record_type, .. }) => find_struct_type(record_type, structs),
        _ => None,
    };
    let record_layout = record_structure.and_then(record_field_layout);
    let element_size = match (iterator_method.as_str(), &source) {
        ("bytes", value) if is_slice_value(value) => 1,
        (
            "iter",
            Value::SliceParameter { element_widths, .. } | Value::SliceLocal { element_widths, .. },
        ) if element_widths.len() == 1 && matches!(element_widths[0], 4 | 8) => element_widths[0],
        ("iter", Value::StaticArray(elements)) => {
            let widths = static_array_element_widths(elements);
            if widths.len() == 1 && matches!(widths[0], 4 | 8) {
                widths[0]
            } else {
                0
            }
        }
        ("iter", Value::SliceRecord { .. }) => {
            record_layout.as_ref().map_or(0, |(_, stride)| *stride)
        }
        _ => 0,
    };
    if element_size == 0 || !is_word_value(&initial) {
        if element_size == 0 {
            return Err(CodegenError::UnsupportedProgram(
                "slice iterator fold requires scalar integer elements",
            ));
        }
    }
    if let (
        [
            ClosureParameter::Tuple(accumulator_names),
            ClosureParameter::Identifier(item_name),
        ],
        Value::Tuple(initial_values),
    ) = (parameters.as_slice(), &initial)
    {
        if accumulator_names.len() != 2
            || initial_values.len() != 2
            || initial_values
                .iter()
                .any(|value| !is_word_value(value) && !matches!(value, Value::Integer(_)))
        {
            return Err(CodegenError::UnsupportedProgram(
                "tuple slice fold currently requires two word accumulator fields",
            ));
        }
        let pointer_slot = *next_slot;
        let length_slot = pointer_slot + 1;
        let index_slot = pointer_slot + 2;
        let accumulator_slots = vec![pointer_slot + 3, pointer_slot + 4];
        let update_slots = vec![pointer_slot + 5, pointer_slot + 6];
        let item_slot = pointer_slot + 7;
        *next_slot += 8;
        let mut closure_environment = environment.clone();
        for ((name, slot), initial_value) in accumulator_names
            .iter()
            .zip(&accumulator_slots)
            .zip(initial_values)
        {
            closure_environment.insert(
                name.clone(),
                if is_word_value(initial_value) {
                    let type_name = match initial_value {
                        Value::TypedWord { type_name, .. } => type_name.clone(),
                        _ => "u64".to_owned(),
                    };
                    Value::TypedWordLocal {
                        slot: *slot,
                        type_name,
                    }
                } else {
                    Value::Local(*slot)
                },
            );
        }
        let mut record_fields = Vec::new();
        if let (Value::SliceRecord { pointer_slot, .. }, Some(structure), Some((layout, stride))) =
            (&source, record_structure, record_layout)
        {
            let mut fields = HashMap::with_capacity(structure.fields.len());
            for (field, (offset, width)) in structure.fields.iter().zip(layout) {
                let field_name = field
                    .name
                    .as_deref()
                    .ok_or(CodegenError::UnsupportedProgram(
                        "slice records require named fields",
                    ))?;
                let slot = *next_slot;
                match width {
                    1 => {
                        *next_slot += 1;
                        fields.insert(
                            field_name.to_owned(),
                            StructLocalField::Scalar {
                                slot,
                                is_word: is_word_type(&field.type_text),
                                type_name: Some(field.type_text.clone()),
                            },
                        );
                        record_fields.push((
                            slot,
                            Value::SliceElement {
                                pointer_slot: *pointer_slot,
                                index_slot,
                                element_size: if is_word_type(&field.type_text) { 8 } else { 4 },
                                element_offset: offset,
                                element_stride: stride,
                            },
                            1,
                        ));
                    }
                    2 => {
                        *next_slot += 2;
                        fields.insert(
                            field_name.to_owned(),
                            StructLocalField::Slice {
                                pointer_slot: slot,
                                length_slot: slot + 1,
                            },
                        );
                        record_fields.push((
                            slot,
                            Value::SliceElementString {
                                pointer_slot: *pointer_slot,
                                index_slot,
                                element_offset: offset,
                                element_stride: stride,
                            },
                            2,
                        ));
                    }
                    3 => {
                        *next_slot += 3;
                        fields.insert(
                            field_name.to_owned(),
                            StructLocalField::Collection {
                                pointer_slot: slot,
                                length_slot: slot + 1,
                                capacity_slot: slot + 2,
                                kind: collection_kind(&field.type_text),
                                element_type: collection_element_type(&field.type_text),
                                indirect: false,
                            },
                        );
                        for component in 0..3 {
                            record_fields.push((
                                slot + component,
                                Value::SliceElement {
                                    pointer_slot: *pointer_slot,
                                    index_slot,
                                    element_size: 8,
                                    element_offset: offset + component * 8,
                                    element_stride: stride,
                                },
                                1,
                            ));
                        }
                    }
                    _ => unreachable!("record layout only contains supported field widths"),
                }
            }
            closure_environment.insert(
                item_name.clone(),
                Value::StructLocal {
                    type_name: structure.name.clone(),
                    fields,
                },
            );
        } else {
            closure_environment.insert(
                item_name.clone(),
                if element_size == 8 {
                    Value::WordLocal(item_slot)
                } else {
                    Value::Local(item_slot)
                },
            );
        }
        let Value::Tuple(updates) = value_from_expression(body, &closure_environment, functions)?
        else {
            return Err(CodegenError::UnsupportedProgram(
                "tuple slice fold closure must return a two-field tuple",
            ));
        };
        if updates.len() != 2
            || updates.iter().zip(initial_values).any(|(update, initial)| {
                is_word_value(update) != is_word_value(initial)
                    || (!is_word_value(update)
                        && !matches!(update, Value::Integer(_) | Value::Binary { .. }))
            })
        {
            return Err(CodegenError::UnsupportedProgram(
                "tuple slice fold closure must preserve two scalar accumulator types",
            ));
        }
        return Ok(Some(Value::TupleSliceFold {
            source: Box::new(source),
            initial: initial_values.clone(),
            updates,
            element_size,
            pointer_slot,
            length_slot,
            index_slot,
            accumulator_slots,
            update_slots,
            item_slot,
            record_fields,
        }));
    }
    let [
        ClosureParameter::Identifier(accumulator_name),
        ClosureParameter::Identifier(item_name),
    ] = parameters.as_slice()
    else {
        return Err(CodegenError::UnsupportedProgram(
            "slice iterator fold closure parameters must be identifiers",
        ));
    };
    if !is_word_value(&initial) {
        return Err(CodegenError::UnsupportedProgram(
            "slice iterator fold requires a word accumulator",
        ));
    }
    let pointer_slot = *next_slot;
    let length_slot = pointer_slot + 1;
    let index_slot = pointer_slot + 2;
    let accumulator_slot = pointer_slot + 3;
    let item_slot = pointer_slot + 4;
    *next_slot += 5;
    let mut closure_environment = environment.clone();
    closure_environment.insert(
        accumulator_name.clone(),
        Value::TypedWordLocal {
            slot: accumulator_slot,
            type_name: "u64".to_owned(),
        },
    );
    closure_environment.insert(
        item_name.clone(),
        if element_size == 8 {
            Value::WordLocal(item_slot)
        } else {
            Value::Local(item_slot)
        },
    );
    let update = value_from_expression(body, &closure_environment, functions)?;
    if !is_word_value(&update) {
        return Err(CodegenError::UnsupportedProgram(
            "slice iterator fold closure must return a u64 accumulator",
        ));
    }
    Ok(Some(Value::SliceFold {
        source: Box::new(source),
        initial: Box::new(initial),
        update: Box::new(update),
        element_size,
        pointer_slot,
        length_slot,
        index_slot,
        accumulator_slot,
        item_slot,
    }))
}

fn value_has_owned_string_tail(value: &Value) -> bool {
    match value {
        Value::OwnedString(_) | Value::FormatString(_) => true,
        Value::CollectionLocal {
            kind: CollectionKind::StringBuffer,
            ..
        } => true,
        Value::Block { tail, .. } => value_has_owned_string_tail(tail),
        _ => false,
    }
}

fn try_result_value(
    expression: &BodyExpr,
    environment: &HashMap<String, Value>,
    functions: &[FunctionItem],
) -> Result<(Value, ResultLayout), CodegenError> {
    let BodyExprKind::Call { callee, .. } = &expression.kind else {
        return Err(CodegenError::UnsupportedProgram(
            "? currently requires a direct call returning niche-encoded Result",
        ));
    };
    let function = functions
        .iter()
        .find(|function| function.name == *callee)
        .ok_or(CodegenError::UnsupportedProgram(
            "? call target must be a local function",
        ))?;
    let layout = function
        .return_type
        .as_deref()
        .and_then(ResultLayout::parse)
        .filter(|layout| {
            matches!(layout.encoding, ResultEncoding::NullPointerNiche { .. })
                || is_native_tagged_result_type(function.return_type.as_deref().unwrap_or(""))
        })
        .ok_or(CodegenError::UnsupportedProgram(
            "? currently requires a direct call returning niche-encoded Result",
        ))?;
    let value = value_from_expression(expression, environment, functions)?;
    Ok((value, layout))
}

fn is_unit_option_type(type_text: &str) -> bool {
    let normalized = type_text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    matches!(
        normalized.as_str(),
        "Option<()>" | "std::option::Option<()>"
    )
}

fn is_i32_option_type(type_text: &str) -> bool {
    let normalized = type_text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    matches!(
        normalized.as_str(),
        "Option<i32>" | "std::option::Option<i32>"
    )
}

fn is_native_tagged_result_type(type_text: &str) -> bool {
    let normalized = type_text
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    let Some((path, arguments)) = normalized.split_once('<') else {
        return false;
    };
    if !matches!(
        path,
        "Result" | "std::result::Result" | "core::result::Result"
    ) {
        return false;
    }
    let Some(arguments) = arguments.strip_suffix('>') else {
        return false;
    };
    let Some((ok_type, err_type)) = arguments.split_once(',') else {
        return false;
    };
    let supported = |ty: &str| matches!(ty, "i32" | "u64" | "usize");
    let same_register_class = (ok_type == "i32" && err_type == "i32")
        || (matches!(ok_type, "u64" | "usize") && matches!(err_type, "u64" | "usize"));
    supported(ok_type)
        && supported(err_type)
        && same_register_class
        && ResultLayout::parse(type_text).is_some_and(|layout| {
            matches!(layout.encoding, ResultEncoding::ExplicitTag { .. }) && !layout.is_indirect()
        })
}

fn is_null_pointer_result_type(type_text: &str) -> bool {
    ResultLayout::parse(type_text)
        .is_some_and(|layout| matches!(layout.encoding, ResultEncoding::NullPointerNiche { .. }))
}

fn tagged_result_payload_is_word(type_text: &str) -> bool {
    let normalized = type_text
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    normalized
        .split_once('<')
        .and_then(|(_, args)| args.strip_suffix('>'))
        .and_then(|args| args.split_once(','))
        .is_some_and(|(ok, _)| matches!(ok, "u64" | "usize"))
}

fn is_option_some_path(path: &str) -> bool {
    matches!(path, "Some" | "Option::Some" | "std::option::Option::Some")
}

fn is_option_none_path(path: &str) -> bool {
    matches!(path, "None" | "Option::None" | "std::option::Option::None")
}

fn try_option_value(
    expression: &BodyExpr,
    environment: &HashMap<String, Value>,
    functions: &[FunctionItem],
) -> Result<Value, CodegenError> {
    let BodyExprKind::Call { callee, .. } = &expression.kind else {
        return Err(CodegenError::UnsupportedProgram(
            "Option `?` currently requires a direct call returning Option<()>.",
        ));
    };
    let supported = functions
        .iter()
        .find(|function| function.name == *callee)
        .and_then(|function| function.return_type.as_deref())
        .is_some_and(|return_type| {
            is_unit_option_type(return_type) || is_i32_option_type(return_type)
        });
    if !supported {
        return Err(CodegenError::UnsupportedProgram(
            "Option `?` currently requires a direct call returning Option<()>.",
        ));
    }
    value_from_expression(expression, environment, functions)
}

fn is_i32_option_call(expression: &BodyExpr, functions: &[FunctionItem]) -> bool {
    let BodyExprKind::Call { callee, .. } = &expression.kind else {
        return false;
    };
    functions
        .iter()
        .find(|function| function.name == *callee)
        .and_then(|function| function.return_type.as_deref())
        .is_some_and(is_i32_option_type)
}

fn is_native_tagged_result_call(expression: &BodyExpr, functions: &[FunctionItem]) -> bool {
    let Some(callee) = direct_call_callee(expression) else {
        return false;
    };
    functions
        .iter()
        .find(|function| function.name == callee)
        .and_then(|function| function.return_type.as_deref())
        .is_some_and(is_native_tagged_result_type)
}

fn direct_call_callee(expression: &BodyExpr) -> Option<&str> {
    match &expression.kind {
        BodyExprKind::Call { callee, .. } => Some(callee),
        BodyExprKind::Parenthesized(inner) => direct_call_callee(inner),
        _ => None,
    }
}

fn receiver_call_name<'a>(
    expression: &'a BodyExpr,
    environment: &'a HashMap<String, Value>,
) -> Option<&'a str> {
    direct_call_callee(expression).or_else(|| {
        let BodyExprKind::Path(local) = &expression.kind else {
            return None;
        };
        let Some(Value::Call { name, .. }) = environment.get(local) else {
            return None;
        };
        Some(name)
    })
}

fn normalize_i32_option_return(value: Value) -> Result<Value, CodegenError> {
    match value {
        Value::Integer(0) => Ok(Value::Tuple(vec![Value::Integer(0), Value::Integer(0)])),
        Value::Tuple(fields)
            if fields.len() == 2
                && matches!(fields[0], Value::Integer(0 | 1))
                && !is_word_value(&fields[1])
                && !is_slice_value(&fields[1]) =>
        {
            Ok(Value::Tuple(fields))
        }
        call @ Value::Call {
            returns_pair: true, ..
        } => Ok(call),
        Value::Block { statements, tail } => Ok(Value::Block {
            statements,
            tail: Box::new(normalize_i32_option_return(*tail)?),
        }),
        _ => Err(CodegenError::UnsupportedProgram(
            "Option<i32> return must be Some(i32), None, or a direct Option<i32> call",
        )),
    }
}

fn normalize_tagged_result_return(
    value: Value,
    payload_is_word: bool,
) -> Result<Value, CodegenError> {
    match value {
        Value::Tuple(fields)
            if fields.len() == 2
                && matches!(fields[0], Value::Integer(0 | 1))
                && if payload_is_word {
                    is_word_value(&fields[1])
                } else {
                    !is_word_value(&fields[1]) && !is_slice_value(&fields[1])
                } =>
        {
            Ok(Value::Tuple(fields))
        }
        call @ Value::Call {
            returns_pair: true, ..
        } => Ok(call),
        Value::Block { statements, tail } => Ok(Value::Block {
            statements,
            tail: Box::new(normalize_tagged_result_return(*tail, payload_is_word)?),
        }),
        _ => Err(CodegenError::UnsupportedProgram(
            "tagged Result return requires a supported scalar payload or a direct Result call",
        )),
    }
}

fn lower_statements(
    statements: &[BodyStatement],
    environment: &mut HashMap<String, Value>,
    mutable_names: &mut HashSet<String>,
    next_slot: &mut usize,
    functions: &[FunctionItem],
    structs: &[StructItem],
) -> Result<Vec<Statement>, CodegenError> {
    let mut lowered = Vec::with_capacity(statements.len());
    for statement in statements {
        if let BodyStatement::Let {
            name: Some(name),
            type_text,
            mutable,
            initializer: Some(initializer),
            span,
        } = statement
            && let BodyExprKind::Block {
                statements: block_statements,
                tail: Some(tail),
            } = &initializer.kind
        {
            let mut block_environment = environment.clone();
            let mut block_mutable_names = mutable_names.clone();
            lowered.extend(lower_statements(
                block_statements,
                &mut block_environment,
                &mut block_mutable_names,
                next_slot,
                functions,
                structs,
            )?);
            let result_binding = BodyStatement::Let {
                name: Some(name.clone()),
                type_text: type_text.clone(),
                mutable: *mutable,
                initializer: Some((**tail).clone()),
                span: span.clone(),
            };
            lowered.extend(lower_statements(
                std::slice::from_ref(&result_binding),
                &mut block_environment,
                &mut block_mutable_names,
                next_slot,
                functions,
                structs,
            )?);
            if let Some(value) = block_environment.get(name) {
                environment.insert(name.clone(), value.clone());
            }
            if *mutable {
                mutable_names.insert(name.clone());
            } else {
                mutable_names.remove(name);
            }
            continue;
        }
        if let BodyStatement::Let {
            name: Some(name),
            mutable,
            initializer: Some(initializer),
            span,
            ..
        } = statement
        {
            let mut rewritten_initializer = initializer.clone();
            let mut receiver_temporaries = Vec::new();
            materialize_method_receivers(
                &mut rewritten_initializer,
                &mut receiver_temporaries,
                *next_slot,
            );
            if !name.starts_with("__quill_vec_index_") {
                materialize_vector_indices(
                    &mut rewritten_initializer,
                    environment,
                    &mut receiver_temporaries,
                    *next_slot,
                    name,
                );
            }
            if !receiver_temporaries.is_empty() {
                lowered.extend(lower_statements(
                    &receiver_temporaries,
                    environment,
                    mutable_names,
                    next_slot,
                    functions,
                    structs,
                )?);
                let rewritten = BodyStatement::Let {
                    name: Some(name.clone()),
                    type_text: None,
                    mutable: *mutable,
                    initializer: Some(rewritten_initializer),
                    span: span.clone(),
                };
                lowered.extend(lower_statements(
                    std::slice::from_ref(&rewritten),
                    environment,
                    mutable_names,
                    next_slot,
                    functions,
                    structs,
                )?);
                continue;
            }
        }
        match statement {
            BodyStatement::Let {
                name: Some(name),
                type_text,
                mutable,
                initializer: Some(initializer),
                ..
            } => {
                if let BodyExprKind::MethodCall {
                    receiver,
                    method,
                    arguments,
                } = &initializer.kind
                    && method == "clone"
                    && arguments.is_empty()
                    && let BodyExprKind::Path(receiver_name) = &receiver.kind
                    && let Some(Value::StructLocal { type_name, fields }) =
                        environment.get(receiver_name)
                    && let Some(structure) = find_struct_type(type_name, structs)
                    && structure.derived_traits.iter().any(|name| name == "Clone")
                    && structure.fields.iter().all(|field| {
                        field.name.as_deref().is_some_and(|name| {
                            matches!(fields.get(name), Some(StructLocalField::Scalar { .. }))
                        })
                    })
                {
                    let mut cloned_fields = HashMap::with_capacity(structure.fields.len());
                    for field in &structure.fields {
                        let field_name = field.name.as_deref().expect("validated named field");
                        let source = fields.get(field_name).expect("field layout was validated");
                        let slot = *next_slot;
                        *next_slot += 1;
                        let value = value_from_struct_local_field(source);
                        cloned_fields.insert(
                            field_name.to_owned(),
                            StructLocalField::Scalar {
                                slot,
                                is_word: is_word_type(&field.type_text),
                                type_name: Some(field.type_text.clone()),
                            },
                        );
                        lowered.push(Statement::Let {
                            slot,
                            initializer: value,
                        });
                    }
                    environment.insert(
                        name.clone(),
                        Value::StructLocal {
                            type_name: type_name.clone(),
                            fields: cloned_fields,
                        },
                    );
                    if *mutable {
                        mutable_names.insert(name.clone());
                    } else {
                        mutable_names.remove(name);
                    }
                    continue;
                }
                if let BodyExprKind::MethodCall {
                    receiver,
                    method,
                    arguments,
                } = &initializer.kind
                    && method == "clone"
                    && arguments.is_empty()
                    && let BodyExprKind::Index { base, index, .. } = &receiver.kind
                    && let BodyExprKind::Path(collection_name) = &base.kind
                    && let Some(Value::CollectionLocal {
                        pointer_slot,
                        kind: CollectionKind::Vector,
                        element_type: Some(element_type),
                        ..
                    }) = environment.get(collection_name)
                    && element_type.rsplit("::").next() == Some("String")
                {
                    let index = match &index.kind {
                        BodyExprKind::Literal(LiteralKind::Integer { text, .. }) => {
                            text.replace('_', "").parse::<usize>().map_err(|_| {
                                CodegenError::UnsupportedProgram(
                                    "vector string index must be a constant target-sized integer",
                                )
                            })?
                        }
                        _ => {
                            return Err(CodegenError::UnsupportedProgram(
                                "vector string index must be a constant target-sized integer",
                            ));
                        }
                    };
                    let collection_pointer_slot = *pointer_slot;
                    let index_slot = *next_slot;
                    *next_slot += 1;
                    lowered.push(Statement::Let {
                        slot: index_slot,
                        initializer: Value::Word(index as u64),
                    });
                    let value_pointer_slot = *next_slot;
                    let value_length_slot = value_pointer_slot + 1;
                    *next_slot += 2;
                    environment.insert(
                        name.clone(),
                        Value::SliceLocal {
                            pointer_slot: value_pointer_slot,
                            length_slot: value_length_slot,
                            element_widths: vec![16],
                        },
                    );
                    lowered.push(Statement::LetSlice {
                        pointer_slot: value_pointer_slot,
                        length_slot: value_length_slot,
                        initializer: Value::SliceElementString {
                            pointer_slot: collection_pointer_slot,
                            index_slot,
                            element_offset: 0,
                            element_stride: 24,
                        },
                    });
                    if *mutable {
                        mutable_names.insert(name.clone());
                    } else {
                        mutable_names.remove(name);
                    }
                    continue;
                }
                if type_text.as_deref().is_some_and(is_i32_option_type) {
                    let option_tag = match &initializer.kind {
                        BodyExprKind::Call {
                            callee, arguments, ..
                        } if is_option_some_path(callee)
                            && arguments.len() == 1
                            && matches!(
                                arguments[0].kind,
                                BodyExprKind::Literal(LiteralKind::Integer { .. })
                            ) =>
                        {
                            Some(1)
                        }
                        BodyExprKind::Path(path) if is_option_none_path(path) => Some(0),
                        _ => None,
                    };
                    if let Some(option_tag) = option_tag {
                        let slot = *next_slot;
                        *next_slot += 1;
                        environment.insert(
                            name.clone(),
                            Value::OptionIsSome(Box::new(Value::Local(slot))),
                        );
                        if *mutable {
                            mutable_names.insert(name.clone());
                        } else {
                            mutable_names.remove(name);
                        }
                        lowered.push(Statement::Let {
                            slot,
                            initializer: Value::Integer(option_tag),
                        });
                        continue;
                    }
                }
                if matches!(&initializer.kind, BodyExprKind::Call { .. })
                    && let Some(function) =
                        function_for_call_expression(initializer, environment, functions)
                    && let Some(field_types) =
                        function.return_type.as_deref().and_then(pair_type_fields)
                    && field_types
                        .iter()
                        .all(|field_type| *field_type == "i32" || is_word_type(field_type))
                {
                    let value = value_from_expression(initializer, environment, functions)?;
                    if !matches!(
                        &value,
                        Value::Call {
                            returns_pair: true,
                            ..
                        }
                    ) {
                        return Err(CodegenError::UnsupportedProgram(
                            "tuple local initializer must be a direct pair-returning call",
                        ));
                    }
                    let mut fields = Vec::with_capacity(2);
                    let mut call_fields = Vec::with_capacity(2);
                    for field_type in field_types {
                        let slot = *next_slot;
                        *next_slot += 1;
                        let is_word = is_word_type(field_type);
                        fields.push(if is_word {
                            Value::TypedWordLocal {
                                slot,
                                type_name: field_type.to_owned(),
                            }
                        } else {
                            Value::Local(slot)
                        });
                        call_fields.push((slot, is_word));
                    }
                    environment.insert(name.clone(), Value::Tuple(fields));
                    if *mutable {
                        mutable_names.insert(name.clone());
                    } else {
                        mutable_names.remove(name);
                    }
                    lowered.push(Statement::LetTupleCall {
                        fields: call_fields,
                        initializer: value,
                    });
                    continue;
                }
                if let BodyExprKind::Index {
                    base,
                    index,
                    source_path,
                    line,
                    column,
                } = &initializer.kind
                    && let BodyExprKind::Path(collection_name) = &base.kind
                    && let Some(Value::CollectionLocal {
                        pointer_slot,
                        length_slot,
                        kind: CollectionKind::Vector,
                        element_type: Some(element_type),
                        ..
                    }) = environment.get(collection_name)
                    && (is_word_type(element_type) || element_type == "i32")
                {
                    let index = match value_from_expression(index, environment, functions)? {
                        Value::Integer(index) if index >= 0 => Value::Word(index as u64),
                        value if is_word_value(&value) => value,
                        _ => {
                            return Err(CodegenError::UnsupportedProgram(
                                "native vector indexing requires a target-sized unsigned index",
                            ));
                        }
                    };
                    let index_slot = *next_slot;
                    *next_slot += 1;
                    lowered.push(Statement::Let {
                        slot: index_slot,
                        initializer: index,
                    });
                    let panic = format!(
                        "\nthread 'main' (0) panicked at {source_path}:{line}:{column}:\nindex out of bounds: the len is "
                    );
                    lowered.push(Statement::If {
                        condition: Condition::Compare {
                            operator: BinaryOperator::GreaterEqual,
                            left: Box::new(Value::WordLocal(index_slot)),
                            right: Box::new(Value::WordLocal(*length_slot)),
                        },
                        then_body: vec![Statement::Println(vec![
                            PrintPart::PanicText(panic),
                            PrintPart::PanicDecimal(Value::WordLocal(*length_slot)),
                            PrintPart::PanicText(" but the index is ".to_owned()),
                            PrintPart::PanicDecimal(Value::WordLocal(index_slot)),
                            PrintPart::Panic(
                                "\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n"
                                    .to_owned(),
                            ),
                        ])],
                        else_body: Vec::new(),
                    });
                    let value = Value::SliceElement {
                        pointer_slot: *pointer_slot,
                        index_slot,
                        element_size: if is_word_type(element_type) { 8 } else { 4 },
                        element_offset: 0,
                        element_stride: 8,
                    };
                    let slot = *next_slot;
                    *next_slot += 1;
                    environment.insert(
                        name.clone(),
                        if is_word_type(element_type) {
                            Value::TypedWordLocal {
                                slot,
                                type_name: element_type.clone(),
                            }
                        } else {
                            Value::Local(slot)
                        },
                    );
                    if *mutable {
                        mutable_names.insert(name.clone());
                    } else {
                        mutable_names.remove(name);
                    }
                    lowered.push(Statement::Let {
                        slot,
                        initializer: value,
                    });
                    continue;
                }
                if let Some(function) =
                    function_for_call_expression(initializer, environment, functions)
                    && let Some(return_type) = function.return_type.as_deref()
                    && let Some(field_types) = struct_return_field_types(return_type, structs)
                {
                    let structure = find_struct_type(return_type, structs).ok_or(
                        CodegenError::UnsupportedProgram("record return type is unresolved"),
                    )?;
                    let mut value = value_from_expression(initializer, environment, functions)?;
                    let Value::Call {
                        returns_pair,
                        returns_aggregate,
                        aggregate_destination,
                        ..
                    } = &mut value
                    else {
                        return Err(CodegenError::UnsupportedProgram(
                            "record local initializer must be a direct function call",
                        ));
                    };
                    *returns_aggregate = requires_sret(&field_types);
                    *returns_pair = field_types.len() == 2 && !*returns_aggregate;
                    let mut field_slots = HashMap::with_capacity(field_types.len());
                    let mut call_fields = Vec::with_capacity(field_types.len());
                    for (field_index, field) in structure.fields.iter().enumerate() {
                        let field_name =
                            field
                                .name
                                .as_deref()
                                .ok_or(CodegenError::UnsupportedProgram(
                                    "record fields must be named",
                                ))?;
                        let field_type = &field_types[field_index];
                        let slot = *next_slot;
                        match aggregate_field_width(field_type).expect("validated aggregate field")
                        {
                            2 => {
                                let length_slot = slot + 1;
                                *next_slot += 2;
                                field_slots.insert(
                                    field_name.to_owned(),
                                    StructLocalField::Slice {
                                        pointer_slot: slot,
                                        length_slot,
                                    },
                                );
                                call_fields.push((slot, false));
                                call_fields.push((length_slot, false));
                            }
                            3 => {
                                let length_slot = slot + 1;
                                let capacity_slot = slot + 2;
                                *next_slot += 3;
                                field_slots.insert(
                                    field_name.to_owned(),
                                    StructLocalField::Collection {
                                        pointer_slot: slot,
                                        length_slot,
                                        capacity_slot,
                                        kind: collection_kind(field_type),
                                        element_type: collection_element_type(field_type),
                                        indirect: false,
                                    },
                                );
                                call_fields.push((slot, false));
                                call_fields.push((length_slot, false));
                                call_fields.push((capacity_slot, false));
                            }
                            _ => {
                                *next_slot += 1;
                                let word = is_word_type(field_type);
                                field_slots.insert(
                                    field_name.to_owned(),
                                    StructLocalField::Scalar {
                                        slot,
                                        is_word: word,
                                        type_name: Some(field_type.clone()),
                                    },
                                );
                                call_fields.push((slot, word));
                            }
                        }
                    }
                    if *returns_aggregate {
                        *aggregate_destination = call_fields.first().map(|(slot, _)| *slot);
                    }
                    environment.insert(
                        name.clone(),
                        Value::StructLocal {
                            type_name: structure.name.clone(),
                            fields: field_slots,
                        },
                    );
                    if *mutable {
                        mutable_names.insert(name.clone());
                    } else {
                        mutable_names.remove(name);
                    }
                    lowered.push(Statement::LetTupleCall {
                        fields: call_fields,
                        initializer: value,
                    });
                    continue;
                }
                if let BodyExprKind::StructLiteral { path, fields } = &initializer.kind {
                    let mut field_slots = HashMap::with_capacity(fields.len());
                    let mut lowered_fields = Vec::with_capacity(fields.len());
                    for (field, expression) in fields {
                        let slot = *next_slot;
                        let mut value = value_from_expression(expression, environment, functions)?;
                        if let Some(field_type) = find_struct_type(path, structs)
                            .and_then(|structure| {
                                structure
                                    .fields
                                    .iter()
                                    .find(|definition| definition.name.as_deref() == Some(field))
                            })
                            .map(|definition| definition.type_text.as_str())
                        {
                            coerce_annotated_integer_values(&mut value, field_type);
                        }
                        let field_local = if matches!(&value, Value::EmptyCollection) {
                            *next_slot += 3;
                            StructLocalField::Collection {
                                pointer_slot: slot,
                                length_slot: slot + 1,
                                capacity_slot: slot + 2,
                                kind: find_struct_type(path, structs)
                                    .and_then(|structure| {
                                        structure.fields.iter().find(|definition| {
                                            definition.name.as_deref() == Some(field)
                                        })
                                    })
                                    .map_or(CollectionKind::Vector, |definition| {
                                        collection_kind(&definition.type_text)
                                    }),
                                element_type: find_struct_type(path, structs)
                                    .and_then(|structure| {
                                        structure.fields.iter().find(|definition| {
                                            definition.name.as_deref() == Some(field)
                                        })
                                    })
                                    .and_then(|definition| {
                                        collection_element_type(&definition.type_text)
                                    }),
                                indirect: false,
                            }
                        } else if is_slice_value(&value) {
                            *next_slot += 2;
                            StructLocalField::Slice {
                                pointer_slot: slot,
                                length_slot: slot + 1,
                            }
                        } else {
                            *next_slot += 1;
                            StructLocalField::Scalar {
                                slot,
                                is_word: is_word_value(&value),
                                type_name: generic_argument_type(&value),
                            }
                        };
                        if field_slots.insert(field.clone(), field_local).is_some() {
                            return Err(CodegenError::UnsupportedProgram(
                                "struct literal contains a duplicate field",
                            ));
                        }
                        lowered_fields.push((slot, value));
                    }
                    environment.insert(
                        name.clone(),
                        Value::StructLocal {
                            type_name: path.rsplit("::").next().unwrap_or(path).to_owned(),
                            fields: field_slots,
                        },
                    );
                    if *mutable {
                        mutable_names.insert(name.clone());
                    } else {
                        mutable_names.remove(name);
                    }
                    lowered.push(Statement::LetStruct {
                        fields: lowered_fields,
                    });
                    continue;
                }
                let mut value = if let BodyExprKind::Try(result) = &initializer.kind {
                    if is_i32_option_call(result, functions) {
                        let result = try_option_value(result, environment, functions)?;
                        let slot = *next_slot;
                        *next_slot += 1;
                        environment.insert(name.clone(), Value::Local(slot));
                        if *mutable {
                            mutable_names.insert(name.clone());
                        } else {
                            mutable_names.remove(name);
                        }
                        lowered.push(Statement::TryOptionBind {
                            result,
                            slot,
                            success_on_zero: false,
                            payload_is_word: false,
                        });
                        continue;
                    }
                    if is_native_tagged_result_call(result, functions) {
                        let BodyExprKind::Call { callee, .. } = &result.kind else {
                            unreachable!()
                        };
                        let payload_is_word = functions
                            .iter()
                            .find(|function| function.name == *callee)
                            .and_then(|function| function.return_type.as_deref())
                            .is_some_and(tagged_result_payload_is_word);
                        let (result, _) = try_result_value(result, environment, functions)?;
                        let slot = *next_slot;
                        *next_slot += 1;
                        environment.insert(
                            name.clone(),
                            if payload_is_word {
                                Value::TypedWordLocal {
                                    slot,
                                    type_name: "u64".to_owned(),
                                }
                            } else {
                                Value::Local(slot)
                            },
                        );
                        if *mutable {
                            mutable_names.insert(name.clone());
                        } else {
                            mutable_names.remove(name);
                        }
                        lowered.push(Statement::TryOptionBind {
                            result,
                            slot,
                            success_on_zero: true,
                            payload_is_word,
                        });
                        continue;
                    }
                    if let Ok(try_value) = try_option_value(result, environment, functions) {
                        lowered.push(Statement::TryOption(try_value));
                    } else {
                        let (try_value, layout) = try_result_value(result, environment, functions)?;
                        if layout.ok_payload.size != 0 {
                            return Err(CodegenError::UnsupportedProgram(
                                "binding a `?` result currently requires a unit success value",
                            ));
                        }
                        lowered.push(Statement::TryResult(try_value));
                    }
                    Value::Integer(0)
                } else {
                    value_from_expression(initializer, environment, functions)?
                };
                if let Some(type_text) = type_text.as_deref() {
                    coerce_annotated_integer_values(&mut value, type_text);
                }
                if matches!(value, Value::EmptyCollection) {
                    let pointer_slot = *next_slot;
                    let length_slot = pointer_slot + 1;
                    let capacity_slot = pointer_slot + 2;
                    *next_slot += 3;
                    environment.insert(
                        name.clone(),
                        Value::CollectionLocal {
                            pointer_slot,
                            length_slot,
                            capacity_slot,
                            kind: if is_supported_string_buffer_expression(initializer) {
                                CollectionKind::StringBuffer
                            } else {
                                type_text.as_deref().map(collection_kind).unwrap_or(CollectionKind::Vector)
                            },
                            element_type: type_text.as_deref().and_then(collection_element_type),
                            indirect: false,
                        },
                    );
                    if *mutable {
                        mutable_names.insert(name.clone());
                    } else {
                        mutable_names.remove(name);
                    }
                    lowered.push(Statement::LetStruct {
                        fields: vec![(pointer_slot, Value::EmptyCollection)],
                    });
                    continue;
                }
                if is_slice_value(&value) {
                    let element_widths = match &value {
                        Value::SliceParameter { element_widths, .. }
                        | Value::SliceLocal { element_widths, .. } => element_widths.clone(),
                        Value::StaticArray(elements) => static_array_element_widths(elements),
                        _ => vec![1],
                    };
                    let pointer_slot = *next_slot;
                    let length_slot = pointer_slot + 1;
                    *next_slot += 2;
                    let local_value = match (&value, mutable) {
                        (Value::StaticArray(elements), false) => {
                            Value::StaticArray(elements.clone())
                        }
                        _ => Value::SliceLocal {
                            pointer_slot,
                            length_slot,
                            element_widths,
                        },
                    };
                    environment.insert(name.clone(), local_value);
                    if *mutable {
                        mutable_names.insert(name.clone());
                    } else {
                        mutable_names.remove(name);
                    }
                    lowered.push(Statement::LetSlice {
                        pointer_slot,
                        length_slot,
                        initializer: value,
                    });
                    continue;
                }
                let slot = *next_slot;
                *next_slot += 1;
                let is_word = is_word_value(&value);
                let local_value = if is_word {
                    generic_argument_type(&value).map_or(Value::WordLocal(slot), |type_name| {
                        Value::TypedWordLocal { slot, type_name }
                    })
                } else {
                    Value::Local(slot)
                };
                environment.insert(name.clone(), local_value);
                if *mutable {
                    mutable_names.insert(name.clone());
                } else {
                    mutable_names.remove(name);
                }
                lowered.push(Statement::Let {
                    slot,
                    initializer: value,
                });
            }
            BodyStatement::LetTuple {
                names, initializer, ..
            } => {
                if let BodyExprKind::Tuple(elements) = &initializer.kind {
                    if names.len() != elements.len() {
                        return Err(CodegenError::UnsupportedProgram(
                            "tuple destructuring arity does not match its initializer",
                        ));
                    }
                    for (name, expression) in names.iter().zip(elements) {
                        let value = value_from_expression(expression, environment, functions)?;
                        let slot = *next_slot;
                        *next_slot += 1;
                        let local = if is_word_value(&value) {
                            generic_argument_type(&value).map_or(
                                Value::WordLocal(slot),
                                |type_name| Value::TypedWordLocal { slot, type_name },
                            )
                        } else {
                            Value::Local(slot)
                        };
                        environment.insert(name.clone(), local);
                        mutable_names.remove(name);
                        lowered.push(Statement::Let {
                            slot,
                            initializer: value,
                        });
                    }
                } else if let BodyExprKind::Call { callee, .. } = &initializer.kind {
                    let function = functions
                        .iter()
                        .find(|function| function.name == *callee)
                        .ok_or(CodegenError::UnsupportedProgram(
                            "tuple-returning call target is unresolved",
                        ))?;
                    let field_types = function
                        .return_type
                        .as_deref()
                        .and_then(pair_type_fields)
                        .ok_or(CodegenError::UnsupportedProgram(
                            "tuple destructuring call must return exactly two scalar values",
                        ))?;
                    if names.len() != field_types.len() {
                        return Err(CodegenError::UnsupportedProgram(
                            "tuple destructuring arity does not match its function return type",
                        ));
                    }
                    let value = value_from_expression(initializer, environment, functions)?;
                    if !matches!(
                        &value,
                        Value::Call {
                            returns_pair: true,
                            ..
                        }
                    ) {
                        return Err(CodegenError::UnsupportedProgram(
                            "tuple destructuring requires a direct pair-returning function call",
                        ));
                    }
                    let mut fields = Vec::with_capacity(2);
                    for (name, field_type) in names.iter().zip(field_types) {
                        let slot = *next_slot;
                        *next_slot += 1;
                        let is_word = is_word_type(field_type);
                        environment.insert(
                            name.clone(),
                            if is_word {
                                Value::TypedWordLocal {
                                    slot,
                                    type_name: field_type.to_owned(),
                                }
                            } else {
                                Value::Local(slot)
                            },
                        );
                        mutable_names.remove(name);
                        fields.push((slot, is_word));
                    }
                    lowered.push(Statement::LetTupleCall {
                        fields,
                        initializer: value,
                    });
                } else {
                    return Err(CodegenError::UnsupportedProgram(
                        "tuple destructuring requires a tuple literal or direct pair-returning call",
                    ));
                }
            }
            BodyStatement::Assign { name, value, .. } => {
                if let Some((base, field_name)) = name.split_once('.') {
                    let field = match environment.get(base) {
                        Some(Value::StructLocal { fields, .. }) => fields.get(field_name),
                        _ => None,
                    }
                    .ok_or(CodegenError::UnsupportedProgram(
                        "field assignment target must be a supported local struct field",
                    ))?;
                    let lowered_value = value_from_expression(value, environment, functions)?;
                    match field {
                        StructLocalField::IndirectScalar {
                            pointer_slot,
                            is_word,
                            ..
                        } => lowered.push(Statement::AssignIndirect {
                            pointer_slot: *pointer_slot,
                            is_word: *is_word,
                            value: lowered_value,
                        }),
                        StructLocalField::Scalar { slot, .. } if mutable_names.contains(base) => {
                            lowered.push(Statement::Assign {
                                slot: *slot,
                                value: lowered_value,
                            });
                        }
                        _ => {
                            return Err(CodegenError::UnsupportedProgram(
                                "field assignment requires a mutable scalar field",
                            ));
                        }
                    }
                    continue;
                }
                let Some(slot) = environment.get(name).and_then(local_slot) else {
                    return Err(CodegenError::UnsupportedProgram(
                        "assignment target must be a local variable",
                    ));
                };
                if !mutable_names.contains(name) {
                    return Err(CodegenError::UnsupportedProgram(
                        "assignment target must be declared `mut`",
                    ));
                }
                lowered.push(Statement::Assign {
                    slot,
                    value: value_from_expression(value, environment, functions)?,
                });
            }
            BodyStatement::Expression {
                expression,
                has_semicolon,
                ..
            } => {
                if let BodyExprKind::Try(result) = &expression.kind {
                    if !has_semicolon {
                        return Err(CodegenError::UnsupportedProgram(
                            "? currently must be a standalone statement",
                        ));
                    }
                    if let Ok(value) = try_option_value(result, environment, functions) {
                        lowered.push(Statement::TryOption(value));
                    } else {
                        let (value, _) = try_result_value(result, environment, functions)?;
                        lowered.push(Statement::TryResult(value));
                    }
                    continue;
                }
                if let BodyExprKind::Return(Some(value)) = &expression.kind {
                    let value = value_from_expression(value, environment, functions)?;
                    if matches!(&value, Value::Tuple(_)) {
                        return Err(CodegenError::UnsupportedProgram(
                            "explicit pair returns are outside the current codegen slice",
                        ));
                    }
                    lowered.push(Statement::Return(value));
                    continue;
                }
                if let BodyExprKind::MethodCall {
                    receiver,
                    method,
                    arguments,
                } = &expression.kind
                    && matches!(method.as_str(), "push" | "extend")
                    && arguments.len() == 1
                    && matches!(
                        value_from_expression(receiver, environment, functions)?,
                        Value::CollectionLocal {
                            kind: CollectionKind::StringBuffer,
                            indirect: false,
                            ..
                        }
                    )
                {
                    let Value::CollectionLocal {
                        pointer_slot,
                        length_slot,
                        capacity_slot,
                        ..
                    } = value_from_expression(receiver, environment, functions)?
                    else {
                        unreachable!("StringBuffer receiver was checked above")
                    };
                    let (character, lowercase) = if method == "push" {
                        (
                            value_from_expression(&arguments[0], environment, functions)?,
                            false,
                        )
                    } else if let BodyExprKind::MethodCall {
                        receiver,
                        method: lower_method,
                        arguments: lower_arguments,
                    } = &arguments[0].kind
                        && lower_method == "to_lowercase"
                        && lower_arguments.is_empty()
                    {
                        (
                            value_from_expression(receiver, environment, functions)?,
                            true,
                        )
                    } else {
                        return Err(CodegenError::UnsupportedProgram(
                            "String::extend currently requires a character lowercase iterator",
                        ));
                    };
                    lowered.push(Statement::StringAppendChar {
                        pointer_slot,
                        length_slot,
                        capacity_slot,
                        character,
                        lowercase,
                    });
                    continue;
                }
                if let BodyExprKind::MethodCall {
                    receiver,
                    method,
                    arguments,
                } = &expression.kind
                    && method == "insert"
                    && arguments.len() == 2
                    && type_name_for_receiver(receiver, environment, functions)
                        .is_some_and(|type_name| type_name.rsplit("::").next() == Some("BTreeMap"))
                {
                    let Value::CollectionLocal {
                        pointer_slot,
                        length_slot,
                        capacity_slot,
                        kind: CollectionKind::BTreeMap,
                        indirect,
                        ..
                    } = value_from_expression(receiver, environment, functions)?
                    else {
                        return Err(CodegenError::UnsupportedProgram(
                            "native BTreeMap::insert requires a local or borrowed BTreeMap field",
                        ));
                    };
                    let key = value_from_expression(&arguments[0], environment, functions)?;
                    let value = value_from_expression(&arguments[1], environment, functions)?;
                    if !matches!(key, Value::OwnedString(_))
                        || !matches!(value, Value::OwnedString(_))
                    {
                        return Err(CodegenError::UnsupportedProgram(
                            "native BTreeMap::insert currently requires owned string keys and values",
                        ));
                    }
                    lowered.push(Statement::BTreeMapInsert {
                        pointer_slot,
                        length_slot,
                        capacity_slot,
                        key,
                        value,
                        indirect,
                    });
                    continue;
                }
                if let BodyExprKind::MethodCall {
                    receiver,
                    method,
                    arguments,
                } = &expression.kind
                    && method == "push"
                    && arguments.len() == 1
                {
                    let Value::CollectionLocal {
                        pointer_slot,
                        length_slot,
                        capacity_slot,
                        kind: CollectionKind::Vector,
                        mut element_type,
                        indirect,
                        ..
                    } = value_from_expression(receiver, environment, functions)?
                    else {
                        return Err(CodegenError::UnsupportedProgram(
                            "`Vec::push` requires a typed local collection field",
                        ));
                    };
                    let mut value = match &arguments[0].kind {
                        BodyExprKind::Path(name) => environment
                            .get(name)
                            .filter(|value| matches!(value, Value::StructLocal { .. }))
                            .cloned()
                            .map_or_else(
                                || value_from_expression(&arguments[0], environment, functions),
                                Ok,
                            )?,
                        _ => value_from_expression(&arguments[0], environment, functions)?,
                    };
                    if element_type.is_none() {
                        element_type = match &value {
                            Value::StructLocal { type_name, .. } => Some(type_name.clone()),
                            Value::OwnedString(_) => Some("String".to_owned()),
                            Value::Call { name, .. }
                                if functions.iter().any(|function| {
                                    function.name == *name
                                        && function.return_type.as_deref() == Some("String")
                                }) =>
                            {
                                Some("String".to_owned())
                            }
                            _ => generic_argument_type(&value),
                        };
                        if let BodyExprKind::Path(name) = &receiver.kind
                            && let Some(Value::CollectionLocal {
                                element_type: stored_type,
                                ..
                            }) = environment.get_mut(name)
                        {
                            *stored_type = element_type.clone();
                        }
                    }
                    if let Some(element_type) = element_type.as_deref() {
                        coerce_annotated_integer_values(&mut value, element_type);
                    }
                    match (element_type.as_deref(), value) {
                        (Some(element_type), value)
                            if (is_word_type(element_type) && is_word_value(&value))
                                || (element_type == "i32"
                                    && matches!(
                                        &value,
                                        Value::Integer(_)
                                            | Value::Local(_)
                                            | Value::Call {
                                                returns_word: false,
                                                returns_slice: false,
                                                returns_pair: false,
                                                returns_aggregate: false,
                                                ..
                                            }
                                    )) =>
                        {
                            lowered.push(Statement::VecPushAggregate {
                                pointer_slot,
                                length_slot,
                                capacity_slot,
                                fields: vec![value],
                                widths: vec![1],
                                indirect,
                            });
                        }
                        (Some(element_type), value)
                            if element_type.rsplit("::").next() == Some("String")
                                && is_slice_value(&value) =>
                        {
                            lowered.push(Statement::VecPushString {
                                pointer_slot,
                                length_slot,
                                capacity_slot,
                                value,
                                indirect,
                            });
                        }
                        (Some(element_type), Value::StructLocal { type_name, fields })
                            if element_type.rsplit("::").next()
                                == type_name.rsplit("::").next() =>
                        {
                            let structure = find_struct_type(&type_name, structs).ok_or(
                                CodegenError::UnsupportedProgram(
                                    "vector element record type is unresolved",
                                ),
                            )?;
                            let mut values = Vec::with_capacity(structure.fields.len());
                            let mut widths = Vec::with_capacity(structure.fields.len());
                            for field in &structure.fields {
                                let name = field.name.as_deref().ok_or(
                                    CodegenError::UnsupportedProgram(
                                        "vector element records require named fields",
                                    ),
                                )?;
                                let local =
                                    fields.get(name).ok_or(CodegenError::UnsupportedProgram(
                                        "vector element record is missing a declared field",
                                    ))?;
                                let width = aggregate_field_width(&field.type_text).ok_or(
                                    CodegenError::UnsupportedProgram(
                                        "vector record fields must have scalar, string, or collection layout",
                                    ),
                                )?;
                                values.push(value_from_struct_local_field(local));
                                widths.push(width);
                            }
                            lowered.push(Statement::VecPushAggregate {
                                pointer_slot,
                                length_slot,
                                capacity_slot,
                                fields: values,
                                widths,
                                indirect,
                            });
                        }
                        _ => {
                            return Err(CodegenError::UnsupportedProgram(
                                "native Vec::push supports owned strings and named records with scalar, string, or collection fields",
                            ));
                        }
                    }
                    continue;
                }
                let BodyExprKind::If {
                    condition,
                    then_branch,
                    else_branch,
                } = &expression.kind
                else {
                    {
                        lowered.push(Statement::Evaluate(value_from_expression(
                            expression,
                            environment,
                            functions,
                        )?));
                        continue;
                    }
                };
                let condition = condition_from_expression_with_structs(
                    condition,
                    environment,
                    functions,
                    structs,
                )?;
                let lower_branch = |branch: &BodyExpr,
                                    next_slot: &mut usize|
                 -> Result<Vec<Statement>, CodegenError> {
                    let BodyExprKind::Block { statements, tail } = &branch.kind else {
                        return Err(CodegenError::UnsupportedProgram(
                            "if branches must be blocks",
                        ));
                    };
                    if tail.is_some() {
                        return Err(CodegenError::UnsupportedProgram(
                            "if statement branches cannot have tail values",
                        ));
                    }
                    let mut branch_environment = environment.clone();
                    let mut branch_mutable = mutable_names.clone();
                    lower_statements(
                        statements,
                        &mut branch_environment,
                        &mut branch_mutable,
                        next_slot,
                        functions,
                        structs,
                    )
                };
                let then_body = lower_branch(then_branch, next_slot)?;
                let else_body = if let Some(branch) = else_branch {
                    lower_branch(branch, next_slot)?
                } else {
                    Vec::new()
                };
                lowered.push(Statement::If {
                    condition,
                    then_body,
                    else_body,
                });
            }
            BodyStatement::While {
                condition, body, ..
            } => {
                let condition = condition_from_expression_with_structs(
                    condition,
                    environment,
                    functions,
                    structs,
                )?;
                let BodyExprKind::Block { statements, tail } = &body.kind else {
                    return Err(CodegenError::UnsupportedProgram(
                        "while body must be a block",
                    ));
                };
                if tail.is_some() {
                    return Err(CodegenError::UnsupportedProgram(
                        "while body must contain statements only",
                    ));
                }
                let mut body_environment = environment.clone();
                let mut body_mutable_names = mutable_names.clone();
                let body = lower_statements(
                    statements,
                    &mut body_environment,
                    &mut body_mutable_names,
                    next_slot,
                    functions,
                    structs,
                )?;
                lowered.push(Statement::While { condition, body });
            }
            BodyStatement::Loop { body, .. } => {
                let BodyExprKind::Block { statements, tail } = &body.kind else {
                    return Err(CodegenError::UnsupportedProgram(
                        "loop body must be a block",
                    ));
                };
                if tail.is_some() {
                    return Err(CodegenError::UnsupportedProgram(
                        "loop body must contain statements only",
                    ));
                }
                let mut loop_environment = environment.clone();
                let mut loop_mutable_names = mutable_names.clone();
                let body = lower_statements(
                    statements,
                    &mut loop_environment,
                    &mut loop_mutable_names,
                    next_slot,
                    functions,
                    structs,
                )?;
                lowered.push(Statement::While {
                    condition: Condition::Boolean(true),
                    body,
                });
            }
            BodyStatement::ForRange {
                name,
                start,
                end,
                inclusive,
                body,
                ..
            } => {
                // Rust evaluates the range start and end exactly once, before
                // the iteration binding enters scope.
                let start_value = value_from_expression(start, environment, functions)?;
                let end_value = value_from_expression(end, environment, functions)?;
                let slot = *next_slot;
                *next_slot += 1;
                let end_slot = *next_slot;
                *next_slot += 1;
                lowered.push(Statement::Let {
                    slot,
                    initializer: start_value,
                });
                lowered.push(Statement::Let {
                    slot: end_slot,
                    initializer: end_value,
                });
                let BodyExprKind::Block { statements, tail } = &body.kind else {
                    return Err(CodegenError::UnsupportedProgram(
                        "for-loop body must be a block",
                    ));
                };
                if tail.is_some() {
                    return Err(CodegenError::UnsupportedProgram(
                        "for-loop body must contain statements only",
                    ));
                }
                let mut loop_environment = environment.clone();
                loop_environment.insert(name.clone(), Value::Local(slot));
                let mut loop_mutable_names = mutable_names.clone();
                loop_mutable_names.remove(name);
                let body = lower_statements(
                    statements,
                    &mut loop_environment,
                    &mut loop_mutable_names,
                    next_slot,
                    functions,
                    structs,
                )?;
                lowered.push(Statement::ForRange {
                    slot,
                    end_slot,
                    inclusive: *inclusive,
                    body,
                });
            }
            BodyStatement::ForEach {
                names,
                iterator,
                body,
                ..
            } => {
                let iterable = value_from_expression(iterator, environment, functions)?;
                if let Value::CharIterator {
                    pointer_slot,
                    length_slot,
                } = iterable
                {
                    if names.len() != 1 {
                        return Err(CodegenError::UnsupportedProgram(
                            "character iteration binds one character variable",
                        ));
                    }
                    let index_slot = *next_slot;
                    let item_slot = index_slot + 1;
                    *next_slot += 2;
                    let mut loop_environment = environment.clone();
                    loop_environment.insert(names[0].clone(), Value::Local(item_slot));
                    let mut loop_mutable_names = mutable_names.clone();
                    loop_mutable_names.remove(&names[0]);
                    let BodyExprKind::Block { statements, tail } = &body.kind else {
                        return Err(CodegenError::UnsupportedProgram(
                            "for-each body must be a block",
                        ));
                    };
                    let mut body_statements = statements.clone();
                    if let Some(tail) = tail
                        && !matches!(tail.kind, BodyExprKind::Unit)
                    {
                        body_statements.push(BodyStatement::Expression {
                            expression: (**tail).clone(),
                            has_semicolon: false,
                        });
                    }
                    let body = lower_statements(
                        &body_statements,
                        &mut loop_environment,
                        &mut loop_mutable_names,
                        next_slot,
                        functions,
                        structs,
                    )?;
                    lowered.push(Statement::ForChars {
                        pointer_slot,
                        length_slot,
                        index_slot,
                        item_slot,
                        body,
                    });
                    continue;
                }
                if let Value::SliceRecord {
                    pointer_slot,
                    length_slot,
                    record_type,
                } = iterable
                {
                    if names.len() != 1 {
                        return Err(CodegenError::UnsupportedProgram(
                            "record slice iteration binds one record variable",
                        ));
                    }
                    let structure = find_struct_type(&record_type, structs).ok_or(
                        CodegenError::UnsupportedProgram("slice record type is unresolved"),
                    )?;
                    let (layout, stride) =
                        record_field_layout(structure).ok_or(CodegenError::UnsupportedProgram(
                            "slice record fields need scalar, string, or collection layout",
                        ))?;
                    lowered.push(Statement::LetSlice {
                        pointer_slot,
                        length_slot,
                        initializer: Value::SliceRecord {
                            pointer_slot,
                            length_slot,
                            record_type: record_type.clone(),
                        },
                    });
                    let index_slot = *next_slot;
                    *next_slot += 1;
                    lowered.push(Statement::Let {
                        slot: index_slot,
                        initializer: Value::Word(0),
                    });
                    let mut loop_environment = environment.clone();
                    let mut loop_mutable_names = mutable_names.clone();
                    loop_mutable_names.remove(&names[0]);
                    let mut loop_fields = HashMap::with_capacity(structure.fields.len());
                    let mut field_loads = Vec::new();
                    for (field, (offset, width)) in structure.fields.iter().zip(layout) {
                        let field_name =
                            field
                                .name
                                .as_deref()
                                .ok_or(CodegenError::UnsupportedProgram(
                                    "slice records require named fields",
                                ))?;
                        let slot = *next_slot;
                        match width {
                            1 => {
                                *next_slot += 1;
                                loop_fields.insert(
                                    field_name.to_owned(),
                                    StructLocalField::Scalar {
                                        slot,
                                        is_word: is_word_type(&field.type_text),
                                        type_name: Some(field.type_text.clone()),
                                    },
                                );
                                field_loads.push(Statement::Let {
                                    slot,
                                    initializer: Value::SliceElement {
                                        pointer_slot,
                                        index_slot,
                                        element_size: if is_word_type(&field.type_text) {
                                            8
                                        } else {
                                            4
                                        },
                                        element_offset: offset,
                                        element_stride: stride,
                                    },
                                });
                            }
                            2 => {
                                *next_slot += 2;
                                loop_fields.insert(
                                    field_name.to_owned(),
                                    StructLocalField::Slice {
                                        pointer_slot: slot,
                                        length_slot: slot + 1,
                                    },
                                );
                                field_loads.push(Statement::LetSlice {
                                    pointer_slot: slot,
                                    length_slot: slot + 1,
                                    initializer: Value::SliceElementString {
                                        pointer_slot,
                                        index_slot,
                                        element_offset: offset,
                                        element_stride: stride,
                                    },
                                });
                            }
                            3 => {
                                *next_slot += 3;
                                loop_fields.insert(
                                    field_name.to_owned(),
                                    StructLocalField::Collection {
                                        pointer_slot: slot,
                                        length_slot: slot + 1,
                                        capacity_slot: slot + 2,
                                        kind: collection_kind(&field.type_text),
                                        element_type: collection_element_type(&field.type_text),
                                        indirect: false,
                                    },
                                );
                                for component in 0..3 {
                                    field_loads.push(Statement::Let {
                                        slot: slot + component,
                                        initializer: Value::SliceElement {
                                            pointer_slot,
                                            index_slot,
                                            element_size: 8,
                                            element_offset: offset + component * 8,
                                            element_stride: stride,
                                        },
                                    });
                                }
                            }
                            _ => unreachable!("record field layout width is 1, 2, or 3"),
                        }
                    }
                    loop_environment.insert(
                        names[0].clone(),
                        Value::StructLocal {
                            type_name: record_type,
                            fields: loop_fields,
                        },
                    );
                    let BodyExprKind::Block { statements, tail } = &body.kind else {
                        return Err(CodegenError::UnsupportedProgram(
                            "for-each body must be a block",
                        ));
                    };
                    if tail.is_some() {
                        return Err(CodegenError::UnsupportedProgram(
                            "for-each body must contain statements only",
                        ));
                    }
                    field_loads.extend(lower_statements(
                        statements,
                        &mut loop_environment,
                        &mut loop_mutable_names,
                        next_slot,
                        functions,
                        structs,
                    )?);
                    lowered.push(Statement::ForSlice {
                        length_slot,
                        index_slot,
                        body: field_loads,
                    });
                    continue;
                }
                if let Value::CollectionLocal {
                    pointer_slot,
                    length_slot,
                    kind: CollectionKind::Vector,
                    element_type: Some(element_type),
                    indirect: false,
                    ..
                } = &iterable
                {
                    if names.len() != 1 || (!is_word_type(element_type) && element_type != "i32") {
                        return Err(CodegenError::UnsupportedProgram(
                            "native Vec iteration currently supports one integer binding",
                        ));
                    }
                    let index_slot = *next_slot;
                    *next_slot += 1;
                    lowered.push(Statement::Let {
                        slot: index_slot,
                        initializer: Value::Word(0),
                    });
                    let item_slot = *next_slot;
                    *next_slot += 1;
                    let mut loop_environment = environment.clone();
                    loop_environment.insert(
                        names[0].clone(),
                        if is_word_type(element_type) {
                            Value::TypedWordLocal {
                                slot: item_slot,
                                type_name: element_type.clone(),
                            }
                        } else {
                            Value::Local(item_slot)
                        },
                    );
                    let mut loop_mutable_names = mutable_names.clone();
                    loop_mutable_names.remove(&names[0]);
                    let BodyExprKind::Block { statements, tail } = &body.kind else {
                        return Err(CodegenError::UnsupportedProgram(
                            "for-each body must be a block",
                        ));
                    };
                    if tail.is_some() {
                        return Err(CodegenError::UnsupportedProgram(
                            "for-each body must contain statements only",
                        ));
                    }
                    let mut loop_body = vec![Statement::Let {
                        slot: item_slot,
                        initializer: Value::SliceElement {
                            pointer_slot: *pointer_slot,
                            index_slot,
                            element_size: if is_word_type(element_type) { 8 } else { 4 },
                            element_offset: 0,
                            element_stride: 8,
                        },
                    }];
                    loop_body.extend(lower_statements(
                        statements,
                        &mut loop_environment,
                        &mut loop_mutable_names,
                        next_slot,
                        functions,
                        structs,
                    )?);
                    lowered.push(Statement::ForSlice {
                        length_slot: *length_slot,
                        index_slot,
                        body: loop_body,
                    });
                    continue;
                }
                let element_widths = match &iterable {
                    Value::SliceParameter { element_widths, .. }
                    | Value::SliceLocal { element_widths, .. } => element_widths.clone(),
                    Value::StaticArray(elements) => static_array_element_widths(elements),
                    _ => {
                        return Err(CodegenError::UnsupportedProgram(
                            "native for-each currently requires a borrowed integer slice",
                        ));
                    }
                };
                if element_widths.len() != names.len()
                    || element_widths.is_empty()
                    || element_widths
                        .iter()
                        .any(|width| !matches!(width, 4 | 8 | 16))
                {
                    return Err(CodegenError::UnsupportedProgram(
                        "native slice iteration supports scalar integers and two-integer tuples",
                    ));
                }
                let pointer_slot = *next_slot;
                let length_slot = pointer_slot + 1;
                *next_slot += 2;
                lowered.push(Statement::LetSlice {
                    pointer_slot,
                    length_slot,
                    initializer: iterable,
                });
                let index_slot = *next_slot;
                *next_slot += 1;
                lowered.push(Statement::Let {
                    slot: index_slot,
                    initializer: Value::Word(0),
                });
                let BodyExprKind::Block { statements, tail } = &body.kind else {
                    return Err(CodegenError::UnsupportedProgram(
                        "for-each body must be a block",
                    ));
                };
                if tail.is_some() {
                    return Err(CodegenError::UnsupportedProgram(
                        "for-each body must contain statements only",
                    ));
                }
                let mut loop_environment = environment.clone();
                let (element_offsets, element_stride) = native_tuple_layout(&element_widths);
                let mut element_loads = Vec::with_capacity(names.len());
                for ((name, width), element_offset) in
                    names.iter().zip(&element_widths).zip(&element_offsets)
                {
                    if *width == 16 {
                        let value_pointer_slot = *next_slot;
                        let value_length_slot = value_pointer_slot + 1;
                        *next_slot += 2;
                        loop_environment.insert(
                            name.clone(),
                            Value::SliceLocal {
                                pointer_slot: value_pointer_slot,
                                length_slot: value_length_slot,
                                element_widths: vec![16],
                            },
                        );
                        element_loads.push(Statement::LetSlice {
                            pointer_slot: value_pointer_slot,
                            length_slot: value_length_slot,
                            initializer: Value::SliceElementString {
                                pointer_slot,
                                index_slot,
                                element_offset: *element_offset,
                                element_stride,
                            },
                        });
                    } else {
                        let slot = *next_slot;
                        *next_slot += 1;
                        loop_environment.insert(
                            name.clone(),
                            if *width == 8 {
                                Value::WordLocal(slot)
                            } else {
                                Value::Local(slot)
                            },
                        );
                        element_loads.push(Statement::Let {
                            slot,
                            initializer: Value::SliceElement {
                                pointer_slot,
                                index_slot,
                                element_size: *width,
                                element_offset: *element_offset,
                                element_stride,
                            },
                        });
                    }
                }
                let mut loop_mutable_names = mutable_names.clone();
                loop_mutable_names.remove(&names[0]);
                let loop_body = lower_statements(
                    statements,
                    &mut loop_environment,
                    &mut loop_mutable_names,
                    next_slot,
                    functions,
                    structs,
                )?;
                element_loads.extend(loop_body);
                let loop_body = element_loads;
                lowered.push(Statement::ForSlice {
                    length_slot,
                    index_slot,
                    body: loop_body,
                });
            }
            BodyStatement::MacroInvocation {
                path,
                format_string,
                arguments,
                ..
            } if path == "println" => {
                if !arguments.is_empty() {
                    return Err(CodegenError::UnsupportedProgram(
                        "`println!` currently supports named local captures only",
                    ));
                }
                let format_string =
                    format_string
                        .as_deref()
                        .ok_or(CodegenError::UnsupportedProgram(
                            "`println!` format must begin with a string literal",
                        ))?;
                lowered.push(Statement::Println(lower_println_parts(
                    format_string,
                    environment,
                )?));
            }
            BodyStatement::MacroInvocation {
                path,
                format_string,
                arguments,
                source_path,
                line,
                column,
                ..
            } if path == "panic" => {
                if !arguments.is_empty() {
                    return Err(CodegenError::UnsupportedProgram(
                        "`panic!` currently supports a static string literal without formatting arguments",
                    ));
                }
                let message = format_string.as_deref().unwrap_or("explicit panic");
                if message.contains('{') || message.contains('}') {
                    return Err(CodegenError::UnsupportedProgram(
                        "`panic!` currently supports a static string literal without formatting placeholders",
                    ));
                }
                let stderr = format!(
                    "\nthread 'main' (0) panicked at {source_path}:{line}:{column}:\n{message}\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n"
                );
                lowered.push(Statement::Println(vec![PrintPart::Panic(stderr)]));
            }
            BodyStatement::Break { .. } => lowered.push(Statement::Break),
            BodyStatement::Continue { .. } => lowered.push(Statement::Continue),
            BodyStatement::MacroInvocation { path, .. } => {
                return Err(CodegenError::UnsupportedProgram(if path == "println" {
                    "`println!` form is outside the native formatter subset"
                } else {
                    "this macro invocation has no native expansion"
                }));
            }
            _ => {
                return Err(CodegenError::UnsupportedProgram(
                    "helper statements must be initialized named locals, mutable assignments, or while loops",
                ));
            }
        }
    }
    Ok(lowered)
}

fn materialize_method_receivers(
    expression: &mut BodyExpr,
    temporaries: &mut Vec<BodyStatement>,
    slot_seed: usize,
) {
    let BodyExprKind::MethodCall {
        receiver, method, ..
    } = &mut expression.kind
    else {
        return;
    };
    if matches!(
        method.as_str(),
        "is_ok" | "is_err" | "is_some" | "unwrap_or" | "unwrap"
    ) && matches!(receiver.kind, BodyExprKind::Call { .. })
    {
        return;
    }
    if method == "unwrap" {
        return;
    }
    if method == "clone" && matches!(receiver.kind, BodyExprKind::Index { .. }) {
        return;
    }
    materialize_method_receivers(receiver, temporaries, slot_seed);
    if matches!(receiver.kind, BodyExprKind::Path(_)) {
        return;
    }
    let name = format!("__quill_receiver_{slot_seed}_{}", temporaries.len());
    temporaries.push(BodyStatement::Let {
        name: Some(name.clone()),
        type_text: None,
        mutable: false,
        initializer: Some((**receiver).clone()),
        span: receiver.span.clone(),
    });
    **receiver = BodyExpr {
        kind: BodyExprKind::Path(name),
        span: receiver.span.clone(),
    };
}

fn materialize_vector_indices(
    expression: &mut BodyExpr,
    environment: &HashMap<String, Value>,
    temporaries: &mut Vec<BodyStatement>,
    slot_seed: usize,
    reserved_name: &str,
) {
    match &mut expression.kind {
        BodyExprKind::Index { base, index, .. } => {
            materialize_vector_indices(base, environment, temporaries, slot_seed, reserved_name);
            materialize_vector_indices(index, environment, temporaries, slot_seed, reserved_name);
            let BodyExprKind::Path(collection_name) = &base.kind else {
                return;
            };
            let Some(Value::CollectionLocal {
                kind: CollectionKind::Vector,
                element_type: Some(element_type),
                ..
            }) = environment.get(collection_name)
            else {
                return;
            };
            if !is_word_type(element_type) && element_type != "i32" {
                return;
            }
            let mut suffix = temporaries.len();
            let name = loop {
                let candidate = format!("__quill_vec_index_{slot_seed}_{suffix}");
                suffix += 1;
                let used_in_source = candidate == reserved_name
                    || environment.contains_key(&candidate)
                    || temporaries.iter().any(|statement| {
                        matches!(
                            statement,
                            BodyStatement::Let {
                                name: Some(name),
                                ..
                            } if name == &candidate
                        )
                    });
                if !used_in_source {
                    break candidate;
                }
            };
            let initializer = expression.clone();
            temporaries.push(BodyStatement::Let {
                name: Some(name.clone()),
                type_text: None,
                mutable: false,
                initializer: Some(initializer),
                span: expression.span.clone(),
            });
            expression.kind = BodyExprKind::Path(name);
        }
        BodyExprKind::Binary {
            operator,
            left,
            right,
        } => {
            materialize_vector_indices(left, environment, temporaries, slot_seed, reserved_name);
            if !matches!(operator, BinaryOperator::And | BinaryOperator::Or) {
                materialize_vector_indices(
                    right,
                    environment,
                    temporaries,
                    slot_seed,
                    reserved_name,
                );
            }
        }
        BodyExprKind::Call { arguments, .. }
        | BodyExprKind::Tuple(arguments)
        | BodyExprKind::ArrayLiteral(arguments)
        | BodyExprKind::FixedArrayLiteral(arguments) => {
            for argument in arguments {
                materialize_vector_indices(
                    argument,
                    environment,
                    temporaries,
                    slot_seed,
                    reserved_name,
                );
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            arguments,
            ..
        } => {
            materialize_vector_indices(receiver, environment, temporaries, slot_seed, reserved_name);
            for argument in arguments {
                materialize_vector_indices(
                    argument,
                    environment,
                    temporaries,
                    slot_seed,
                    reserved_name,
                );
            }
        }
        BodyExprKind::Cast { expression, .. }
        | BodyExprKind::Negate(expression)
        | BodyExprKind::Not(expression)
        | BodyExprKind::Try(expression)
        | BodyExprKind::Parenthesized(expression)
        | BodyExprKind::FieldAccess {
            base: expression, ..
        } => materialize_vector_indices(
            expression,
            environment,
            temporaries,
            slot_seed,
            reserved_name,
        ),
        BodyExprKind::If { condition, .. } => {
            materialize_vector_indices(
                condition,
                environment,
                temporaries,
                slot_seed,
                reserved_name,
            );
        }
        BodyExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                materialize_vector_indices(value, environment, temporaries, slot_seed, reserved_name);
            }
        }
        BodyExprKind::Match { scrutinee, .. } => {
            materialize_vector_indices(
                scrutinee,
                environment,
                temporaries,
                slot_seed,
                reserved_name,
            );
        }
        BodyExprKind::Return(Some(value)) => {
            materialize_vector_indices(value, environment, temporaries, slot_seed, reserved_name);
        }
        BodyExprKind::Unit
        | BodyExprKind::Literal(_)
        | BodyExprKind::EmptySliceReference
        | BodyExprKind::MacroInvocation(_)
        | BodyExprKind::Closure { .. }
        | BodyExprKind::Path(_)
        | BodyExprKind::Return(None)
        | BodyExprKind::Reference { .. }
        | BodyExprKind::Dereference(_)
        | BodyExprKind::Block { .. }
        | BodyExprKind::Unsupported => {}
    }
}

fn lower_println_parts(
    format_string: &str,
    environment: &HashMap<String, Value>,
) -> Result<Vec<PrintPart>, CodegenError> {
    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut characters = format_string.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '{' if characters.peek() == Some(&'{') => {
                characters.next();
                literal.push('{');
            }
            '}' if characters.peek() == Some(&'}') => {
                characters.next();
                literal.push('}');
            }
            '{' => {
                if !literal.is_empty() {
                    parts.push(PrintPart::Text(std::mem::take(&mut literal)));
                }
                let mut field = String::new();
                let mut closed = false;
                for next in characters.by_ref() {
                    if next == '}' {
                        closed = true;
                        break;
                    }
                    field.push(next);
                }
                if !closed {
                    return Err(CodegenError::UnsupportedProgram(
                        "`println!` format contains an unclosed placeholder",
                    ));
                }
                let (name, specifier) = field
                    .split_once(':')
                    .map_or((field.as_str(), ""), |(name, specifier)| (name, specifier));
                if name.is_empty()
                    || !name
                        .chars()
                        .all(|character| character == '_' || character.is_ascii_alphanumeric())
                    || name.as_bytes()[0].is_ascii_digit()
                {
                    return Err(CodegenError::UnsupportedProgram(
                        "`println!` supports simple named local captures",
                    ));
                }
                let value =
                    environment
                        .get(name)
                        .cloned()
                        .ok_or(CodegenError::UnsupportedProgram(
                            "`println!` capture must name an initialized local",
                        ))?;
                match specifier {
                    "" if is_slice_value(&value) => parts.push(PrintPart::Slice(value)),
                    "" if is_word_value(&value) => parts.push(PrintPart::Decimal(value)),
                    "016x" => parts.push(PrintPart::Hex16(value)),
                    "" => {
                        return Err(CodegenError::UnsupportedProgram(
                            "`println!` captures must be strings or unsigned integers",
                        ));
                    }
                    _ => {
                        return Err(CodegenError::UnsupportedProgram(
                            "`println!` supports decimal and `:016x` captures",
                        ));
                    }
                }
            }
            '}' => {
                return Err(CodegenError::UnsupportedProgram(
                    "`println!` format contains an unmatched closing brace",
                ));
            }
            other => literal.push(other),
        }
    }
    if !literal.is_empty() {
        parts.push(PrintPart::Text(literal));
    }
    parts.push(PrintPart::Text("\n".to_owned()));
    Ok(parts)
}

fn value_from_expression_with_structs(
    expression: &BodyExpr,
    environment: &HashMap<String, Value>,
    functions: &[FunctionItem],
    structs: &[StructItem],
) -> Result<Value, CodegenError> {
    if let BodyExprKind::FieldAccess { base, field } = &expression.kind
        && let BodyExprKind::Call { callee, .. } = &base.kind
    {
        let function = functions
            .iter()
            .find(|function| function.name == *callee)
            .ok_or(CodegenError::UnsupportedProgram(
                "record field access call target is unresolved",
            ))?;
        if function
            .return_type
            .as_deref()
            .and_then(pair_type_fields)
            .is_some()
        {
            let index = field
                .parse::<usize>()
                .ok()
                .filter(|index| *index < 2)
                .ok_or(CodegenError::UnsupportedProgram(
                    "tuple field projection index must be zero or one",
                ))?;
            return Ok(Value::PairField {
                aggregate: Box::new(value_from_expression(base, environment, functions)?),
                index,
            });
        }
        let return_type =
            function
                .return_type
                .as_deref()
                .ok_or(CodegenError::UnsupportedProgram(
                    "record field access requires a record-returning function",
                ))?;
        if struct_pair_field_types(return_type, structs).is_none() {
            return Err(CodegenError::UnsupportedProgram(
                "record field access currently supports two-word records",
            ));
        }
        let structure = find_struct_type(return_type, structs).ok_or(
            CodegenError::UnsupportedProgram("record return type is unresolved"),
        )?;
        let index = structure
            .fields
            .iter()
            .position(|candidate| candidate.name.as_deref() == Some(field.as_str()))
            .ok_or(CodegenError::UnsupportedProgram(
                "record field does not exist",
            ))?;
        let aggregate = value_from_expression(base, environment, functions)?;
        return Ok(Value::PairField {
            aggregate: Box::new(aggregate),
            index,
        });
    }
    value_from_expression(expression, environment, functions)
}

fn type_name_for_receiver(
    expression: &BodyExpr,
    environment: &HashMap<String, Value>,
    functions: &[FunctionItem],
) -> Option<String> {
    match &expression.kind {
        BodyExprKind::Path(name) => match environment.get(name)? {
            Value::StructLocal { type_name, .. } | Value::StructReference { type_name, .. } => {
                Some(type_name.clone())
            }
            Value::CollectionLocal { kind, .. } => Some(
                match kind {
                    CollectionKind::Vector => "Vec",
                    CollectionKind::BTreeMap => "BTreeMap",
                    CollectionKind::HashMap => "HashMap",
                    CollectionKind::StringBuffer => "String",
                }
                .to_owned(),
            ),
            _ => None,
        },
        BodyExprKind::FieldAccess { base, field } => {
            let BodyExprKind::Path(name) = &base.kind else {
                return None;
            };
            let Value::StructLocal { fields, .. } = environment.get(name)? else {
                return None;
            };
            match fields.get(field)? {
                StructLocalField::Collection { kind, .. } => Some(
                    match kind {
                        CollectionKind::Vector => "Vec",
                        CollectionKind::BTreeMap => "BTreeMap",
                        CollectionKind::HashMap => "HashMap",
                        CollectionKind::StringBuffer => "String",
                    }
                    .to_owned(),
                ),
                StructLocalField::Slice { .. } => Some("String".to_owned()),
                _ => None,
            }
        }
        BodyExprKind::Reference { expression, .. } | BodyExprKind::Parenthesized(expression) => {
            type_name_for_receiver(expression, environment, functions)
        }
        BodyExprKind::StructLiteral { path, .. } => Some(path.clone()),
        BodyExprKind::Call { callee, .. } => functions
            .iter()
            .find(|function| function.name == *callee)
            .and_then(|function| function.return_type.clone()),
        BodyExprKind::MethodCall {
            method, receiver, ..
        } => {
            let owner = type_name_for_receiver(receiver, environment, functions)?;
            resolve_impl_method(&owner, method, functions)?
                .return_type
                .clone()
        }
        _ => None,
    }
}

fn function_for_call_expression<'a>(
    expression: &BodyExpr,
    environment: &HashMap<String, Value>,
    functions: &'a [FunctionItem],
) -> Option<&'a FunctionItem> {
    match &expression.kind {
        BodyExprKind::Call { callee, .. } => {
            functions.iter().find(|function| function.name == *callee)
        }
        BodyExprKind::MethodCall {
            receiver, method, ..
        } => {
            let owner = type_name_for_receiver(receiver, environment, functions)?;
            resolve_impl_method(&owner, method, functions)
        }
        _ => None,
    }
}

fn resolve_impl_method<'a>(
    receiver_type: &str,
    method: &str,
    functions: &'a [FunctionItem],
) -> Option<&'a FunctionItem> {
    let owner = receiver_type
        .trim()
        .trim_start_matches('&')
        .trim_start_matches("mut ")
        .rsplit("::")
        .next()?;
    functions.iter().find(|function| {
        function.name.rsplit("::").next() == Some(method)
            && (function.scope.iter().any(|scope| {
                scope
                    .strip_prefix("impl:")
                    .is_some_and(|scope_owner| scope_owner.rsplit("::").next() == Some(owner))
            }) || function.name == format!("{owner}::{method}")
                || function.name.ends_with(&format!("::{owner}::{method}")))
    })
}

fn value_from_expression(
    expr: &BodyExpr,
    environment: &HashMap<String, Value>,
    functions: &[FunctionItem],
) -> Result<Value, CodegenError> {
    match &expr.kind {
        BodyExprKind::MethodCall { receiver, method, arguments }
            if method == "unwrap"
                && arguments.is_empty()
                && let BodyExprKind::MethodCall { receiver: map, method: get, arguments: keys } = &receiver.kind
                && get == "get" && keys.len() == 1 =>
        {
            let Value::CollectionLocal {
                pointer_slot,
                length_slot,
                kind: CollectionKind::BTreeMap,
                ..
            } = value_from_expression(map, environment, functions)? else {
                return Err(CodegenError::UnsupportedProgram("BTreeMap::get requires a local BTreeMap"));
            };
            let key = value_from_expression(&keys[0], environment, functions)?;
            if !is_slice_value(&key) {
                return Err(CodegenError::UnsupportedProgram("BTreeMap::get currently requires a string key"));
            }
            Ok(Value::BTreeMapGetString {
                pointer_slot,
                length_slot,
                key: Box::new(key),
            })
        }
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } if method == "any"
            && arguments.len() == 1
            && is_trimmed_empty_string_closure(&arguments[0]) =>
        {
            let Value::SliceLocal {
                pointer_slot,
                length_slot,
                element_widths,
            } = value_from_expression(receiver, environment, functions)?
            else {
                return Err(CodegenError::UnsupportedProgram(
                    "`.any()` currently requires an iterator over owned strings",
                ));
            };
            if element_widths != [16] {
                return Err(CodegenError::UnsupportedProgram(
                    "`.any()` currently requires an iterator over owned strings",
                ));
            }
            Ok(Value::AnyStringTrimEmpty {
                pointer_slot,
                length_slot,
            })
        }
        BodyExprKind::Call {
            callee, arguments, ..
        } if callee == "u64::from" && arguments.len() == 1 => {
            let value = value_from_expression(&arguments[0], environment, functions)?;
            if matches!(value, Value::Integer(_) | Value::Local(_)) {
                Ok(Value::ZeroExtend(Box::new(value)))
            } else {
                Err(CodegenError::UnsupportedProgram(
                    "`u64::from` currently requires a byte or i32 value",
                ))
            }
        }
        BodyExprKind::Literal(LiteralKind::Integer { text, suffix })
            if matches!(suffix.as_str(), "u64" | "usize") =>
        {
            text.parse::<u64>()
                .map(|value| Value::TypedWord {
                    value,
                    type_name: suffix.clone(),
                })
                .map_err(|_| CodegenError::UnsupportedProgram("integer literal must fit `u64`"))
        }
        BodyExprKind::Literal(LiteralKind::Integer { text, suffix })
            if suffix.is_empty() || suffix == "i32" =>
        {
            text.parse::<i32>()
                .map(Value::Integer)
                .map_err(|_| CodegenError::UnsupportedProgram("integer literal must fit `i32`"))
        }
        BodyExprKind::Literal(LiteralKind::Boolean(value)) => Ok(Value::Integer(i32::from(*value))),
        BodyExprKind::Literal(LiteralKind::String { value }) => Ok(Value::TextSlice(value.clone())),
        BodyExprKind::Literal(LiteralKind::Character(value)) => Ok(Value::Integer(*value as i32)),
        BodyExprKind::EmptySliceReference => Ok(Value::EmptySlice),
        BodyExprKind::Parenthesized(inner) => value_from_expression(inner, environment, functions),
        BodyExprKind::Reference { expression, .. } => {
            value_from_expression(expression, environment, functions)
        }
        BodyExprKind::Cast {
            expression,
            type_name,
        } => {
            let value = value_from_expression(expression, environment, functions)?;
            if !matches!(type_name.trim(), "u64" | "usize") {
                Err(CodegenError::UnsupportedProgram(
                    "native casts currently support u64/usize values that already use word representation",
                ))
            } else if is_word_value(&value) {
                Ok(value)
            } else if generic_argument_type(&value).as_deref() == Some("i32") {
                Ok(Value::SignExtend(Box::new(value)))
            } else {
                Err(CodegenError::UnsupportedProgram(
                    "native casts currently support u64/usize targets from word or i32 values",
                ))
            }
        }
        BodyExprKind::Negate(operand) => {
            let value = value_from_expression(operand, environment, functions)?;
            if let Value::Integer(number) = value {
                number
                    .checked_neg()
                    .map(Value::Integer)
                    .ok_or(CodegenError::UnsupportedProgram(
                        "constant negation overflows `i32`",
                    ))
            } else {
                Ok(Value::Negate(Box::new(value)))
            }
        }
        BodyExprKind::Not(operand) => {
            let value = value_from_expression(operand, environment, functions)?;
            Ok(Value::If {
                condition: Condition::Compare {
                    operator: BinaryOperator::Equal,
                    left: Box::new(value),
                    right: Box::new(Value::Integer(0)),
                },
                then_value: Box::new(Value::Integer(1)),
                else_value: Box::new(Value::Integer(0)),
            })
        }
        BodyExprKind::Block {
            statements,
            tail: Some(tail),
        } if statements.is_empty() => value_from_expression(tail, environment, functions),
        BodyExprKind::If {
            condition,
            then_branch,
            else_branch: Some(else_branch),
        } => Ok(Value::If {
            condition: condition_from_expression(condition, environment, functions)?,
            then_value: Box::new(value_from_expression(then_branch, environment, functions)?),
            else_value: Box::new(value_from_expression(else_branch, environment, functions)?),
        }),
        BodyExprKind::Match { scrutinee, arms } => {
            if !is_repeatable_match_scrutinee(scrutinee) {
                return Err(CodegenError::UnsupportedExpression(
                    "match scrutinee must be a literal or local path in this backend slice"
                        .to_owned(),
                ));
            }
            let mut fallback = None;
            let mut patterns = Vec::new();
            let mut bool_true = false;
            let mut bool_false = false;
            let mut enum_patterns = HashMap::<String, HashSet<String>>::new();
            let mut enum_variant_counts = HashMap::<String, usize>::new();
            for (index, arm) in arms.iter().enumerate() {
                for pattern in match_pattern_alternatives(&arm.pattern) {
                    let mut arm_environment = environment.clone();
                    if let MatchPattern::Binding(name) = pattern {
                        arm_environment.insert(
                            name.clone(),
                            value_from_expression(scrutinee, environment, functions)?,
                        );
                    }
                    let arm_value = value_from_expression(&arm.body, &arm_environment, functions)?;
                    match pattern {
                        MatchPattern::Wildcard => {
                            if fallback.is_some() || index + 1 != arms.len() {
                                return Err(CodegenError::UnsupportedExpression(
                                    "wildcard match arm must be last".to_owned(),
                                ));
                            }
                            fallback = Some(arm_value.clone());
                        }
                        MatchPattern::Binding(_) => {
                            if fallback.is_some() || index + 1 != arms.len() {
                                return Err(CodegenError::UnsupportedExpression(
                                    "binding match arm must be last".to_owned(),
                                ));
                            }
                            fallback = Some(arm_value.clone());
                        }
                        MatchPattern::Literal(literal) => {
                            match literal {
                                LiteralKind::Boolean(true) => bool_true = true,
                                LiteralKind::Boolean(false) => bool_false = true,
                                _ => {}
                            }
                            let left = value_from_expression(scrutinee, environment, functions)?;
                            let right = value_from_expression(
                                &BodyExpr {
                                    kind: BodyExprKind::Literal(literal.clone()),
                                    span: arm.span.clone(),
                                },
                                environment,
                                functions,
                            )?;
                            patterns.push((
                                Condition::Compare {
                                    operator: BinaryOperator::Equal,
                                    left: Box::new(left),
                                    right: Box::new(right),
                                },
                                arm_value.clone(),
                            ));
                        }
                        MatchPattern::Range {
                            start,
                            end,
                            inclusive,
                        } => {
                            let make_comparison = |operator, literal: &LiteralKind| {
                                Ok::<_, CodegenError>(Condition::Compare {
                                    operator,
                                    left: Box::new(value_from_expression(
                                        scrutinee,
                                        environment,
                                        functions,
                                    )?),
                                    right: Box::new(value_from_expression(
                                        &BodyExpr {
                                            kind: BodyExprKind::Literal(literal.clone()),
                                            span: arm.span.clone(),
                                        },
                                        environment,
                                        functions,
                                    )?),
                                })
                            };
                            patterns.push((
                                Condition::And(
                                    Box::new(make_comparison(BinaryOperator::GreaterEqual, start)?),
                                    Box::new(make_comparison(
                                        if *inclusive {
                                            BinaryOperator::LessEqual
                                        } else {
                                            BinaryOperator::Less
                                        },
                                        end,
                                    )?),
                                ),
                                arm_value.clone(),
                            ));
                        }
                        MatchPattern::Path(_) => {
                            return Err(CodegenError::UnsupportedExpression(
                                "enum variant pattern was not resolved before codegen".to_owned(),
                            ));
                        }
                        MatchPattern::EnumVariant {
                            path,
                            discriminant,
                            variant_count,
                        } => {
                            let Some((enum_path, variant)) = path.rsplit_once("::") else {
                                return Err(CodegenError::UnsupportedExpression(
                                    "invalid enum variant pattern path".to_owned(),
                                ));
                            };
                            enum_patterns
                                .entry(enum_path.to_owned())
                                .or_default()
                                .insert(variant.to_owned());
                            enum_variant_counts.insert(enum_path.to_owned(), *variant_count);
                            let literal = LiteralKind::Integer {
                                text: discriminant.to_string(),
                                suffix: "i32".to_owned(),
                            };
                            let right = value_from_expression(
                                &BodyExpr {
                                    kind: BodyExprKind::Literal(literal),
                                    span: arm.span.clone(),
                                },
                                environment,
                                functions,
                            )?;
                            patterns.push((
                                Condition::Compare {
                                    operator: BinaryOperator::Equal,
                                    left: Box::new(value_from_expression(
                                        scrutinee,
                                        environment,
                                        functions,
                                    )?),
                                    right: Box::new(right),
                                },
                                arm_value.clone(),
                            ));
                        }
                        MatchPattern::Or(_) => {
                            unreachable!("or-pattern alternatives are flattened")
                        }
                        MatchPattern::Unsupported => {
                            return Err(CodegenError::UnsupportedExpression(
                                "match pattern is outside the supported scalar subset".to_owned(),
                            ));
                        }
                    }
                }
            }
            let exhaustive_bool = bool_true && bool_false;
            let exhaustive_enum = enum_patterns.iter().any(|(enum_path, variants)| {
                enum_variant_counts
                    .get(enum_path)
                    .is_some_and(|count| variants.len() == *count)
            });
            if fallback.is_none() && !exhaustive_bool && !exhaustive_enum {
                return Err(CodegenError::UnsupportedExpression(
                    "match requires a wildcard arm or both boolean values".to_owned(),
                ));
            }
            let mut value = fallback
                .or_else(|| patterns.pop().map(|(_, value)| value))
                .ok_or(CodegenError::UnsupportedExpression(
                    "match has no arms".to_owned(),
                ))?;
            for (condition, arm_value) in patterns.into_iter().rev() {
                value = Value::If {
                    condition,
                    then_value: Box::new(arm_value),
                    else_value: Box::new(value),
                };
            }
            Ok(value)
        }
        BodyExprKind::Dereference(expression) => {
            let BodyExprKind::Path(name) = &expression.kind else {
                return Err(CodegenError::UnsupportedProgram(
                    "native dereference currently requires a scalar local",
                ));
            };
            match environment.get(name) {
                Some(
                    value @ (Value::Local(_) | Value::WordLocal(_) | Value::TypedWordLocal { .. }),
                ) => Ok(value.clone()),
                _ => Err(CodegenError::UnsupportedProgram(
                    "native dereference currently requires a scalar local",
                )),
            }
        }
        BodyExprKind::Binary {
            operator,
            left,
            right,
        } => {
            let mut l = value_from_expression(left, environment, functions)?;
            let mut r = value_from_expression(right, environment, functions)?;
            let is_arithmetic = matches!(
                operator,
                BinaryOperator::Add
                    | BinaryOperator::Subtract
                    | BinaryOperator::Multiply
                    | BinaryOperator::Divide
                    | BinaryOperator::Remainder
                    | BinaryOperator::BitXor
            );
            if !is_arithmetic {
                return Err(CodegenError::UnsupportedProgram(
                    "only integer addition, subtraction, multiplication, division, and remainder are supported as values",
                ));
            }
            if let (Value::Integer(l), Value::Integer(r)) = (&l, &r) {
                let result = match operator {
                    BinaryOperator::Add => l.checked_add(*r),
                    BinaryOperator::Subtract => l.checked_sub(*r),
                    BinaryOperator::Multiply => l.checked_mul(*r),
                    BinaryOperator::Divide => l.checked_div(*r),
                    BinaryOperator::Remainder => l.checked_rem(*r),
                    _ => unreachable!("arithmetic operator filtered above"),
                };
                result
                    .map(Value::Integer)
                    .ok_or(CodegenError::UnsupportedProgram(
                        "constant arithmetic overflows or divides by zero",
                    ))
            } else {
                if is_word_value(&l)
                    && let Value::Integer(value) = &r
                    && *value >= 0
                {
                    r = Value::Word(*value as u64);
                }
                if is_word_value(&r)
                    && let Value::Integer(value) = &l
                    && *value >= 0
                {
                    l = Value::Word(*value as u64);
                }
                if is_word_value(&l)
                    && is_word_value(&r)
                    && matches!(
                        operator,
                        BinaryOperator::Add
                            | BinaryOperator::Subtract
                            | BinaryOperator::Multiply
                            | BinaryOperator::Divide
                            | BinaryOperator::Remainder
                            | BinaryOperator::BitXor
                    )
                {
                    Ok(Value::WrappingBinary {
                        operator: *operator,
                        left: Box::new(l),
                        right: Box::new(r),
                    })
                } else {
                    Ok(Value::Binary {
                        operator: *operator,
                        left: Box::new(l),
                        right: Box::new(r),
                    })
                }
            }
        }
        BodyExprKind::Path(path) => match environment.get(path).cloned() {
            Some(Value::StructLocal { type_name, .. }) => {
                Err(CodegenError::UnsupportedExpression(format!(
                    "struct value `{path}: {type_name}` can only be used through a supported field read"
                )))
            }
            Some(value) => Ok(value),
            None if is_option_none_path(path) => Ok(Value::Integer(0)),
            None if path
                .chars()
                .next()
                .is_some_and(char::is_uppercase)
                && !path.contains("::") => Ok(Value::StructLocal {
                type_name: path.clone(),
                fields: HashMap::new(),
            }),
            None => Err(CodegenError::UnsupportedExpression(format!(
                "path `{path}` does not name a supported value, unit struct, or enum variant"
            ))),
        },
        BodyExprKind::FieldAccess { base, field } => {
            if let BodyExprKind::Call { callee, .. } = &base.kind
                && let Some(function) = functions.iter().find(|function| function.name == *callee)
                && function
                    .return_type
                    .as_deref()
                    .and_then(pair_type_fields)
                    .is_some()
            {
                let index = field
                    .parse::<usize>()
                    .ok()
                    .filter(|index| *index < 2)
                    .ok_or(CodegenError::UnsupportedProgram(
                        "tuple field projection index must be zero or one",
                    ))?;
                return Ok(Value::PairField {
                    aggregate: Box::new(value_from_expression(base, environment, functions)?),
                    index,
                });
            }
            let BodyExprKind::Path(name) = &base.kind else {
                return Err(CodegenError::UnsupportedProgram(
                    "field reads currently require a local struct value",
                ));
            };
            if let Some(Value::Tuple(fields)) = environment.get(name) {
                let index = field
                    .parse::<usize>()
                    .ok()
                    .filter(|index| *index < fields.len())
                    .ok_or(CodegenError::UnsupportedProgram(
                        "tuple field projection index is out of bounds",
                    ))?;
                return Ok(fields[index].clone());
            }
            let Some(Value::StructLocal { fields, .. }) = environment.get(name) else {
                return Err(CodegenError::UnsupportedProgram(
                    "field read base is not a supported local struct",
                ));
            };
            fields.get(field).map(value_from_struct_local_field).ok_or(
                CodegenError::UnsupportedProgram("struct field does not exist"),
            )
        }
        BodyExprKind::Index { base, index, .. } => {
            let array = value_from_expression(base, environment, functions)?;
            let index = value_from_expression(index, environment, functions)?;
            let index = match index {
                Value::Integer(index) if index >= 0 => index as usize,
                Value::Word(index) => usize::try_from(index).map_err(|_| {
                    CodegenError::UnsupportedProgram("array index does not fit this target")
                })?,
                Value::TypedWord { value, .. } => usize::try_from(value).map_err(|_| {
                    CodegenError::UnsupportedProgram("array index does not fit this target")
                })?,
                _ => {
                    return Err(CodegenError::UnsupportedProgram(
                        "array indexing currently requires a constant integer index",
                    ));
                }
            };
            let Value::StaticArray(elements) = array else {
                return Err(CodegenError::UnsupportedProgram(
                    "array indexing currently requires a statically known array",
                ));
            };
            elements
                .get(index)
                .cloned()
                .ok_or(CodegenError::UnsupportedProgram(
                    "constant array index is out of bounds",
                ))
        }
        BodyExprKind::Tuple(elements) => Ok(Value::Tuple(
            elements
                .iter()
                .map(|element| value_from_expression(element, environment, functions))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        BodyExprKind::ArrayLiteral(elements) | BodyExprKind::FixedArrayLiteral(elements) => {
            Ok(Value::StaticArray(
                elements
                    .iter()
                    .map(|element| value_from_expression(element, environment, functions))
                    .collect::<Result<Vec<_>, _>>()?,
            ))
        }
        BodyExprKind::Call {
            callee, arguments, ..
        } if matches!(
            callee.as_str(),
            "String::from" | "std::string::String::from"
        ) && arguments.len() == 1 =>
        {
            let value = value_from_expression(&arguments[0], environment, functions)?;
            if is_slice_value(&value) {
                Ok(Value::OwnedString(Box::new(value)))
            } else {
                Err(CodegenError::UnsupportedProgram(
                    "`String::from` currently requires a supported string-slice value",
                ))
            }
        }
        BodyExprKind::Call {
            callee, arguments, ..
        } if (callee == "Ok" || callee == "Err")
            && arguments.len() == 1
            && !functions.iter().any(|function| function.name == *callee) =>
        {
            if callee == "Ok" && matches!(arguments[0].kind, BodyExprKind::Unit) {
                // For Result<(), &'static str>, rustc uses a null pointer as
                // the Ok discriminant. The second word is initialized too.
                Ok(Value::NicheOk)
            } else if callee == "Err"
                && !matches!(
                    arguments[0].kind,
                    BodyExprKind::Literal(LiteralKind::String { .. })
                )
            {
                let value = value_from_expression(&arguments[0], environment, functions)?;
                if is_slice_value(&value) {
                    return Err(CodegenError::UnsupportedProgram(
                        "tagged Result constructors require a supported scalar payload",
                    ));
                }
                Ok(Value::Tuple(vec![
                    Value::Integer(if callee == "Err" { 1 } else { 0 }),
                    value,
                ]))
            } else if callee == "Ok" {
                let value = value_from_expression(&arguments[0], environment, functions)?;
                if is_slice_value(&value) {
                    Err(CodegenError::UnsupportedProgram(
                        "Ok payload is outside the supported native Result subset",
                    ))
                } else {
                    Ok(Value::Tuple(vec![Value::Integer(0), value]))
                }
            } else if callee == "Err" {
                let value = value_from_expression(&arguments[0], environment, functions)?;
                if is_slice_value(&value) {
                    Ok(value)
                } else {
                    Err(CodegenError::UnsupportedProgram(
                        "`Err` requires a string-slice payload in the niche `Result` subset",
                    ))
                }
            } else {
                Err(CodegenError::UnsupportedProgram(
                    "`Ok` currently supports unit in the string-reference niche `Result` subset",
                ))
            }
        }
        BodyExprKind::Call {
            callee, arguments, ..
        } if is_option_some_path(callee)
            && arguments.len() == 1
            && !functions.iter().any(|function| function.name == *callee) =>
        {
            if matches!(arguments[0].kind, BodyExprKind::Unit) {
                Ok(Value::Integer(1))
            } else {
                let value = value_from_expression(&arguments[0], environment, functions)?;
                if is_word_value(&value) || is_slice_value(&value) {
                    Err(CodegenError::UnsupportedProgram(
                        "native Option construction currently supports Some(()) or Some(i32)",
                    ))
                } else {
                    Ok(Value::Tuple(vec![Value::Integer(1), value]))
                }
            }
        }
        BodyExprKind::Call { .. }
            if is_supported_empty_collection_expression(expr)
                || is_supported_string_buffer_expression(expr) =>
        {
            Ok(Value::EmptyCollection)
        }
        BodyExprKind::Call {
            callee,
            generic_arguments,
            arguments,
        } if !callee.is_empty() && arguments.len() <= 64 => {
            let function = functions.iter().find(|function| function.name == *callee);
            let mut args = Vec::with_capacity(arguments.len());
            for (argument_index, argument) in arguments.iter().enumerate() {
                let explicit_type =
                    function
                        .filter(|function| function.is_generic)
                        .and_then(|function| {
                            let parameter = function.parameters.get(argument_index)?;
                            let generic_index = function
                                .generic_parameters
                                .iter()
                                .position(|generic| generic == parameter.type_text.trim())?;
                            generic_arguments.get(generic_index).map(String::as_str)
                        });
                if matches!(explicit_type, Some("u64" | "usize"))
                    && let BodyExprKind::Literal(LiteralKind::Integer { text, suffix }) =
                        &argument.kind
                    && suffix.is_empty()
                {
                    let value = text.parse::<u64>().map_err(|_| {
                        CodegenError::UnsupportedProgram(
                            "explicit unsigned generic integer literal must fit `u64`",
                        )
                    })?;
                    args.push(Value::TypedWord {
                        value,
                        type_name: explicit_type.unwrap().to_owned(),
                    });
                } else if function
                    .and_then(|function| function.parameters.get(argument_index))
                    .is_some_and(|parameter| {
                        !is_slice_type(&parameter.type_text)
                            && (referenced_type_name(&parameter.type_text).is_some()
                                || matches!(
                                    &argument.kind,
                                    BodyExprKind::Path(name)
                                        if matches!(environment.get(name), Some(Value::StructLocal { .. }))
                                ))
                    })
                    && let Some(name) = match &argument.kind {
                        BodyExprKind::Path(name) => Some(name.as_str()),
                        BodyExprKind::Reference { expression, .. } => match &expression.kind {
                            BodyExprKind::Path(name) => Some(name.as_str()),
                            _ => None,
                        },
                        _ => None,
                    }
                    && let Some(value @ Value::StructLocal { .. }) = environment.get(name)
                {
                    args.push(value.clone());
                } else if let Some(parameter) = function
                    .filter(|function| !function.is_generic)
                    .and_then(|function| function.parameters.get(argument_index))
                    .filter(|parameter| is_slice_type(&parameter.type_text))
                    && let BodyExprKind::Reference { expression, .. } = &argument.kind
                    && let BodyExprKind::Path(name) = &expression.kind
                    && let Some(Value::CollectionLocal {
                        pointer_slot,
                        length_slot,
                        ..
                    }) = environment.get(name)
                {
                    let expected_element = parameter
                        .type_text
                        .trim()
                        .strip_prefix("&[")
                        .and_then(|inner| inner.strip_suffix(']'))
                        .unwrap_or("")
                        .trim();
                    let width = scalar_element_width(expected_element);
                    if width != 0 || expected_element == "String" || expected_element == "&str" {
                        args.push(Value::SliceLocal {
                            pointer_slot: *pointer_slot,
                            length_slot: *length_slot,
                            element_widths: vec![if width == 0 { 16 } else { width }],
                        });
                    } else {
                        args.push(Value::SliceRecord {
                            pointer_slot: *pointer_slot,
                            length_slot: *length_slot,
                            record_type: expected_element.to_owned(),
                        });
                    }
                } else {
                    args.push(value_from_expression(argument, environment, functions)?);
                }
            }
            if let Some(function) = function {
                for (index, argument) in args.iter_mut().enumerate() {
                    let Some(parameter) = function.parameters.get(index) else {
                        continue;
                    };
                    let expected_type = referenced_type_name(&parameter.type_text).map(str::to_owned).or_else(|| {
                        matches!(argument, Value::StructLocal { .. }).then_some(
                            parameter
                                .type_text
                                .trim()
                                .strip_prefix("mut ")
                                .unwrap_or(parameter.type_text.trim())
                                .to_owned(),
                        )
                    });
                    let expected_type = expected_type.map(|expected_type| {
                        let generic_index = function.is_generic.then(|| {
                            function
                                .generic_parameters
                                .iter()
                                .position(|generic| generic == &expected_type)
                        }).flatten();
                        generic_index
                            .and_then(|index| generic_arguments.get(index).cloned())
                            .or_else(|| {
                                generic_index
                                    .and_then(|_| generic_argument_type(argument))
                            })
                            .unwrap_or(expected_type)
                    });
                    let Some(expected_type) = expected_type else {
                        continue;
                    };
                    if is_slice_type(&parameter.type_text) {
                        continue;
                    }
                    let Value::StructLocal { type_name, fields } = argument else {
                        return Err(CodegenError::UnsupportedProgram(
                            "borrowed struct arguments must refer to a local struct value",
                        ));
                    };
                    if !type_name
                        .rsplit("::")
                        .next()
                        .unwrap_or(type_name)
                        .eq_ignore_ascii_case(expected_type.rsplit("::").next().unwrap_or(&expected_type))
                    {
                        return Err(CodegenError::UnsupportedExpression(format!(
                            "borrowed struct `{type_name}` does not match parameter type `{expected_type}`"
                        )));
                    }
                    let mut ordered = fields.iter().collect::<Vec<_>>();
                    ordered.sort_by(|left, right| left.0.cmp(right.0));
                    *argument = Value::StructReference {
                        type_name: type_name.clone(),
                        fields: ordered
                            .into_iter()
                            .map(|(_, field)| {
                                if parameter.type_text.trim_start().starts_with("&mut ") {
                                    address_of_struct_local_field(field)
                                } else {
                                    Ok(value_from_struct_local_field(field))
                                }
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                    };
                }
            }
            let target_name = if let Some(function) =
                function.filter(|function| function.is_generic)
            {
                if !generic_arguments.is_empty() {
                    if generic_arguments.len() != function.generic_parameters.len() {
                        return Err(CodegenError::UnsupportedProgram(
                            "explicit generic argument count does not match the function definition",
                        ));
                    }
                    for (parameter_index, parameter) in function.parameters.iter().enumerate() {
                        if let Some(generic_index) = function
                            .generic_parameters
                            .iter()
                            .position(|generic| generic == parameter.type_text.trim())
                            && matches!(generic_arguments[generic_index].as_str(), "u64" | "usize")
                            && let Value::Integer(integer) = args[parameter_index]
                        {
                            if integer < 0 {
                                return Err(CodegenError::UnsupportedProgram(
                                    "negative integer cannot instantiate an unsigned generic type",
                                ));
                            }
                            args[parameter_index] = Value::TypedWord {
                                value: integer as u64,
                                type_name: generic_arguments[generic_index].clone(),
                            };
                        }
                    }
                }
                let index = functions
                    .iter()
                    .position(|candidate| std::ptr::eq(candidate, function))
                    .expect("generic callee came from the same function table");
                generic_instance_name(index, function, &args, generic_arguments).ok_or(
                    CodegenError::UnsupportedProgram(
                        "generic call arguments must resolve to consistent i32, u64, or usize instances",
                    ),
                )?
            } else {
                if !generic_arguments.is_empty() {
                    return Err(CodegenError::UnsupportedProgram(
                        "explicit type arguments require a supported generic function",
                    ));
                }
                callee.clone()
            };
            let generic_return_type =
                function
                    .filter(|function| function.is_generic)
                    .and_then(|function| {
                        let return_type = function.return_type.as_deref()?.trim();
                        let generic_index = function
                            .generic_parameters
                            .iter()
                            .position(|generic| generic == return_type)?;
                        let parameter_index = function.parameters.iter().position(|parameter| {
                            parameter.type_text.trim() == function.generic_parameters[generic_index]
                        })?;
                        generic_argument_type(args.get(parameter_index)?)
                    });
            let arguments = args
                .into_iter()
                .enumerate()
                .map(|(index, value)| {
                    if !function.is_some_and(|function| function.is_generic)
                        && function
                            .and_then(|function| function.parameters.get(index))
                            .is_some_and(|parameter| is_word_type(&parameter.type_text))
                    {
                        match value {
                            Value::Integer(integer) if integer >= 0 => Value::TypedWord {
                                value: integer as u64,
                                type_name: function
                                    .and_then(|function| function.parameters.get(index))
                                    .map(|parameter| parameter.type_text.clone())
                                    .unwrap_or_else(|| "u64".to_owned()),
                            },
                            value => value,
                        }
                    } else {
                        value
                    }
                })
                .collect();
            Ok(Value::Call {
                name: target_name,
                arguments,
                returns_word: function
                    .and_then(|function| function.return_type.as_deref())
                    .is_some_and(|return_type| {
                        is_word_type(return_type) || is_null_pointer_result_type(return_type)
                    })
                    || generic_return_type.as_deref().is_some_and(is_word_type),
                word_type: generic_return_type.or_else(|| {
                    function
                        .and_then(|function| function.return_type.as_deref())
                        .filter(|return_type| {
                            is_word_type(return_type) || is_null_pointer_result_type(return_type)
                        })
                        .map(str::to_owned)
                }),
                returns_pair: function
                    .and_then(|function| function.return_type.as_deref())
                    .is_some_and(|return_type| {
                        pair_type_fields(return_type).is_some()
                            || is_i32_option_type(return_type)
                            || is_native_tagged_result_type(return_type)
                    }),
                returns_slice: function
                    .and_then(|function| function.return_type.as_deref())
                    .is_some_and(is_slice_type),
                returns_aggregate: false,
                aggregate_destination: None,
            })
        }
        BodyExprKind::MacroInvocation(macro_expression) if macro_expression.path == "format" => {
            if !macro_expression.arguments.is_empty() {
                return Err(CodegenError::UnsupportedProgram(
                    "`format!` currently supports named captures only",
                ));
            }
            let format = macro_expression.format_string.as_deref().ok_or(
                CodegenError::UnsupportedProgram("`format!` must begin with a string literal"),
            )?;
            Ok(Value::FormatString(lower_format_pieces(
                format,
                environment,
            )?))
        }
        BodyExprKind::MacroInvocation(macro_expression)
            if matches!(macro_expression.path.as_str(), "concat" | "stringify") =>
        {
            let value = macro_expression.const_expansion.as_ref().ok_or(
                CodegenError::UnsupportedProgram(match macro_expression.path.as_str() {
                    "concat" => "`concat!` currently requires literal arguments",
                    "stringify" => "`stringify!` could not render its token stream",
                    _ => unreachable!(),
                }),
            )?;
            Ok(Value::TextSlice(value.clone()))
        }
        BodyExprKind::MacroInvocation(macro_expression) if macro_expression.path == "env" => {
            let name = macro_expression.format_string.as_deref().ok_or(
                CodegenError::UnsupportedProgram("`env!` requires a string literal name"),
            )?;
            std::env::var(name)
                .map(Value::TextSlice)
                .map_err(|_| {
                    CodegenError::UnsupportedExpression(format!(
                        "`env!` could not read environment variable `{name}`"
                    ))
                })
        }
        BodyExprKind::MacroInvocation(_) => Err(CodegenError::UnsupportedProgram(
            "expression macro has no native expansion",
        )),
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } if method == "is_some" && arguments.is_empty() => {
            if let BodyExprKind::Path(name) = &receiver.kind
                && let Some(value @ Value::OptionIsSome(_)) = environment.get(name)
            {
                return Ok(value.clone());
            }
            let Some(callee) = receiver_call_name(receiver, environment) else {
                return Err(CodegenError::UnsupportedProgram(
                    "`.is_some()` currently requires a supported Option function call",
                ));
            };
            let option_function = functions
                .iter()
                .find(|function| function.name == callee)
                .ok_or(CodegenError::UnsupportedProgram(
                    "`.is_some()` call target is unresolved",
                ))?;
            if !option_function
                .return_type
                .as_deref()
                .is_some_and(|return_type| {
                    is_unit_option_type(return_type) || is_i32_option_type(return_type)
                })
            {
                return Err(CodegenError::UnsupportedProgram(
                    "`.is_some()` currently requires Option<()>.",
                ));
            }
            Ok(Value::OptionIsSome(Box::new(value_from_expression(
                receiver,
                environment,
                functions,
            )?)))
        }
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } if method == "unwrap_or" && arguments.len() == 1 => {
            let Some(callee) = receiver_call_name(receiver, environment) else {
                return Err(CodegenError::UnsupportedProgram(
                    "`.unwrap_or()` currently requires a supported Option or Result call",
                ));
            };
            let option_function = functions
                .iter()
                .find(|function| function.name == callee)
                .ok_or(CodegenError::UnsupportedProgram(
                    "`.unwrap_or()` call target is unresolved",
                ))?;
            let Some(return_type) = option_function.return_type.as_deref() else {
                return Err(CodegenError::UnsupportedProgram(
                    "`.unwrap_or()` requires Option<i32> or a supported tagged Result",
                ));
            };
            let success_on_zero = is_native_tagged_result_type(return_type);
            let payload_is_word = success_on_zero && tagged_result_payload_is_word(return_type);
            if !is_i32_option_type(return_type) && !success_on_zero {
                return Err(CodegenError::UnsupportedProgram(
                    "`.unwrap_or()` currently requires Option<i32> or a supported tagged Result",
                ));
            }
            let option = value_from_expression(receiver, environment, functions)?;
            let fallback = value_from_expression(&arguments[0], environment, functions)?;
            if (is_word_value(&fallback) && !payload_is_word)
                || (!is_word_value(&fallback) && payload_is_word)
                || is_slice_value(&fallback)
            {
                return Err(CodegenError::UnsupportedProgram(
                    "`.unwrap_or()` fallback must match the supported enum payload type",
                ));
            }
            Ok(Value::OptionUnwrapOr {
                option: Box::new(option),
                fallback: Box::new(fallback),
                success_on_zero,
                payload_is_word,
            })
        }
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } if matches!(method.as_str(), "is_ok" | "is_err") && arguments.is_empty() => {
            let return_type = direct_call_callee(receiver)
                .and_then(|callee| {
                    functions
                        .iter()
                        .find(|function| function.name == callee)
                        .and_then(|function| function.return_type.clone())
                })
                .or_else(|| match &receiver.kind {
                    BodyExprKind::Path(name) => {
                        environment.get(name).and_then(generic_argument_type)
                    }
                    _ => None,
                })
                .ok_or(CodegenError::UnsupportedProgram(
                    "Result status checks require a supported Result value",
                ))?;
            let layout =
                ResultLayout::parse(&return_type).ok_or(CodegenError::UnsupportedProgram(
                    "`.is_ok()` requires a supported `Result` return type",
                ))?;
            if !matches!(layout.encoding, ResultEncoding::NullPointerNiche { .. })
                && !is_native_tagged_result_type(&return_type)
            {
                return Err(CodegenError::UnsupportedProgram(
                    "Result status checks require a supported native Result layout",
                ));
            }
            let is_ok = Value::ResultIsOk(Box::new(value_from_expression(
                receiver,
                environment,
                functions,
            )?));
            if method == "is_ok" {
                Ok(is_ok)
            } else {
                Ok(Value::If {
                    condition: Condition::Compare {
                        operator: BinaryOperator::Equal,
                        left: Box::new(is_ok),
                        right: Box::new(Value::Integer(0)),
                    },
                    then_value: Box::new(Value::Integer(1)),
                    else_value: Box::new(Value::Integer(0)),
                })
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } if method == "as_ref" && arguments.is_empty() => {
            let value = value_from_expression(receiver, environment, functions)?;
            if !is_slice_value(&value) {
                return Err(CodegenError::UnsupportedProgram(
                    "`AsRef<str>` requires a supported string slice value",
                ));
            }
            Ok(value)
        }
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } if matches!(method.as_str(), "to_owned" | "into") && arguments.is_empty() => {
            let value = value_from_expression(receiver, environment, functions)?;
            if !is_slice_value(&value) {
                return Err(CodegenError::UnsupportedProgram(
                    "string ownership conversion requires a string or slice value",
                ));
            }
            Ok(Value::OwnedString(Box::new(value)))
        }
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } if method == "len" && arguments.is_empty() => {
            let value = value_from_expression(receiver, environment, functions)?;
            match value {
                Value::SliceParameter { length_slot, .. }
                | Value::SliceLocal { length_slot, .. } => Ok(Value::WordLocal(length_slot)),
                Value::StaticArray(elements) => Ok(Value::Word(elements.len() as u64)),
                Value::CollectionLocal { length_slot, .. } => Ok(Value::WordLocal(length_slot)),
                value @ Value::Call {
                    returns_slice: true,
                    ..
                } => Ok(Value::SliceLength(Box::new(value))),
                Value::TextSlice(text) => Ok(Value::Word(text.len() as u64)),
                _ => Err(CodegenError::UnsupportedProgram(
                    "slice `len` requires a supported string or slice value",
                )),
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } if method == "iter" && arguments.is_empty() => {
            match value_from_expression(receiver, environment, functions)? {
                Value::CollectionLocal {
                    pointer_slot,
                    length_slot,
                    kind: CollectionKind::Vector,
                    element_type: Some(element_type),
                    ..
                } => {
                    let width = scalar_element_width(&element_type);
                    if width != 0 {
                        Ok(Value::SliceLocal {
                            pointer_slot,
                            length_slot,
                            element_widths: vec![width],
                        })
                    } else if element_type.rsplit("::").next() == Some("String") {
                        Ok(Value::SliceLocal {
                            pointer_slot,
                            length_slot,
                            element_widths: vec![16],
                        })
                    } else {
                        Ok(Value::SliceRecord {
                            pointer_slot,
                            length_slot,
                            record_type: element_type,
                        })
                    }
                }
                value @ Value::SliceRecord { .. } => Ok(value),
                value @ (Value::SliceParameter { .. }
                | Value::SliceLocal { .. }
                | Value::StaticArray(_)) => Ok(value),
                _ => Err(CodegenError::UnsupportedProgram(
                    "`.iter()` currently requires a supported borrowed slice or record vector",
                )),
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } if method == "chars" && arguments.is_empty() => {
            match value_from_expression(receiver, environment, functions)? {
                Value::SliceParameter {
                    pointer_slot,
                    length_slot,
                    ..
                }
                | Value::SliceLocal {
                    pointer_slot,
                    length_slot,
                    ..
                } => Ok(Value::CharIterator {
                    pointer_slot,
                    length_slot,
                }),
                _ => Err(CodegenError::UnsupportedProgram(
                    "`.chars()` requires a string slice stored in local pointer/length slots",
                )),
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } if method == "is_ascii_alphanumeric" && arguments.is_empty() => {
            Ok(Value::CharIsAsciiAlphanumeric(Box::new(
                value_from_expression(receiver, environment, functions)?,
            )))
        }
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } if method == "is_empty" && arguments.is_empty() => {
            let value = value_from_expression(receiver, environment, functions)?;
            if is_slice_value(&value)
                || matches!(
                    value,
                    Value::EmptyCollection | Value::CollectionLocal { .. }
                )
            {
                Ok(Value::SliceIsEmpty(Box::new(value)))
            } else {
                Err(CodegenError::UnsupportedProgram(
                    "`.is_empty()` requires a supported string or slice value",
                ))
            }
        }
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } if matches!(method.as_str(), "wrapping_add" | "wrapping_mul") && arguments.len() == 1 => {
            let left = value_from_expression(receiver, environment, functions)?;
            let right = value_from_expression(&arguments[0], environment, functions)?;
            let right = match right {
                Value::Integer(value) if value >= 0 => Value::Word(value as u64),
                value => value,
            };
            if !is_word_value(&left) || !is_word_value(&right) {
                return Err(CodegenError::UnsupportedProgram(
                    "wrapping arithmetic currently requires `u64` operands",
                ));
            }
            Ok(Value::WrappingBinary {
                operator: if method == "wrapping_add" {
                    BinaryOperator::Add
                } else {
                    BinaryOperator::Multiply
                },
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        BodyExprKind::MethodCall {
            receiver,
            method,
            arguments,
        } => {
            let owner =
                type_name_for_receiver(receiver, environment, functions).ok_or_else(|| {
                    CodegenError::UnsupportedExpression(format!(
                        "method receiver for `{method}` could not be resolved: {:?}",
                        receiver.kind
                    ))
                })?;
            let function = resolve_impl_method(&owner, method, functions).ok_or(
                CodegenError::UnsupportedProgram("method is not a supported inherent method"),
            )?;
            if function.is_generic || function.parameters.len() != arguments.len() + 1 {
                return Err(CodegenError::UnsupportedProgram(
                    "method must have a concrete receiver and matching argument count",
                ));
            }
            let receiver_name = match &receiver.kind {
                BodyExprKind::Path(name) => Some(name.as_str()),
                BodyExprKind::Reference { expression, .. }
                | BodyExprKind::Parenthesized(expression) => match &expression.kind {
                    BodyExprKind::Path(name) => Some(name.as_str()),
                    _ => None,
                },
                _ => None,
            };
            let Value::StructLocal { type_name, fields } = receiver_name
                .and_then(|name| environment.get(name))
                .ok_or(CodegenError::UnsupportedProgram(
                    "method receiver must be a local struct value",
                ))?
            else {
                return Err(CodegenError::UnsupportedProgram(
                    "method receiver must be a local struct value",
                ));
            };
            if !type_name.rsplit("::").next().is_some_and(|receiver| {
                receiver.eq_ignore_ascii_case(owner.rsplit("::").next().unwrap_or(&owner))
            }) {
                return Err(CodegenError::UnsupportedProgram(
                    "method receiver type does not match its implementation",
                ));
            }
            let mut ordered_fields = fields.iter().collect::<Vec<_>>();
            ordered_fields.sort_by(|left, right| left.0.cmp(right.0));
            let mutable_receiver = function.parameters[0]
                .type_text
                .trim_start()
                .starts_with("&mut ");
            let mut call_arguments = vec![Value::StructReference {
                type_name: type_name.clone(),
                fields: ordered_fields
                    .into_iter()
                    .map(|(_, field)| {
                        if mutable_receiver {
                            address_of_struct_local_field(field)
                        } else {
                            Ok(value_from_struct_local_field(field))
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            }];
            for argument in arguments {
                call_arguments.push(value_from_expression(argument, environment, functions)?);
            }
            let return_type = function.return_type.as_deref();
            Ok(Value::Call {
                name: function.name.clone(),
                arguments: call_arguments,
                returns_word: return_type.is_some_and(is_word_type),
                word_type: return_type
                    .filter(|type_name| is_word_type(type_name))
                    .map(str::to_owned),
                returns_pair: return_type.is_some_and(|return_type| {
                    pair_type_fields(return_type).is_some()
                        || is_i32_option_type(return_type)
                        || is_native_tagged_result_type(return_type)
                }),
                returns_slice: return_type.is_some_and(is_slice_type),
                returns_aggregate: false,
                aggregate_destination: None,
            })
        }
        _ => Err(CodegenError::UnsupportedExpression(format!(
            "{:?}",
            expr.kind
        ))),
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

fn is_repeatable_match_scrutinee(expression: &BodyExpr) -> bool {
    match &expression.kind {
        BodyExprKind::Path(_) | BodyExprKind::Literal(_) => true,
        BodyExprKind::Parenthesized(inner) => is_repeatable_match_scrutinee(inner),
        _ => false,
    }
}

fn is_trimmed_empty_string_closure(expression: &BodyExpr) -> bool {
    let BodyExprKind::Closure { parameters, body } = &expression.kind else {
        return false;
    };
    let [quill_syntax::ClosureParameter::Identifier(parameter)] = parameters.as_slice() else {
        return false;
    };
    let BodyExprKind::MethodCall {
        receiver: trim_receiver,
        method: is_empty_method,
        arguments: is_empty_arguments,
    } = &body.kind
    else {
        return false;
    };
    if is_empty_method != "is_empty" || !is_empty_arguments.is_empty() {
        return false;
    }
    let BodyExprKind::MethodCall {
        receiver,
        method,
        arguments,
    } = &trim_receiver.kind
    else {
        return false;
    };
    method == "trim"
        && arguments.is_empty()
        && matches!(&receiver.kind, BodyExprKind::Path(name) if name == parameter)
}

fn lower_format_pieces(
    format: &str,
    environment: &HashMap<String, Value>,
) -> Result<Vec<FormatPiece>, CodegenError> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut characters = format.chars().peekable();
    let mut maximum_length = 0usize;
    while let Some(character) = characters.next() {
        match character {
            '{' if characters.peek() == Some(&'{') => {
                characters.next();
                literal.push('{');
            }
            '}' if characters.peek() == Some(&'}') => {
                characters.next();
                literal.push('}');
            }
            '{' => {
                if !literal.is_empty() {
                    maximum_length = maximum_length.saturating_add(literal.len());
                    pieces.push(FormatPiece::Text(std::mem::take(&mut literal)));
                }
                let mut capture = String::new();
                let mut closed = false;
                for next in characters.by_ref() {
                    if next == '}' {
                        closed = true;
                        break;
                    }
                    capture.push(next);
                }
                let capture = capture.trim();
                if !closed || capture.is_empty() || capture.contains(':') || capture.contains('!') {
                    return Err(CodegenError::UnsupportedProgram(
                        "`format!` supports only simple named decimal captures",
                    ));
                }
                let value =
                    environment
                        .get(capture)
                        .cloned()
                        .ok_or(CodegenError::UnsupportedProgram(
                            "`format!` capture is not a local value",
                        ))?;
                maximum_length = maximum_length.saturating_add(20);
                if is_word_value(&value) {
                    pieces.push(FormatPiece::Decimal(value));
                } else if matches!(
                    value,
                    Value::Local(_) | Value::Parameter(_) | Value::Integer(_)
                ) {
                    pieces.push(FormatPiece::SignedDecimal(value));
                } else {
                    return Err(CodegenError::UnsupportedProgram(
                        "`format!` decimal captures currently support signed i32 and unsigned word values",
                    ));
                }
            }
            '}' => {
                return Err(CodegenError::UnsupportedProgram(
                    "`format!` contains an unmatched closing brace",
                ));
            }
            other => literal.push(other),
        }
    }
    if !literal.is_empty() {
        maximum_length = maximum_length.saturating_add(literal.len());
        pieces.push(FormatPiece::Text(literal));
    }
    if maximum_length > 4096 {
        return Err(CodegenError::UnsupportedProgram(
            "`format!` output may not exceed the native 4096-byte string allocation limit",
        ));
    }
    Ok(pieces)
}

fn condition_from_expression(
    expression: &BodyExpr,
    environment: &HashMap<String, Value>,
    functions: &[FunctionItem],
) -> Result<Condition, CodegenError> {
    condition_from_expression_with_structs(expression, environment, functions, &[])
}

fn condition_from_expression_with_structs(
    expression: &BodyExpr,
    environment: &HashMap<String, Value>,
    functions: &[FunctionItem],
    structs: &[StructItem],
) -> Result<Condition, CodegenError> {
    match &expression.kind {
        BodyExprKind::Literal(LiteralKind::Boolean(value)) => Ok(Condition::Boolean(*value)),
        BodyExprKind::Path(name)
            if matches!(
                environment.get(name),
                Some(Value::Local(_) | Value::Parameter(_))
            ) =>
        {
            Ok(Condition::Compare {
                operator: BinaryOperator::NotEqual,
                left: Box::new(value_from_expression(expression, environment, functions)?),
                right: Box::new(Value::Integer(0)),
            })
        }
        BodyExprKind::MethodCall { method, .. }
            if matches!(method.as_str(), "is_ok" | "is_err" | "is_some") =>
        {
            let value = value_from_expression(expression, environment, functions)?;
            Ok(Condition::Compare {
                operator: BinaryOperator::Equal,
                left: Box::new(value),
                right: Box::new(Value::Integer(1)),
            })
        }
        BodyExprKind::MethodCall {
            method, arguments, ..
        } if method == "any" && arguments.len() == 1 => Ok(Condition::Compare {
            operator: BinaryOperator::NotEqual,
            left: Box::new(value_from_expression(expression, environment, functions)?),
            right: Box::new(Value::Integer(0)),
        }),
        BodyExprKind::MethodCall { method, .. } if method == "is_empty" => {
            let value = value_from_expression(expression, environment, functions)?;
            Ok(Condition::Compare {
                operator: BinaryOperator::Equal,
                left: Box::new(value),
                right: Box::new(Value::Integer(1)),
            })
        }
        BodyExprKind::Not(operand) => Ok(Condition::Compare {
            operator: BinaryOperator::Equal,
            left: Box::new(value_from_expression(operand, environment, functions)?),
            right: Box::new(Value::Integer(0)),
        }),
        BodyExprKind::MethodCall {
            method, arguments, ..
        } if method == "is_ascii_alphanumeric" && arguments.is_empty() => Ok(Condition::Compare {
            operator: BinaryOperator::NotEqual,
            left: Box::new(value_from_expression(expression, environment, functions)?),
            right: Box::new(Value::Integer(0)),
        }),
        BodyExprKind::Parenthesized(inner) => {
            condition_from_expression_with_structs(inner, environment, functions, structs)
        }
        BodyExprKind::Binary {
            operator: BinaryOperator::And,
            left,
            right,
        } => Ok(Condition::And(
            Box::new(condition_from_expression_with_structs(
                left,
                environment,
                functions,
                structs,
            )?),
            Box::new(condition_from_expression_with_structs(
                right,
                environment,
                functions,
                structs,
            )?),
        )),
        BodyExprKind::Binary {
            operator: BinaryOperator::Or,
            left,
            right,
        } => Ok(Condition::Or(
            Box::new(condition_from_expression_with_structs(
                left,
                environment,
                functions,
                structs,
            )?),
            Box::new(condition_from_expression_with_structs(
                right,
                environment,
                functions,
                structs,
            )?),
        )),
        BodyExprKind::Binary {
            operator: BinaryOperator::Equal,
            left,
            right,
        } if let Some(condition) =
            derived_struct_equality_condition(left, right, environment, structs)? =>
        {
            Ok(condition)
        }
        BodyExprKind::Binary {
            operator:
                operator @ (BinaryOperator::Equal
                | BinaryOperator::PatternEqual
                | BinaryOperator::NotEqual
                | BinaryOperator::Less
                | BinaryOperator::LessEqual
                | BinaryOperator::Greater
                | BinaryOperator::GreaterEqual),
            left,
            right,
        } => Ok(Condition::Compare {
            operator: *operator,
            left: Box::new(value_from_expression(left, environment, functions)?),
            right: Box::new(value_from_expression(right, environment, functions)?),
        }),
        _ => Err(CodegenError::UnsupportedProgram(
            "if conditions must be boolean literals or signed integer comparisons",
        )),
    }
}

fn derived_struct_equality_condition(
    left: &BodyExpr,
    right: &BodyExpr,
    environment: &HashMap<String, Value>,
    structs: &[StructItem],
) -> Result<Option<Condition>, CodegenError> {
    let BodyExprKind::Path(left_name) = &left.kind else {
        return Ok(None);
    };
    let BodyExprKind::Path(right_name) = &right.kind else {
        return Ok(None);
    };
    let (
        Some(Value::StructLocal {
            type_name: left_type,
            fields: left_fields,
        }),
        Some(Value::StructLocal {
            type_name: right_type,
            fields: right_fields,
        }),
    ) = (environment.get(left_name), environment.get(right_name))
    else {
        return Ok(None);
    };
    if left_type.rsplit("::").next() != right_type.rsplit("::").next() {
        return Ok(None);
    }
    let Some(structure) = find_struct_type(left_type, structs) else {
        return Ok(None);
    };
    if !structure
        .derived_traits
        .iter()
        .any(|name| name == "PartialEq")
    {
        return Ok(None);
    }

    let mut comparisons = Vec::with_capacity(structure.fields.len());
    for field in &structure.fields {
        let Some(field_name) = field.name.as_deref() else {
            return Ok(None);
        };
        let (Some(left), Some(right)) = (left_fields.get(field_name), right_fields.get(field_name))
        else {
            return Ok(None);
        };
        if !matches!(left, StructLocalField::Scalar { .. })
            || !matches!(right, StructLocalField::Scalar { .. })
        {
            return Ok(None);
        }
        comparisons.push(Condition::Compare {
            operator: BinaryOperator::Equal,
            left: Box::new(value_from_struct_local_field(left)),
            right: Box::new(value_from_struct_local_field(right)),
        });
    }
    let Some(mut condition) = comparisons.pop() else {
        return Ok(Some(Condition::Boolean(true)));
    };
    while let Some(next) = comparisons.pop() {
        condition = Condition::And(Box::new(condition), Box::new(next));
    }
    Ok(Some(condition))
}

fn validate_value_target(
    value: &Value,
    functions: &HashMap<&str, usize>,
    items: &[FunctionItem],
    parameter_count: usize,
) -> Result<(), CodegenError> {
    match value {
        Value::Integer(_) | Value::Word(_) | Value::TypedWord { .. } => Ok(()),
        Value::CollectionAddress { .. } => Ok(()),
        Value::EmptySlice | Value::NicheOk | Value::EmptyCollection | Value::TextSlice(_) => Ok(()),
        Value::StaticArray(elements) => {
            let widths = static_array_element_widths(elements);
            if widths.is_empty() || widths.iter().any(|width| !matches!(width, 4 | 8 | 16)) {
                return Err(CodegenError::UnsupportedProgram(
                    "native arrays support nonempty i32 and 64-bit integer elements",
                ));
            }
            if elements.iter().any(|element| {
                static_array_fields(element).is_none_or(|fields| {
                    fields.len() != widths.len()
                        || fields.iter().zip(&widths).any(|(field, width)| {
                            !matches!(
                                field,
                                Value::Integer(_)
                                    | Value::Word(_)
                                    | Value::TypedWord { .. }
                                    | Value::TextSlice(_)
                            ) || static_array_field_width(field) != *width
                        })
                })
            }) {
                return Err(CodegenError::UnsupportedProgram(
                    "native slice arrays require a homogeneous scalar or two-scalar tuple layout",
                ));
            }
            for element in elements {
                validate_value_target(element, functions, items, parameter_count)?;
            }
            Ok(())
        }
        Value::OwnedString(value) => {
            validate_value_target(value, functions, items, parameter_count)
        }
        Value::FormatString(pieces) => {
            for piece in pieces {
                if let FormatPiece::Decimal(value) | FormatPiece::SignedDecimal(value) = piece {
                    validate_value_target(value, functions, items, parameter_count)?;
                }
            }
            Ok(())
        }
        Value::Parameter(i) if *i < parameter_count => Ok(()),
        Value::Parameter(_) => Err(CodegenError::UnsupportedProgram(
            "main cannot refer to a helper parameter",
        )),
        Value::Local(_) => Ok(()),
        Value::WordLocal(_) => Ok(()),
        Value::TypedWordParameter { slot, .. } if *slot < parameter_count => Ok(()),
        Value::TypedWordParameter { .. } => Err(CodegenError::UnsupportedProgram(
            "main cannot refer to a helper parameter",
        )),
        Value::TypedWordLocal { .. } => Ok(()),
        Value::AddressOfLocal(_) | Value::IndirectLocal { .. } => Ok(()),
        Value::SliceParameter { .. }
        | Value::SliceLocal { .. }
        | Value::SliceRecord { .. }
        | Value::CharIterator { .. }
        | Value::AnyStringTrimEmpty { .. }
        | Value::SliceElementString { .. }
        | Value::SliceElement { .. }
        | Value::CollectionLocal { .. } => Ok(()),
        Value::StructLocal { .. } => Err(CodegenError::UnsupportedProgram(
            "struct values can only be consumed through their fields",
        )),
        Value::StructReference { fields, .. } => {
            for field in fields {
                validate_value_target(field, functions, items, parameter_count)?;
            }
            Ok(())
        }
        Value::SliceIsEmpty(value) | Value::SliceLength(value) => {
            validate_value_target(value, functions, items, parameter_count)
        }
        Value::ZeroExtend(value) | Value::SignExtend(value) => {
            validate_value_target(value, functions, items, parameter_count)
        }
        Value::SliceFold {
            source,
            initial,
            update,
            ..
        } => {
            validate_value_target(source, functions, items, parameter_count)?;
            validate_value_target(initial, functions, items, parameter_count)?;
            validate_value_target(update, functions, items, parameter_count)
        }
        Value::TupleSliceFold {
            source,
            initial,
            updates,
            ..
        } => {
            validate_value_target(source, functions, items, parameter_count)?;
            for value in initial.iter().chain(updates) {
                validate_value_target(value, functions, items, parameter_count)?;
            }
            Ok(())
        }
        Value::Tuple(values) => {
            if !(2..=64).contains(&values.len()) {
                return Err(CodegenError::UnsupportedProgram(
                    "aggregate tuple must contain between two and sixty-four scalar fields",
                ));
            }
            for value in values {
                validate_value_target(value, functions, items, parameter_count)?;
            }
            Ok(())
        }
        Value::PairField { aggregate, index } if *index < 2 => {
            validate_value_target(aggregate, functions, items, parameter_count)
        }
        Value::PairField { .. } => Err(CodegenError::UnsupportedProgram(
            "two-word record field index is outside the returned aggregate",
        )),
        Value::Block { statements, tail } => {
            for statement in statements {
                validate_statement_targets(statement, functions, items, parameter_count)?;
            }
            validate_value_target(tail, functions, items, parameter_count)
        }
        Value::Binary { left, right, .. } => {
            validate_value_target(left, functions, items, parameter_count)?;
            validate_value_target(right, functions, items, parameter_count)
        }
        Value::OptionUnwrapOr {
            option, fallback, ..
        } => {
            validate_value_target(option, functions, items, parameter_count)?;
            validate_value_target(fallback, functions, items, parameter_count)
        }
        Value::WrappingBinary { left, right, .. } => {
            validate_value_target(left, functions, items, parameter_count)?;
            validate_value_target(right, functions, items, parameter_count)
        }
        Value::Negate(operand) => validate_value_target(operand, functions, items, parameter_count),
        Value::ResultIsOk(result) | Value::OptionIsSome(result) => {
            validate_value_target(result, functions, items, parameter_count)
        }
        Value::CharIsAsciiAlphanumeric(character) => {
            validate_value_target(character, functions, items, parameter_count)
        }
        Value::Call {
            name, arguments, ..
        } => {
            let index = functions
                .get(name.as_str())
                .copied()
                .or_else(|| parse_generic_instance_name(name).map(|(index, _)| index))
                .ok_or_else(|| CodegenError::UnresolvedCall(name.clone()))?;
            let normalized_item = normalize_impl_function(&items[index]);
            if name == "main" || arguments.len() != normalized_item.parameters.len() {
                return Err(CodegenError::UnsupportedProgram(
                    "helper call arity does not match its supported signature",
                ));
            }
            let concrete_types = parse_generic_instance_name(name)
                .map(|(_, types)| types)
                .unwrap_or_default();
            let substitutions = items[index]
                .generic_parameters
                .iter()
                .zip(&concrete_types)
                .map(|(generic, concrete)| (generic.as_str(), concrete.as_str()))
                .collect::<HashMap<_, _>>();
            let expected_types = normalized_item
                .parameters
                .iter()
                .map(|parameter| substitute_generic_types(&parameter.type_text, &substitutions))
                .collect::<Vec<_>>();
            if arguments
                .iter()
                .zip(&expected_types)
                .any(|(argument, expected_type)| {
                    let expected_struct =
                        referenced_type_name(expected_type).or_else(|| {
                            matches!(argument, Value::StructReference { .. }).then_some(
                                expected_type
                                    .trim()
                                    .strip_prefix("mut ")
                                    .unwrap_or(expected_type.trim()),
                            )
                        });
                    is_slice_value(argument) != is_slice_type(expected_type)
                        || (matches!(argument, Value::StructReference { .. })
                            != (!is_slice_type(expected_type) && expected_struct.is_some()))
                        || matches!(argument, Value::StructReference { type_name, .. }
                        if !type_name.rsplit("::").next().unwrap_or(type_name).eq_ignore_ascii_case(
                            expected_struct
                                .unwrap_or("")
                                .rsplit("::")
                                .next()
                                .unwrap_or("")
                        ))
                })
            {
                return Err(CodegenError::UnsupportedExpression(format!(
                    "call `{name}` has an argument shape that does not match its parameters: {arguments:?} versus {:?}",
                    normalized_item
                        .parameters
                        .iter()
                        .map(|parameter| &parameter.type_text)
                        .collect::<Vec<_>>()
                )));
            }
            for arg in arguments {
                validate_value_target(arg, functions, items, parameter_count)?;
            }
            Ok(())
        }
        Value::BTreeMapGetString { key, .. } => {
            validate_value_target(key, functions, items, parameter_count)
        }
        Value::If {
            condition,
            then_value,
            else_value,
        } => {
            validate_condition_targets(condition, functions, items, parameter_count)?;
            validate_value_target(then_value, functions, items, parameter_count)?;
            validate_value_target(else_value, functions, items, parameter_count)
        }
    }
}

fn validate_statement_targets(
    statement: &Statement,
    functions: &HashMap<&str, usize>,
    items: &[FunctionItem],
    parameter_count: usize,
) -> Result<(), CodegenError> {
    validate_statement_targets_at_depth(statement, functions, items, parameter_count, 0)
}

fn validate_statement_targets_at_depth(
    statement: &Statement,
    functions: &HashMap<&str, usize>,
    items: &[FunctionItem],
    parameter_count: usize,
    depth: usize,
) -> Result<(), CodegenError> {
    match statement {
        Statement::Println(parts) => {
            for part in parts {
                match part {
                    PrintPart::Slice(value)
                    | PrintPart::Decimal(value)
                    | PrintPart::Hex16(value)
                    | PrintPart::PanicDecimal(value) => {
                        validate_value_target(value, functions, items, parameter_count)?;
                    }
                    PrintPart::Text(_) | PrintPart::Panic(_) | PrintPart::PanicText(_) => {}
                }
            }
            Ok(())
        }
        Statement::LetStruct { fields } => {
            for (_, value) in fields {
                validate_value_target(value, functions, items, parameter_count)?;
            }
            Ok(())
        }
        Statement::Let { initializer, .. } => {
            validate_value_target(initializer, functions, items, parameter_count)
        }
        Statement::LetSlice { initializer, .. } => {
            validate_value_target(initializer, functions, items, parameter_count)
        }
        Statement::LetTupleCall { initializer, .. } => {
            validate_value_target(initializer, functions, items, parameter_count)
        }
        Statement::Evaluate(value)
        | Statement::TryResult(value)
        | Statement::TryOption(value)
        | Statement::TryOptionBind { result: value, .. } => {
            validate_value_target(value, functions, items, parameter_count)
        }
        Statement::Assign { value, .. } | Statement::AssignIndirect { value, .. } => {
            validate_value_target(value, functions, items, parameter_count)
        }
        Statement::VecPushString { value, .. } => {
            validate_value_target(value, functions, items, parameter_count)
        }
        Statement::StringAppendChar { character, .. } => {
            validate_value_target(character, functions, items, parameter_count)
        }
        Statement::BTreeMapInsert { key, value, .. } => {
            validate_value_target(key, functions, items, parameter_count)?;
            validate_value_target(value, functions, items, parameter_count)
        }
        Statement::VecPushAggregate { fields, .. } => {
            for value in fields {
                validate_value_target(value, functions, items, parameter_count)?;
            }
            Ok(())
        }
        Statement::If {
            condition,
            then_body,
            else_body,
        } => {
            validate_condition_targets(condition, functions, items, parameter_count)?;
            for branch in [then_body, else_body] {
                for statement in branch {
                    validate_statement_targets_at_depth(
                        statement,
                        functions,
                        items,
                        parameter_count,
                        depth,
                    )?;
                }
            }
            Ok(())
        }
        Statement::While { condition, body } => {
            validate_condition_targets(condition, functions, items, parameter_count)?;
            for statement in body {
                validate_statement_targets_at_depth(
                    statement,
                    functions,
                    items,
                    parameter_count,
                    depth + 1,
                )?;
            }
            Ok(())
        }
        Statement::ForRange { body, .. } => {
            for statement in body {
                validate_statement_targets_at_depth(
                    statement,
                    functions,
                    items,
                    parameter_count,
                    depth + 1,
                )?;
            }
            Ok(())
        }
        Statement::ForSlice { body, .. } | Statement::ForChars { body, .. } => {
            for statement in body {
                validate_statement_targets_at_depth(
                    statement,
                    functions,
                    items,
                    parameter_count,
                    depth + 1,
                )?;
            }
            Ok(())
        }
        Statement::Break | Statement::Continue if depth == 0 => Err(
            CodegenError::UnsupportedProgram("loop control outside of a loop"),
        ),
        Statement::Break | Statement::Continue => Ok(()),
        Statement::Return(value) => validate_value_target(value, functions, items, parameter_count),
    }
}

fn validate_condition_targets(
    condition: &Condition,
    functions: &HashMap<&str, usize>,
    items: &[FunctionItem],
    parameter_count: usize,
) -> Result<(), CodegenError> {
    match condition {
        Condition::Boolean(_) => Ok(()),
        Condition::And(left, right) | Condition::Or(left, right) => {
            validate_condition_targets(left, functions, items, parameter_count)?;
            validate_condition_targets(right, functions, items, parameter_count)
        }
        Condition::Compare { left, right, .. } => {
            validate_value_target(left, functions, items, parameter_count)?;
            validate_value_target(right, functions, items, parameter_count)
        }
    }
}

fn encode_value(
    value: &Value,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
) -> Vec<u8> {
    let mut code = Vec::new();
    match value {
        Value::Integer(n) => {
            code.push(0xb8);
            code.extend_from_slice(&n.to_le_bytes());
        }
        Value::Word(n) => {
            code.extend_from_slice(&[0x48, 0xb8]); // movabs rax, imm64
            code.extend_from_slice(&n.to_le_bytes());
        }
        Value::TypedWord { value, .. } => {
            code.extend_from_slice(&[0x48, 0xb8]);
            code.extend_from_slice(&value.to_le_bytes());
        }
        Value::EmptySlice => code.extend_from_slice(&[
            0x48, 0xb8, 8, 0, 0, 0, 0, 0, 0, 0, // mov rax, 8: aligned dangling pointer
            0x31, 0xd2, // xor edx, edx: zero length
        ]),
        Value::NicheOk => code.extend_from_slice(&[
            0x31, 0xc0, // xor eax, eax: null-pointer Ok niche
            0x31, 0xd2, // xor edx, edx: initialize the metadata word
        ]),
        Value::EmptyCollection => code.extend_from_slice(&[
            0x48, 0xb8, 8, 0, 0, 0, 0, 0, 0, 0, // aligned dangling pointer
            0x31, 0xd2, // zero length; capacity is stored as zero
        ]),
        Value::TextSlice(text) => {
            code.extend_from_slice(&[0x48, 0x8d, 0x05]); // lea rax, [rip + text]
            let offset = code.len() as u64;
            code.extend_from_slice(&[0; 4]);
            relocations.push(PendingRelocation {
                offset,
                target: text_symbol_name(text),
            });
            code.push(0xba); // mov edx, byte length
            code.extend_from_slice(&(text.len() as u32).to_le_bytes());
        }
        Value::StaticArray(elements) => {
            let symbol = static_array_symbol_name(elements);
            code.extend_from_slice(&[0x48, 0x8d, 0x05]); // lea rax, [rip + array]
            let offset = code.len() as u64;
            code.extend_from_slice(&[0; 4]);
            relocations.push(PendingRelocation {
                offset,
                target: symbol,
            });
            code.push(0xba); // mov edx, element count
            code.extend_from_slice(&(elements.len() as u32).to_le_bytes());
        }
        Value::OwnedString(source) => {
            code.extend(encode_owned_string(source, in_function, relocations));
        }
        Value::FormatString(pieces) => {
            code.extend(encode_format_string(pieces, in_function, relocations));
        }
        Value::SliceParameter {
            pointer_slot,
            length_slot,
            ..
        }
        | Value::SliceLocal {
            pointer_slot,
            length_slot,
            ..
        }
        | Value::SliceRecord {
            pointer_slot,
            length_slot,
            ..
        }
        | Value::CharIterator {
            pointer_slot,
            length_slot,
        } => {
            encode_rbp_load_rax(&mut code, -8 * (*pointer_slot as i32 + 1));
            encode_rbp_load_rdx(&mut code, -8 * (*length_slot as i32 + 1));
        }
        Value::CharIsAsciiAlphanumeric(character) => {
            code.extend(encode_char_is_ascii_alphanumeric(
                character,
                in_function,
                relocations,
            ));
        }
        Value::AnyStringTrimEmpty {
            pointer_slot,
            length_slot,
        } => code.extend(encode_any_string_trim_empty(
            *pointer_slot,
            *length_slot,
            relocations,
        )),
        Value::SliceElement {
            pointer_slot,
            index_slot,
            element_size,
            element_offset,
            element_stride,
        } => {
            encode_rbp_load_rax(&mut code, -8 * (*pointer_slot as i32 + 1));
            encode_rbp_load_rcx(&mut code, -8 * (*index_slot as i32 + 1));
            if matches!(element_stride, 1 | 2 | 4 | 8) {
                let scale = element_stride.trailing_zeros() as u8;
                let sib = (scale << 6) | 0x08; // [rax + rcx*scale]
                let index_base = if *element_offset == 0 { 0x04 } else { 0x44 };
                match element_size {
                    4 => code.extend_from_slice(&[0x8b, index_base, sib]),
                    8 => code.extend_from_slice(&[0x48, 0x8b, index_base, sib]),
                    _ => unreachable!("slice element width was validated during lowering"),
                }
                if *element_offset != 0 {
                    code.push(*element_offset as u8);
                }
            } else {
                code.extend_from_slice(&[0x48, 0x6b, 0xc9, *element_stride as u8]); // imul rcx, rcx, stride
                code.extend_from_slice(&[0x48, 0x01, 0xc8]); // add rax, rcx
                if *element_offset != 0 {
                    code.extend_from_slice(&[0x48, 0x83, 0xc0, *element_offset as u8]);
                }
                match element_size {
                    4 => code.extend_from_slice(&[0x8b, 0x00]),
                    8 => code.extend_from_slice(&[0x48, 0x8b, 0x00]),
                    _ => unreachable!("slice element width was validated during lowering"),
                }
            }
        }
        Value::SliceElementString {
            pointer_slot,
            index_slot,
            element_offset,
            element_stride,
        } => {
            encode_rbp_load_rax(&mut code, -8 * (*pointer_slot as i32 + 1));
            encode_rbp_load_rcx(&mut code, -8 * (*index_slot as i32 + 1));
            code.extend_from_slice(&[0x48, 0x6b, 0xc9, *element_stride as u8]);
            code.extend_from_slice(&[0x48, 0x01, 0xc8]);
            if *element_offset != 0 {
                code.extend_from_slice(&[0x48, 0x83, 0xc0, *element_offset as u8]);
            }
            code.extend_from_slice(&[0x48, 0x8b, 0x50, 0x08]);
            code.extend_from_slice(&[0x48, 0x8b, 0x00]);
        }
        Value::BTreeMapGetString { pointer_slot, length_slot, key } => {
            code.extend(encode_btree_map_get_string(*pointer_slot, *length_slot, key, in_function, relocations));
        }
        Value::CollectionLocal {
            pointer_slot,
            length_slot,
            indirect: true,
            ..
        } => {
            encode_rbp_load_collection_rax(&mut code, *pointer_slot, true);
            encode_rbp_load_collection_rax(&mut code, *length_slot, true);
        }
        Value::CollectionLocal {
            pointer_slot,
            length_slot,
            ..
        } => {
            encode_rbp_load_rax(&mut code, -8 * (*pointer_slot as i32 + 1));
            encode_rbp_load_rdx(&mut code, -8 * (*length_slot as i32 + 1));
        }
        Value::CollectionAddress { .. } => {
            unreachable!("collection addresses are only emitted as mutable call arguments")
        }
        Value::Parameter(index) if in_function => {
            encode_rbp_load_eax(&mut code, -8 * (*index as i32 + 1));
        }
        Value::Parameter(_) => unreachable!("main has no parameters"),
        Value::TypedWordParameter { slot, .. } if in_function => {
            encode_rbp_load_rax(&mut code, -8 * (*slot as i32 + 1));
        }
        Value::TypedWordParameter { .. } => unreachable!("main has no parameters"),
        Value::Local(index) => encode_rbp_load_eax(&mut code, -8 * (*index as i32 + 1)),
        Value::WordLocal(index) => encode_rbp_load_rax(&mut code, -8 * (*index as i32 + 1)),
        Value::TypedWordLocal { slot, .. } => {
            encode_rbp_load_rax(&mut code, -8 * (*slot as i32 + 1));
        }
        Value::AddressOfLocal(slot) => {
            code.extend_from_slice(&[0x48, 0x8d, 0x85]);
            code.extend_from_slice(&(-8 * (*slot as i32 + 1)).to_le_bytes());
        }
        Value::IndirectLocal {
            pointer_slot,
            is_word,
            ..
        } => {
            encode_rbp_load_rax(&mut code, -8 * (*pointer_slot as i32 + 1));
            if *is_word {
                code.extend_from_slice(&[0x48, 0x8b, 0x00]);
            } else {
                code.extend_from_slice(&[0x8b, 0x00]);
            }
        }
        Value::StructLocal { .. } => unreachable!("struct locals are only used for field lookup"),
        Value::StructReference { .. } => {
            unreachable!("borrowed records are flattened at call sites")
        }
        Value::Tuple(_) => unreachable!("tuple values are emitted through the pair return path"),
        Value::PairField { aggregate, index } => {
            append_encoded_value(&mut code, aggregate, in_function, relocations);
            if *index == 1 {
                code.extend_from_slice(&[0x48, 0x89, 0xd0]); // mov rax, rdx
            }
        }
        Value::Block { statements, tail } => {
            let mut loops = Vec::new();
            for statement in statements {
                encode_statement_into(&mut code, statement, in_function, relocations, &mut loops);
            }
            append_encoded_value(&mut code, tail, in_function, relocations);
        }
        Value::Binary {
            operator,
            left,
            right,
        } => code.extend(encode_binary_value(
            *operator,
            left,
            right,
            in_function,
            relocations,
        )),
        Value::WrappingBinary {
            operator,
            left,
            right,
        } => code.extend(encode_wrapping_binary_value(
            *operator,
            left,
            right,
            in_function,
            relocations,
        )),
        Value::Negate(operand) => {
            append_encoded_value(&mut code, operand, in_function, relocations);
            code.extend_from_slice(&[0xf7, 0xd8]); // neg eax
        }
        Value::SliceIsEmpty(slice) => {
            append_encoded_value(&mut code, slice, in_function, relocations);
            code.extend_from_slice(&[0x48, 0x85, 0xd2, 0x0f, 0x94, 0xc0, 0x0f, 0xb6, 0xc0]);
        }
        Value::SliceLength(slice) => {
            append_encoded_value(&mut code, slice, in_function, relocations);
            code.extend_from_slice(&[0x48, 0x89, 0xd0]); // mov rax, rdx: slice length
        }
        Value::ZeroExtend(value) => {
            append_encoded_value(&mut code, value, in_function, relocations);
            code.extend_from_slice(&[0x89, 0xc0]); // mov eax, eax: zero-extend to rax
        }
        Value::SignExtend(value) => {
            append_encoded_value(&mut code, value, in_function, relocations);
            code.extend_from_slice(&[0x48, 0x63, 0xc0]); // movsxd rax, eax
        }
        Value::SliceFold {
            source,
            initial,
            update,
            element_size,
            pointer_slot,
            length_slot,
            index_slot,
            accumulator_slot,
            item_slot,
            ..
        } => {
            append_encoded_value(&mut code, source, in_function, relocations);
            encode_rbp_store_rax(&mut code, -8 * (*pointer_slot as i32 + 1));
            encode_rbp_store_rdx(&mut code, -8 * (*length_slot as i32 + 1));
            append_encoded_value(&mut code, initial, in_function, relocations);
            encode_rbp_store_rax(&mut code, -8 * (*accumulator_slot as i32 + 1));
            code.extend_from_slice(&[0x31, 0xc0]);
            encode_rbp_store_eax(&mut code, -8 * (*index_slot as i32 + 1));
            let loop_start = code.len();
            encode_rbp_load_rax(&mut code, -8 * (*index_slot as i32 + 1));
            encode_rbp_load_rcx(&mut code, -8 * (*length_slot as i32 + 1));
            code.extend_from_slice(&[0x48, 0x39, 0xc8, 0x0f, 0x83]); // cmp rax, rcx; jae end
            let exit_displacement = code.len();
            code.extend_from_slice(&[0; 4]);
            encode_rbp_load_rax(&mut code, -8 * (*pointer_slot as i32 + 1));
            encode_rbp_load_rcx(&mut code, -8 * (*index_slot as i32 + 1));
            match element_size {
                1 => code.extend_from_slice(&[0x0f, 0xb6, 0x04, 0x08]), // movzx eax, byte [rax + rcx]
                4 => code.extend_from_slice(&[0x8b, 0x04, 0x88]),       // mov eax, [rax + rcx*4]
                8 => code.extend_from_slice(&[0x48, 0x8b, 0x04, 0xc8]), // mov rax, [rax + rcx*8]
                _ => unreachable!("slice fold element width is validated during lowering"),
            }
            if *element_size == 8 {
                encode_rbp_store_rax(&mut code, -8 * (*item_slot as i32 + 1));
            } else {
                encode_rbp_store_eax(&mut code, -8 * (*item_slot as i32 + 1));
            }
            append_encoded_value(&mut code, update, in_function, relocations);
            encode_rbp_store_rax(&mut code, -8 * (*accumulator_slot as i32 + 1));
            encode_rbp_load_rax(&mut code, -8 * (*index_slot as i32 + 1));
            code.extend_from_slice(&[0x48, 0x83, 0xc0, 1]);
            encode_rbp_store_rax(&mut code, -8 * (*index_slot as i32 + 1));
            code.push(0xe9);
            let back_displacement = code.len();
            code.extend_from_slice(&[0; 4]);
            patch_rel32(&mut code, back_displacement, loop_start);
            let loop_end = code.len();
            patch_rel32(&mut code, exit_displacement, loop_end);
            encode_rbp_load_rax(&mut code, -8 * (*accumulator_slot as i32 + 1));
        }
        Value::TupleSliceFold {
            source,
            initial,
            updates,
            element_size,
            pointer_slot,
            length_slot,
            index_slot,
            accumulator_slots,
            update_slots,
            item_slot,
            record_fields,
            ..
        } => {
            append_encoded_value(&mut code, source, in_function, relocations);
            encode_rbp_store_rax(&mut code, -8 * (*pointer_slot as i32 + 1));
            encode_rbp_store_rdx(&mut code, -8 * (*length_slot as i32 + 1));
            for (value, slot) in initial.iter().zip(accumulator_slots) {
                append_encoded_value(&mut code, value, in_function, relocations);
                encode_rbp_store_rax(&mut code, -8 * (*slot as i32 + 1));
            }
            code.extend_from_slice(&[0x31, 0xc0]);
            encode_rbp_store_eax(&mut code, -8 * (*index_slot as i32 + 1));
            let loop_start = code.len();
            encode_rbp_load_rax(&mut code, -8 * (*index_slot as i32 + 1));
            encode_rbp_load_rcx(&mut code, -8 * (*length_slot as i32 + 1));
            code.extend_from_slice(&[0x48, 0x39, 0xc8, 0x0f, 0x83]);
            let exit_displacement = code.len();
            code.extend_from_slice(&[0; 4]);
            if record_fields.is_empty() {
                encode_rbp_load_rax(&mut code, -8 * (*pointer_slot as i32 + 1));
                encode_rbp_load_rcx(&mut code, -8 * (*index_slot as i32 + 1));
                match element_size {
                    4 => code.extend_from_slice(&[0x8b, 0x04, 0x88]),
                    8 => code.extend_from_slice(&[0x48, 0x8b, 0x04, 0xc8]),
                    _ => unreachable!("tuple fold element width was validated during lowering"),
                }
                encode_rbp_store_rax(&mut code, -8 * (*item_slot as i32 + 1));
            } else {
                for (slot, initializer, width) in record_fields {
                    append_encoded_value(&mut code, initializer, in_function, relocations);
                    encode_rbp_store_rax(&mut code, -8 * (*slot as i32 + 1));
                    if *width == 2 {
                        encode_rbp_store_rdx(&mut code, -8 * (*slot as i32 + 2));
                    }
                }
            }
            for (value, slot) in updates.iter().zip(update_slots) {
                append_encoded_value(&mut code, value, in_function, relocations);
                encode_rbp_store_rax(&mut code, -8 * (*slot as i32 + 1));
            }
            for (source_slot, target_slot) in update_slots.iter().zip(accumulator_slots) {
                encode_rbp_load_rax(&mut code, -8 * (*source_slot as i32 + 1));
                encode_rbp_store_rax(&mut code, -8 * (*target_slot as i32 + 1));
            }
            encode_rbp_load_rax(&mut code, -8 * (*index_slot as i32 + 1));
            code.extend_from_slice(&[0x48, 0x83, 0xc0, 1]);
            encode_rbp_store_rax(&mut code, -8 * (*index_slot as i32 + 1));
            code.push(0xe9);
            let back_displacement = code.len();
            code.extend_from_slice(&[0; 4]);
            patch_rel32(&mut code, back_displacement, loop_start);
            let loop_end = code.len();
            patch_rel32(&mut code, exit_displacement, loop_end);
            encode_rbp_load_rax(&mut code, -8 * (accumulator_slots[0] as i32 + 1));
            encode_rbp_load_rdx(&mut code, -8 * (accumulator_slots[1] as i32 + 1));
        }
        Value::Call {
            name,
            arguments,
            returns_aggregate,
            aggregate_destination,
            ..
        } => {
            let mut argument_offsets = Vec::with_capacity(arguments.len());
            let aggregate_argument = usize::from(*returns_aggregate);
            let mut physical_arguments = aggregate_argument;
            for argument in arguments {
                argument_offsets.push(physical_arguments * 8);
                physical_arguments += call_argument_component_count(argument);
            }
            let stack_argument_count = physical_arguments.saturating_sub(6);
            let outgoing_size = (stack_argument_count * 8).div_ceil(16) * 16;
            let temporary_size = (physical_arguments * 8).div_ceil(16) * 16;
            let total_size = outgoing_size + temporary_size;
            emit_rsp_adjust(&mut code, true, total_size);
            if *returns_aggregate {
                let slot = aggregate_destination.expect("aggregate call destination is assigned");
                code.extend_from_slice(&[0x48, 0x8d, 0x85]); // lea rax, [rbp + disp32]
                code.extend_from_slice(&(-8 * (slot as i32 + 1)).to_le_bytes());
                encode_rsp_store_rax(&mut code, outgoing_size);
            }
            for (argument, temporary_offset) in arguments.iter().zip(argument_offsets) {
                append_call_argument(
                    &mut code,
                    argument,
                    in_function,
                    relocations,
                    outgoing_size + temporary_offset,
                );
            }
            for index in 6..physical_arguments {
                encode_rsp_load_rax(&mut code, outgoing_size + index * 8);
                encode_rsp_store_rax(&mut code, (index - 6) * 8);
            }
            for index in 0..physical_arguments.min(6) {
                encode_rsp_load_argument_rax(&mut code, index, outgoing_size + index * 8);
            }
            code.push(0xe8);
            let offset = code.len() as u64;
            code.extend_from_slice(&[0; 4]);
            emit_rsp_adjust(&mut code, false, total_size);
            relocations.push(PendingRelocation {
                offset,
                target: name.clone(),
            });
        }
        Value::ResultIsOk(result) => {
            append_encoded_value(&mut code, result, in_function, relocations);
            code.extend_from_slice(&[
                0x48, 0x85, 0xc0, // test rax, rax: null pointer means Ok
                0x0f, 0x94, 0xc0, // sete al
                0x0f, 0xb6, 0xc0, // movzx eax, al
            ]);
        }
        Value::OptionIsSome(result) => {
            append_encoded_value(&mut code, result, in_function, relocations);
            code.extend_from_slice(&[
                0x85, 0xc0, // test eax, eax: nonzero means Some
                0x0f, 0x95, 0xc0, // setne al
                0x0f, 0xb6, 0xc0, // movzx eax, al
            ]);
        }
        Value::OptionUnwrapOr {
            option,
            fallback,
            success_on_zero,
            payload_is_word,
        } => {
            append_encoded_value(&mut code, option, in_function, relocations);
            code.extend_from_slice(&[0x85, 0xc0]); // test eax, eax
            code.extend_from_slice(if *success_on_zero {
                &[0x0f, 0x85][..]
            } else {
                &[0x0f, 0x84][..]
            }); // select fallback by enum tag
            let fallback_displacement = code.len();
            code.extend_from_slice(&[0; 4]);
            code.extend_from_slice(if *payload_is_word {
                &[0x48, 0x89, 0xd0][..]
            } else {
                &[0x89, 0xd0][..]
            });
            code.push(0xe9); // jmp end
            let end_displacement = code.len();
            code.extend_from_slice(&[0; 4]);
            let fallback_start = code.len();
            append_encoded_value(&mut code, fallback, in_function, relocations);
            let end = code.len();
            patch_rel32(&mut code, fallback_displacement, fallback_start);
            patch_rel32(&mut code, end_displacement, end);
        }
        Value::If {
            condition,
            then_value,
            else_value,
        } => code.extend(encode_if_value(
            condition,
            then_value,
            else_value,
            in_function,
            relocations,
        )),
    }
    code
}

fn call_argument_component_count(value: &Value) -> usize {
    match value {
        Value::StructReference { fields, .. } => {
            fields.iter().map(call_argument_component_count).sum()
        }
        Value::CollectionLocal { .. } => 3,
        Value::CollectionAddress { .. } => 3,
        value if is_slice_value(value) => 2,
        _ => 1,
    }
}

fn append_call_argument(
    code: &mut Vec<u8>,
    value: &Value,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
    displacement: usize,
) {
    match value {
        Value::StructReference { fields, .. } => {
            let mut offset = displacement;
            for field in fields {
                append_call_argument(code, field, in_function, relocations, offset);
                offset += call_argument_component_count(field) * 8;
            }
        }
        Value::CollectionAddress {
            pointer_slot,
            length_slot,
            capacity_slot,
            indirect,
        } => {
            for (slot, offset) in [
                (*pointer_slot, displacement),
                (*length_slot, displacement + 8),
                (*capacity_slot, displacement + 16),
            ] {
                if *indirect {
                    encode_rbp_load_rax(code, -8 * (slot as i32 + 1));
                } else {
                    code.extend_from_slice(&[0x48, 0x8d, 0x85]);
                    code.extend_from_slice(&(-8 * (slot as i32 + 1)).to_le_bytes());
                }
                encode_rsp_store_rax(code, offset);
            }
        }
        Value::CollectionLocal {
            pointer_slot,
            length_slot,
            capacity_slot,
            indirect: true,
            ..
        } => {
            for (slot, offset) in [
                (*pointer_slot, displacement),
                (*length_slot, displacement + 8),
                (*capacity_slot, displacement + 16),
            ] {
                encode_rbp_load_rax(code, -8 * (slot as i32 + 1));
                encode_rsp_store_rax(code, offset);
            }
        }
        Value::CollectionLocal { capacity_slot, .. } => {
            append_encoded_value(code, value, in_function, relocations);
            encode_rsp_store_rax(code, displacement);
            encode_rsp_store_rdx(code, displacement + 8);
            encode_rbp_load_rax(code, -8 * (*capacity_slot as i32 + 1));
            encode_rsp_store_rax(code, displacement + 16);
        }
        value if is_slice_value(value) => {
            append_encoded_value(code, value, in_function, relocations);
            encode_rsp_store_rax(code, displacement);
            encode_rsp_store_rdx(code, displacement + 8);
        }
        value => {
            append_encoded_value(code, value, in_function, relocations);
            encode_rsp_store_rax(code, displacement);
        }
    }
}

fn encode_vec_push_string(
    pointer_slot: usize,
    length_slot: usize,
    capacity_slot: usize,
    value: &Value,
    indirect: bool,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
) -> Vec<u8> {
    let mut code = Vec::new();
    emit_rsp_adjust(&mut code, true, 64);
    append_encoded_value(&mut code, value, in_function, relocations);
    code.extend_from_slice(&[0x48, 0x89, 0x04, 0x24]); // save element data pointer
    code.extend_from_slice(&[0x48, 0x89, 0x54, 0x24, 0x08]); // save element length
    code.extend_from_slice(&[0x48, 0xc7, 0x44, 0x24, 0x10, 0x00, 0x10, 0x00, 0x00]); // String capacity
    encode_rbp_load_collection_rax(&mut code, pointer_slot, indirect);
    encode_rsp_store_rax(&mut code, 32);
    encode_rbp_load_collection_rax(&mut code, length_slot, indirect);
    encode_rsp_store_rax(&mut code, 40);
    encode_rbp_load_collection_rax(&mut code, length_slot, indirect);
    encode_rbp_load_collection_rcx(&mut code, capacity_slot, indirect);
    code.extend_from_slice(&[0x48, 0x39, 0xc8, 0x0f, 0x82]); // if len < cap, skip growth
    let skip_growth = code.len();
    code.extend_from_slice(&[0; 4]);

    encode_rbp_load_collection_rax(&mut code, capacity_slot, indirect);
    code.extend_from_slice(&[0x48, 0x85, 0xc0, 0x0f, 0x84]); // zero capacity?
    let default_capacity = code.len();
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x48, 0xd1, 0xe0]); // double existing capacity
    code.push(0xe9);
    let have_capacity = code.len();
    code.extend_from_slice(&[0; 4]);
    let default_capacity_offset = code.len();
    code.extend_from_slice(&[0x48, 0xc7, 0xc0, 0x04, 0, 0, 0]); // initial capacity = 4
    let store_capacity_offset = code.len();
    encode_rsp_store_rax(&mut code, 24);
    encode_rsp_load_rax(&mut code, 24);
    code.extend_from_slice(&[0x48, 0x6b, 0xf0, 0x18]); // bytes = capacity * 24
    code.extend_from_slice(&[0x48, 0x81, 0xc6, 0xff, 0x0f, 0, 0]);
    code.extend_from_slice(&[0x48, 0x81, 0xe6, 0x00, 0xf0, 0xff, 0xff]); // page round-up
    code.extend_from_slice(&[
        0x31, 0xff, // mmap(NULL, ...)
        0xba, 0x03, 0, 0, 0, // PROT_READ | PROT_WRITE
        0x41, 0xba, 0x22, 0, 0, 0, // MAP_PRIVATE | MAP_ANONYMOUS
        0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, // fd = -1
        0x45, 0x31, 0xc9, // offset = 0
        0xb8, 0x09, 0, 0, 0, // SYS_mmap
        0x0f, 0x05,
    ]);
    encode_rsp_store_rax(&mut code, 48);
    code.extend_from_slice(&[0x48, 0x8b, 0x7c, 0x24, 0x30]); // new vector pointer
    code.extend_from_slice(&[0x48, 0x8b, 0x74, 0x24, 0x20]); // old vector pointer
    code.extend_from_slice(&[0x48, 0x8b, 0x4c, 0x24, 0x28]); // old length
    code.extend_from_slice(&[0x48, 0x6b, 0xc9, 0x18]); // copy length * 24 bytes
    code.extend_from_slice(&[0xf3, 0xa4]); // rep movsb
    encode_rsp_load_rax(&mut code, 48);
    encode_rbp_store_collection_rax(&mut code, pointer_slot, indirect);
    encode_rsp_load_rax(&mut code, 24);
    encode_rbp_store_collection_rax(&mut code, capacity_slot, indirect);

    let write_element = code.len();
    patch_rel32(&mut code, skip_growth, write_element);
    patch_rel32(&mut code, default_capacity, default_capacity_offset);
    patch_rel32(&mut code, have_capacity, store_capacity_offset);
    encode_rbp_load_collection_rax(&mut code, pointer_slot, indirect);
    encode_rbp_load_collection_rcx(&mut code, length_slot, indirect);
    code.extend_from_slice(&[0x48, 0x6b, 0xc9, 0x18, 0x48, 0x01, 0xc8]);
    code.extend_from_slice(&[0x48, 0x89, 0xc7]); // destination element address
    encode_rsp_load_rax(&mut code, 0);
    code.extend_from_slice(&[0x48, 0x89, 0x07]);
    encode_rsp_load_rax(&mut code, 8);
    code.extend_from_slice(&[0x48, 0x89, 0x47, 0x08]);
    encode_rsp_load_rax(&mut code, 16);
    code.extend_from_slice(&[0x48, 0x89, 0x47, 0x10]);
    encode_rbp_load_collection_rax(&mut code, length_slot, indirect);
    code.extend_from_slice(&[0x48, 0x83, 0xc0, 1]);
    encode_rbp_store_collection_rax(&mut code, length_slot, indirect);
    emit_rsp_adjust(&mut code, false, 64);
    code
}

fn encode_vec_push_aggregate(
    pointer_slot: usize,
    length_slot: usize,
    capacity_slot: usize,
    fields: &[Value],
    widths: &[usize],
    indirect: bool,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
) -> Vec<u8> {
    debug_assert_eq!(fields.len(), widths.len());
    let stride = widths.iter().sum::<usize>() * 8;
    debug_assert!(stride > 0 && stride <= i32::MAX as usize);
    let mut code = Vec::new();
    emit_rsp_adjust(&mut code, true, 80);
    encode_rbp_load_collection_rax(&mut code, pointer_slot, indirect);
    encode_rsp_store_rax(&mut code, 32);
    encode_rbp_load_collection_rax(&mut code, length_slot, indirect);
    encode_rsp_store_rax(&mut code, 40);
    encode_rbp_load_collection_rax(&mut code, length_slot, indirect);
    encode_rbp_load_collection_rcx(&mut code, capacity_slot, indirect);
    code.extend_from_slice(&[0x48, 0x39, 0xc8, 0x0f, 0x82]);
    let skip_growth = code.len();
    code.extend_from_slice(&[0; 4]);

    encode_rbp_load_collection_rax(&mut code, capacity_slot, indirect);
    code.extend_from_slice(&[0x48, 0x85, 0xc0, 0x0f, 0x84]);
    let default_capacity = code.len();
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x48, 0xd1, 0xe0]);
    code.push(0xe9);
    let have_capacity = code.len();
    code.extend_from_slice(&[0; 4]);
    let default_capacity_offset = code.len();
    code.extend_from_slice(&[0x48, 0xc7, 0xc0, 0x04, 0, 0, 0]);
    let store_capacity_offset = code.len();
    encode_rsp_store_rax(&mut code, 24);
    encode_rsp_load_rax(&mut code, 24);
    code.extend_from_slice(&[0x48, 0x69, 0xf0]); // imul rsi, rax, stride
    code.extend_from_slice(&(stride as u32).to_le_bytes());
    code.extend_from_slice(&[0x48, 0x81, 0xc6, 0xff, 0x0f, 0, 0]);
    code.extend_from_slice(&[0x48, 0x81, 0xe6, 0x00, 0xf0, 0xff, 0xff]);
    code.extend_from_slice(&[
        0x31, 0xff, // mmap(NULL, ...)
        0xba, 0x03, 0, 0, 0, // PROT_READ | PROT_WRITE
        0x41, 0xba, 0x22, 0, 0, 0, // MAP_PRIVATE | MAP_ANONYMOUS
        0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, // fd = -1
        0x45, 0x31, 0xc9, // offset = 0
        0xb8, 0x09, 0, 0, 0, // SYS_mmap
        0x0f, 0x05,
    ]);
    encode_rsp_store_rax(&mut code, 48);
    code.extend_from_slice(&[0x48, 0x8b, 0x7c, 0x24, 0x30]); // new vector pointer
    code.extend_from_slice(&[0x48, 0x8b, 0x74, 0x24, 0x20]); // old vector pointer
    code.extend_from_slice(&[0x48, 0x8b, 0x4c, 0x24, 0x28]); // old length
    code.extend_from_slice(&[0x48, 0x69, 0xc9]); // imul rcx, rcx, stride
    code.extend_from_slice(&(stride as u32).to_le_bytes());
    code.extend_from_slice(&[0xf3, 0xa4]); // rep movsb
    encode_rsp_load_rax(&mut code, 48);
    encode_rbp_store_collection_rax(&mut code, pointer_slot, indirect);
    encode_rsp_load_rax(&mut code, 24);
    encode_rbp_store_collection_rax(&mut code, capacity_slot, indirect);
    let write_element = code.len();
    patch_rel32(&mut code, skip_growth, write_element);
    patch_rel32(&mut code, default_capacity, default_capacity_offset);
    patch_rel32(&mut code, have_capacity, store_capacity_offset);

    encode_rbp_load_collection_rax(&mut code, pointer_slot, indirect);
    encode_rbp_load_collection_rcx(&mut code, length_slot, indirect);
    code.extend_from_slice(&[0x48, 0x69, 0xc9]); // imul rcx, rcx, stride
    code.extend_from_slice(&(stride as u32).to_le_bytes());
    code.extend_from_slice(&[0x48, 0x01, 0xc8]);
    encode_rsp_store_rax(&mut code, 56); // destination element pointer

    let mut byte_offset = 0usize;
    for (field, width) in fields.iter().zip(widths) {
        assert!((1..=3).contains(width));
        append_encoded_value(&mut code, field, in_function, relocations);
        encode_rsp_store_rax(&mut code, 0);
        if *width >= 2 {
            encode_rsp_store_rdx(&mut code, 8);
        }
        if *width == 3 {
            let Value::CollectionLocal {
                capacity_slot,
                indirect,
                ..
            } = field
            else {
                unreachable!("three-word aggregate field is a collection")
            };
            encode_rbp_load_collection_rax(&mut code, *capacity_slot, *indirect);
            encode_rsp_store_rax(&mut code, 16);
        }
        for component in 0..*width {
            encode_rsp_load_rax(&mut code, component * 8);
            code.extend_from_slice(&[0x48, 0x8b, 0x7c, 0x24, 0x38]); // rdi = destination
            code.push(0x48);
            code.push(0x89);
            code.push(0x87); // mov [rdi + disp32], rax
            code.extend_from_slice(&((byte_offset + component * 8) as u32).to_le_bytes());
        }
        byte_offset += width * 8;
    }
    encode_rbp_load_collection_rax(&mut code, length_slot, indirect);
    code.extend_from_slice(&[0x48, 0x83, 0xc0, 1]);
    encode_rbp_store_collection_rax(&mut code, length_slot, indirect);
    emit_rsp_adjust(&mut code, false, 80);
    code
}

fn encode_btree_map_insert(
    pointer_slot: usize,
    length_slot: usize,
    capacity_slot: usize,
    key: &Value,
    value: &Value,
    indirect: bool,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
) -> Vec<u8> {
    // The native subset stores BTreeMap<String, String> as a flat array of
    // (key pointer, key length, value pointer, value length) entries. Insert
    // scans for an existing key before appending, so replacement preserves
    // map length. Iteration ordering is added by the map iterator lowering.
    let mut code = Vec::new();
    emit_rsp_adjust(&mut code, true, 112);
    append_encoded_value(&mut code, key, in_function, relocations);
    encode_rsp_store_rax(&mut code, 0);
    encode_rsp_store_rdx(&mut code, 8);
    append_encoded_value(&mut code, value, in_function, relocations);
    encode_rsp_store_rax(&mut code, 16);
    encode_rsp_store_rdx(&mut code, 24);

    encode_rbp_load_collection_rax(&mut code, pointer_slot, indirect);
    encode_rsp_store_rax(&mut code, 32);
    encode_rbp_load_collection_rax(&mut code, length_slot, indirect);
    encode_rsp_store_rax(&mut code, 40);
    encode_rbp_load_collection_rax(&mut code, capacity_slot, indirect);
    encode_rsp_store_rax(&mut code, 48);
    code.extend_from_slice(&[0x48, 0xc7, 0x44, 0x24, 0x38, 0, 0, 0, 0]); // index = 0

    let scan = code.len();
    encode_rsp_load_rax(&mut code, 56);
    encode_rsp_load_rcx(&mut code, 40);
    code.extend_from_slice(&[0x48, 0x39, 0xc8]); // cmp rax, rcx
    code.extend_from_slice(&[0x0f, 0x83]); // jae append
    let scan_done = code.len();
    code.extend_from_slice(&[0; 4]);

    code.extend_from_slice(&[0x48, 0xc1, 0xe0, 0x05]); // index *= 32
    encode_rsp_load_rcx(&mut code, 32);
    code.extend_from_slice(&[0x48, 0x01, 0xc8]); // entry pointer
    encode_rsp_store_rax(&mut code, 64);
    code.extend_from_slice(&[0x48, 0x8b, 0x48, 0x08]); // entry key length
    encode_rsp_load_rax(&mut code, 8);
    code.extend_from_slice(&[0x48, 0x39, 0xc1, 0x0f, 0x85]); // unequal lengths => next
    let next_from_length = code.len();
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x48, 0xc7, 0x44, 0x24, 0x48, 0, 0, 0, 0]); // byte index = 0

    let compare_byte = code.len();
    encode_rsp_load_rdx(&mut code, 72);
    encode_rsp_load_rcx(&mut code, 8);
    code.extend_from_slice(&[0x48, 0x39, 0xca, 0x0f, 0x83]); // end of key => equal
    let key_equal = code.len();
    code.extend_from_slice(&[0; 4]);
    encode_rsp_load_rax(&mut code, 64);
    code.extend_from_slice(&[0x48, 0x8b, 0x38]); // rdi = entry key
    encode_rsp_load_rsi(&mut code, 0);
    encode_rsp_load_rdx(&mut code, 72);
    code.extend_from_slice(&[0x8a, 0x04, 0x17]); // al = entry key[index]
    code.extend_from_slice(&[0x8a, 0x0c, 0x16]); // cl = new key[index]
    code.extend_from_slice(&[0x38, 0xc8, 0x0f, 0x85]); // unequal => next
    let next_from_byte = code.len();
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x48, 0x83, 0x44, 0x24, 0x48, 1]); // byte index++
    code.push(0xe9);
    let byte_loop_back = code.len();
    code.extend_from_slice(&[0; 4]);

    let update_existing = code.len();
    encode_rsp_load_rax(&mut code, 64);
    code.extend_from_slice(&[0x48, 0x8b, 0x7c, 0x24, 0x40]); // rdi = entry
    encode_rsp_load_rax(&mut code, 16);
    code.extend_from_slice(&[0x48, 0x89, 0x47, 0x10]); // replace value pointer
    encode_rsp_load_rax(&mut code, 24);
    code.extend_from_slice(&[0x48, 0x89, 0x47, 0x18]); // replace value length
    code.push(0xe9);
    let done_from_update = code.len();
    code.extend_from_slice(&[0; 4]);

    let next_entry = code.len();
    code.extend_from_slice(&[0x48, 0x83, 0x44, 0x24, 0x38, 1]); // index++
    code.push(0xe9);
    let scan_loop_back = code.len();
    code.extend_from_slice(&[0; 4]);

    let append_entry = code.len();
    encode_rsp_load_rax(&mut code, 40);
    encode_rsp_load_rcx(&mut code, 48);
    code.extend_from_slice(&[0x48, 0x39, 0xc8, 0x0f, 0x82]); // len < cap => write
    let have_capacity = code.len();
    code.extend_from_slice(&[0; 4]);
    encode_rsp_load_rax(&mut code, 48);
    code.extend_from_slice(&[0x48, 0x85, 0xc0, 0x0f, 0x84]); // cap == 0
    let empty_capacity = code.len();
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x48, 0xd1, 0xe0]); // cap *= 2
    code.push(0xe9);
    let capacity_ready = code.len();
    code.extend_from_slice(&[0; 4]);
    let first_capacity = code.len();
    code.extend_from_slice(&[0x48, 0xc7, 0xc0, 4, 0, 0, 0]);
    let store_capacity = code.len();
    encode_rsp_store_rax(&mut code, 88);
    encode_rsp_load_rax(&mut code, 88);
    code.extend_from_slice(&[0x48, 0xc1, 0xe0, 0x05]); // bytes = cap * 32
    code.extend_from_slice(&[0x48, 0x81, 0xc0, 0xff, 0x0f, 0, 0]); // page round up
    code.extend_from_slice(&[0x48, 0x81, 0xe0, 0, 0xf0, 0xff, 0xff]);
    code.extend_from_slice(&[
        0x48, 0x89, 0xc6, // rsi = allocation size
        0x31, 0xff, // mmap(NULL, ...)
        0xba, 0x03, 0, 0, 0, // PROT_READ | PROT_WRITE
        0x41, 0xba, 0x22, 0, 0, 0, // MAP_PRIVATE | MAP_ANONYMOUS
        0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, // fd = -1
        0x45, 0x31, 0xc9, // offset = 0
        0xb8, 0x09, 0, 0, 0, // SYS_mmap
        0x0f, 0x05,
    ]);
    encode_rsp_store_rax(&mut code, 96);
    code.extend_from_slice(&[0x48, 0x8b, 0x7c, 0x24, 0x60]); // rdi = new base
    encode_rsp_load_rsi(&mut code, 32);
    encode_rsp_load_rcx(&mut code, 40);
    code.extend_from_slice(&[0x48, 0xc1, 0xe1, 0x05, 0xf3, 0xa4]); // copy len*32
    encode_rsp_load_rax(&mut code, 96);
    encode_rbp_store_collection_rax(&mut code, pointer_slot, indirect);
    encode_rsp_load_rax(&mut code, 88);
    encode_rbp_store_collection_rax(&mut code, capacity_slot, indirect);

    let write_entry = code.len();
    encode_rbp_load_collection_rax(&mut code, pointer_slot, indirect);
    encode_rbp_load_collection_rcx(&mut code, length_slot, indirect);
    code.extend_from_slice(&[0x48, 0xc1, 0xe1, 0x05, 0x48, 0x01, 0xc8]);
    code.extend_from_slice(&[0x48, 0x89, 0xc7]); // rdi = next entry
    encode_rsp_load_rax(&mut code, 0);
    code.extend_from_slice(&[0x48, 0x89, 0x07]);
    encode_rsp_load_rax(&mut code, 8);
    code.extend_from_slice(&[0x48, 0x89, 0x47, 0x08]);
    encode_rsp_load_rax(&mut code, 16);
    code.extend_from_slice(&[0x48, 0x89, 0x47, 0x10]);
    encode_rsp_load_rax(&mut code, 24);
    code.extend_from_slice(&[0x48, 0x89, 0x47, 0x18]);
    encode_rbp_load_collection_rax(&mut code, length_slot, indirect);
    code.extend_from_slice(&[0x48, 0x83, 0xc0, 1]);
    encode_rbp_store_collection_rax(&mut code, length_slot, indirect);
    let done = code.len();
    emit_rsp_adjust(&mut code, false, 112);

    patch_rel32(&mut code, scan_done, append_entry);
    patch_rel32(&mut code, next_from_length, next_entry);
    patch_rel32(&mut code, key_equal, update_existing);
    patch_rel32(&mut code, next_from_byte, next_entry);
    patch_rel32(&mut code, byte_loop_back, compare_byte);
    patch_rel32(&mut code, done_from_update, done);
    patch_rel32(&mut code, scan_loop_back, scan);
    patch_rel32(&mut code, have_capacity, write_entry);
    patch_rel32(&mut code, empty_capacity, first_capacity);
    patch_rel32(&mut code, capacity_ready, store_capacity);
    code
}

fn encode_btree_map_get_string(
    pointer_slot: usize,
    length_slot: usize,
    key: &Value,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
) -> Vec<u8> {
    // BTreeMap's current native representation is a flat array of 32-byte
    // string entries. A missing key traps rather than returning a fabricated
    // value; Option branching is a later slice.
    let mut code = Vec::new();
    emit_rsp_adjust(&mut code, true, 64);
    append_encoded_value(&mut code, key, in_function, relocations);
    encode_rsp_store_rax(&mut code, 0);
    encode_rsp_store_rdx(&mut code, 8);
    encode_rbp_load_rax(&mut code, -8 * (pointer_slot as i32 + 1));
    encode_rsp_store_rax(&mut code, 16);
    encode_rbp_load_rax(&mut code, -8 * (length_slot as i32 + 1));
    encode_rsp_store_rax(&mut code, 24);
    code.extend_from_slice(&[0x48, 0xc7, 0x44, 0x24, 0x20, 0, 0, 0, 0]); // index = 0
    let scan = code.len();
    encode_rsp_load_rax(&mut code, 32);
    encode_rsp_load_rcx(&mut code, 24);
    code.extend_from_slice(&[0x48, 0x39, 0xc8, 0x0f, 0x83]); // exhausted => trap
    let missing = code.len();
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x48, 0xc1, 0xe0, 0x05]); // index *= 32
    encode_rsp_load_rcx(&mut code, 16);
    code.extend_from_slice(&[0x48, 0x01, 0xc8]);
    encode_rsp_store_rax(&mut code, 40); // current entry
    code.extend_from_slice(&[0x48, 0x8b, 0x48, 0x08]); // current key length
    encode_rsp_load_rax(&mut code, 8);
    code.extend_from_slice(&[0x48, 0x39, 0xc1, 0x0f, 0x85]);
    let next_length = code.len();
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x48, 0xc7, 0x44, 0x24, 0x30, 0, 0, 0, 0]); // byte = 0
    let compare = code.len();
    encode_rsp_load_rdx(&mut code, 48);
    encode_rsp_load_rcx(&mut code, 8);
    code.extend_from_slice(&[0x48, 0x39, 0xca, 0x0f, 0x83]); // all bytes matched
    let equal = code.len();
    code.extend_from_slice(&[0; 4]);
    encode_rsp_load_rax(&mut code, 40);
    code.extend_from_slice(&[0x48, 0x8b, 0x38]); // entry key pointer
    encode_rsp_load_rsi(&mut code, 0);
    encode_rsp_load_rdx(&mut code, 48);
    code.extend_from_slice(&[0x8a, 0x04, 0x17, 0x8a, 0x0c, 0x16, 0x38, 0xc8, 0x0f, 0x85]);
    let next_byte = code.len();
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x48, 0x83, 0x44, 0x24, 0x30, 1]);
    code.push(0xe9);
    let loop_back = code.len();
    code.extend_from_slice(&[0; 4]);
    let found = code.len();
    encode_rsp_load_rcx(&mut code, 40);
    code.extend_from_slice(&[0x48, 0x8b, 0x41, 0x10]); // value pointer
    code.extend_from_slice(&[0x48, 0x8b, 0x51, 0x18]); // value length
    code.push(0xe9);
    let found_jump = code.len();
    code.extend_from_slice(&[0; 4]);
    let next = code.len();
    code.extend_from_slice(&[0x48, 0x83, 0x44, 0x24, 0x20, 1]);
    code.push(0xe9);
    let next_loop = code.len();
    code.extend_from_slice(&[0; 4]);
    let trap = code.len();
    code.extend_from_slice(&[0x0f, 0x0b]);
    let done = code.len();
    emit_rsp_adjust(&mut code, false, 64);
    patch_rel32(&mut code, missing, trap);
    patch_rel32(&mut code, next_length, next);
    patch_rel32(&mut code, equal, found);
    patch_rel32(&mut code, next_byte, next);
    patch_rel32(&mut code, loop_back, compare);
    patch_rel32(&mut code, found_jump, done);
    patch_rel32(&mut code, next_loop, scan);
    code
}

fn encode_any_string_trim_empty(
    pointer_slot: usize,
    length_slot: usize,
    relocations: &mut Vec<PendingRelocation>,
) -> Vec<u8> {
    let mut code = Vec::new();
    emit_rsp_adjust(&mut code, true, 64);
    encode_rbp_load_rax(&mut code, -8 * (pointer_slot as i32 + 1));
    encode_rsp_store_rax(&mut code, 0);
    encode_rbp_load_rax(&mut code, -8 * (length_slot as i32 + 1));
    encode_rsp_store_rax(&mut code, 8);
    code.extend_from_slice(&[0x48, 0xc7, 0x44, 0x24, 0x10, 0, 0, 0, 0]);

    let next_string = code.len();
    encode_rsp_load_rax(&mut code, 16);
    encode_rsp_load_rcx(&mut code, 8);
    code.extend_from_slice(&[0x48, 0x39, 0xc8, 0x0f, 0x83]);
    let no_empty_string = code.len();
    code.extend_from_slice(&[0; 4]);
    encode_rsp_load_rax(&mut code, 0);
    encode_rsp_load_rcx(&mut code, 16);
    code.extend_from_slice(&[0x48, 0xc1, 0xe1, 4, 0x48, 0x01, 0xc8]);
    code.extend_from_slice(&[0x48, 0x8b, 0x08]);
    encode_rsp_store_rcx(&mut code, 24);
    code.extend_from_slice(&[0x48, 0x8b, 0x40, 0x08]);
    encode_rsp_store_rax(&mut code, 32);
    code.extend_from_slice(&[0x48, 0xc7, 0x44, 0x24, 0x28, 0, 0, 0, 0]);

    let next_character = code.len();
    encode_rsp_load_rax(&mut code, 40);
    encode_rsp_load_rcx(&mut code, 32);
    code.extend_from_slice(&[0x48, 0x39, 0xc8, 0x0f, 0x83]);
    let string_is_empty = code.len();
    code.extend_from_slice(&[0; 4]);
    encode_rsp_load_rax(&mut code, 24);
    code.extend_from_slice(&[0x48, 0x89, 0xc7]);
    code.extend_from_slice(&[0x48, 0x8d, 0x74, 0x24, 0x28]);
    code.push(0xe8);
    let utf8_call = code.len();
    code.extend_from_slice(&[0; 4]);
    relocations.push(PendingRelocation {
        offset: utf8_call as u64,
        target: "__quill_utf8_next".to_owned(),
    });
    code.push(0xe8);
    let whitespace_call = code.len();
    code.extend_from_slice(&[0; 4]);
    relocations.push(PendingRelocation {
        offset: whitespace_call as u64,
        target: "__quill_char_is_whitespace".to_owned(),
    });
    code.extend_from_slice(&[0x85, 0xc0, 0x0f, 0x84]);
    let non_whitespace = code.len();
    code.extend_from_slice(&[0; 4]);
    code.push(0xe9);
    let character_loop = code.len();
    code.extend_from_slice(&[0; 4]);

    let string_is_empty_target = code.len();
    code.extend_from_slice(&[0xb8, 1, 0, 0, 0]);
    code.push(0xe9);
    let empty_exit = code.len();
    code.extend_from_slice(&[0; 4]);

    let non_whitespace_target = code.len();
    encode_rsp_load_rax(&mut code, 16);
    code.extend_from_slice(&[0x48, 0x83, 0xc0, 1]);
    encode_rsp_store_rax(&mut code, 16);
    code.push(0xe9);
    let outer_loop = code.len();
    code.extend_from_slice(&[0; 4]);

    let false_result = code.len();
    code.extend_from_slice(&[0x31, 0xc0]);
    let done = code.len();
    emit_rsp_adjust(&mut code, false, 64);
    patch_rel32(&mut code, no_empty_string, false_result);
    patch_rel32(&mut code, string_is_empty, string_is_empty_target);
    patch_rel32(&mut code, character_loop, next_character);
    patch_rel32(&mut code, non_whitespace, non_whitespace_target);
    patch_rel32(&mut code, empty_exit, done);
    patch_rel32(&mut code, outer_loop, next_string);
    code
}

fn encode_string_append_char(
    pointer_slot: usize,
    length_slot: usize,
    capacity_slot: usize,
    character: &Value,
    lowercase: bool,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
) -> Vec<u8> {
    let mut code = Vec::new();
    emit_rsp_adjust(&mut code, true, 112);
    append_encoded_value(&mut code, character, in_function, relocations);
    encode_rsp_store_eax(&mut code, 0); // code point
    if lowercase {
        encode_rsp_load_eax(&mut code, 0);
        code.extend_from_slice(&[0x83, 0xf8, b'A', 0x0f, 0x82]);
        let not_upper_before = code.len();
        code.extend_from_slice(&[0; 4]);
        code.extend_from_slice(&[0x83, 0xf8, b'Z', 0x0f, 0x87]);
        let not_upper_after = code.len();
        code.extend_from_slice(&[0; 4]);
        code.extend_from_slice(&[0x83, 0xc0, 32]); // ASCII uppercase to lowercase
        encode_rsp_store_eax(&mut code, 0);
        let after_lowercase = code.len();
        patch_rel32(&mut code, not_upper_before, after_lowercase);
        patch_rel32(&mut code, not_upper_after, after_lowercase);
    }

    code.extend_from_slice(&[0xc7, 0x44, 0x24, 0x10, 0, 0, 0, 0]); // clear UTF-8 byte scratch
    encode_rsp_load_eax(&mut code, 0);
    code.extend_from_slice(&[0x3d, 0x80, 0, 0, 0, 0x0f, 0x82]);
    let ascii_branch = code.len();
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x3d, 0x00, 0x08, 0, 0, 0x0f, 0x82]);
    let two_byte_branch = code.len();
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x3d, 0, 0, 1, 0, 0x0f, 0x82]);
    let three_byte_branch = code.len();
    code.extend_from_slice(&[0; 4]);

    // Four-byte encoding.
    code.extend_from_slice(&[0x89, 0xc1, 0xc1, 0xe9, 18, 0x83, 0xc9, 0xf0]);
    code.extend_from_slice(&[0x88, 0x4c, 0x24, 0x10]);
    code.extend_from_slice(&[
        0x89, 0xc1, 0xc1, 0xe9, 12, 0x83, 0xe1, 0x3f, 0x83, 0xc9, 0x80, 0x88, 0x4c, 0x24, 0x11,
    ]);
    code.extend_from_slice(&[
        0x89, 0xc1, 0xc1, 0xe9, 6, 0x83, 0xe1, 0x3f, 0x83, 0xc9, 0x80, 0x88, 0x4c, 0x24, 0x12,
    ]);
    code.extend_from_slice(&[
        0x89, 0xc1, 0x83, 0xe1, 0x3f, 0x83, 0xc9, 0x80, 0x88, 0x4c, 0x24, 0x13, 0x48, 0xc7, 0x44,
        0x24, 0x08, 4, 0, 0, 0,
    ]);
    code.push(0xe9);
    let encoded_bytes = code.len();
    code.extend_from_slice(&[0; 4]);

    let three_byte = code.len();
    code.extend_from_slice(&[
        0x89, 0xc1, 0xc1, 0xe9, 12, 0x83, 0xc9, 0xe0, 0x88, 0x4c, 0x24, 0x10, 0x89, 0xc1, 0xc1,
        0xe9, 6, 0x83, 0xe1, 0x3f, 0x83, 0xc9, 0x80, 0x88, 0x4c, 0x24, 0x11, 0x89, 0xc1, 0x83,
        0xe1, 0x3f, 0x83, 0xc9, 0x80, 0x88, 0x4c, 0x24, 0x12, 0x48, 0xc7, 0x44, 0x24, 0x08, 3, 0,
        0, 0,
    ]);
    code.push(0xe9);
    let encoded_three = code.len();
    code.extend_from_slice(&[0; 4]);

    let two_byte = code.len();
    code.extend_from_slice(&[
        0x89, 0xc1, 0xc1, 0xe9, 6, 0x83, 0xc9, 0xc0, 0x88, 0x4c, 0x24, 0x10, 0x89, 0xc1, 0x83,
        0xe1, 0x3f, 0x83, 0xc9, 0x80, 0x88, 0x4c, 0x24, 0x11, 0x48, 0xc7, 0x44, 0x24, 0x08, 2, 0,
        0, 0,
    ]);
    code.push(0xe9);
    let encoded_two = code.len();
    code.extend_from_slice(&[0; 4]);

    let ascii = code.len();
    code.extend_from_slice(&[0x88, 0x44, 0x24, 0x10]);
    code.extend_from_slice(&[0x48, 0xc7, 0x44, 0x24, 0x08, 1, 0, 0, 0]);
    let start_append = code.len();

    encode_rbp_load_collection_rax(&mut code, pointer_slot, false);
    encode_rsp_store_rax(&mut code, 24);
    encode_rbp_load_collection_rax(&mut code, length_slot, false);
    encode_rsp_store_rax(&mut code, 32);
    encode_rbp_load_collection_rax(&mut code, capacity_slot, false);
    encode_rsp_store_rax(&mut code, 40);
    encode_rsp_load_rax(&mut code, 32);
    encode_rsp_load_rcx(&mut code, 8);
    code.extend_from_slice(&[0x48, 0x01, 0xc8]);
    encode_rsp_store_rax(&mut code, 48); // required capacity
    encode_rsp_load_rcx(&mut code, 40);
    code.extend_from_slice(&[0x48, 0x39, 0xc8, 0x0f, 0x86]); // required <= cap
    let has_capacity = code.len();
    code.extend_from_slice(&[0; 4]);
    encode_rsp_load_rax(&mut code, 40);
    code.extend_from_slice(&[0x48, 0x85, 0xc0, 0x0f, 0x84]);
    let first_allocation = code.len();
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x48, 0xd1, 0xe0]);
    encode_rsp_store_rax(&mut code, 56);
    code.push(0xe9);
    let allocation_block = code.len();
    code.extend_from_slice(&[0; 4]);
    let initial_capacity = code.len();
    code.extend_from_slice(&[0x48, 0xc7, 0xc0, 16, 0, 0, 0]);
    encode_rsp_store_rax(&mut code, 56);
    let allocate = code.len();
    encode_rsp_load_rax(&mut code, 56);
    code.extend_from_slice(&[0x48, 0x81, 0xc0, 0xff, 0x0f, 0, 0]);
    code.extend_from_slice(&[0x48, 0x81, 0xe0, 0, 0xf0, 0xff, 0xff]);
    code.extend_from_slice(&[
        0x48, 0x89, 0xc6, 0x31, 0xff, 0xba, 0x03, 0, 0, 0, 0x41, 0xba, 0x22, 0, 0, 0, 0x49, 0xc7,
        0xc0, 0xff, 0xff, 0xff, 0xff, 0x45, 0x31, 0xc9, 0xb8, 9, 0, 0, 0, 0x0f, 0x05,
    ]);
    encode_rsp_store_rax(&mut code, 64);
    code.extend_from_slice(&[0x48, 0x8b, 0x7c, 0x24, 0x40]);
    encode_rsp_load_rsi(&mut code, 24);
    encode_rsp_load_rcx(&mut code, 32);
    code.extend_from_slice(&[0xf3, 0xa4]); // copy old string bytes
    encode_rsp_load_rax(&mut code, 64);
    encode_rbp_store_collection_rax(&mut code, pointer_slot, false);
    encode_rsp_load_rax(&mut code, 56);
    encode_rbp_store_collection_rax(&mut code, capacity_slot, false);

    let write_bytes = code.len();
    encode_rbp_load_collection_rax(&mut code, pointer_slot, false);
    encode_rsp_load_rcx(&mut code, 32);
    code.extend_from_slice(&[0x48, 0x01, 0xc8, 0x48, 0x89, 0xc7]);
    code.extend_from_slice(&[0x48, 0x8d, 0x74, 0x24, 0x10]);
    encode_rsp_load_rcx(&mut code, 8);
    code.extend_from_slice(&[0xf3, 0xa4]);
    encode_rsp_load_rax(&mut code, 48);
    encode_rbp_store_collection_rax(&mut code, length_slot, false);
    emit_rsp_adjust(&mut code, false, 112);

    patch_rel32(&mut code, ascii_branch, ascii);
    patch_rel32(&mut code, two_byte_branch, two_byte);
    patch_rel32(&mut code, three_byte_branch, three_byte);
    patch_rel32(&mut code, encoded_bytes, start_append);
    patch_rel32(&mut code, encoded_three, start_append);
    patch_rel32(&mut code, encoded_two, start_append);
    patch_rel32(&mut code, has_capacity, write_bytes);
    patch_rel32(&mut code, first_allocation, initial_capacity);
    patch_rel32(&mut code, allocation_block, allocate);
    code
}

fn encode_owned_string(
    source: &Value,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
) -> Vec<u8> {
    let mut code = Vec::new();
    emit_rsp_adjust(&mut code, true, 32);
    append_encoded_value(&mut code, source, in_function, relocations);
    code.extend_from_slice(&[
        0x48, 0x89, 0x04, 0x24, // save source pointer
        0x48, 0x89, 0x54, 0x24, 0x08, // save source length
        0x48, 0x89, 0xd6, // mov rsi, rdx
        0x48, 0x81, 0xc6, 0xff, 0x0f, 0x00, 0x00, // add rsi, 4095
        0x48, 0x81, 0xe6, 0x00, 0xf0, 0xff, 0xff, // round up to 4096 bytes
        0x48, 0x85, 0xf6, // test rsi, rsi
        0x0f, 0x85, 0x05, 0x00, 0x00, 0x00, // jnz past minimum-size move
        0xbe, 0x00, 0x10, 0x00, 0x00, // mov esi, 4096
        0x31, 0xff, // xor edi, edi: kernel chooses the address
        0xba, 0x03, 0x00, 0x00, 0x00, // mov edx, PROT_READ | PROT_WRITE
        0x41, 0xba, 0x22, 0x00, 0x00, 0x00, // mov r10d, MAP_PRIVATE | MAP_ANONYMOUS
        0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, // mov r8, -1
        0x45, 0x31, 0xc9, // xor r9d, r9d
        0xb8, 0x09, 0x00, 0x00, 0x00, // mov eax, SYS_mmap
        0x0f, 0x05, // syscall
        0x48, 0x89, 0xc7, // mov rdi, rax: destination pointer
        0x48, 0x8b, 0x34, 0x24, // mov rsi, [rsp]: source pointer
        0x48, 0x8b, 0x4c, 0x24, 0x08, // mov rcx, [rsp + 8]: byte length
        0xf3, 0xa4, // rep movsb
        0x48, 0x8b, 0x54, 0x24, 0x08, // restore byte length in rdx
    ]);
    emit_rsp_adjust(&mut code, false, 32);
    code
}

fn encode_char_is_ascii_alphanumeric(
    character: &Value,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
) -> Vec<u8> {
    let mut code = Vec::new();
    append_encoded_value(&mut code, character, in_function, relocations);
    let mut false_branches = Vec::new();
    let mut true_branches = Vec::new();
    for (lower, upper) in [(b'0', b'9'), (b'A', b'Z')] {
        code.extend_from_slice(&[0x83, 0xf8, lower]); // cmp eax, range lower bound
        code.extend_from_slice(&[0x0f, 0x82]); // jb false
        false_branches.push(code.len());
        code.extend_from_slice(&[0; 4]);
        code.extend_from_slice(&[0x83, 0xf8, upper]); // cmp eax, range upper bound
        code.extend_from_slice(&[0x0f, 0x86]); // jbe true
        true_branches.push(code.len());
        code.extend_from_slice(&[0; 4]);
    }
    code.extend_from_slice(&[0x83, 0xf8, b'a']); // cmp eax, 'a'
    code.extend_from_slice(&[0x0f, 0x82]);
    false_branches.push(code.len());
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x83, 0xf8, b'z']); // cmp eax, 'z'
    code.extend_from_slice(&[0x0f, 0x96, 0xc0, 0x0f, 0xb6, 0xc0]); // setbe al; movzx eax,al
    code.push(0xe9);
    let finish_lowercase = code.len();
    code.extend_from_slice(&[0; 4]);
    let is_alphanumeric = code.len();
    code.extend_from_slice(&[0xb8, 1, 0, 0, 0]);
    code.push(0xe9);
    let finish_true = code.len();
    code.extend_from_slice(&[0; 4]);
    let not_alphanumeric = code.len();
    code.extend_from_slice(&[0x31, 0xc0]);
    let done = code.len();
    for displacement in false_branches {
        patch_rel32(&mut code, displacement, not_alphanumeric);
    }
    for displacement in true_branches {
        patch_rel32(&mut code, displacement, is_alphanumeric);
    }
    patch_rel32(&mut code, finish_lowercase, done);
    patch_rel32(&mut code, finish_true, done);
    code
}

fn encode_format_string(
    pieces: &[FormatPiece],
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
) -> Vec<u8> {
    let mut code = vec![
        0x48, 0x83, 0xec, 0x20, // reserve base pointer, length, and conversion scratch
        0x31, 0xff, // xor edi, edi
        0xbe, 0x00, 0x10, 0x00, 0x00, // mov esi, 4096
        0xba, 0x03, 0x00, 0x00, 0x00, // mov edx, PROT_READ | PROT_WRITE
        0x41, 0xba, 0x22, 0x00, 0x00, 0x00, // mov r10d, MAP_PRIVATE | MAP_ANONYMOUS
        0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, // mov r8, -1
        0x45, 0x31, 0xc9, // xor r9d, r9d
        0xb8, 0x09, 0x00, 0x00, 0x00, // mov eax, SYS_mmap
        0x0f, 0x05, // syscall
        0x48, 0x89, 0x04, 0x24, // store allocation base
        0x48, 0xc7, 0x44, 0x24, 0x08, 0, 0, 0, 0, // initialize output length
    ];
    for piece in pieces {
        match piece {
            FormatPiece::Text(text) if !text.is_empty() => {
                code.extend_from_slice(&[
                    0x48, 0x8b, 0x3c, 0x24, // mov rdi, [rsp]: destination base
                    0x48, 0x8b, 0x44, 0x24, 0x08, // mov rax, [rsp + 8]: current length
                    0x48, 0x01, 0xc7, // add rdi, rax
                    0x48, 0x8d, 0x35, // lea rsi, [rip + literal]
                ]);
                let offset = code.len() as u64;
                code.extend_from_slice(&[0; 4]);
                relocations.push(PendingRelocation {
                    offset,
                    target: text_symbol_name(text),
                });
                code.push(0xb9); // mov ecx, byte length
                code.extend_from_slice(&(text.len() as u32).to_le_bytes());
                code.extend_from_slice(&[
                    0xf3, 0xa4, // rep movsb
                    0x48, 0x81, 0x44, 0x24, 0x08, // add qword [rsp + 8], literal length
                ]);
                code.extend_from_slice(&(text.len() as u32).to_le_bytes());
            }
            FormatPiece::Text(_) => {}
            FormatPiece::Decimal(value) | FormatPiece::SignedDecimal(value) => {
                let signed = matches!(piece, FormatPiece::SignedDecimal(_));
                append_encoded_value(&mut code, value, in_function, relocations);
                if signed {
                    code.extend_from_slice(&[0x48, 0x98]); // sign-extend i32 in eax to rax
                }
                code.extend_from_slice(&[
                    0x48, 0x89, 0x44, 0x24, 0x10, // save number
                    0x48, 0x8b, 0x3c, 0x24, // mov rdi, [rsp]
                    0x48, 0x8b, 0x44, 0x24, 0x08, // mov rax, [rsp + 8]
                    0x48, 0x01, 0xc7, // add rdi, rax
                    0x48, 0x8b, 0x74, 0x24, 0x10, // mov rsi, [rsp + 16]
                    0xe8, // call __quill_format_decimal
                ]);
                let offset = code.len() as u64;
                code.extend_from_slice(&[0; 4]);
                relocations.push(PendingRelocation {
                    offset,
                    target: if signed {
                        "__quill_format_signed_decimal".to_owned()
                    } else {
                        "__quill_format_decimal".to_owned()
                    },
                });
                code.extend_from_slice(&[
                    0x48, 0x01, 0x44, 0x24, 0x08, // add [rsp + 8], rax
                ]);
            }
        }
    }
    code.extend_from_slice(&[
        0x48, 0x8b, 0x04, 0x24, // mov rax, [rsp]: owned data pointer
        0x48, 0x8b, 0x54, 0x24, 0x08, // mov rdx, [rsp + 8]: byte length
        0x48, 0x83, 0xc4, 0x20, // release scratch
    ]);
    code
}

#[derive(Default)]
struct LoopPatches {
    breaks: Vec<usize>,
    continues: Vec<usize>,
}

fn encode_statement_into(
    code: &mut Vec<u8>,
    statement: &Statement,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
    loops: &mut Vec<LoopPatches>,
) {
    match statement {
        Statement::Evaluate(value) => append_encoded_value(code, value, in_function, relocations),
        Statement::Println(parts) => {
            for part in parts {
                match part {
                    PrintPart::Panic(text) => {
                        code.extend_from_slice(&[0x48, 0x8d, 0x35]); // lea rsi, [rip + panic text]
                        let offset = code.len() as u64;
                        code.extend_from_slice(&[0; 4]);
                        relocations.push(PendingRelocation {
                            offset,
                            target: text_symbol_name(text),
                        });
                        code.extend_from_slice(&[0xbf, 2, 0, 0, 0]); // mov edi, stderr
                        code.extend_from_slice(&[0xba]); // mov edx, byte length
                        code.extend_from_slice(&(text.len() as u32).to_le_bytes());
                        code.extend_from_slice(&[0xb8, 1, 0, 0, 0, 0x0f, 0x05]); // write syscall
                        code.extend_from_slice(&[
                            0xbf, 101, 0, 0, 0, // mov edi, Rust panic exit status
                            0xb8, 60, 0, 0, 0, // mov eax, SYS_exit
                            0x0f, 0x05, // syscall
                        ]);
                    }
                    PrintPart::PanicText(text) => {
                        code.extend_from_slice(&[0x48, 0x8d, 0x35]); // lea rsi, [rip + panic text]
                        let offset = code.len() as u64;
                        code.extend_from_slice(&[0; 4]);
                        relocations.push(PendingRelocation {
                            offset,
                            target: text_symbol_name(text),
                        });
                        code.extend_from_slice(&[0xbf, 2, 0, 0, 0]); // mov edi, stderr
                        code.push(0xba); // mov edx, byte length
                        code.extend_from_slice(&(text.len() as u32).to_le_bytes());
                        code.extend_from_slice(&[0xb8, 1, 0, 0, 0, 0x0f, 0x05]); // write syscall
                    }
                    PrintPart::PanicDecimal(value) => {
                        append_encoded_value(code, value, in_function, relocations);
                        code.extend_from_slice(&[0x48, 0x89, 0xc7]); // mov rdi, rax
                        code.push(0xe8);
                        let offset = code.len() as u64;
                        code.extend_from_slice(&[0; 4]);
                        relocations.push(PendingRelocation {
                            offset,
                            target: "__quill_print_decimal_stderr".to_owned(),
                        });
                    }
                    PrintPart::Text(text) if !text.is_empty() => {
                        code.extend_from_slice(&[0x48, 0x8d, 0x35]); // lea rsi, [rip + disp32]
                        let offset = code.len() as u64;
                        code.extend_from_slice(&[0; 4]);
                        relocations.push(PendingRelocation {
                            offset,
                            target: text_symbol_name(text),
                        });
                        code.extend_from_slice(&[0xbf, 1, 0, 0, 0]); // mov edi, 1
                        code.extend_from_slice(&[0xba]); // mov edx, len
                        code.extend_from_slice(&(text.len() as u32).to_le_bytes());
                        code.extend_from_slice(&[0xb8, 1, 0, 0, 0, 0x0f, 0x05]); // write syscall
                    }
                    PrintPart::Text(_) => {}
                    PrintPart::Slice(value) => {
                        append_encoded_value(code, value, in_function, relocations);
                        code.extend_from_slice(&[
                            0x48, 0x89, 0xc6, // mov rsi, rax: string pointer
                            0xbf, 1, 0, 0, 0, // mov edi, stdout
                            0xb8, 1, 0, 0, 0, // mov eax, SYS_write
                            0x0f, 0x05, // syscall (rdx already has the byte length)
                        ]);
                    }
                    PrintPart::Decimal(value) | PrintPart::Hex16(value) => {
                        append_encoded_value(code, value, in_function, relocations);
                        code.extend_from_slice(&[0x48, 0x89, 0xc7]); // mov rdi, rax
                        code.push(0xe8);
                        let offset = code.len() as u64;
                        code.extend_from_slice(&[0; 4]);
                        relocations.push(PendingRelocation {
                            offset,
                            target: if matches!(part, PrintPart::Decimal(_)) {
                                "__quill_print_decimal".to_owned()
                            } else {
                                "__quill_print_hex16".to_owned()
                            },
                        });
                    }
                }
            }
        }
        Statement::Let { slot, initializer } => {
            append_encoded_value(code, initializer, in_function, relocations);
            if is_word_value(initializer) {
                encode_rbp_store_rax(code, -8 * (*slot as i32 + 1));
            } else {
                encode_rbp_store_eax(code, -8 * (*slot as i32 + 1));
            }
        }
        Statement::LetSlice {
            pointer_slot,
            length_slot,
            initializer,
        } => {
            append_encoded_value(code, initializer, in_function, relocations);
            encode_rbp_store_rax(code, -8 * (*pointer_slot as i32 + 1));
            encode_rbp_store_rdx(code, -8 * (*length_slot as i32 + 1));
        }
        Statement::LetStruct { fields } => {
            for (slot, value) in fields {
                append_encoded_value(code, value, in_function, relocations);
                if matches!(value, Value::EmptyCollection) {
                    encode_rbp_store_rax(code, -8 * (*slot as i32 + 1));
                    encode_rbp_store_rdx(code, -8 * (*slot as i32 + 2));
                    code.extend_from_slice(&[0x31, 0xc0]); // xor eax, eax: zero capacity
                    encode_rbp_store_rax(code, -8 * (*slot as i32 + 3));
                } else if is_slice_value(value) {
                    encode_rbp_store_rax(code, -8 * (*slot as i32 + 1));
                    encode_rbp_store_rdx(code, -8 * (*slot as i32 + 2));
                } else if is_word_value(value) {
                    encode_rbp_store_rax(code, -8 * (*slot as i32 + 1));
                } else {
                    encode_rbp_store_eax(code, -8 * (*slot as i32 + 1));
                }
            }
        }
        Statement::LetTupleCall {
            fields,
            initializer,
        } => {
            append_encoded_value(code, initializer, in_function, relocations);
            let aggregate = matches!(
                initializer,
                Value::Call {
                    returns_aggregate: true,
                    ..
                }
            );
            if !aggregate {
                debug_assert_eq!(fields.len(), 2);
                let (first_slot, first_is_word) = fields[0];
                let (second_slot, second_is_word) = fields[1];
                if first_is_word {
                    encode_rbp_store_rax(code, -8 * (first_slot as i32 + 1));
                } else {
                    encode_rbp_store_eax(code, -8 * (first_slot as i32 + 1));
                }
                if second_is_word {
                    encode_rbp_store_rdx(code, -8 * (second_slot as i32 + 1));
                } else {
                    encode_rbp_store_edx(code, -8 * (second_slot as i32 + 1));
                }
            }
        }
        Statement::Assign { slot, value } => {
            append_encoded_value(code, value, in_function, relocations);
            if is_word_value(value) {
                encode_rbp_store_rax(code, -8 * (*slot as i32 + 1));
            } else {
                encode_rbp_store_eax(code, -8 * (*slot as i32 + 1));
            }
        }
        Statement::AssignIndirect {
            pointer_slot,
            is_word,
            value,
        } => {
            append_encoded_value(code, value, in_function, relocations);
            code.push(0x50); // preserve the assigned value
            encode_rbp_load_rcx(code, -8 * (*pointer_slot as i32 + 1));
            code.push(0x58);
            if *is_word {
                code.extend_from_slice(&[0x48, 0x89, 0x01]);
            } else {
                code.extend_from_slice(&[0x89, 0x01]);
            }
        }
        Statement::VecPushString {
            pointer_slot,
            length_slot,
            capacity_slot,
            value,
            indirect,
        } => {
            let first_relocation = relocations.len();
            let encoded = encode_vec_push_string(
                *pointer_slot,
                *length_slot,
                *capacity_slot,
                value,
                *indirect,
                in_function,
                relocations,
            );
            append_encoded_chunk(code, encoded, relocations, first_relocation);
        }
        Statement::StringAppendChar {
            pointer_slot,
            length_slot,
            capacity_slot,
            character,
            lowercase,
        } => {
            let first_relocation = relocations.len();
            let encoded = encode_string_append_char(
                *pointer_slot,
                *length_slot,
                *capacity_slot,
                character,
                *lowercase,
                in_function,
                relocations,
            );
            append_encoded_chunk(code, encoded, relocations, first_relocation);
        }
        Statement::VecPushAggregate {
            pointer_slot,
            length_slot,
            capacity_slot,
            fields,
            widths,
            indirect,
        } => {
            let first_relocation = relocations.len();
            let encoded = encode_vec_push_aggregate(
                *pointer_slot,
                *length_slot,
                *capacity_slot,
                fields,
                widths,
                *indirect,
                in_function,
                relocations,
            );
            append_encoded_chunk(code, encoded, relocations, first_relocation);
        }
        Statement::BTreeMapInsert {
            pointer_slot,
            length_slot,
            capacity_slot,
            key,
            value,
            indirect,
        } => {
            let first_relocation = relocations.len();
            let encoded = encode_btree_map_insert(
                *pointer_slot,
                *length_slot,
                *capacity_slot,
                key,
                value,
                *indirect,
                in_function,
                relocations,
            );
            append_encoded_chunk(code, encoded, relocations, first_relocation);
        }
        Statement::If {
            condition,
            then_body,
            else_body,
        } => {
            append_encoded_condition(code, condition, in_function, relocations);
            code.extend_from_slice(&[0x85, 0xc0]);
            let else_displacement = code.len() + 2;
            code.extend_from_slice(&[0x0f, 0x84, 0, 0, 0, 0]);
            for statement in then_body {
                encode_statement_into(code, statement, in_function, relocations, loops);
            }
            if else_body.is_empty() {
                let end = code.len();
                patch_rel32(code, else_displacement, end);
                return;
            }
            let end_displacement = code.len() + 1;
            code.extend_from_slice(&[0xe9, 0, 0, 0, 0]);
            let else_start = code.len();
            patch_rel32(code, else_displacement, else_start);
            for statement in else_body {
                encode_statement_into(code, statement, in_function, relocations, loops);
            }
            let end = code.len();
            patch_rel32(code, end_displacement, end);
        }
        Statement::While { condition, body } => {
            let loop_start = code.len();
            append_encoded_condition(code, condition, in_function, relocations);
            code.extend_from_slice(&[0x85, 0xc0]);
            let exit_displacement = code.len() + 2;
            code.extend_from_slice(&[0x0f, 0x84, 0, 0, 0, 0]);
            loops.push(LoopPatches::default());
            for statement in body {
                encode_statement_into(code, statement, in_function, relocations, loops);
            }
            let patches = loops.pop().unwrap();
            code.push(0xe9);
            let back_displacement = code.len();
            code.extend_from_slice(&[0; 4]);
            patch_rel32(code, back_displacement, loop_start);
            let loop_end = code.len();
            patch_rel32(code, exit_displacement, loop_end);
            for at in patches.breaks {
                patch_rel32(code, at, loop_end);
            }
            for at in patches.continues {
                patch_rel32(code, at, loop_start);
            }
        }
        Statement::ForRange {
            slot,
            end_slot,
            inclusive,
            body,
        } => {
            let loop_start = code.len();
            let condition = Condition::Compare {
                operator: if *inclusive {
                    BinaryOperator::LessEqual
                } else {
                    BinaryOperator::Less
                },
                left: Box::new(Value::Local(*slot)),
                right: Box::new(Value::Local(*end_slot)),
            };
            append_encoded_condition(code, &condition, in_function, relocations);
            code.extend_from_slice(&[0x85, 0xc0]);
            let condition_exit_displacement = code.len() + 2;
            code.extend_from_slice(&[0x0f, 0x84, 0, 0, 0, 0]);
            loops.push(LoopPatches::default());
            for statement in body {
                encode_statement_into(code, statement, in_function, relocations, loops);
            }
            let patches = loops.pop().unwrap();
            let continue_target = code.len();
            for at in patches.continues {
                patch_rel32(code, at, continue_target);
            }
            let inclusive_exit_displacement = if *inclusive {
                let at_end = Condition::Compare {
                    operator: BinaryOperator::Equal,
                    left: Box::new(Value::Local(*slot)),
                    right: Box::new(Value::Local(*end_slot)),
                };
                append_encoded_condition(code, &at_end, in_function, relocations);
                code.extend_from_slice(&[0x85, 0xc0]); // test eax, eax
                let displacement = code.len() + 2;
                code.extend_from_slice(&[0x0f, 0x85, 0, 0, 0, 0]); // jnz loop end
                Some(displacement)
            } else {
                None
            };
            encode_rbp_load_eax(code, -8 * (*slot as i32 + 1));
            code.extend_from_slice(&[0x05, 1, 0, 0, 0]);
            encode_rbp_store_eax(code, -8 * (*slot as i32 + 1));
            code.push(0xe9);
            let back_displacement = code.len();
            code.extend_from_slice(&[0; 4]);
            patch_rel32(code, back_displacement, loop_start);
            let loop_end = code.len();
            patch_rel32(code, condition_exit_displacement, loop_end);
            for at in patches.breaks {
                patch_rel32(code, at, loop_end);
            }
            if let Some(displacement) = inclusive_exit_displacement {
                patch_rel32(code, displacement, loop_end);
            }
        }
        Statement::ForSlice {
            length_slot,
            index_slot,
            body,
        } => {
            let loop_start = code.len();
            encode_rbp_load_rax(code, -8 * (*index_slot as i32 + 1));
            encode_rbp_load_rcx(code, -8 * (*length_slot as i32 + 1));
            code.extend_from_slice(&[0x48, 0x39, 0xc8]); // cmp rax, rcx
            code.extend_from_slice(&[0x0f, 0x83]); // jae loop end
            let exit_displacement = code.len();
            code.extend_from_slice(&[0; 4]);
            loops.push(LoopPatches::default());
            for statement in body {
                encode_statement_into(code, statement, in_function, relocations, loops);
            }
            let patches = loops.pop().unwrap();
            let continue_target = code.len();
            for at in patches.continues {
                patch_rel32(code, at, continue_target);
            }
            encode_rbp_load_rax(code, -8 * (*index_slot as i32 + 1));
            code.extend_from_slice(&[0x48, 0x83, 0xc0, 1]); // add rax, 1
            encode_rbp_store_rax(code, -8 * (*index_slot as i32 + 1));
            code.push(0xe9);
            let back_displacement = code.len();
            code.extend_from_slice(&[0; 4]);
            patch_rel32(code, back_displacement, loop_start);
            let loop_end = code.len();
            patch_rel32(code, exit_displacement, loop_end);
            for at in patches.breaks {
                patch_rel32(code, at, loop_end);
            }
        }
        Statement::ForChars {
            pointer_slot,
            length_slot,
            index_slot,
            item_slot,
            body,
        } => {
            code.extend_from_slice(&[0x31, 0xc0]);
            encode_rbp_store_rax(code, -8 * (*index_slot as i32 + 1));
            let loop_start = code.len();
            encode_rbp_load_rax(code, -8 * (*index_slot as i32 + 1));
            encode_rbp_load_rcx(code, -8 * (*length_slot as i32 + 1));
            code.extend_from_slice(&[0x48, 0x39, 0xc8, 0x0f, 0x83]);
            let exit_displacement = code.len();
            code.extend_from_slice(&[0; 4]);
            encode_rbp_load_rax(code, -8 * (*pointer_slot as i32 + 1));
            code.extend_from_slice(&[0x48, 0x89, 0xc7]);
            code.extend_from_slice(&[0x48, 0x8d, 0xb5]);
            code.extend_from_slice(&(-8 * (*index_slot as i32 + 1)).to_le_bytes());
            code.push(0xe8);
            let call_offset = code.len() as u64;
            code.extend_from_slice(&[0; 4]);
            relocations.push(PendingRelocation {
                offset: call_offset,
                target: "__quill_utf8_next".to_owned(),
            });
            encode_rbp_store_eax(code, -8 * (*item_slot as i32 + 1));
            loops.push(LoopPatches::default());
            for statement in body {
                encode_statement_into(code, statement, in_function, relocations, loops);
            }
            let patches = loops.pop().unwrap();
            let continue_target = code.len();
            for at in patches.continues {
                patch_rel32(code, at, continue_target);
            }
            code.push(0xe9);
            let back_displacement = code.len();
            code.extend_from_slice(&[0; 4]);
            patch_rel32(code, back_displacement, loop_start);
            let loop_end = code.len();
            patch_rel32(code, exit_displacement, loop_end);
            for at in patches.breaks {
                patch_rel32(code, at, loop_end);
            }
        }
        Statement::Break | Statement::Continue => {
            code.push(0xe9);
            let displacement = code.len();
            code.extend_from_slice(&[0; 4]);
            let context = loops.last_mut().expect("validated loop control");
            if matches!(statement, Statement::Break) {
                context.breaks.push(displacement);
            } else {
                context.continues.push(displacement);
            }
        }
        Statement::Return(value) => {
            append_encoded_value(code, value, in_function, relocations);
            code.extend_from_slice(&[0xc9, 0xc3]); // leave; ret
        }
        Statement::TryResult(result) => {
            append_encoded_value(code, result, in_function, relocations);
            code.extend_from_slice(&[0x48, 0x85, 0xc0]); // test rax, rax
            let success_displacement = code.len() + 2;
            code.extend_from_slice(&[0x0f, 0x84, 0, 0, 0, 0]); // je success
            code.extend_from_slice(&[0xc9, 0xc3]); // return the original Err in rax:rdx
            let success = code.len();
            patch_rel32(code, success_displacement, success);
        }
        Statement::TryOption(result) => {
            append_encoded_value(code, result, in_function, relocations);
            code.extend_from_slice(&[0x85, 0xc0]); // test eax, eax; Some is nonzero
            let success_displacement = code.len() + 2;
            code.extend_from_slice(&[0x0f, 0x85, 0, 0, 0, 0]); // jne success
            code.extend_from_slice(&[0x31, 0xc0, 0x31, 0xd2, 0xc9, 0xc3]); // return None in both registers
            let success = code.len();
            patch_rel32(code, success_displacement, success);
        }
        Statement::TryOptionBind {
            result,
            slot,
            success_on_zero,
            payload_is_word,
        } => {
            append_encoded_value(code, result, in_function, relocations);
            code.extend_from_slice(&[0x85, 0xc0]); // test eax, eax; inspect enum tag
            let success_displacement = code.len() + 2;
            code.extend_from_slice(if *success_on_zero {
                &[0x0f, 0x84][..]
            } else {
                &[0x0f, 0x85][..]
            });
            code.extend_from_slice(&[0, 0, 0, 0]);
            if *success_on_zero {
                code.extend_from_slice(&[0xc9, 0xc3]); // return original Err in eax:edx
            } else {
                code.extend_from_slice(&[0x31, 0xc0, 0x31, 0xd2, 0xc9, 0xc3]); // return None
            }
            let success = code.len();
            patch_rel32(code, success_displacement, success);
            code.extend_from_slice(if *payload_is_word {
                &[0x48, 0x89, 0xd0][..]
            } else {
                &[0x89, 0xd0][..]
            });
            if *payload_is_word {
                encode_rbp_store_rax(code, -8 * (*slot as i32 + 1));
            } else {
                encode_rbp_store_eax(code, -8 * (*slot as i32 + 1));
            }
        }
    }
}

fn encode_if_value(
    condition: &Condition,
    then_value: &Value,
    else_value: &Value,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
) -> Vec<u8> {
    let mut code = Vec::new();
    append_encoded_condition(&mut code, condition, in_function, relocations);
    code.extend_from_slice(&[0x85, 0xc0]); // test eax, eax
    let conditional_displacement = code.len() + 2;
    code.extend_from_slice(&[0x0f, 0x84, 0, 0, 0, 0]); // je else
    append_encoded_value(&mut code, then_value, in_function, relocations);
    code.push(0xe9); // jmp end
    let end_displacement = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    let else_start = code.len();
    patch_rel32(&mut code, conditional_displacement, else_start);
    append_encoded_value(&mut code, else_value, in_function, relocations);
    let end = code.len();
    patch_rel32(&mut code, end_displacement, end);
    code
}

fn encode_binary_value(
    operator: BinaryOperator,
    left: &Value,
    right: &Value,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
) -> Vec<u8> {
    let mut code = Vec::new();
    append_encoded_value(&mut code, left, in_function, relocations);
    emit_rsp_adjust(&mut code, true, 16);
    encode_rsp_store_eax(&mut code, 0);
    append_encoded_value(&mut code, right, in_function, relocations);
    code.extend_from_slice(&[0x89, 0xc1]); // mov ecx, eax
    encode_rsp_load_eax(&mut code, 0);
    emit_rsp_adjust(&mut code, false, 16);
    match operator {
        BinaryOperator::Add => code.extend_from_slice(&[0x01, 0xc8]), // add eax, ecx
        BinaryOperator::Subtract => code.extend_from_slice(&[0x29, 0xc8]), // sub eax, ecx
        BinaryOperator::Multiply => code.extend_from_slice(&[0x0f, 0xaf, 0xc1]), // imul eax, ecx
        BinaryOperator::Divide => code.extend_from_slice(&[0x99, 0xf7, 0xf9]), // cdq; idiv ecx
        BinaryOperator::Remainder => {
            code.extend_from_slice(&[0x99, 0xf7, 0xf9, 0x89, 0xd0]); // cdq; idiv; mov eax, edx
        }
        _ => unreachable!("value lowering only creates integer arithmetic"),
    }
    code
}

fn encode_wrapping_binary_value(
    operator: BinaryOperator,
    left: &Value,
    right: &Value,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
) -> Vec<u8> {
    let mut code = Vec::new();
    append_encoded_value(&mut code, left, in_function, relocations);
    emit_rsp_adjust(&mut code, true, 16);
    encode_rsp_store_rax(&mut code, 0);
    append_encoded_value(&mut code, right, in_function, relocations);
    code.extend_from_slice(&[0x48, 0x89, 0xc1]); // mov rcx, rax
    encode_rsp_load_rax(&mut code, 0);
    emit_rsp_adjust(&mut code, false, 16);
    match operator {
        BinaryOperator::Add => code.extend_from_slice(&[0x48, 0x01, 0xc8]), // add rax, rcx
        BinaryOperator::Subtract => code.extend_from_slice(&[0x48, 0x29, 0xc8]), // sub rax, rcx
        BinaryOperator::Multiply => code.extend_from_slice(&[0x48, 0x0f, 0xaf, 0xc1]), // imul rax, rcx
        BinaryOperator::BitXor => code.extend_from_slice(&[0x48, 0x31, 0xc8]), // xor rax, rcx
        BinaryOperator::Divide => {
            code.extend_from_slice(&[0x48, 0x31, 0xd2, 0x48, 0xf7, 0xf1]); // xor rdx, rdx; div rcx
        }
        BinaryOperator::Remainder => {
            code.extend_from_slice(&[0x48, 0x31, 0xd2, 0x48, 0xf7, 0xf1, 0x48, 0x89, 0xd0]); // div rcx; mov rax, rdx
        }
        _ => unreachable!("word arithmetic lowering filters arithmetic operators"),
    }
    code
}

fn is_word_value(value: &Value) -> bool {
    matches!(
        value,
        Value::Word(_)
            | Value::AddressOfLocal(_)
            | Value::TypedWord { .. }
            | Value::TypedWordParameter { .. }
            | Value::WordLocal(_)
            | Value::TypedWordLocal { .. }
            | Value::PairField { .. }
            | Value::SliceLength(_)
            | Value::ZeroExtend(_)
            | Value::SignExtend(_)
            | Value::SliceFold { .. }
            | Value::WrappingBinary { .. }
            | Value::Call {
                returns_word: true,
                ..
            }
    ) || matches!(
        value,
        Value::SliceElement {
            element_size: 8,
            ..
        }
    ) || matches!(
        value,
        Value::OptionUnwrapOr {
            payload_is_word: true,
            ..
        }
    ) || matches!(
        value,
        Value::If {
            then_value,
            else_value,
            ..
        } if is_word_value(then_value) && is_word_value(else_value)
    )
}

fn local_slot(value: &Value) -> Option<usize> {
    match value {
        Value::Local(slot) | Value::WordLocal(slot) => Some(*slot),
        Value::TypedWordLocal { slot, .. } => Some(*slot),
        _ => None,
    }
}

fn encode_condition_value(
    condition: &Condition,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
) -> Vec<u8> {
    let mut code = Vec::new();
    match condition {
        Condition::Boolean(value) => {
            code.extend_from_slice(&[0xb8]);
            code.extend_from_slice(&i32::from(*value).to_le_bytes());
        }
        Condition::Compare {
            operator,
            left,
            right,
        } => {
            append_encoded_value(&mut code, left, in_function, relocations);
            emit_rsp_adjust(&mut code, true, 16);
            let word_comparison = is_word_value(left) || is_word_value(right);
            if word_comparison {
                encode_rsp_store_rax(&mut code, 0);
            } else {
                encode_rsp_store_eax(&mut code, 0);
            }
            append_encoded_value(&mut code, right, in_function, relocations);
            if word_comparison {
                code.extend_from_slice(&[0x48, 0x89, 0xc1]); // mov rcx, rax
                encode_rsp_load_rax(&mut code, 0);
            } else {
                code.extend_from_slice(&[0x89, 0xc1]); // mov ecx, eax
                encode_rsp_load_eax(&mut code, 0);
            }
            emit_rsp_adjust(&mut code, false, 16);
            if word_comparison {
                code.extend_from_slice(&[0x48, 0x39, 0xc8]); // cmp rax, rcx
            } else {
                code.extend_from_slice(&[0x39, 0xc8]); // cmp eax, ecx
            }
            let set_opcode = if word_comparison {
                unsigned_set_opcode(*operator)
            } else {
                true_set_opcode(*operator)
            };
            code.extend_from_slice(&[0x0f, set_opcode, 0xc0]); // setcc al
            code.extend_from_slice(&[0x0f, 0xb6, 0xc0]); // movzx eax, al
        }
        Condition::And(left, right) | Condition::Or(left, right) => {
            append_encoded_condition(&mut code, left, in_function, relocations);
            code.extend_from_slice(&[0x85, 0xc0]); // test eax, eax
            let opcode = if matches!(condition, Condition::And(_, _)) {
                0x84 // jz: false lhs completes && without evaluating rhs
            } else {
                0x85 // jnz: true lhs completes || without evaluating rhs
            };
            let skip_rhs_displacement = code.len() + 2;
            code.extend_from_slice(&[0x0f, opcode, 0, 0, 0, 0]);
            append_encoded_condition(&mut code, right, in_function, relocations);
            let end = code.len();
            patch_rel32(&mut code, skip_rhs_displacement, end);
        }
    }
    code
}

fn append_encoded_condition(
    output: &mut Vec<u8>,
    condition: &Condition,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
) {
    let base = output.len() as u64;
    let first_relocation = relocations.len();
    output.extend(encode_condition_value(condition, in_function, relocations));
    for relocation in &mut relocations[first_relocation..] {
        relocation.offset += base;
    }
}

fn true_set_opcode(operator: BinaryOperator) -> u8 {
    match operator {
        BinaryOperator::Equal => 0x94,        // sete
        BinaryOperator::PatternEqual => 0x94, // sete
        BinaryOperator::NotEqual => 0x95,     // setne
        BinaryOperator::Less => 0x9c,         // setl
        BinaryOperator::LessEqual => 0x9e,    // setle
        BinaryOperator::Greater => 0x9f,      // setg
        BinaryOperator::GreaterEqual => 0x9d, // setge
        _ => unreachable!("condition parser restricts operators to comparisons"),
    }
}

fn unsigned_set_opcode(operator: BinaryOperator) -> u8 {
    match operator {
        BinaryOperator::Equal => 0x94,        // sete
        BinaryOperator::PatternEqual => 0x94, // sete
        BinaryOperator::NotEqual => 0x95,     // setne
        BinaryOperator::Less => 0x92,         // setb
        BinaryOperator::LessEqual => 0x96,    // setbe
        BinaryOperator::Greater => 0x97,      // seta
        BinaryOperator::GreaterEqual => 0x93, // setae
        _ => unreachable!("condition parser restricts operators to comparisons"),
    }
}

fn patch_rel32(code: &mut [u8], displacement_offset: usize, target_offset: usize) {
    let displacement = target_offset as i64 - (displacement_offset + 4) as i64;
    let displacement = i32::try_from(displacement).expect("local branch fits in rel32");
    code[displacement_offset..displacement_offset + 4].copy_from_slice(&displacement.to_le_bytes());
}

fn required_frame_slots(value: &Value, parameter_count: usize) -> usize {
    let mut slots = parameter_count;
    visit_value(value, &mut slots);
    slots
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PrintKind {
    Decimal,
    PanicDecimal,
    Hex16,
}

fn visit_print_statements(statements: &[Statement], visit: &mut impl FnMut(&PrintPart)) {
    for statement in statements {
        match statement {
            Statement::Println(parts) => {
                for part in parts {
                    visit(part);
                }
            }
            Statement::If {
                then_body,
                else_body,
                ..
            } => {
                visit_print_statements(then_body, visit);
                visit_print_statements(else_body, visit);
            }
            Statement::While { body, .. }
            | Statement::ForRange { body, .. }
            | Statement::ForSlice { body, .. } => {
                visit_print_statements(body, visit);
            }
            _ => {}
        }
    }
}

fn print_parts(value: &Value, visit: &mut impl FnMut(&PrintPart)) {
    if let Value::Block { statements, tail } = value {
        visit_print_statements(statements, visit);
        print_parts(tail, visit);
    }
}

fn collect_print_text(value: &Value, output: &mut HashSet<String>) {
    print_parts(value, &mut |part| {
        if let PrintPart::Text(text) | PrintPart::Panic(text) | PrintPart::PanicText(text) = part {
            output.insert(text.clone());
        }
    });
}

fn collect_value_text(value: &Value, output: &mut HashSet<String>) {
    match value {
        Value::BTreeMapGetString { key, .. } => collect_value_text(key, output),
        Value::TextSlice(text) => {
            output.insert(text.clone());
        }
        Value::Block { statements, tail } => {
            for statement in statements {
                match statement {
                    Statement::Evaluate(value)
                    | Statement::Let {
                        initializer: value, ..
                    }
                    | Statement::Assign { value, .. }
                    | Statement::AssignIndirect { value, .. }
                    | Statement::TryResult(value)
                    | Statement::TryOption(value)
                    | Statement::TryOptionBind { result: value, .. }
                    | Statement::Return(value) => collect_value_text(value, output),
                    Statement::VecPushString { value, .. } => collect_value_text(value, output),
                    Statement::StringAppendChar { character, .. } => {
                        collect_value_text(character, output)
                    }
                    Statement::BTreeMapInsert { key, value, .. } => {
                        collect_value_text(key, output);
                        collect_value_text(value, output);
                    }
                    Statement::VecPushAggregate { fields, .. } => {
                        for value in fields {
                            collect_value_text(value, output);
                        }
                    }
                    Statement::LetSlice { initializer, .. } => {
                        collect_value_text(initializer, output);
                    }
                    Statement::LetStruct { fields } => {
                        for (_, value) in fields {
                            collect_value_text(value, output);
                        }
                    }
                    Statement::LetTupleCall { initializer, .. } => {
                        collect_value_text(initializer, output);
                    }
                    Statement::Println(parts) => {
                        for part in parts {
                            match part {
                                PrintPart::Slice(value) | PrintPart::PanicDecimal(value) => {
                                    collect_value_text(value, output)
                                }
                                PrintPart::Panic(text) | PrintPart::PanicText(text) => {
                                    output.insert(text.clone());
                                }
                                _ => {}
                            }
                        }
                    }
                    Statement::Break | Statement::Continue => {}
                    Statement::If {
                        condition,
                        then_body,
                        else_body,
                    } => {
                        collect_condition_text(condition, output);
                        collect_statement_text(then_body, output);
                        collect_statement_text(else_body, output);
                    }
                    Statement::While { condition, body } => {
                        collect_condition_text(condition, output);
                        collect_statement_text(body, output);
                    }
                    Statement::ForRange { body, .. } => {
                        collect_statement_text(body, output);
                    }
                    Statement::ForSlice { body, .. } | Statement::ForChars { body, .. } => {
                        collect_statement_text(body, output)
                    }
                }
            }
            collect_value_text(tail, output);
        }
        Value::Tuple(values) => {
            for value in values {
                collect_value_text(value, output);
            }
        }
        Value::StaticArray(values) => {
            for value in values {
                collect_value_text(value, output);
            }
        }
        Value::PairField { aggregate, .. }
        | Value::Negate(aggregate)
        | Value::OwnedString(aggregate)
        | Value::ResultIsOk(aggregate)
        | Value::OptionIsSome(aggregate)
        | Value::SliceLength(aggregate)
        | Value::ZeroExtend(aggregate)
        | Value::SignExtend(aggregate)
        | Value::CharIsAsciiAlphanumeric(aggregate) => {
            collect_value_text(aggregate, output);
        }
        Value::SliceFold {
            source,
            initial,
            update,
            ..
        } => {
            collect_value_text(source, output);
            collect_value_text(initial, output);
            collect_value_text(update, output);
        }
        Value::TupleSliceFold {
            source,
            initial,
            updates,
            ..
        } => {
            collect_value_text(source, output);
            for value in initial.iter().chain(updates) {
                collect_value_text(value, output);
            }
        }
        Value::FormatString(pieces) => {
            for piece in pieces {
                match piece {
                    FormatPiece::Text(text) => {
                        output.insert(text.clone());
                    }
                    FormatPiece::Decimal(value) | FormatPiece::SignedDecimal(value) => {
                        collect_value_text(value, output)
                    }
                }
            }
        }
        Value::Call { arguments, .. } => {
            for argument in arguments {
                collect_value_text(argument, output);
            }
        }
        Value::Binary { left, right, .. } | Value::WrappingBinary { left, right, .. } => {
            collect_value_text(left, output);
            collect_value_text(right, output);
        }
        Value::OptionUnwrapOr {
            option, fallback, ..
        } => {
            collect_value_text(option, output);
            collect_value_text(fallback, output);
        }
        Value::If {
            condition,
            then_value,
            else_value,
        } => {
            collect_condition_text(condition, output);
            collect_value_text(then_value, output);
            collect_value_text(else_value, output);
        }
        Value::Integer(_)
        | Value::Word(_)
        | Value::TypedWord { .. }
        | Value::EmptySlice
        | Value::NicheOk
        | Value::EmptyCollection
        | Value::SliceParameter { .. }
        | Value::SliceLocal { .. }
        | Value::SliceRecord { .. }
        | Value::CharIterator { .. }
        | Value::AnyStringTrimEmpty { .. }
        | Value::SliceElementString { .. }
        | Value::SliceElement { .. }
        | Value::CollectionLocal { .. }
        | Value::CollectionAddress { .. }
        | Value::Parameter(_)
        | Value::TypedWordParameter { .. }
        | Value::Local(_)
        | Value::WordLocal(_)
        | Value::TypedWordLocal { .. }
        | Value::AddressOfLocal(_)
        | Value::IndirectLocal { .. }
        | Value::StructLocal { .. } => {}
        Value::StructReference { fields, .. } => {
            for field in fields {
                collect_value_text(field, output);
            }
        }
        Value::SliceIsEmpty(value) => collect_value_text(value, output),
    }
}

fn static_array_symbol_name(values: &[Value]) -> String {
    let mut name = String::from("__quill_array");
    for value in values {
        let fields = static_array_fields(value)
            .expect("static slice elements are validated as scalars or pairs");
        if fields.len() > 1 {
            name.push_str("_t");
        }
        for field in fields {
            match field {
                Value::Integer(number) => name.push_str(&format!("_i{number}")),
                Value::Word(number) => name.push_str(&format!("_u{number}")),
                Value::TypedWord { value, .. } => name.push_str(&format!("_u{value}")),
                Value::TextSlice(text) => {
                    name.push_str("_s");
                    name.push_str(&text_symbol_name(text));
                }
                _ => unreachable!("static slice fields are validated before symbol naming"),
            }
        }
    }
    name
}

fn collect_static_arrays(value: &Value, output: &mut HashMap<String, Vec<Value>>) {
    match value {
        Value::BTreeMapGetString { key, .. } => collect_static_arrays(key, output),
        Value::StaticArray(values) => {
            output.insert(static_array_symbol_name(values), values.clone());
        }
        Value::Block { statements, tail } => {
            for statement in statements {
                collect_static_array_statement(statement, output);
            }
            collect_static_arrays(tail, output);
        }
        Value::Tuple(values)
        | Value::Call {
            arguments: values, ..
        } => {
            for value in values {
                collect_static_arrays(value, output);
            }
        }
        Value::PairField { aggregate, .. }
        | Value::OwnedString(aggregate)
        | Value::ResultIsOk(aggregate)
        | Value::OptionIsSome(aggregate)
        | Value::SliceLength(aggregate)
        | Value::ZeroExtend(aggregate)
        | Value::SignExtend(aggregate)
        | Value::CharIsAsciiAlphanumeric(aggregate) => {
            collect_static_arrays(aggregate, output);
        }
        Value::SliceFold {
            source,
            initial,
            update,
            ..
        } => {
            collect_static_arrays(source, output);
            collect_static_arrays(initial, output);
            collect_static_arrays(update, output);
        }
        Value::TupleSliceFold {
            source,
            initial,
            updates,
            ..
        } => {
            collect_static_arrays(source, output);
            for value in initial.iter().chain(updates) {
                collect_static_arrays(value, output);
            }
        }
        Value::FormatString(pieces) => {
            for piece in pieces {
                if let FormatPiece::Decimal(value) | FormatPiece::SignedDecimal(value) = piece {
                    collect_static_arrays(value, output);
                }
            }
        }
        Value::Binary { left, right, .. } | Value::WrappingBinary { left, right, .. } => {
            collect_static_arrays(left, output);
            collect_static_arrays(right, output);
        }
        Value::OptionUnwrapOr {
            option, fallback, ..
        } => {
            collect_static_arrays(option, output);
            collect_static_arrays(fallback, output);
        }
        Value::Negate(value) => collect_static_arrays(value, output),
        Value::If {
            condition,
            then_value,
            else_value,
        } => {
            collect_static_array_condition(condition, output);
            collect_static_arrays(then_value, output);
            collect_static_arrays(else_value, output);
        }
        Value::Integer(_)
        | Value::Word(_)
        | Value::TypedWord { .. }
        | Value::EmptySlice
        | Value::NicheOk
        | Value::EmptyCollection
        | Value::TextSlice(_)
        | Value::SliceParameter { .. }
        | Value::SliceLocal { .. }
        | Value::SliceRecord { .. }
        | Value::CharIterator { .. }
        | Value::AnyStringTrimEmpty { .. }
        | Value::SliceElementString { .. }
        | Value::SliceElement { .. }
        | Value::CollectionLocal { .. }
        | Value::CollectionAddress { .. }
        | Value::Parameter(_)
        | Value::TypedWordParameter { .. }
        | Value::Local(_)
        | Value::WordLocal(_)
        | Value::TypedWordLocal { .. }
        | Value::AddressOfLocal(_)
        | Value::IndirectLocal { .. }
        | Value::StructLocal { .. } => {}
        Value::StructReference { fields, .. } => {
            for field in fields {
                collect_static_arrays(field, output);
            }
        }
        Value::SliceIsEmpty(value) => collect_static_arrays(value, output),
    }
}

fn collect_static_array_statement(statement: &Statement, output: &mut HashMap<String, Vec<Value>>) {
    match statement {
        Statement::Evaluate(value)
        | Statement::Let {
            initializer: value, ..
        }
        | Statement::LetSlice {
            initializer: value, ..
        }
        | Statement::Assign { value, .. }
        | Statement::AssignIndirect { value, .. }
        | Statement::TryResult(value)
        | Statement::TryOption(value)
        | Statement::TryOptionBind { result: value, .. }
        | Statement::Return(value) => collect_static_arrays(value, output),
        Statement::VecPushString { value, .. } => collect_static_arrays(value, output),
        Statement::StringAppendChar { character, .. } => collect_static_arrays(character, output),
        Statement::BTreeMapInsert { key, value, .. } => {
            collect_static_arrays(key, output);
            collect_static_arrays(value, output);
        }
        Statement::VecPushAggregate { fields, .. } => {
            for value in fields {
                collect_static_arrays(value, output);
            }
        }
        Statement::LetStruct { fields } => {
            for (_, value) in fields {
                collect_static_arrays(value, output);
            }
        }
        Statement::LetTupleCall { initializer, .. } => collect_static_arrays(initializer, output),
        Statement::If {
            condition,
            then_body,
            else_body,
        } => {
            collect_static_array_condition(condition, output);
            for statement in then_body.iter().chain(else_body) {
                collect_static_array_statement(statement, output);
            }
        }
        Statement::While { condition, body } => {
            collect_static_array_condition(condition, output);
            for statement in body {
                collect_static_array_statement(statement, output);
            }
        }
        Statement::ForRange { body, .. }
        | Statement::ForSlice { body, .. }
        | Statement::ForChars { body, .. } => {
            for statement in body {
                collect_static_array_statement(statement, output);
            }
        }
        Statement::Println(parts) => {
            for part in parts {
                match part {
                    PrintPart::Slice(value)
                    | PrintPart::Decimal(value)
                    | PrintPart::Hex16(value)
                    | PrintPart::PanicDecimal(value) => {
                        collect_static_arrays(value, output);
                    }
                    PrintPart::Text(_) | PrintPart::Panic(_) | PrintPart::PanicText(_) => {}
                }
            }
        }
        Statement::Break | Statement::Continue => {}
    }
}

fn collect_static_array_condition(condition: &Condition, output: &mut HashMap<String, Vec<Value>>) {
    match condition {
        Condition::Compare { left, right, .. } => {
            collect_static_arrays(left, output);
            collect_static_arrays(right, output);
        }
        Condition::And(left, right) | Condition::Or(left, right) => {
            collect_static_array_condition(left, output);
            collect_static_array_condition(right, output);
        }
        Condition::Boolean(_) => {}
    }
}

fn collect_statement_text(statements: &[Statement], output: &mut HashSet<String>) {
    for statement in statements {
        match statement {
            Statement::Evaluate(value)
            | Statement::Let {
                initializer: value, ..
            }
            | Statement::Assign { value, .. }
            | Statement::AssignIndirect { value, .. }
            | Statement::TryResult(value)
            | Statement::TryOption(value)
            | Statement::TryOptionBind { result: value, .. }
            | Statement::Return(value) => collect_value_text(value, output),
            Statement::VecPushString { value, .. } => collect_value_text(value, output),
            Statement::StringAppendChar { character, .. } => collect_value_text(character, output),
            Statement::BTreeMapInsert { key, value, .. } => {
                collect_value_text(key, output);
                collect_value_text(value, output);
            }
            Statement::VecPushAggregate { fields, .. } => {
                for value in fields {
                    collect_value_text(value, output);
                }
            }
            Statement::LetSlice { initializer, .. } => {
                collect_value_text(initializer, output);
            }
            Statement::LetStruct { fields } => {
                for (_, value) in fields {
                    collect_value_text(value, output);
                }
            }
            Statement::LetTupleCall { initializer, .. } => {
                collect_value_text(initializer, output);
            }
            Statement::If {
                condition,
                then_body,
                else_body,
            } => {
                collect_condition_text(condition, output);
                collect_statement_text(then_body, output);
                collect_statement_text(else_body, output);
            }
            Statement::While { condition, body } => {
                collect_condition_text(condition, output);
                collect_statement_text(body, output);
            }
            Statement::ForRange { body, .. } => {
                collect_statement_text(body, output);
            }
            Statement::ForSlice { body, .. } | Statement::ForChars { body, .. } => {
                collect_statement_text(body, output)
            }
            Statement::Println(_) | Statement::Break | Statement::Continue => {}
        }
    }
}

fn collect_condition_text(condition: &Condition, output: &mut HashSet<String>) {
    match condition {
        Condition::Compare { left, right, .. } => {
            collect_value_text(left, output);
            collect_value_text(right, output);
        }
        Condition::And(left, right) | Condition::Or(left, right) => {
            collect_condition_text(left, output);
            collect_condition_text(right, output);
        }
        Condition::Boolean(_) => {}
    }
}

fn has_print_kind(value: &Value, kind: PrintKind) -> bool {
    let mut found = false;
    print_parts(value, &mut |part| {
        found |= matches!(
            (kind, part),
            (PrintKind::Decimal, PrintPart::Decimal(_))
                | (PrintKind::Hex16, PrintPart::Hex16(_))
                | (PrintKind::PanicDecimal, PrintPart::PanicDecimal(_))
        );
    });
    found
}

fn contains_format_decimal(value: &Value) -> bool {
    match value {
        Value::FormatString(pieces) => pieces.iter().any(|piece| {
            matches!(
                piece,
                FormatPiece::Decimal(_) | FormatPiece::SignedDecimal(_)
            )
        }),
        Value::OwnedString(value)
        | Value::PairField {
            aggregate: value, ..
        }
        | Value::ResultIsOk(value)
        | Value::OptionIsSome(value)
        | Value::SliceLength(value)
        | Value::ZeroExtend(value)
        | Value::SignExtend(value)
        | Value::CharIsAsciiAlphanumeric(value) => contains_format_decimal(value),
        Value::OptionUnwrapOr {
            option, fallback, ..
        } => contains_format_decimal(option) || contains_format_decimal(fallback),
        Value::SliceFold {
            source,
            initial,
            update,
            ..
        } => {
            contains_format_decimal(source)
                || contains_format_decimal(initial)
                || contains_format_decimal(update)
        }
        Value::TupleSliceFold {
            source,
            initial,
            updates,
            ..
        } => {
            contains_format_decimal(source)
                || initial.iter().chain(updates).any(contains_format_decimal)
        }
        Value::Block { statements, tail } => {
            statements.iter().any(statement_contains_format_decimal)
                || contains_format_decimal(tail)
        }
        Value::Tuple(values)
        | Value::Call {
            arguments: values, ..
        } => values.iter().any(contains_format_decimal),
        Value::Binary { left, right, .. } | Value::WrappingBinary { left, right, .. } => {
            contains_format_decimal(left) || contains_format_decimal(right)
        }
        Value::Negate(value) => contains_format_decimal(value),
        Value::If {
            condition,
            then_value,
            else_value,
        } => {
            condition_contains_format_decimal(condition)
                || contains_format_decimal(then_value)
                || contains_format_decimal(else_value)
        }
        Value::Integer(_)
        | Value::Word(_)
        | Value::TypedWord { .. }
        | Value::EmptySlice
        | Value::NicheOk
        | Value::EmptyCollection
        | Value::TextSlice(_)
        | Value::StaticArray(_)
        | Value::SliceParameter { .. }
        | Value::SliceLocal { .. }
        | Value::SliceRecord { .. }
        | Value::CharIterator { .. }
        | Value::AnyStringTrimEmpty { .. }
        | Value::SliceElementString { .. }
        | Value::SliceElement { .. }
        | Value::CollectionLocal { .. }
        | Value::CollectionAddress { .. }
        | Value::Parameter(_)
        | Value::TypedWordParameter { .. }
        | Value::Local(_)
        | Value::WordLocal(_)
        | Value::TypedWordLocal { .. }
        | Value::AddressOfLocal(_)
        | Value::IndirectLocal { .. }
        | Value::StructLocal { .. } => false,
        Value::BTreeMapGetString { key, .. } => contains_format_decimal(key),
        Value::StructReference { fields, .. } => fields.iter().any(contains_format_decimal),
        Value::SliceIsEmpty(value) => contains_format_decimal(value),
    }
}

fn statement_contains_format_decimal(statement: &Statement) -> bool {
    match statement {
        Statement::Evaluate(value)
        | Statement::Let {
            initializer: value, ..
        }
        | Statement::LetSlice {
            initializer: value, ..
        }
        | Statement::Assign { value, .. }
        | Statement::AssignIndirect { value, .. }
        | Statement::TryResult(value)
        | Statement::TryOption(value)
        | Statement::TryOptionBind { result: value, .. }
        | Statement::Return(value) => contains_format_decimal(value),
        Statement::VecPushString { value, .. } => contains_format_decimal(value),
        Statement::StringAppendChar { character, .. } => contains_format_decimal(character),
        Statement::BTreeMapInsert { key, value, .. } => {
            contains_format_decimal(key) || contains_format_decimal(value)
        }
        Statement::VecPushAggregate { fields, .. } => fields.iter().any(contains_format_decimal),
        Statement::LetStruct { fields } => fields
            .iter()
            .any(|(_, value)| contains_format_decimal(value)),
        Statement::LetTupleCall { initializer, .. } => contains_format_decimal(initializer),
        Statement::If {
            condition,
            then_body,
            else_body,
        } => {
            condition_contains_format_decimal(condition)
                || then_body.iter().any(statement_contains_format_decimal)
                || else_body.iter().any(statement_contains_format_decimal)
        }
        Statement::While { condition, body } => {
            condition_contains_format_decimal(condition)
                || body.iter().any(statement_contains_format_decimal)
        }
        Statement::ForRange { body, .. } => body.iter().any(statement_contains_format_decimal),
        Statement::ForSlice { body, .. } | Statement::ForChars { body, .. } => {
            body.iter().any(statement_contains_format_decimal)
        }
        Statement::Println(_) | Statement::Break | Statement::Continue => false,
    }
}

fn condition_contains_format_decimal(condition: &Condition) -> bool {
    match condition {
        Condition::Compare { left, right, .. } => {
            contains_format_decimal(left) || contains_format_decimal(right)
        }
        Condition::And(left, right) | Condition::Or(left, right) => {
            condition_contains_format_decimal(left) || condition_contains_format_decimal(right)
        }
        Condition::Boolean(_) => false,
    }
}

fn value_uses_character_iteration(value: &Value) -> bool {
    match value {
        Value::AnyStringTrimEmpty { .. } => true,
        Value::Block { statements, tail } => {
            statements.iter().any(statement_uses_character_iteration)
                || value_uses_character_iteration(tail)
        }
        Value::Tuple(values)
        | Value::Call {
            arguments: values, ..
        } => values.iter().any(value_uses_character_iteration),
        Value::If {
            then_value,
            else_value,
            ..
        } => {
            value_uses_character_iteration(then_value) || value_uses_character_iteration(else_value)
        }
        _ => false,
    }
}

fn statement_uses_character_iteration(statement: &Statement) -> bool {
    match statement {
        Statement::ForChars { .. } => true,
        Statement::If {
            then_body,
            else_body,
            ..
        } => {
            then_body.iter().any(statement_uses_character_iteration)
                || else_body.iter().any(statement_uses_character_iteration)
        }
        Statement::While { body, .. }
        | Statement::ForRange { body, .. }
        | Statement::ForSlice { body, .. } => body.iter().any(statement_uses_character_iteration),
        _ => false,
    }
}

fn text_symbol_name(text: &str) -> String {
    let mut name = String::from("__quill_text_");
    for byte in text.bytes() {
        use std::fmt::Write as _;
        write!(name, "{byte:02x}").expect("writing into a String cannot fail");
    }
    name
}

fn add_internal_function_symbol(object: &mut Object<'_>, name: &str) -> object::write::SymbolId {
    object.add_symbol(Symbol {
        name: name.as_bytes().to_vec(),
        value: 0,
        size: 0,
        kind: SymbolKind::Text,
        scope: SymbolScope::Compilation,
        weak: false,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    })
}

/// Stable ELF name shared by separately compiled Quill crate units.
pub fn quill_export_symbol(definition_path: &str) -> String {
    quill_hir::exported_symbol(definition_path)
}

fn utf8_next_character_helper() -> Vec<u8> {
    // SysV ABI: rdi = UTF-8 base pointer, rsi = byte-offset pointer; eax is the
    // decoded Unicode scalar and the offset advances by its encoded width.
    let mut code = vec![
        0x48, 0x8b, 0x0e, // mov rcx, [rsi]
        0x48, 0x8d, 0x14, 0x0f, // lea rdx, [rdi + rcx]
        0x0f, 0xb6, 0x02, // movzx eax, byte [rdx]
        0x3c, 0x80, // cmp al, 0x80
        0x0f, 0x82, // jb ASCII
    ];
    let ascii_branch = code.len();
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[
        0x3c, 0xe0, // cmp al, 0xe0
        0x0f, 0x82, // jb two-byte
    ]);
    let two_branch = code.len();
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[
        0x3c, 0xf0, // cmp al, 0xf0
        0x0f, 0x82, // jb three-byte
    ]);
    let three_branch = code.len();
    code.extend_from_slice(&[0; 4]);

    // Four-byte scalar.
    code.extend_from_slice(&[
        0x83, 0xe0, 0x07, // eax &= 7
        0xc1, 0xe0, 0x12, // eax <<= 18
        0x0f, 0xb6, 0x4a, 0x01, // ecx = byte [rdx + 1]
        0x83, 0xe1, 0x3f, // ecx &= 0x3f
        0xc1, 0xe1, 0x0c, // ecx <<= 12
        0x09, 0xc8, // eax |= ecx
        0x0f, 0xb6, 0x52, 0x02, // edx = byte [rdx + 2]
        0x83, 0xe2, 0x3f, // edx &= 0x3f
        0xc1, 0xe2, 0x06, // edx <<= 6
        0x09, 0xd0, // eax |= edx
    ]);
    // Reload the original byte pointer from base + current byte offset. The
    // four-byte tail uses r8d so the accumulated code point in eax survives.
    code.extend_from_slice(&[
        0x48, 0x8b, 0x0e, // rcx = current offset
        0x48, 0x8d, 0x14, 0x0f, // rdx = current byte pointer
        0x44, 0x0f, 0xb6, 0x42, 0x03, // r8d = byte [rdx + 3]
        0x41, 0x83, 0xe0, 0x3f, // r8d &= 0x3f
        0x44, 0x09, 0xc0, // eax |= r8d
        0x48, 0x83, 0x06, 0x04, // offset += 4
        0xc3,
    ]);
    let three_byte = code.len();
    code.extend_from_slice(&[
        0x83, 0xe0, 0x0f, // eax &= 0xf
        0xc1, 0xe0, 0x0c, // eax <<= 12
        0x0f, 0xb6, 0x4a, 0x01, // ecx = byte [rdx + 1]
        0x83, 0xe1, 0x3f, // ecx &= 0x3f
        0xc1, 0xe1, 0x06, // ecx <<= 6
        0x09, 0xc8, // eax |= ecx
        0x0f, 0xb6, 0x52, 0x02, // edx = byte [rdx + 2]
        0x83, 0xe2, 0x3f, // edx &= 0x3f
        0x09, 0xd0, // eax |= edx
        0x48, 0x83, 0x06, 0x03, // offset += 3
        0xc3,
    ]);
    let two_byte = code.len();
    code.extend_from_slice(&[
        0x83, 0xe0, 0x1f, // eax &= 0x1f
        0xc1, 0xe0, 0x06, // eax <<= 6
        0x0f, 0xb6, 0x4a, 0x01, // ecx = byte [rdx + 1]
        0x83, 0xe1, 0x3f, // ecx &= 0x3f
        0x09, 0xc8, // eax |= ecx
        0x48, 0x83, 0x06, 0x02, // offset += 2
        0xc3,
    ]);
    let ascii = code.len();
    code.extend_from_slice(&[
        0x48, 0x83, 0x06, 0x01, // offset += 1
        0xc3,
    ]);
    patch_rel32(&mut code, ascii_branch, ascii);
    patch_rel32(&mut code, two_branch, two_byte);
    patch_rel32(&mut code, three_branch, three_byte);
    code
}

fn char_is_whitespace_helper() -> Vec<u8> {
    let mut code = Vec::new();
    let mut below_ascii_range = Vec::new();
    let mut at_most_ascii_range = Vec::new();
    let mut equal_whitespace = Vec::new();
    code.extend_from_slice(&[0x3d, 9, 0, 0, 0, 0x0f, 0x82]);
    below_ascii_range.push(code.len());
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x3d, 13, 0, 0, 0, 0x0f, 0x86]);
    at_most_ascii_range.push(code.len());
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x3d, 32, 0, 0, 0, 0x0f, 0x84]);
    equal_whitespace.push(code.len());
    code.extend_from_slice(&[0; 4]);

    for value in [0x85u32, 0xa0, 0x1680] {
        code.push(0x3d);
        code.extend_from_slice(&value.to_le_bytes());
        code.extend_from_slice(&[0x0f, 0x84]);
        equal_whitespace.push(code.len());
        code.extend_from_slice(&[0; 4]);
    }
    code.extend_from_slice(&[0x3d]);
    code.extend_from_slice(&0x2000u32.to_le_bytes());
    code.extend_from_slice(&[0x0f, 0x82]);
    below_ascii_range.push(code.len());
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x3d]);
    code.extend_from_slice(&0x200au32.to_le_bytes());
    code.extend_from_slice(&[0x0f, 0x86]);
    at_most_ascii_range.push(code.len());
    code.extend_from_slice(&[0; 4]);
    for value in [0x2028u32, 0x2029, 0x202f, 0x205f, 0x3000] {
        code.push(0x3d);
        code.extend_from_slice(&value.to_le_bytes());
        code.extend_from_slice(&[0x0f, 0x84]);
        equal_whitespace.push(code.len());
        code.extend_from_slice(&[0; 4]);
    }

    code.extend_from_slice(&[0x31, 0xc0, 0xc3]);
    let whitespace = code.len();
    code.extend_from_slice(&[0xb8, 1, 0, 0, 0, 0xc3]);
    let next = code.len();
    for displacement in below_ascii_range {
        patch_rel32(&mut code, displacement, next);
    }
    let yes = whitespace;
    for displacement in at_most_ascii_range.into_iter().chain(equal_whitespace) {
        patch_rel32(&mut code, displacement, yes);
    }
    code
}

fn value_has_pair_return(value: &Value) -> bool {
    match value {
        Value::Tuple(values) => values.len() == 2,
        Value::TupleSliceFold { .. } => true,
        Value::Block { tail, .. } => {
            matches!(tail.as_ref(), Value::Tuple(values) if values.len() == 2)
                || matches!(tail.as_ref(), Value::TupleSliceFold { .. })
        }
        _ => false,
    }
}

fn encode_store_rdx_to_rcx(output: &mut Vec<u8>, displacement: i32, wide: bool) {
    if wide {
        output.push(0x48);
    }
    if displacement == 0 {
        output.extend_from_slice(&[0x89, 0x11]);
    } else if i8::try_from(displacement).is_ok() {
        output.extend_from_slice(&[0x89, 0x51]);
        output.push(displacement as i8 as u8);
    } else {
        output.extend_from_slice(&[0x89, 0x91]);
        output.extend_from_slice(&displacement.to_le_bytes());
    }
}

fn encode_store_r8_to_rcx(output: &mut Vec<u8>, displacement: i32) {
    encode_extended_register_store(output, displacement, 0x00);
}

fn encode_store_r9_to_rcx(output: &mut Vec<u8>, displacement: i32) {
    encode_extended_register_store(output, displacement, 0x08);
}

fn encode_store_rax_to_rcx(output: &mut Vec<u8>, displacement: i32) {
    if displacement == 0 {
        output.extend_from_slice(&[0x48, 0x89, 0x01]);
    } else if i8::try_from(displacement).is_ok() {
        output.extend_from_slice(&[0x48, 0x89, 0x41, displacement as i8 as u8]);
    } else {
        output.extend_from_slice(&[0x48, 0x89, 0x81]);
        output.extend_from_slice(&displacement.to_le_bytes());
    }
}

fn encode_extended_register_store(output: &mut Vec<u8>, displacement: i32, reg_bits: u8) {
    if displacement == 0 {
        output.extend_from_slice(&[0x4c, 0x89, 0x01 | reg_bits]);
    } else if i8::try_from(displacement).is_ok() {
        output.extend_from_slice(&[0x4c, 0x89, 0x41 | reg_bits, displacement as i8 as u8]);
    } else {
        output.extend_from_slice(&[0x4c, 0x89, 0x81 | reg_bits]);
        output.extend_from_slice(&displacement.to_le_bytes());
    }
}

fn append_encoded_helper_return(
    output: &mut Vec<u8>,
    value: &Value,
    pair_scratch_slot: Option<usize>,
    aggregate_pointer_slot: Option<usize>,
    relocations: &mut Vec<PendingRelocation>,
) {
    let (statements, tail) = match value {
        Value::Block { statements, tail } => (statements.as_slice(), tail.as_ref()),
        other => (&[][..], other),
    };
    let mut loops = Vec::new();
    for statement in statements {
        encode_statement_into(output, statement, true, relocations, &mut loops);
    }
    if matches!(tail, Value::TupleSliceFold { .. }) {
        append_encoded_value(output, tail, true, relocations);
        return;
    }
    if let Value::Tuple(values) = tail {
        if let Some(pointer_slot) = aggregate_pointer_slot {
            let pointer_displacement = -8 * (pointer_slot as i32 + 1);
            let mut word_offset = 0i32;
            for field in values {
                append_encoded_value(output, field, true, relocations);
                let displacement = -word_offset * 8;
                if matches!(field, Value::EmptyCollection) {
                    output.extend_from_slice(&[0x49, 0x89, 0xc0]); // mov r8, rax
                    output.extend_from_slice(&[0x49, 0x89, 0xd1]); // mov r9, rdx
                    encode_rbp_load_rax(output, pointer_displacement);
                    output.extend_from_slice(&[0x48, 0x89, 0xc1]); // mov rcx, rax
                    encode_store_r8_to_rcx(output, displacement);
                    encode_store_r9_to_rcx(output, displacement - 8);
                    output.extend_from_slice(&[0x31, 0xc0]); // zero capacity
                    encode_store_rax_to_rcx(output, displacement - 16);
                    word_offset += 3;
                    continue;
                }
                if let Value::CollectionLocal { capacity_slot, .. } = field {
                    output.extend_from_slice(&[0x49, 0x89, 0xc0]); // preserve vector pointer
                    output.extend_from_slice(&[0x49, 0x89, 0xd1]); // preserve vector length
                    encode_rbp_load_rax(output, -8 * (*capacity_slot as i32 + 1));
                    encode_rbp_load_rcx(output, pointer_displacement);
                    encode_store_r8_to_rcx(output, displacement);
                    encode_store_r9_to_rcx(output, displacement - 8);
                    encode_store_rax_to_rcx(output, displacement - 16);
                    word_offset += 3;
                    continue;
                }
                if is_slice_value(field) {
                    output.extend_from_slice(&[0x49, 0x89, 0xc0]); // mov r8, rax
                    output.extend_from_slice(&[0x49, 0x89, 0xd1]); // mov r9, rdx
                    encode_rbp_load_rax(output, pointer_displacement);
                    output.extend_from_slice(&[0x48, 0x89, 0xc1]); // mov rcx, rax
                    encode_store_r8_to_rcx(output, displacement);
                    encode_store_r9_to_rcx(output, displacement - 8);
                    word_offset += 2;
                    continue;
                }
                if is_word_value(field) {
                    output.extend_from_slice(&[0x48, 0x89, 0xc2]); // mov rdx, rax
                } else {
                    output.extend_from_slice(&[0x89, 0xc2]); // mov edx, eax
                }
                encode_rbp_load_rax(output, pointer_displacement);
                output.extend_from_slice(&[0x48, 0x89, 0xc1]); // mov rcx, rax
                if is_word_value(field) {
                    encode_store_rdx_to_rcx(output, displacement, true);
                } else if displacement == 0 {
                    output.extend_from_slice(&[0x89, 0x11]); // mov [rcx], edx
                } else {
                    encode_store_rdx_to_rcx(output, displacement, false);
                }
                word_offset += 1;
            }
            output.extend_from_slice(&[0x31, 0xc0]); // return unit
            return;
        }
        debug_assert_eq!(values.len(), 2);
        let scratch = pair_scratch_slot.expect("pair return reserves a scratch frame slot");
        append_encoded_value(output, &values[0], true, relocations);
        let displacement = -8 * (scratch as i32 + 1);
        if is_word_value(&values[0]) {
            encode_rbp_store_rax(output, displacement);
        } else {
            encode_rbp_store_eax(output, displacement);
        }
        append_encoded_value(output, &values[1], true, relocations);
        if is_word_value(&values[1]) {
            output.extend_from_slice(&[0x48, 0x89, 0xc2]); // mov rdx, rax
        } else {
            output.extend_from_slice(&[0x89, 0xc2]); // mov edx, eax
        }
        if is_word_value(&values[0]) {
            encode_rbp_load_rax(output, displacement);
        } else {
            encode_rbp_load_eax(output, displacement);
        }
    } else {
        append_encoded_value(output, tail, true, relocations);
    }
}

fn visit_value(value: &Value, max_slot: &mut usize) {
    match value {
        Value::Local(slot) | Value::WordLocal(slot) => *max_slot = (*max_slot).max(slot + 1),
        Value::TypedWordLocal { slot, .. } => *max_slot = (*max_slot).max(slot + 1),
        Value::AddressOfLocal(slot) => *max_slot = (*max_slot).max(slot + 1),
        Value::IndirectLocal { pointer_slot, .. } => *max_slot = (*max_slot).max(pointer_slot + 1),
        Value::Block { statements, tail } => {
            for statement in statements {
                visit_statement(statement, max_slot);
            }
            visit_value(tail, max_slot);
        }
        Value::Tuple(values) => {
            for value in values {
                visit_value(value, max_slot);
            }
        }
        Value::OptionUnwrapOr {
            option, fallback, ..
        } => {
            visit_value(option, max_slot);
            visit_value(fallback, max_slot);
        }
        Value::PairField { aggregate, .. } | Value::OwnedString(aggregate) => {
            visit_value(aggregate, max_slot)
        }
        Value::BTreeMapGetString { pointer_slot, length_slot, key } => {
            *max_slot = (*max_slot).max(pointer_slot + 1).max(length_slot + 1);
            visit_value(key, max_slot);
        }
        Value::ResultIsOk(result)
        | Value::OptionIsSome(result)
        | Value::SliceIsEmpty(result)
        | Value::SliceLength(result)
        | Value::ZeroExtend(result)
        | Value::SignExtend(result)
        | Value::CharIsAsciiAlphanumeric(result) => visit_value(result, max_slot),
        Value::SliceFold {
            source,
            initial,
            update,
            pointer_slot,
            length_slot,
            index_slot,
            accumulator_slot,
            item_slot,
            ..
        } => {
            *max_slot = (*max_slot)
                .max(pointer_slot + 1)
                .max(length_slot + 1)
                .max(index_slot + 1)
                .max(accumulator_slot + 1)
                .max(item_slot + 1);
            visit_value(source, max_slot);
            visit_value(initial, max_slot);
            visit_value(update, max_slot);
        }
        Value::TupleSliceFold {
            source,
            initial,
            updates,
            pointer_slot,
            length_slot,
            index_slot,
            accumulator_slots,
            update_slots,
            item_slot,
            record_fields,
            ..
        } => {
            for slot in [pointer_slot, length_slot, index_slot, item_slot]
                .into_iter()
                .chain(accumulator_slots)
                .chain(update_slots)
            {
                *max_slot = (*max_slot).max(slot + 1);
            }
            visit_value(source, max_slot);
            for value in initial.iter().chain(updates) {
                visit_value(value, max_slot);
            }
            for (slot, initializer, width) in record_fields {
                *max_slot = (*max_slot).max(slot + width);
                visit_value(initializer, max_slot);
            }
        }
        Value::StructReference { fields, .. } => {
            for field in fields {
                visit_value(field, max_slot);
            }
        }
        Value::FormatString(pieces) => {
            for piece in pieces {
                if let FormatPiece::Decimal(value) | FormatPiece::SignedDecimal(value) = piece {
                    visit_value(value, max_slot);
                }
            }
        }
        Value::Call { arguments, .. } => {
            for value in arguments {
                visit_value(value, max_slot);
            }
        }
        Value::Binary { left, right, .. } => {
            visit_value(left, max_slot);
            visit_value(right, max_slot);
        }
        Value::WrappingBinary { left, right, .. } => {
            visit_value(left, max_slot);
            visit_value(right, max_slot);
        }
        Value::Negate(operand) => visit_value(operand, max_slot),
        Value::SliceElement {
            pointer_slot,
            index_slot,
            ..
        } => {
            *max_slot = (*max_slot).max(pointer_slot + 1).max(index_slot + 1);
        }
        Value::SliceElementString {
            pointer_slot,
            index_slot,
            ..
        } => {
            *max_slot = (*max_slot).max(pointer_slot + 1).max(index_slot + 1);
        }
        Value::If {
            condition,
            then_value,
            else_value,
        } => {
            visit_condition(condition, max_slot);
            visit_value(then_value, max_slot);
            visit_value(else_value, max_slot);
        }
        Value::Integer(_)
        | Value::Word(_)
        | Value::TypedWord { .. }
        | Value::EmptySlice
        | Value::NicheOk
        | Value::EmptyCollection
        | Value::TextSlice(_)
        | Value::StaticArray(_)
        | Value::Parameter(_)
        | Value::TypedWordParameter { .. }
        | Value::StructLocal { .. } => {}
        Value::SliceParameter {
            pointer_slot,
            length_slot,
            ..
        } => {
            *max_slot = (*max_slot).max(pointer_slot + 1).max(length_slot + 1);
        }
        Value::SliceLocal {
            pointer_slot,
            length_slot,
            ..
        } => {
            *max_slot = (*max_slot).max(pointer_slot + 1).max(length_slot + 1);
        }
        Value::SliceRecord {
            pointer_slot,
            length_slot,
            ..
        } => {
            *max_slot = (*max_slot).max(pointer_slot + 1).max(length_slot + 1);
        }
        Value::CharIterator {
            pointer_slot,
            length_slot,
        } => {
            *max_slot = (*max_slot).max(pointer_slot + 1).max(length_slot + 1);
        }
        Value::AnyStringTrimEmpty {
            pointer_slot,
            length_slot,
        } => {
            *max_slot = (*max_slot).max(pointer_slot + 1).max(length_slot + 1);
        }
        Value::CollectionLocal {
            pointer_slot,
            length_slot,
            capacity_slot,
            ..
        } => {
            *max_slot = (*max_slot)
                .max(pointer_slot + 1)
                .max(length_slot + 1)
                .max(capacity_slot + 1);
        }
        Value::CollectionAddress {
            pointer_slot,
            length_slot,
            capacity_slot,
            ..
        } => {
            *max_slot = (*max_slot)
                .max(pointer_slot + 1)
                .max(length_slot + 1)
                .max(capacity_slot + 1);
        }
    }
}

fn visit_statement(statement: &Statement, max_slot: &mut usize) {
    match statement {
        Statement::Println(parts) => {
            for part in parts {
                match part {
                    PrintPart::Slice(value)
                    | PrintPart::Decimal(value)
                    | PrintPart::Hex16(value)
                    | PrintPart::PanicDecimal(value) => {
                        visit_value(value, max_slot);
                    }
                    PrintPart::Text(_) | PrintPart::Panic(_) | PrintPart::PanicText(_) => {}
                }
            }
        }
        Statement::Evaluate(value) => visit_value(value, max_slot),
        Statement::VecPushString {
            pointer_slot,
            length_slot,
            capacity_slot,
            value,
            ..
        } => {
            *max_slot = (*max_slot)
                .max(pointer_slot + 1)
                .max(length_slot + 1)
                .max(capacity_slot + 1);
            visit_value(value, max_slot);
        }
        Statement::StringAppendChar {
            pointer_slot,
            length_slot,
            capacity_slot,
            character,
            ..
        } => {
            *max_slot = (*max_slot)
                .max(pointer_slot + 1)
                .max(length_slot + 1)
                .max(capacity_slot + 1);
            visit_value(character, max_slot);
        }
        Statement::BTreeMapInsert {
            pointer_slot,
            length_slot,
            capacity_slot,
            key,
            value,
            ..
        } => {
            *max_slot = (*max_slot)
                .max(pointer_slot + 1)
                .max(length_slot + 1)
                .max(capacity_slot + 1);
            visit_value(key, max_slot);
            visit_value(value, max_slot);
        }
        Statement::VecPushAggregate {
            pointer_slot,
            length_slot,
            capacity_slot,
            fields,
            ..
        } => {
            *max_slot = (*max_slot)
                .max(pointer_slot + 1)
                .max(length_slot + 1)
                .max(capacity_slot + 1);
            for value in fields {
                visit_value(value, max_slot);
            }
        }
        Statement::Let { slot, initializer } => {
            *max_slot = (*max_slot).max(slot + 1);
            visit_value(initializer, max_slot);
        }
        Statement::LetSlice {
            pointer_slot,
            length_slot,
            initializer,
        } => {
            *max_slot = (*max_slot).max(pointer_slot + 1).max(length_slot + 1);
            visit_value(initializer, max_slot);
        }
        Statement::LetStruct { fields } => {
            for (slot, value) in fields {
                let width = if matches!(value, Value::EmptyCollection) {
                    3
                } else if is_slice_value(value) {
                    2
                } else {
                    1
                };
                *max_slot = (*max_slot).max(slot + width);
                visit_value(value, max_slot);
            }
        }
        Statement::LetTupleCall {
            fields,
            initializer,
        } => {
            for (slot, _) in fields {
                *max_slot = (*max_slot).max(slot + 1);
            }
            visit_value(initializer, max_slot);
        }
        Statement::Assign { slot, value } => {
            *max_slot = (*max_slot).max(slot + 1);
            visit_value(value, max_slot);
        }
        Statement::AssignIndirect {
            pointer_slot,
            value,
            ..
        } => {
            *max_slot = (*max_slot).max(pointer_slot + 1);
            visit_value(value, max_slot);
        }
        Statement::If {
            then_body,
            else_body,
            ..
        } => {
            for statement in then_body.iter().chain(else_body) {
                visit_statement(statement, max_slot);
            }
        }
        Statement::While { condition, body } => {
            visit_condition(condition, max_slot);
            for statement in body {
                visit_statement(statement, max_slot);
            }
        }
        Statement::ForSlice {
            length_slot,
            index_slot,
            body,
        } => {
            *max_slot = (*max_slot).max(length_slot + 1).max(index_slot + 1);
            for statement in body {
                visit_statement(statement, max_slot);
            }
        }
        Statement::ForChars {
            pointer_slot,
            length_slot,
            index_slot,
            item_slot,
            body,
        } => {
            *max_slot = (*max_slot)
                .max(pointer_slot + 1)
                .max(length_slot + 1)
                .max(index_slot + 1)
                .max(item_slot + 1);
            for statement in body {
                visit_statement(statement, max_slot);
            }
        }
        Statement::ForRange {
            slot,
            end_slot,
            body,
            ..
        } => {
            *max_slot = (*max_slot).max(slot + 1).max(end_slot + 1);
            for statement in body {
                visit_statement(statement, max_slot);
            }
        }
        Statement::Break | Statement::Continue => {}
        Statement::TryResult(value) | Statement::TryOption(value) | Statement::Return(value) => {
            visit_value(value, max_slot)
        }
        Statement::TryOptionBind { result, slot, .. } => {
            *max_slot = (*max_slot).max(slot + 1);
            visit_value(result, max_slot);
        }
    }
}

fn visit_condition(condition: &Condition, max_slot: &mut usize) {
    match condition {
        Condition::Compare { left, right, .. } => {
            visit_value(left, max_slot);
            visit_value(right, max_slot);
        }
        Condition::And(left, right) | Condition::Or(left, right) => {
            visit_condition(left, max_slot);
            visit_condition(right, max_slot);
        }
        Condition::Boolean(_) => {}
    }
}

fn print_decimal_helper(file_descriptor: u8) -> Vec<u8> {
    let mut code = vec![
        0x55, // push rbp
        0x48, 0x89, 0xe5, // mov rbp, rsp
        0x48, 0x83, 0xec, 0x20, // sub rsp, 32
        0x48, 0x89, 0xf8, // mov rax, rdi
        0x48, 0x8d, 0x75, 0xff, // lea rsi, [rbp - 1]
        0x31, 0xc9, // xor ecx, ecx
    ];
    let loop_start = code.len();
    code.extend_from_slice(&[
        0x31, 0xd2, // xor edx, edx
        0x41, 0xb8, 10, 0, 0, 0, // mov r8d, 10
        0x49, 0xf7, 0xf0, // div r8
        0x80, 0xc2, b'0', // add dl, '0'
        0x88, 0x16, // mov byte ptr [rsi], dl
        0x48, 0xff, 0xce, // dec rsi
        0xff, 0xc1, // inc ecx
        0x48, 0x85, 0xc0, // test rax, rax
        0x0f, 0x85,
    ]);
    let loop_displacement = code.len();
    code.extend_from_slice(&[0; 4]);
    patch_rel32(&mut code, loop_displacement, loop_start);
    let file_descriptor_offset = code.len() + 6;
    code.extend_from_slice(&[
        0x48, 0xff, 0xc6, // inc rsi
        0x89, 0xca, // mov edx, ecx
        0xbf, 1, 0, 0, 0, // mov edi, file descriptor
        0xb8, 1, 0, 0, 0, // mov eax, 1
        0x0f, 0x05, // syscall
        0xc9, 0xc3, // leave; ret
    ]);
    code[file_descriptor_offset] = file_descriptor;
    code
}

fn format_decimal_helper() -> Vec<u8> {
    let mut code = vec![
        0x55, // push rbp
        0x48, 0x89, 0xe5, // mov rbp, rsp
        0x48, 0x83, 0xec, 0x30, // sub rsp, 48
        0x48, 0x89, 0x7d, 0xe0, // save destination at [rbp - 32]
        0x48, 0x89, 0xf0, // mov rax, rsi: value
        0x48, 0x8d, 0x75, 0xff, // lea rsi, [rbp - 1]
        0x31, 0xc9, // xor ecx, ecx
    ];
    let loop_start = code.len();
    code.extend_from_slice(&[
        0x31, 0xd2, // xor edx, edx
        0x41, 0xb8, 10, 0, 0, 0, // mov r8d, 10
        0x49, 0xf7, 0xf0, // div r8
        0x80, 0xc2, b'0', // add dl, '0'
        0x88, 0x16, // mov byte ptr [rsi], dl
        0x48, 0xff, 0xce, // dec rsi
        0xff, 0xc1, // inc ecx
        0x48, 0x85, 0xc0, // test rax, rax
        0x0f, 0x85,
    ]);
    let loop_displacement = code.len();
    code.extend_from_slice(&[0; 4]);
    patch_rel32(&mut code, loop_displacement, loop_start);
    code.extend_from_slice(&[
        0x48, 0xff, 0xc6, // inc rsi: first digit
        0x89, 0x4d, 0xd8, // save digit count at [rbp - 40]
        0x48, 0x8b, 0x7d, 0xe0, // load destination pointer
        0x8b, 0x4d, 0xd8, // load copy count
        0xf3, 0xa4, // rep movsb
        0x8b, 0x45, 0xd8, // return byte count in eax
        0xc9, 0xc3, // leave; ret
    ]);
    code
}

fn format_signed_decimal_helper() -> Vec<u8> {
    let mut code = vec![
        0x55, // push rbp
        0x48, 0x89, 0xe5, // mov rbp, rsp
        0x48, 0x83, 0xec, 0x30, // sub rsp, 48
        0x48, 0x89, 0x7d, 0xe0, // save destination at [rbp - 32]
        0x48, 0x89, 0xf0, // mov rax, rsi: signed value
        0x45, 0x31, 0xc9, // xor r9d, r9d: positive flag
        0x48, 0x85, 0xc0, // test rax, rax
        0x0f, 0x89, 0, 0, 0, 0, // jns positive
        0x41, 0xb9, 1, 0, 0, 0, // mov r9d, 1
        0x48, 0xf7, 0xd8, // neg rax
        0x48, 0x8d, 0x75, 0xff, // positive:
        0x31, 0xc9, // xor ecx, ecx
    ];
    let positive_offset = code.len() - 6;
    patch_rel32(&mut code, 23, positive_offset);
    let loop_start = code.len();
    code.extend_from_slice(&[
        0x31, 0xd2, // xor edx, edx
        0x41, 0xb8, 10, 0, 0, 0, // mov r8d, 10
        0x49, 0xf7, 0xf0, // div r8
        0x80, 0xc2, b'0', // add dl, '0'
        0x88, 0x16, // mov byte ptr [rsi], dl
        0x48, 0xff, 0xce, // dec rsi
        0xff, 0xc1, // inc ecx
        0x48, 0x85, 0xc0, // test rax, rax
        0x0f, 0x85,
    ]);
    let loop_displacement = code.len();
    code.extend_from_slice(&[0; 4]);
    patch_rel32(&mut code, loop_displacement, loop_start);
    code.extend_from_slice(&[
        0x48, 0xff, 0xc6, // first digit
        0x45, 0x85, 0xc9, // test r9d, r9d
        0x0f, 0x84,
    ]);
    let positive_sign = code.len();
    code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[
        0x48, 0xff, 0xce, // dec rsi: prefix slot
        0xc6, 0x06, b'-', // mov byte ptr [rsi], '-'
        0xff, 0xc1, // inc ecx
    ]);
    let copy_start = code.len();
    patch_rel32(&mut code, positive_sign, copy_start);
    code.extend_from_slice(&[
        0x89, 0x4d, 0xd8, // save length at [rbp - 40]
        0x48, 0x8b, 0x7d, 0xe0, // load destination
        0x8b, 0x4d, 0xd8, // load length
        0xf3, 0xa4, // rep movsb
        0x8b, 0x45, 0xd8, // return length in eax
        0xc9, 0xc3, // leave; ret
    ]);
    code
}

fn print_hex16_helper() -> Vec<u8> {
    let mut code = vec![
        0x55, // push rbp
        0x48, 0x89, 0xe5, // mov rbp, rsp
        0x48, 0x83, 0xec, 0x20, // sub rsp, 32
        0x48, 0x89, 0xf8, // mov rax, rdi
        0x48, 0x8d, 0x75, 0xff, // lea rsi, [rbp - 1]
    ];
    for _ in 0..16 {
        code.extend_from_slice(&[
            0x89, 0xc2, // mov edx, eax
            0x83, 0xe2, 0x0f, // and edx, 15
            0x80, 0xfa, 10, // cmp dl, 10
            0x72, 5, // jb add_digit
            0x80, 0xc2, 87, // add dl, 'a' - 10
            0xeb, 3, // jmp store
            0x80, 0xc2, b'0', // add dl, '0'
            0x88, 0x16, // mov byte ptr [rsi], dl
            0x48, 0xff, 0xce, // dec rsi
            0x48, 0xc1, 0xe8, 4, // shr rax, 4
        ]);
    }
    code.extend_from_slice(&[
        0x48, 0x8d, 0x75, 0xf0, // lea rsi, [rbp - 16]
        0xba, 16, 0, 0, 0, // mov edx, 16
        0xbf, 1, 0, 0, 0, // mov edi, 1
        0xb8, 1, 0, 0, 0, // mov eax, 1
        0x0f, 0x05, // syscall
        0xc9, 0xc3, // leave; ret
    ]);
    code
}

fn helper_prologue(
    parameters: &[quill_syntax::ParameterItem],
    structs: &[StructItem],
    frame_slots: usize,
    has_aggregate_return: bool,
) -> Vec<u8> {
    let frame_size = (frame_slots * 8).div_ceil(16) * 16;
    let mut code = vec![0x55, 0x48, 0x89, 0xe5]; // push rbp; mov rbp, rsp
    if frame_size != 0 {
        emit_rsp_adjust(&mut code, true, frame_size);
    }
    if has_aggregate_return {
        encode_rbp_store_argument_rax(&mut code, 0, -8 * (frame_slots as i32));
    }
    let mut physical_argument = usize::from(has_aggregate_return);
    let mut frame_slot = 0usize;
    for parameter in parameters {
        let component_word_kinds = parameter_component_word_kinds(&parameter.type_text, structs)
            .expect("helper signature validation checked parameter component types");
        for is_word in component_word_kinds {
            let displacement = -8 * (frame_slot as i32 + 1);
            if physical_argument < 6 {
                if is_word {
                    encode_rbp_store_argument_rax(&mut code, physical_argument, displacement);
                } else {
                    encode_rbp_store_argument_eax(&mut code, physical_argument, displacement);
                }
            } else {
                let incoming_offset = 16 + (physical_argument - 6) * 8;
                encode_rbp_load_rax(&mut code, incoming_offset as i32);
                if is_word {
                    encode_rbp_store_rax(&mut code, displacement);
                } else {
                    encode_rbp_store_eax(&mut code, displacement);
                }
            }
            physical_argument += 1;
            frame_slot += 1;
        }
    }
    code
}

fn entry_prologue(frame_slots: usize) -> Vec<u8> {
    // _start begins aligned; the extra eight bytes compensate for push rbp,
    // keeping calls from the entry function aligned at each call site.
    let frame_size = (frame_slots * 8).div_ceil(16) * 16 + 8;
    let mut code = vec![0x55, 0x48, 0x89, 0xe5];
    emit_rsp_adjust(&mut code, true, frame_size);
    code
}

fn emit_rsp_adjust(code: &mut Vec<u8>, subtract: bool, amount: usize) {
    if amount == 0 {
        return;
    }
    code.extend_from_slice(if subtract {
        &[0x48, 0x81, 0xec]
    } else {
        &[0x48, 0x81, 0xc4]
    });
    code.extend_from_slice(&(amount as u32).to_le_bytes());
}

fn encode_rsp_load_eax(code: &mut Vec<u8>, displacement: usize) {
    if displacement <= i8::MAX as usize {
        code.extend_from_slice(&[0x8b, 0x44, 0x24, displacement as u8]);
    } else {
        code.extend_from_slice(&[0x8b, 0x84, 0x24]);
        code.extend_from_slice(&(displacement as u32).to_le_bytes());
    }
}

fn encode_rsp_load_rax(code: &mut Vec<u8>, displacement: usize) {
    if displacement <= i8::MAX as usize {
        code.extend_from_slice(&[0x48, 0x8b, 0x44, 0x24, displacement as u8]);
    } else {
        code.extend_from_slice(&[0x48, 0x8b, 0x84, 0x24]);
        code.extend_from_slice(&(displacement as u32).to_le_bytes());
    }
}

fn encode_rsp_load_rcx(code: &mut Vec<u8>, displacement: usize) {
    if displacement <= i8::MAX as usize {
        code.extend_from_slice(&[0x48, 0x8b, 0x4c, 0x24, displacement as u8]);
    } else {
        code.extend_from_slice(&[0x48, 0x8b, 0x8c, 0x24]);
        code.extend_from_slice(&(displacement as u32).to_le_bytes());
    }
}

fn encode_rsp_load_rdx(code: &mut Vec<u8>, displacement: usize) {
    if displacement <= i8::MAX as usize {
        code.extend_from_slice(&[0x48, 0x8b, 0x54, 0x24, displacement as u8]);
    } else {
        code.extend_from_slice(&[0x48, 0x8b, 0x94, 0x24]);
        code.extend_from_slice(&(displacement as u32).to_le_bytes());
    }
}

fn encode_rsp_load_rsi(code: &mut Vec<u8>, displacement: usize) {
    if displacement <= i8::MAX as usize {
        code.extend_from_slice(&[0x48, 0x8b, 0x74, 0x24, displacement as u8]);
    } else {
        code.extend_from_slice(&[0x48, 0x8b, 0xb4, 0x24]);
        code.extend_from_slice(&(displacement as u32).to_le_bytes());
    }
}

fn encode_rsp_store_eax(code: &mut Vec<u8>, displacement: usize) {
    if displacement <= i8::MAX as usize {
        code.extend_from_slice(&[0x89, 0x44, 0x24, displacement as u8]);
    } else {
        code.extend_from_slice(&[0x89, 0x84, 0x24]);
        code.extend_from_slice(&(displacement as u32).to_le_bytes());
    }
}

fn encode_rsp_store_rax(code: &mut Vec<u8>, displacement: usize) {
    if displacement <= i8::MAX as usize {
        code.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, displacement as u8]);
    } else {
        code.extend_from_slice(&[0x48, 0x89, 0x84, 0x24]);
        code.extend_from_slice(&(displacement as u32).to_le_bytes());
    }
}

fn encode_rsp_store_rcx(code: &mut Vec<u8>, displacement: usize) {
    if displacement <= i8::MAX as usize {
        code.extend_from_slice(&[0x48, 0x89, 0x4c, 0x24, displacement as u8]);
    } else {
        code.extend_from_slice(&[0x48, 0x89, 0x8c, 0x24]);
        code.extend_from_slice(&(displacement as u32).to_le_bytes());
    }
}

fn encode_rsp_store_rdx(code: &mut Vec<u8>, displacement: usize) {
    if displacement <= i8::MAX as usize {
        code.extend_from_slice(&[0x48, 0x89, 0x54, 0x24, displacement as u8]);
    } else {
        code.extend_from_slice(&[0x48, 0x89, 0x94, 0x24]);
        code.extend_from_slice(&(displacement as u32).to_le_bytes());
    }
}

fn encode_rsp_load_argument_rax(code: &mut Vec<u8>, argument: usize, displacement: usize) {
    let (rex, modrm_disp8, modrm_disp32) = match argument {
        0 => (0x48, 0x7c, 0xbc), // rdi
        1 => (0x48, 0x74, 0xb4), // rsi
        2 => (0x48, 0x54, 0x94), // rdx
        3 => (0x48, 0x4c, 0x8c), // rcx
        4 => (0x4c, 0x44, 0x84), // r8
        5 => (0x4c, 0x4c, 0x8c), // r9
        _ => unreachable!("only six integer arguments are register-passed"),
    };
    code.push(rex);
    code.extend_from_slice(&[
        0x8b,
        if displacement <= i8::MAX as usize {
            modrm_disp8
        } else {
            modrm_disp32
        },
        0x24,
    ]);
    if displacement <= i8::MAX as usize {
        code.push(displacement as u8);
    } else {
        code.extend_from_slice(&(displacement as u32).to_le_bytes());
    }
}

fn encode_rbp_load_eax(code: &mut Vec<u8>, displacement: i32) {
    if i8::try_from(displacement).is_ok() {
        code.extend_from_slice(&[0x8b, 0x45, displacement as i8 as u8]);
    } else {
        code.extend_from_slice(&[0x8b, 0x85]);
        code.extend_from_slice(&displacement.to_le_bytes());
    }
}

fn encode_rbp_load_rax(code: &mut Vec<u8>, displacement: i32) {
    code.push(0x48);
    if i8::try_from(displacement).is_ok() {
        code.extend_from_slice(&[0x8b, 0x45, displacement as i8 as u8]);
    } else {
        code.push(0x8b);
        code.push(0x85);
        code.extend_from_slice(&displacement.to_le_bytes());
    }
}

fn encode_rbp_load_rcx(code: &mut Vec<u8>, displacement: i32) {
    code.extend_from_slice(&[0x48, 0x8b, 0x8d]);
    code.extend_from_slice(&displacement.to_le_bytes());
}

fn encode_rbp_load_rdx(code: &mut Vec<u8>, displacement: i32) {
    code.push(0x48);
    if i8::try_from(displacement).is_ok() {
        code.extend_from_slice(&[0x8b, 0x55, displacement as i8 as u8]);
    } else {
        code.extend_from_slice(&[0x8b, 0x95]);
        code.extend_from_slice(&displacement.to_le_bytes());
    }
}

fn encode_rbp_store_eax(code: &mut Vec<u8>, displacement: i32) {
    if i8::try_from(displacement).is_ok() {
        code.extend_from_slice(&[0x89, 0x45, displacement as i8 as u8]);
    } else {
        code.extend_from_slice(&[0x89, 0x85]);
        code.extend_from_slice(&displacement.to_le_bytes());
    }
}

fn encode_rbp_store_edx(code: &mut Vec<u8>, displacement: i32) {
    if i8::try_from(displacement).is_ok() {
        code.extend_from_slice(&[0x89, 0x55, displacement as i8 as u8]);
    } else {
        code.extend_from_slice(&[0x89, 0x95]);
        code.extend_from_slice(&displacement.to_le_bytes());
    }
}

fn encode_rbp_store_rax(code: &mut Vec<u8>, displacement: i32) {
    code.push(0x48);
    if i8::try_from(displacement).is_ok() {
        code.extend_from_slice(&[0x89, 0x45, displacement as i8 as u8]);
    } else {
        code.push(0x89);
        code.push(0x85);
        code.extend_from_slice(&displacement.to_le_bytes());
    }
}

fn encode_rbp_load_collection_rax(code: &mut Vec<u8>, slot: usize, indirect: bool) {
    encode_rbp_load_rax(code, -8 * (slot as i32 + 1));
    if indirect {
        code.extend_from_slice(&[0x48, 0x8b, 0x00]);
    }
}

fn encode_rbp_load_collection_rcx(code: &mut Vec<u8>, slot: usize, indirect: bool) {
    encode_rbp_load_rcx(code, -8 * (slot as i32 + 1));
    if indirect {
        code.extend_from_slice(&[0x48, 0x8b, 0x09]);
    }
}

fn encode_rbp_store_collection_rax(code: &mut Vec<u8>, slot: usize, indirect: bool) {
    if indirect {
        code.push(0x50);
        encode_rbp_load_rcx(code, -8 * (slot as i32 + 1));
        code.extend_from_slice(&[0x58, 0x48, 0x89, 0x01]);
    } else {
        encode_rbp_store_rax(code, -8 * (slot as i32 + 1));
    }
}

fn encode_rbp_store_rdx(code: &mut Vec<u8>, displacement: i32) {
    code.push(0x48);
    if i8::try_from(displacement).is_ok() {
        code.extend_from_slice(&[0x89, 0x55, displacement as i8 as u8]);
    } else {
        code.extend_from_slice(&[0x89, 0x95]);
        code.extend_from_slice(&displacement.to_le_bytes());
    }
}

fn encode_rbp_store_argument_eax(code: &mut Vec<u8>, argument: usize, displacement: i32) {
    let opcode: &[u8] = match argument {
        0 => &[0x89, 0x7d],       // edi
        1 => &[0x89, 0x75],       // esi
        2 => &[0x89, 0x55],       // edx
        3 => &[0x89, 0x4d],       // ecx
        4 => &[0x44, 0x89, 0x45], // r8d
        5 => &[0x44, 0x89, 0x4d], // r9d
        _ => unreachable!("only six integer arguments are register-passed"),
    };
    code.extend_from_slice(opcode);
    code.push(displacement as i8 as u8);
}

fn encode_rbp_store_argument_rax(code: &mut Vec<u8>, argument: usize, displacement: i32) {
    let opcode: &[u8] = match argument {
        0 => &[0x48, 0x89, 0x7d], // rdi
        1 => &[0x48, 0x89, 0x75], // rsi
        2 => &[0x48, 0x89, 0x55], // rdx
        3 => &[0x48, 0x89, 0x4d], // rcx
        4 => &[0x4c, 0x89, 0x45], // r8
        5 => &[0x4c, 0x89, 0x4d], // r9
        _ => unreachable!("only six integer arguments are register-passed"),
    };
    code.extend_from_slice(opcode);
    code.push(displacement as i8 as u8);
}

fn append_encoded_value(
    output: &mut Vec<u8>,
    value: &Value,
    in_function: bool,
    relocations: &mut Vec<PendingRelocation>,
) {
    let base = output.len() as u64;
    let first_relocation = relocations.len();
    output.extend(encode_value(value, in_function, relocations));
    for relocation in &mut relocations[first_relocation..] {
        relocation.offset += base;
    }
}

fn append_encoded_chunk(
    output: &mut Vec<u8>,
    encoded: Vec<u8>,
    relocations: &mut [PendingRelocation],
    first_relocation: usize,
) {
    let base = output.len() as u64;
    for relocation in &mut relocations[first_relocation..] {
        relocation.offset += base;
    }
    output.extend(encoded);
}

fn materialize_relocations(
    pending: Vec<PendingRelocation>,
    base: u64,
    symbols: &HashMap<String, object::write::SymbolId>,
) -> Result<Vec<Relocation>, CodegenError> {
    pending
        .into_iter()
        .map(|rel| {
            let symbol = symbols
                .get(&rel.target)
                .copied()
                .ok_or_else(|| CodegenError::UnresolvedCall(rel.target.clone()))?;
            Ok(Relocation {
                offset: base + rel.offset,
                symbol,
                addend: -4,
                flags: RelocationFlags::Generic {
                    kind: RelocationKind::Relative,
                    encoding: RelocationEncoding::X86Branch,
                    size: 32,
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use object::Object as _;
    use object::ObjectSection as _;
    use object::ObjectSymbol as _;
    use quill_source::SourceFile;
    use quill_syntax::parse;

    fn emit(source: &str) -> Result<Vec<u8>, CodegenError> {
        let file = SourceFile::from_text("main.rs", source);
        emit_subset_main_object(&parse(&file))
    }

    fn external_function(name: &str, return_type: &str) -> FunctionItem {
        FunctionItem {
            name: name.to_owned(),
            scope: Vec::new(),
            calls: Vec::new(),
            qualified_calls: Vec::new(),
            name_span: 0..0,
            span: 0..0,
            body_span: None,
            parameters: Vec::new(),
            return_type: Some(return_type.to_owned()),
            body: None,
            is_public: false,
            is_generic: false,
            generic_parameters: Vec::new(),
            generic_copy_parameters: Vec::new(),
            generic_as_ref_str_parameters: Vec::new(),
            generic_add_parameters: Vec::new(),
        generic_trait_bounds: Vec::new(),
            generic_has_bounds: false,
            is_async: false,
            is_const: false,
            is_unsafe: false,
            has_abi: false,
        }
    }

    fn executable_start_object(target_symbol: &str) -> Vec<u8> {
        let mut object = Object::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let text = object.section_id(StandardSection::Text);
        let entry = object.add_symbol(Symbol {
            name: b"_start".to_vec(),
            value: 0,
            size: 0,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        });
        let target = object.add_symbol(Symbol {
            name: target_symbol.as_bytes().to_vec(),
            value: 0,
            size: 0,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        });
        let code = [
            0xe8, 0, 0, 0, 0, // call target
            0x89, 0xc7, // mov edi, eax
            0xb8, 60, 0, 0, 0, // mov eax, 60 (exit)
            0x0f, 0x05, // syscall
            0x0f, 0x0b, // ud2 if exit returns
        ];
        object.add_symbol_data(entry, text, &code, 1);
        object
            .add_relocation(
                text,
                Relocation {
                    offset: 1,
                    symbol: target,
                    addend: -4,
                    flags: RelocationFlags::Generic {
                        kind: RelocationKind::Relative,
                        encoding: RelocationEncoding::X86Branch,
                        size: 32,
                    },
                },
            )
            .unwrap();
        object.write().unwrap()
    }

    #[test]
    fn collection_metadata_distinguishes_vector_and_map_layouts() {
        assert_eq!(collection_kind("Vec<String>"), CollectionKind::Vector);
        assert_eq!(
            collection_kind("std::collections::BTreeMap<String, String>"),
            CollectionKind::BTreeMap
        );
        assert_eq!(
            collection_kind("std::collections::HashMap<u64, String>"),
            CollectionKind::HashMap
        );
    }

    #[test]
    fn emits_elf_and_syscall_entry_for_main() {
        let object = emit("fn main() { std::process::exit(37); }").unwrap();
        assert!(object.starts_with(b"\x7fELF"));
        assert!(object.windows(2).any(|bytes| bytes == [0x0f, 0x05]));
    }

    #[test]
    fn signed_i32_cast_to_u64_sign_extends_before_unsigned_formatting() {
        let object =
            emit("fn main() { let value = -2i32 as u64; println!(\"{value}\"); }").unwrap();
        let output = std::env::temp_dir().join(format!("quill-sign-extend-{}", std::process::id()));
        quill_link::link_elf_executable(&object, &output, "_start").unwrap();
        let result = std::process::Command::new(&output).output().unwrap();
        let _ = std::fs::remove_file(&output);
        assert!(result.status.success(), "{result:?}");
        assert_eq!(result.stdout, b"18446744073709551614\n");
    }

    #[test]
    fn library_object_exports_public_functions_without_an_executable_entry() {
        let file = SourceFile::from_text(
            "lib.rs",
            "pub fn answer() -> i32 { 42 } fn private_helper() -> i32 { 1 }",
        );
        let bytes = emit_subset_library_object(&parse(&file)).unwrap();
        let object = object::File::parse(bytes.as_slice()).unwrap();
        let symbols = object
            .symbols()
            .filter_map(|symbol| Some((symbol.name().ok()?.to_owned(), symbol.is_global())))
            .collect::<HashMap<_, _>>();
        assert_eq!(symbols.get(&quill_export_symbol("answer")), Some(&true));
        assert!(!symbols.contains_key(&quill_export_symbol("private_helper")));
        assert!(!symbols.contains_key("__quill_char_is_whitespace"));
        assert!(!symbols.contains_key("_start"));
    }

    #[test]
    fn library_object_keeps_imported_calls_as_external_elf_relocations() {
        let source = SourceFile::from_text(
            "lib.rs",
            "pub fn answer_from_dependency() -> i32 { dependency_answer() }",
        );
        let parsed = parse(&source);
        let external_symbol = quill_export_symbol("dependency-crate::answer");
        let bytes = emit_subset_library_object_with_imports(
            &parsed,
            &[ExternalFunctionImport {
                declaration: external_function("dependency_answer", "i32"),
                symbol: external_symbol.clone(),
            }],
        )
        .unwrap();
        let object = object::File::parse(bytes.as_slice()).unwrap();
        let imported = object
            .symbols()
            .find(|symbol| symbol.name().ok() == Some(external_symbol.as_str()))
            .unwrap();
        assert!(imported.is_global());
        assert!(imported.is_undefined());
        assert!(
            object
                .sections()
                .any(|section| section.relocations().next().is_some())
        );
    }

    #[test]
    fn separately_emitted_library_objects_link_and_execute_across_crates() {
        let dependency_source =
            SourceFile::from_text("dependency.rs", "pub fn answer() -> i32 { 42 }");
        let mut dependency_parse = parse(&dependency_source);
        let dependency_interface = quill_hir::exported_interface(&quill_hir::lower_functions(
            "dependency_crate",
            &dependency_parse.functions,
        ));
        let exported_answer = dependency_interface
            .functions
            .iter()
            .find(|function| function.path == "answer")
            .unwrap();
        dependency_parse.functions[0].name = "dependency_crate::answer".to_owned();
        let dependency_object = emit_subset_library_object(&dependency_parse).unwrap();

        let caller_source =
            SourceFile::from_text("caller.rs", "pub fn relay() -> i32 { dependency_answer() }");
        let caller_object = emit_subset_library_object_with_imports(
            &parse(&caller_source),
            &[ExternalFunctionImport {
                declaration: external_function(
                    "dependency_answer",
                    exported_answer.signature.return_type.as_deref().unwrap(),
                ),
                symbol: exported_answer.symbol.clone(),
            }],
        )
        .unwrap();
        let caller_symbol = quill_export_symbol("relay");
        let entry_object = executable_start_object(&caller_symbol);
        let output =
            std::env::temp_dir().join(format!("quill-codegen-multi-object-{}", std::process::id()));
        quill_link::link_elf_objects(
            &[&entry_object, &caller_object, &dependency_object],
            &output,
            "_start",
        )
        .unwrap();
        let status = std::process::Command::new(&output).status().unwrap();
        let _ = std::fs::remove_file(output);
        assert_eq!(status.code(), Some(42));
    }

    #[test]
    fn emits_local_named_i32_struct_field_reads() {
        let object = emit(
            "struct Pair { left: i32, right: i32 } fn main() { let pair = Pair { left: 40, right: 2 }; std::process::exit(pair.right); }",
        )
        .unwrap();
        assert!(object.starts_with(b"\x7fELF"));
        assert!(object.windows(2).any(|bytes| bytes == [0x0f, 0x05]));
    }

    #[test]
    fn emits_two_register_returns_for_plain_two_word_records() {
        let object = emit(
            "struct Point { x: u64, y: u64 } fn make() -> Point { Point { x: 40u64, y: 41u64 } } fn main() { make(); std::process::exit(0); }",
        )
        .unwrap();
        assert!(object.starts_with(b"\x7fELF"));
    }

    #[test]
    fn emits_hidden_result_pointer_for_three_field_records() {
        let object = emit(
            "struct Triple { first: u64, answer: i32, last: usize } fn make() -> Triple { Triple { first: 100u64, answer: 42, last: 9usize } } fn main() { let result = make(); std::process::exit(result.answer); }",
        )
        .unwrap();
        assert!(object.starts_with(b"\x7fELF"));
    }

    #[test]
    fn rejects_two_word_record_returns_with_unmodeled_representation() {
        assert!(matches!(
            emit(
                "#[repr(packed)] struct Point { x: u64, y: u64 } fn point() -> Point { Point { x: 40u64, y: 42u64 } } fn main() { point(); std::process::exit(0); }"
            ),
            Err(CodegenError::UnsupportedSignature(_))
        ));
    }

    #[test]
    fn rejects_struct_literals_with_unknown_fields() {
        assert!(matches!(
            emit(
                "struct Pair { left: i32 } fn main() { let pair = Pair { right: 42 }; std::process::exit(0); }"
            ),
            Err(CodegenError::UnsupportedProgram(
                "struct literal has a duplicate or unknown field"
            ))
        ));
    }

    #[test]
    fn rejects_values_outside_the_declared_struct_field_subset() {
        assert!(matches!(
            emit(
                "struct Pair { left: i32 } fn main() { let pair = Pair { left: true }; std::process::exit(0); }"
            ),
            Err(CodegenError::UnsupportedProgram(
                "struct field initializer does not match its supported field type"
            ))
        ));
    }

    #[test]
    fn dead_functions_do_not_trigger_backend_struct_layout_restrictions() {
        let object = emit(
            "struct Unsupported { value: bool } fn dead() -> i32 { let value = Unsupported { value: true }; 0 } fn main() { std::process::exit(42); }",
        )
        .unwrap();
        assert!(object.starts_with(b"\x7fELF"));
    }

    #[test]
    fn dead_functions_do_not_trigger_backend_signature_restrictions() {
        let object = emit(
            "fn dead() -> String { \"dead\".to_owned() } fn main() { std::process::exit(42); }",
        )
        .unwrap();
        assert!(object.starts_with(b"\x7fELF"));
    }

    #[test]
    fn emits_named_u64_struct_field_initializers_and_reads() {
        let object = emit(
            "struct Pair { count: u64 } fn main() { let pair = Pair { count: 42u64 }; std::process::exit(pair.count); }",
        )
        .unwrap();
        assert!(object.starts_with(b"\x7fELF"));
        assert!(object.windows(3).any(|bytes| bytes == [0x48, 0x89, 0x45]));
        assert!(object.windows(3).any(|bytes| bytes == [0x48, 0x8b, 0x45]));
    }

    #[test]
    fn emits_named_usize_struct_field_initializers_and_reads() {
        let object = emit(
            "struct Pair { count: usize } fn main() { let pair = Pair { count: 42usize }; std::process::exit(pair.count); }",
        )
        .unwrap();
        assert!(object.starts_with(b"\x7fELF"));
        assert!(object.windows(3).any(|bytes| bytes == [0x48, 0x89, 0x45]));
        assert!(object.windows(3).any(|bytes| bytes == [0x48, 0x8b, 0x45]));
    }

    #[test]
    fn emits_reachable_functions_and_prunes_dead_code() {
        let object = emit("fn main() { std::process::exit(answer()); } fn answer() -> i32 { 41 } fn unused() -> i32 { 99 }").unwrap();
        assert!(
            object
                .windows(b"__quill_fn_1".len())
                .any(|bytes| bytes == b"__quill_fn_1")
        );
        assert!(
            !object
                .windows(b"__quill_fn_2".len())
                .any(|bytes| bytes == b"__quill_fn_2")
        );
    }

    #[test]
    fn prunes_public_helpers_that_have_no_executable_callers() {
        let object = emit(
            "fn main() { std::process::exit(answer()); } fn answer() -> i32 { 41 } pub fn unused_public() -> i32 { 99 }",
        ).unwrap();
        assert!(
            object
                .windows(b"__quill_fn_1".len())
                .any(|bytes| bytes == b"__quill_fn_1")
        );
        assert!(
            !object
                .windows(b"__quill_fn_2".len())
                .any(|bytes| bytes == b"__quill_fn_2")
        );
    }

    #[test]
    fn emits_second_parameter_for_two_type_generic() {
        let object = emit("fn second<T, U>(first: T, value: U) -> U { value } fn main() { std::process::exit(second(17, 42)); }").unwrap();
        assert!(
            object
                .windows(5)
                .any(|bytes| bytes == [0x8b, 0x45, 0xf0, 0xc9, 0xc3])
        );
    }

    #[test]
    fn emits_static_string_panics_with_rust_message_and_exit_status() {
        let object = emit("fn main() { panic!(\"expected panic\"); }").unwrap();
        assert!(object.starts_with(b"\x7fELF"));
        assert!(
            object
                .windows(b"expected panic".len())
                .any(|bytes| bytes == b"expected panic")
        );
        assert!(
            object
                .windows(b"RUST_BACKTRACE=1".len())
                .any(|bytes| bytes == b"RUST_BACKTRACE=1")
        );
    }

    #[test]
    fn rejects_unsupported_panic_formatting_and_other_items() {
        assert!(matches!(
            emit("fn main() { panic!(\"value: {}\", 42); }"),
            Err(CodegenError::UnsupportedProgram(_))
        ));
        assert!(matches!(
            emit("const VALUE: usize = 0; fn main() {}"),
            Err(CodegenError::UnsupportedProgram(_))
        ));
    }

    #[test]
    fn lowers_initialized_locals_in_main() {
        assert!(emit("fn main() { let value = 1; }").is_ok());
    }

    #[test]
    fn lowers_option_is_some_for_a_local_some_value() {
        let object = emit(
            "fn main() { let value: Option<i32> = Some(42); let result = if value.is_some() { 42u64 } else { 0u64 }; println!(\"{result}\"); }",
        )
        .unwrap();
        assert!(object.starts_with(b"\x7fELF"));
    }

    #[test]
    fn lowers_scalar_tuple_literal_bindings_in_main() {
        let object = emit(
            "fn main() { let (count, digest) = (40i32, 2i32); std::process::exit(count + digest); }",
        )
        .unwrap();
        assert!(object.starts_with(b"\x7fELF"));
    }

    #[test]
    fn emits_pair_return_helpers_and_direct_destructuring_calls() {
        let object = emit(
            "fn pair(value: u64) -> (u64, i32) { (value, 42) } fn main() { let (whole, answer) = pair(100u64); std::process::exit(answer); }",
        )
        .unwrap();
        assert!(object.starts_with(b"\x7fELF"));
    }

    #[test]
    fn stores_pair_return_locals_and_reads_each_field() {
        let object = emit(
            "fn pair() -> (u64, u64) { (19, 23) } fn main() { let values = pair(); let result = values.0 + values.1; println!(\"{result}\"); }",
        )
        .unwrap();
        let output = std::env::temp_dir().join(format!("quill-pair-local-{}", std::process::id()));
        quill_link::link_elf_executable(&object, &output, "_start").unwrap();
        let result = std::process::Command::new(&output).output().unwrap();
        let _ = std::fs::remove_file(&output);
        assert!(result.status.success(), "{result:?}");
        assert_eq!(result.stdout, b"42\n");
    }

    #[test]
    fn checks_status_of_a_locally_stored_niche_result() {
        let object = emit(
            "fn check(value: u64) -> Result<(), &'static str> { if value == 42 { Ok(()) } else { Err(\"wrong\") } } fn main() { let result = check(42); let answer = if result.is_ok() { 42u64 } else { 0u64 }; println!(\"{answer}\"); }",
        )
        .unwrap();
        let output =
            std::env::temp_dir().join(format!("quill-result-local-{}", std::process::id()));
        quill_link::link_elf_executable(&object, &output, "_start").unwrap();
        let result = std::process::Command::new(&output).output().unwrap();
        let _ = std::fs::remove_file(&output);
        assert!(result.status.success(), "{result:?}");
        assert_eq!(result.stdout, b"42\n");
    }

    #[test]
    fn iterates_and_dereferences_a_borrowed_integer_array_as_a_slice() {
        let object = emit(
            "fn sum(values: &[u64]) -> u64 { let mut total: u64 = 0; for value in values { total = total + *value; } total } fn main() { let values: [u64; 3] = [7, 11, 24]; let result = sum(&values); println!(\"{result}\"); }",
        )
        .unwrap();
        let output =
            std::env::temp_dir().join(format!("quill-borrowed-slice-{}", std::process::id()));
        quill_link::link_elf_executable(&object, &output, "_start").unwrap();
        let result = std::process::Command::new(&output).output().unwrap();
        let _ = std::fs::remove_file(&output);
        assert!(result.status.success(), "{result:?}");
        assert_eq!(result.stdout, b"42\n");
    }

    #[test]
    fn grows_string_vectors_and_reads_the_first_cloned_value() {
        let object = emit(
            "fn main() { let mut values: Vec<String> = Vec::new(); values.push(String::from(\"quill\")); values.push(String::from(\"rust\")); let first = values[0].clone(); println!(\"{first}\"); }",
        )
        .unwrap();
        let output =
            std::env::temp_dir().join(format!("quill-vector-string-index-{}", std::process::id()));
        quill_link::link_elf_executable(&object, &output, "_start").unwrap();
        let result = std::process::Command::new(&output).output().unwrap();
        let _ = std::fs::remove_file(&output);
        assert!(result.status.success(), "{result:?}");
        assert_eq!(result.stdout, b"quill\n");
    }

    #[test]
    fn backend_skips_call_resolution_for_unreachable_functions() {
        assert!(emit("fn main() {} fn unused() -> i32 { missing() }").is_ok());
        assert!(emit("fn main() {} fn unused() -> i32 { missing::function() }").is_ok());
    }

    #[test]
    fn backend_reports_the_function_for_unsupported_lowering() {
        let error = emit(
            "fn count(input: &str) -> u64 { input.chars().fold(0u64, |sum, ch| sum + 1u64) } fn main() { count(\"x\"); }",
        )
        .unwrap_err();
        assert!(matches!(
            error,
            CodegenError::UnsupportedFunction { function, .. } if function == "count"
        ));
    }

    #[test]
    fn reports_the_same_first_unsupported_owned_string_operation_each_time() {
        let source = "fn target() -> String { String::new() } fn main() { target(); }";
        for _ in 0..8 {
            assert!(matches!(
                emit(source),
                Err(CodegenError::UnsupportedFunction { function, reason })
                    if function == "target"
                        && matches!(reason.as_ref(), CodegenError::UnsupportedProgram(
                            "string or slice helper must return a supported pointer/length value"
                        ))
            ));
        }
    }

    #[test]
    fn skips_backend_lowering_for_unreachable_function_bodies() {
        assert!(emit("fn main() {} fn unused() -> i32 { \"dead body\" }").is_ok());
    }

    #[test]
    fn folds_checked_constant_arithmetic() {
        let object =
            emit("fn answer() -> i32 { 6 * 7 } fn main() { std::process::exit(answer()); }")
                .unwrap();
        assert!(
            object
                .windows(b"__quill_fn_".len())
                .any(|bytes| bytes == b"__quill_fn_")
        );
    }

    #[test]
    fn emits_runtime_integer_arithmetic() {
        let object = emit("fn add(a: i32, b: i32) -> i32 { a + b } fn sub(a: i32, b: i32) -> i32 { a - b } fn mul(a: i32, b: i32) -> i32 { a * b } fn div(a: i32, b: i32) -> i32 { a / b } fn rem(a: i32, b: i32) -> i32 { a % b } fn main() { std::process::exit(add(sub(mul(div(84, 2), 3), 1), rem(17, 5))); }").unwrap();
        for opcode in [
            &[0x01, 0xc8][..],
            &[0x29, 0xc8][..],
            &[0x0f, 0xaf, 0xc1][..],
            &[0x99, 0xf7, 0xf9][..],
            &[0x99, 0xf7, 0xf9, 0x89, 0xd0][..],
        ] {
            assert!(object.windows(opcode.len()).any(|bytes| bytes == opcode));
        }
    }

    #[test]
    fn lowers_scalar_slice_for_each_loop() {
        let object = emit(
            "fn total(values: &[i32]) -> i32 { let mut sum = 0; for value in values { sum = sum + value; } sum } fn main() { std::process::exit(total(&[])); }",
        )
        .unwrap();
        assert!(object.starts_with(b"\x7fELF"));
        assert!(object.windows(3).any(|bytes| bytes == [0x8b, 0x04, 0x88]));
        assert!(object.windows(3).any(|bytes| bytes == [0x48, 0x39, 0xc8]));
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    #[test]
    fn native_scalar_slice_loop_sums_nonempty_input() {
        use std::ffi::c_void;

        unsafe extern "C" {
            fn mmap(
                address: *mut c_void,
                length: usize,
                protection: i32,
                flags: i32,
                file: i32,
                offset: isize,
            ) -> *mut c_void;
            fn mprotect(address: *mut c_void, length: usize, protection: i32) -> i32;
            fn munmap(address: *mut c_void, length: usize) -> i32;
        }

        let file = SourceFile::from_text(
            "slice-loop.rs",
            "fn total(values: &[i32]) -> i32 { let mut sum = 0; for value in values { sum = sum + value; } sum }",
        );
        let parsed = parse(&file);
        let function = &parsed.functions[0];
        let value = helper_return_value(function, &parsed.functions, &parsed.structs).unwrap();
        let slots = parameter_slot_count(&function.parameters, &parsed.structs);
        let frame_slots = required_frame_slots(&value, slots);
        let mut code = helper_prologue(&function.parameters, &parsed.structs, frame_slots, false);
        append_encoded_helper_return(&mut code, &value, None, None, &mut Vec::new());
        code.extend_from_slice(&[0xc9, 0xc3]);

        // Execute the generated function directly so the indexed load,
        // unsigned bound check, and loop backedge are exercised with data.
        // The mapping is writable only while copying code, then executable.
        let mapping = unsafe {
            mmap(
                std::ptr::null_mut(),
                code.len(),
                0x1 | 0x2,
                0x02 | 0x20,
                -1,
                0,
            )
        };
        assert_ne!(mapping as isize, -1, "mmap failed");
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), mapping.cast::<u8>(), code.len());
        }
        assert_eq!(unsafe { mprotect(mapping, code.len(), 0x1 | 0x4) }, 0);
        let native: unsafe extern "C" fn(*const i32, usize) -> i32 =
            unsafe { std::mem::transmute(mapping) };
        let values = [11, 13, 18];
        let result = unsafe { native(values.as_ptr(), values.len()) };
        assert_eq!(unsafe { munmap(mapping, code.len()) }, 0);
        assert_eq!(result, values.iter().sum::<i32>());
    }

    #[test]
    fn emits_parameter_forwarding_call_relocations() {
        let object = emit("fn identity(value: i32) -> i32 { value } fn forward(value: i32) -> i32 { identity(value) } fn main() { std::process::exit(forward(42)); }").unwrap();
        assert!(object.starts_with(b"\x7fELF"));
        assert!(
            object
                .windows(b"__quill_fn_1".len())
                .any(|bytes| bytes == b"__quill_fn_1")
        );
        assert!(
            !object
                .windows(b"__quill_fn_2".len())
                .any(|bytes| bytes == b"__quill_fn_2")
        );
    }

    #[test]
    fn lowers_initialized_locals_and_shadowing_to_frame_slots() {
        let object = emit("fn helper(value: i32) -> i32 { let value = value; let result = value; result } fn main() { std::process::exit(helper(42)); }").unwrap();
        // The parameter and each local receive a distinct frame slot; the
        // second initializer reads the first local before binding `result`.
        assert!(object.windows(3).any(|bytes| bytes == [0x89, 0x45, 0xf0]));
        assert!(object.windows(3).any(|bytes| bytes == [0x89, 0x45, 0xe8]));
        assert!(object.windows(3).any(|bytes| bytes == [0x8b, 0x45, 0xf0]));
    }

    #[test]
    fn deduplicates_nested_i32_generic_instances() {
        let object = emit("fn identity<T>(value: T) -> T { value } fn main() { std::process::exit(identity(identity(42))); }").unwrap();
        assert_eq!(
            object
                .windows(b"__quill_fn_0_i32".len())
                .filter(|bytes| *bytes == b"__quill_fn_0_i32")
                .count(),
            1
        );
    }

    #[test]
    fn emits_distinct_i32_and_u64_instances_of_one_generic_definition() {
        let object = emit(
            "fn identity<T>(value: T) -> T { value } fn main() { let _small = identity(7i32); std::process::exit(identity(4294967296u64)); }",
        )
        .unwrap();
        for symbol in [b"__quill_fn_0_i32".as_slice(), b"__quill_fn_0_u64"] {
            assert!(object.windows(symbol.len()).any(|bytes| bytes == symbol));
        }
    }

    #[test]
    fn preserves_usize_and_u64_as_distinct_generic_instance_keys() {
        let object = emit(
            "fn identity<T>(value: T) -> T { value } fn main() { let _wide = identity(7u64); let _native = identity(42usize); std::process::exit(42); }",
        )
        .unwrap();
        for symbol in [b"__quill_fn_0_u64".as_slice(), b"__quill_fn_0_usize"] {
            assert!(object.windows(symbol.len()).any(|bytes| bytes == symbol));
        }
    }

    #[test]
    fn does_not_emit_an_uninstantiated_generic() {
        let object = emit("pub fn identity<T>(value: T) -> T { value } fn main() {}").unwrap();
        assert!(
            !object
                .windows(b"__quill_fn_0_i32".len())
                .any(|bytes| bytes == b"__quill_fn_0_i32")
        );
    }

    #[test]
    fn resolves_explicit_generic_arguments_to_specialized_symbols() {
        let object = emit(
            "fn identity<T>(value: T) -> T { value } fn main() { let _small = identity::<i32>(7); let _wide = identity::<u64>(identity::<u64>(4294967296)); std::process::exit(identity::<usize>(42)); }",
        )
        .unwrap();
        for symbol in [
            b"__quill_fn_0_i32".as_slice(),
            b"__quill_fn_0_u64",
            b"__quill_fn_0_usize",
        ] {
            assert!(object.windows(symbol.len()).any(|bytes| bytes == symbol));
        }
        assert_eq!(
            object
                .windows(b"__quill_fn_0_u64".len())
                .filter(|bytes| *bytes == b"__quill_fn_0_u64")
                .count(),
            1
        );
    }

    #[test]
    fn rejects_unimplemented_generic_trait_bounds() {
        let result = emit(
            "fn identity<T: Display>(value: T) -> T { value } fn main() { let _value = identity(1); }",
        );
        assert!(matches!(result, Err(CodegenError::UnsupportedSignature(_))));
    }

    #[test]
    fn rejects_constant_overflow_instead_of_wrapping() {
        let result = emit("fn main() { std::process::exit(2147483647 + 1); }");
        assert!(matches!(
            result,
            Err(CodegenError::UnsupportedProgram(
                "constant arithmetic overflows or divides by zero"
            ))
        ));
    }
}
