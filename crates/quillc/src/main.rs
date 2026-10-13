use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

type ImportAliasMap = HashMap<String, HashMap<String, String>>;
type CallResolutionCache = HashMap<(String, String), Option<String>>;
static BUILD_SCRIPT_ID: AtomicU64 = AtomicU64::new(0);

struct HasherWriter<'a>(&'a mut blake3::Hasher);

impl std::io::Write for HasherWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct BuildFileStamp {
    path: PathBuf,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    device: u64,
    inode: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct BuildArtifactIndex {
    workspace_root: PathBuf,
    manifest: PathBuf,
    compiler: BuildFileStamp,
    environment: Vec<(String, Option<String>)>,
    uses_build_script: bool,
    cfg_digest: [u8; 32],
    inputs: Vec<BuildFileStamp>,
    external_inputs: Vec<PathBuf>,
    semantic_inputs: Vec<(PathBuf, [u8; 32])>,
    artifact: PathBuf,
    output_path: Option<PathBuf>,
    output_stamp: Option<BuildFileStamp>,
}

struct CargoBuildArtifact {
    path: PathBuf,
    cfg_digest: [u8; 32],
    uses_build_script: bool,
    semantic_inputs: Vec<(PathBuf, [u8; 32])>,
}

struct ParsedSource {
    source: quill_source::SourceFile,
    module_path: Vec<String>,
    parse: quill_syntax::Parse,
    owner: Option<CrateTargetId>,
    summary_only: bool,
}

#[derive(Clone)]
struct EnumDefinition {
    path: String,
    variants: Vec<(String, i32)>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CrateTargetId {
    package: String,
    target: String,
    cfg: quill_cfg::CfgSet,
    edition: String,
}

impl CrateTargetId {
    fn key(&self) -> String {
        format!("{}::{}", self.package, self.target)
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("lex") => process_files(args.collect(), false),
        Some("parse") => process_files(args.collect(), true),
        Some("check-subset") => check_subset_files(args.collect()),
        Some("build-subset") => build_subset_command(args.collect()),
        Some("build-lib-object") => build_library_object_command(args.collect()),
        Some("cargo-build-subset") => cargo_build_subset_command(args.collect()),
        Some("cargo-plan") => cargo_plan_command(args.collect()),
        Some("cargo-parse") => cargo_parse_command(args.collect()),
        Some("cargo-check-subset") => cargo_check_subset_command(args.collect()),
        Some("lsp") if args.next().is_none() => match quill_lsp::run_stdio() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("quillc lsp: {error}");
                ExitCode::FAILURE
            }
        },
        Some("--version") if args.next().is_none() => {
            println!("quillc {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!(
                "usage: quillc lsp | quillc lex|parse|check-subset <file.rs> [more.rs ...] | quillc build-subset <main.rs> -o <program> | quillc build-lib-object <lib.rs> -o <lib.o> | quillc cargo-plan|cargo-parse|cargo-check-subset [Cargo.toml] | quillc cargo-build-subset <Cargo.toml> [--bin <target>] -o <program> | quillc --version"
            );
            ExitCode::from(2)
        }
    }
}

fn cargo_plan_command(arguments: Vec<String>) -> ExitCode {
    if arguments.len() > 1 {
        eprintln!("usage: quillc cargo-plan [Cargo.toml]");
        return ExitCode::from(2);
    }
    let manifest = arguments
        .first()
        .map(String::as_str)
        .unwrap_or("Cargo.toml");
    match quill_cargo::load_workspace(manifest) {
        Ok(workspace) => {
            println!("workspace {}", workspace.root.display());
            let default_members = workspace
                .packages
                .iter()
                .filter(|package| package.is_default_member)
                .count();
            println!(
                "{} reachable packages ({} default members); {} non-workspace dependency edges",
                workspace.packages.len(),
                default_members,
                workspace.external_dependency_count
            );
            for (wave_index, wave) in workspace.dependency_waves.iter().enumerate() {
                println!("wave {}", wave_index + 1);
                for package_index in wave {
                    let package = &workspace.packages[*package_index];
                    let role = if package.is_default_member {
                        "default member"
                    } else {
                        "dependency"
                    };
                    let deps = if package.local_dependencies.is_empty() {
                        "-".to_owned()
                    } else {
                        package.local_dependencies.join(",")
                    };
                    println!(
                        "  package {} {} [{role}] (local deps: {deps})",
                        package.name, package.version
                    );
                    for target in &package.targets {
                        println!(
                            "    target {} [{}] {}",
                            target.name,
                            target.kinds.join(","),
                            target.source_path.display()
                        );
                    }
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("quillc: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Parse the default build targets from all selected Cargo workspace packages.
/// Target roots share one module-discovery frontier so independent files and
/// crates use the same bounded work-stealing pool.
fn cargo_parse_command(arguments: Vec<String>) -> ExitCode {
    cargo_workspace_command(arguments, false)
}

fn cargo_check_subset_command(arguments: Vec<String>) -> ExitCode {
    cargo_workspace_command(arguments, true)
}

fn cargo_workspace_command(arguments: Vec<String>, check_types: bool) -> ExitCode {
    let mut manifest = None;
    let mut selection = quill_cargo::CargoFeatureSelection::default();
    let mut index = 0;
    let mut invalid = false;
    while index < arguments.len() {
        if let Some(features) = arguments[index].strip_prefix("--features=") {
            if features.is_empty() {
                invalid = true;
                break;
            }
            selection.features.push(features.to_owned());
            index += 1;
            continue;
        }
        match arguments[index].as_str() {
            "--features" => {
                index += 1;
                if let Some(features) = arguments.get(index) {
                    selection.features.push(features.clone());
                } else {
                    invalid = true;
                    break;
                }
            }
            "--no-default-features" if !selection.no_default_features => {
                selection.no_default_features = true;
            }
            "--all-features" if !selection.all_features => {
                selection.all_features = true;
            }
            option if option.starts_with('-') => invalid = true,
            path if manifest.is_none() => manifest = Some(path.to_owned()),
            _ => invalid = true,
        }
        if invalid {
            break;
        }
        index += 1;
    }
    if invalid {
        eprintln!(
            "usage: quillc cargo-parse|cargo-check-subset [Cargo.toml] [--features <features>] [--no-default-features] [--all-features]"
        );
        return ExitCode::from(2);
    }
    let manifest = manifest.unwrap_or_else(|| "Cargo.toml".to_owned());
    let workspace = match quill_cargo::load_workspace_with_features(&manifest, &selection) {
        Ok(workspace) => workspace,
        Err(error) => {
            eprintln!("quillc: {error}");
            return ExitCode::FAILURE;
        }
    };
    let targets: Vec<_> = workspace
        .packages
        .iter()
        .flat_map(|package| {
            package.targets.iter().filter_map(move |target| {
                let is_compilation_target = target.kinds.iter().any(|kind| {
                    if package.is_default_member {
                        matches!(kind.as_str(), "lib" | "bin" | "proc-macro" | "custom-build")
                    } else {
                        matches!(kind.as_str(), "lib" | "proc-macro" | "custom-build")
                    }
                });
                is_compilation_target.then_some((package, target))
            })
        })
        .collect();
    if targets.is_empty() {
        println!(
            "no default Cargo build targets in {}",
            workspace.root.display()
        );
        return ExitCode::SUCCESS;
    }
    let roots: Vec<_> = targets
        .iter()
        .map(|(package, target)| {
            (
                CrateTargetId {
                    package: package.id.clone(),
                    target: target.name.clone(),
                    cfg: package.cfg.clone(),
                    edition: target.edition.clone(),
                },
                target.source_path.display().to_string(),
            )
        })
        .collect();
    let mut pool = None;
    let (parsed_files, module_errors) = match parse_cargo_module_trees(&roots, &mut pool, false) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("quillc: could not parse Cargo target roots: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut had_error = false;
    for file in &parsed_files {
        for diagnostic in &file.parse.diagnostics {
            had_error = true;
            let (line, column) = file.source.line_column(diagnostic.span.start);
            eprintln!(
                "{}:{line}:{column}: {}",
                file.source.path().display(),
                diagnostic.message
            );
        }
    }
    for error in module_errors {
        had_error = true;
        eprintln!("quillc: {error}");
    }
    let item_count: usize = parsed_files
        .iter()
        .map(|file| file.parse.functions.len())
        .sum();
    let default_members = workspace
        .packages
        .iter()
        .filter(|package| package.is_default_member)
        .count();
    let mut target_parses: HashMap<CrateTargetId, Vec<(Vec<String>, quill_syntax::Parse)>> =
        HashMap::new();
    for file in &parsed_files {
        if let Some(owner) = &file.owner {
            target_parses
                .entry(owner.clone())
                .or_default()
                .push((file.module_path.clone(), file.parse.clone()));
        }
    }
    let crate_hirs: Vec<_> = targets
        .iter()
        .map(|(package, target)| {
            let owner = CrateTargetId {
                package: package.id.clone(),
                target: target.name.clone(),
                cfg: package.cfg.clone(),
                edition: target.edition.clone(),
            };
            let parses = target_parses.remove(&owner).unwrap_or_default();
            quill_hir::lower_module_files(&owner.key(), &parses)
        })
        .collect();
    let mut dependency_aliases = HashMap::new();
    for (package, target) in &targets {
        let owner = CrateTargetId {
            package: package.id.clone(),
            target: target.name.clone(),
            cfg: package.cfg.clone(),
            edition: target.edition.clone(),
        };
        let aliases = dependency_aliases
            .entry(owner.key())
            .or_insert_with(HashMap::new);
        for dependency in &package.dependencies {
            if let Some(dependency_package) = workspace
                .packages
                .iter()
                .find(|candidate| candidate.id == dependency.package_id)
                && let Some(library) = dependency_package.targets.iter().find(|target| {
                    target
                        .kinds
                        .iter()
                        .any(|kind| matches!(kind.as_str(), "lib" | "proc-macro"))
                })
            {
                aliases.insert(
                    dependency.crate_name.clone(),
                    CrateTargetId {
                        package: dependency_package.id.clone(),
                        target: library.name.clone(),
                        cfg: dependency_package.cfg.clone(),
                        edition: library.edition.clone(),
                    }
                    .key(),
                );
            }
        }
        if target.kinds.iter().any(|kind| kind == "bin")
            && let Some(library) = package
                .targets
                .iter()
                .find(|target| target.kinds.iter().any(|kind| kind == "lib"))
        {
            aliases.insert(
                library.name.clone(),
                CrateTargetId {
                    package: package.id.clone(),
                    target: library.name.clone(),
                    cfg: package.cfg.clone(),
                    edition: library.edition.clone(),
                }
                .key(),
            );
        }
    }
    let workspace_calls = quill_hir::resolve_workspace_calls(&crate_hirs, &dependency_aliases);
    let resolved_workspace_calls = workspace_calls
        .iter()
        .filter(|call| matches!(call.target, quill_hir::CallTarget::Unique(_)))
        .count();
    let mut checked_bodies = 0usize;
    let mut type_check_errors = false;
    if check_types {
        let mut cache_stats = Vec::new();
        let mut jobs = Vec::new();
        for (package, target) in &targets {
            let owner = CrateTargetId {
                package: package.id.clone(),
                target: target.name.clone(),
                cfg: package.cfg.clone(),
                edition: target.edition.clone(),
            };
            let Some(hir) = crate_hirs.iter().find(|hir| hir.name == owner.key()) else {
                continue;
            };
            let signatures = signatures_for_target(hir, &crate_hirs, &dependency_aliases);
            for file in parsed_files
                .iter()
                .filter(|file| file.owner.as_ref() == Some(&owner) && !file.parse.had_error)
            {
                checked_bodies += file
                    .parse
                    .functions
                    .iter()
                    .filter(|function| function.body.is_some())
                    .count();
                jobs.push((
                    file.source.clone(),
                    file.parse.functions.clone(),
                    signatures.clone(),
                    parsed_files
                        .iter()
                        .filter(|candidate| {
                            candidate.owner.as_ref() == Some(&owner)
                                && candidate.module_path.len() <= file.module_path.len()
                                && file.module_path.starts_with(&candidate.module_path)
                        })
                        .flat_map(typeck_struct_signatures)
                        .collect::<Vec<_>>(),
                    owner.cfg.clone(),
                ));
            }
        }
        if jobs.len() > 1 && pool.is_none() {
            pool = match quill_parallel::WorkPool::for_tasks(jobs.len()) {
                Ok(pool) => Some(pool),
                Err(error) => {
                    eprintln!("quillc: could not start checker workers: {error}");
                    return ExitCode::FAILURE;
                }
            };
        }
        let pool = pool.as_ref();
        let checked = if let Some(pool) = pool {
            pool.map(&jobs, |(source, functions, signatures, structs, cfg)| {
                let cache = quill_query::TypeCheckCache::for_source_with_cfg(source, cfg);
                let diagnostics = cache.check_functions_per_body_with_structs(
                    source, functions, false, signatures, structs,
                );
                (source.clone(), diagnostics, cache.stats())
            })
        } else {
            jobs.iter()
                .map(|(source, functions, signatures, structs, cfg)| {
                    let cache = quill_query::TypeCheckCache::for_source_with_cfg(source, cfg);
                    let diagnostics = cache.check_functions_per_body_with_structs(
                        source, functions, false, signatures, structs,
                    );
                    (source.clone(), diagnostics, cache.stats())
                })
                .collect()
        };
        for (source, diagnostics, stats) in checked {
            cache_stats.push(stats);
            for diagnostic in diagnostics {
                type_check_errors = true;
                let (line, column) = source.line_column(diagnostic.span.start);
                eprintln!(
                    "{}:{line}:{column}: {}",
                    source.path().display(),
                    diagnostic.message
                );
            }
        }
        if std::env::var_os("QUILL_CACHE_STATS").is_some() {
            let totals = cache_stats.iter().fold(
                quill_query::TypeCheckCacheStats::default(),
                |mut total, stats| {
                    total.hits += stats.hits;
                    total.misses += stats.misses;
                    total.invalid_entries += stats.invalid_entries;
                    total.write_failures += stats.write_failures;
                    total
                },
            );
            eprintln!(
                "quill-typeck-query: hits={} misses={} invalid={} write_failures={}",
                totals.hits, totals.misses, totals.invalid_entries, totals.write_failures
            );
        }
    }
    println!(
        "parsed {} Cargo target roots into {} source files ({} function items) across {} reachable packages ({} default members); resolved {} qualified function calls against workspace HIR{}",
        targets.len(),
        parsed_files.len(),
        item_count,
        workspace.packages.len(),
        default_members,
        resolved_workspace_calls,
        if check_types {
            format!("; checked {checked_bodies} bodies with the primitive checker")
        } else {
            String::new()
        }
    );
    if had_error || type_check_errors {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn signatures_for_target(
    hir: &quill_hir::HirCrate,
    crates: &[quill_hir::HirCrate],
    dependency_aliases: &HashMap<String, HashMap<String, String>>,
) -> Vec<quill_typeck::ExternalFunctionSignature> {
    let mut signatures = Vec::new();
    for function in &hir.functions {
        if function.def_path.scope.iter().any(|scope| {
            !scope.starts_with("mod:")
                && !scope.starts_with("impl:")
                && !scope.starts_with("traitimpl:")
        })
        {
            continue;
        }
        let mut path: Vec<_> = function
            .def_path
            .scope
            .iter()
            .filter_map(|scope| {
                scope
                    .strip_prefix("mod:")
                    .or_else(|| scope.strip_prefix("impl:"))
            })
            .map(str::to_owned)
            .collect();
        path.push(function.def_path.name.clone());
        if path.len() > 1 {
            signatures.push(typeck_signature(
                path.join("::"),
                &function.signature,
                function.def_path.scope.clone(),
            ));
            let crate_path = format!("crate::{}", path.join("::"));
            signatures.push(typeck_signature(
                crate_path,
                &function.signature,
                function.def_path.scope.clone(),
            ));
        }
    }
    if let Some(aliases) = dependency_aliases.get(&hir.name) {
        for (alias, dependency_name) in aliases {
            let Some(dependency) = crates
                .iter()
                .find(|candidate| candidate.name == *dependency_name)
            else {
                continue;
            };
            for exported in quill_hir::exported_functions(dependency) {
                signatures.push(typeck_signature(
                    format!("{alias}::{}", exported.path),
                    &exported.signature,
                    Vec::new(),
                ));
            }
        }
    }
    signatures.sort();
    signatures.dedup();
    signatures
}

fn typeck_signature(
    path: String,
    signature: &quill_hir::FunctionSignature,
    scope: Vec<String>,
) -> quill_typeck::ExternalFunctionSignature {
    quill_typeck::ExternalFunctionSignature {
        scope,
        path,
        parameter_types: signature.parameter_types.clone(),
        return_type: signature.return_type.clone(),
        generic_parameters: signature.generic_parameters.clone(),
        generic_copy_parameters: signature.generic_copy_parameters.clone(),
        generic_as_ref_str_parameters: signature.generic_as_ref_str_parameters.clone(),
        generic_add_parameters: signature.generic_add_parameters.clone(),
        generic_has_bounds: signature.generic_has_bounds,
    }
}

fn typeck_struct_signatures(file: &ParsedSource) -> Vec<quill_typeck::StructSignature> {
    typeck_struct_signatures_with_prefix(file, "")
}

fn typeck_struct_signatures_with_prefix(
    file: &ParsedSource,
    crate_prefix: &str,
) -> Vec<quill_typeck::StructSignature> {
    let mut signatures = file
        .parse
        .structs
        .iter()
        .map(|structure| {
            let mut path = Vec::new();
            if !crate_prefix.is_empty() {
                path.push(crate_prefix.to_owned());
            }
            path.extend(file.module_path.clone());
            path.extend(
                structure
                    .scope
                    .iter()
                    .filter_map(|scope| scope.strip_prefix("mod:").map(str::to_owned)),
            );
            path.push(structure.name.clone());
            quill_typeck::StructSignature {
                path: path.join("::"),
                generic_parameters: structure.generic_parameters.clone(),
                derived_traits: structure.derived_traits.clone(),
                fields: structure
                    .fields
                    .iter()
                    .enumerate()
                    .filter_map(|field| {
                        let (index, field) = field;
                        Some(quill_typeck::StructFieldSignature {
                            // Tuple-struct fields have no source name. Keep
                            // their Rust positional field number for `.0`, `.1`, etc.
                            name: field.name.clone().unwrap_or_else(|| index.to_string()),
                            type_text: field.type_text.clone(),
                        })
                    })
                    .collect(),
                is_enum: false,
                enum_variants: Vec::new(),
            }
        })
        .collect::<Vec<_>>();
    signatures.extend(typeck_enum_signatures_with_prefix(file, crate_prefix));
    signatures
}

fn typeck_enum_signatures_with_prefix(
    file: &ParsedSource,
    crate_prefix: &str,
) -> Vec<quill_typeck::StructSignature> {
    file.parse
        .enums
        .iter()
        .map(|item| quill_typeck::StructSignature {
            path: canonical_enum_path(file, item, crate_prefix),
            generic_parameters: Vec::new(),
            derived_traits: item.derived_traits.clone(),
            fields: Vec::new(),
            is_enum: true,
            enum_variants: item
                .variants
                .iter()
                .map(|variant| quill_typeck::EnumVariantSignature {
                    name: variant.name.clone(),
                    discriminant: variant.discriminant,
                })
                .collect(),
        })
        .collect()
}

fn build_subset_command(arguments: Vec<String>) -> ExitCode {
    if arguments.len() != 3 || arguments[1] != "-o" {
        eprintln!("usage: quillc build-subset <main.rs> -o <program>");
        return ExitCode::from(2);
    }
    match compile_subset_main(&arguments[0], &arguments[2]) {
        Ok(()) => {
            println!("built {}", arguments[2]);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("quillc: {error}");
            ExitCode::FAILURE
        }
    }
}

fn build_library_object_command(arguments: Vec<String>) -> ExitCode {
    if arguments.len() != 3 || arguments[1] != "-o" {
        eprintln!("usage: quillc build-lib-object <lib.rs> -o <lib.o>");
        return ExitCode::from(2);
    }
    match compile_library_object(&arguments[0], &arguments[2]) {
        Ok(()) => {
            println!("built {}", arguments[2]);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("quillc: {error}");
            ExitCode::FAILURE
        }
    }
}

fn compile_library_object(source_path: &str, output_path: &str) -> Result<(), String> {
    let source = quill_source::SourceFile::load(source_path)
        .map_err(|error| format!("could not read library source `{source_path}`: {error}"))?;
    let parsed = quill_syntax::parse(&source);
    if parsed.had_error {
        return Err(parsed
            .diagnostics
            .iter()
            .map(|diagnostic| {
                let (line, column) = source.line_column(diagnostic.span.start);
                format!(
                    "{}:{line}:{column}: {}",
                    source.path().display(),
                    diagnostic.message
                )
            })
            .collect::<Vec<_>>()
            .join("\n"));
    }
    let parsed_source = ParsedSource {
        source: source.clone(),
        module_path: Vec::new(),
        parse: parsed,
        owner: None,
        summary_only: false,
    };
    let diagnostics = quill_typeck::check_functions_strict_with_structs(
        &parsed_source.parse.functions,
        &typeck_struct_signatures(&parsed_source),
    );
    if !diagnostics.is_empty() {
        return Err(diagnostics
            .iter()
            .map(|diagnostic| {
                let (line, column) = source.line_column(diagnostic.span.start);
                format!(
                    "{}:{line}:{column}: {}",
                    source.path().display(),
                    diagnostic.message
                )
            })
            .collect::<Vec<_>>()
            .join("\n"));
    }
    let object = quill_codegen::emit_subset_library_object(&parsed_source.parse)
        .map_err(|error| error.to_string())?;
    std::fs::write(output_path, object)
        .map_err(|error| format!("could not write library object `{output_path}`: {error}"))
}

fn cargo_build_subset_command(arguments: Vec<String>) -> ExitCode {
    let mut manifest = None;
    let mut bin_target = None;
    let mut output = None;
    let mut feature_selection = quill_cargo::CargoFeatureSelection::default();
    let mut index = 0;
    let mut invalid = false;
    while index < arguments.len() {
        if let Some(features) = arguments[index].strip_prefix("--features=") {
            if features.is_empty() {
                invalid = true;
                break;
            }
            feature_selection.features.push(features.to_owned());
            index += 1;
            continue;
        }
        match arguments[index].as_str() {
            "--bin" | "-o" | "--features" => {
                let option = arguments[index].as_str();
                index += 1;
                let Some(value) = arguments.get(index) else {
                    invalid = true;
                    break;
                };
                match option {
                    "--bin" if bin_target.is_none() => bin_target = Some(value.clone()),
                    "-o" if output.is_none() => output = Some(value.clone()),
                    "--features" => feature_selection.features.push(value.clone()),
                    _ => invalid = true,
                }
            }
            "--no-default-features" if !feature_selection.no_default_features => {
                feature_selection.no_default_features = true;
            }
            "--all-features" if !feature_selection.all_features => {
                feature_selection.all_features = true;
            }
            option if option.starts_with('-') => invalid = true,
            value if manifest.is_none() => manifest = Some(value.to_owned()),
            _ => invalid = true,
        }
        if invalid {
            break;
        }
        index += 1;
    }
    let (Some(manifest), Some(output)) = (manifest, output) else {
        eprintln!(
            "usage: quillc cargo-build-subset <Cargo.toml> [--bin <target>] [--features <features>] [--no-default-features] [--all-features] -o <program>"
        );
        return ExitCode::from(2);
    };
    if invalid {
        eprintln!(
            "usage: quillc cargo-build-subset <Cargo.toml> [--bin <target>] [--features <features>] [--no-default-features] [--all-features] -o <program>"
        );
        return ExitCode::from(2);
    }
    let result = if feature_selection == quill_cargo::CargoFeatureSelection::default() {
        bin_target.map_or_else(
            || compile_cargo_subset(&manifest, &output),
            |bin| compile_cargo_subset_for_target(&manifest, &output, Some(&bin)),
        )
    } else {
        load_workspace_with_feature_selection(&manifest, &feature_selection).and_then(|workspace| {
            match bin_target.as_deref() {
                Some(bin) => compile_cargo_subset_for_target_with_workspace(
                    &manifest,
                    &output,
                    Some(bin),
                    &workspace,
                ),
                None => compile_cargo_subset_with_workspace(&manifest, &output, &workspace),
            }
        })
    };
    match result {
        Ok(()) => {
            println!("built {output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("quillc: {error}");
            ExitCode::FAILURE
        }
    }
}

fn compile_cargo_subset(manifest: &str, output: &str) -> Result<(), String> {
    let mut profile_timer = profile_timer();
    if restore_cached_cargo_build(manifest, output) {
        profile_checkpoint("restore warm Cargo build", &mut profile_timer);
        return Ok(());
    }
    profile_checkpoint("warm Cargo build cache miss", &mut profile_timer);
    let workspace = load_cargo_workspace(manifest)?;
    profile_checkpoint("load Cargo workspace", &mut profile_timer);
    compile_cargo_subset_with_workspace(manifest, output, &workspace)
}

fn compile_cargo_subset_with_workspace(
    manifest: &str,
    output: &str,
    workspace: &quill_cargo::CargoWorkspace,
) -> Result<(), String> {
    let mut profile_timer = profile_timer();
    let binary_names: Vec<_> = workspace
        .packages
        .iter()
        .filter(|package| package.is_default_member)
        .flat_map(|package| {
            package
                .targets
                .iter()
                .filter(|target| target.kinds.iter().any(|kind| kind == "bin"))
                .map(|target| target.name.clone())
        })
        .collect();
    profile_checkpoint("discover Cargo binary targets", &mut profile_timer);
    match binary_names.as_slice() {
        [_] => compile_cargo_subset_for_target_with_workspace(manifest, output, None, workspace),
        [] => Err("cargo-build-subset found no default-member binary targets".into()),
        names => {
            std::fs::create_dir_all(output).map_err(|error| {
                format!("could not create output directory `{output}`: {error}")
            })?;
            for name in names {
                let binary_output = Path::new(output).join(name);
                compile_cargo_subset_for_target_with_workspace(
                    manifest,
                    &binary_output.display().to_string(),
                    Some(name),
                    workspace,
                )?;
            }
            Ok(())
        }
    }
}

fn load_workspace_with_feature_selection(
    manifest: &str,
    selection: &quill_cargo::CargoFeatureSelection,
) -> Result<quill_cargo::CargoWorkspace, String> {
    quill_cargo::load_workspace_with_features(manifest, selection)
        .map_err(|error| error.to_string())
}

fn run_cargo_build_script(
    target: &quill_cargo::CargoTarget,
    package: &quill_cargo::CargoPackage,
    cfg: &quill_cfg::CfgSet,
) -> Result<quill_cfg::CfgSet, String> {
    let id = BUILD_SCRIPT_ID.fetch_add(1, Ordering::Relaxed);
    let temporary_root =
        std::env::temp_dir().join(format!("quill-build-script-{}-{id}", std::process::id()));
    let executable = temporary_root.join("build-script");
    let out_dir = temporary_root.join("out");
    std::fs::create_dir_all(&out_dir)
        .map_err(|error| format!("could not create build-script output directory: {error}"))?;
    let compile_result = compile_subset_main_with_cfg(
        &target.source_path.display().to_string(),
        &executable.display().to_string(),
        cfg,
    );
    if let Err(error) = compile_result {
        let _ = std::fs::remove_dir_all(&temporary_root);
        return Err(format!("could not compile package build script: {error}"));
    }
    let manifest_dir = package
        .manifest_path
        .parent()
        .ok_or_else(|| "package manifest has no parent directory".to_owned())?;
    let output = std::process::Command::new(&executable)
        .current_dir(manifest_dir)
        .env("CARGO_MANIFEST_DIR", manifest_dir)
        .env("CARGO_MANIFEST_PATH", &package.manifest_path)
        .env("CARGO_PKG_NAME", &package.name)
        .env("CARGO_PKG_VERSION", &package.version)
        .env("OUT_DIR", &out_dir)
        .env("CARGO_CFG_TARGET_ARCH", "x86_64")
        .env("CARGO_CFG_TARGET_OS", "linux")
        .env("CARGO_CFG_TARGET_ENV", "")
        .env("CARGO_CFG_TARGET_ENDIAN", "little")
        .env("CARGO_CFG_TARGET_POINTER_WIDTH", "64")
        .env("CARGO_CFG_UNIX", "")
        .output()
        .map_err(|error| format!("could not run package build script: {error}"));
    let _ = std::fs::remove_dir_all(&temporary_root);
    let output = output?;
    if !output.status.success() {
        return Err(format!(
            "package build script failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut build_cfg = quill_cfg::CfgSet::default();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((directive, value)) = line.split_once('=') else {
            continue;
        };
        match directive.trim() {
            "cargo:rustc-cfg" | "cargo::rustc-cfg" => {
                build_cfg.extend(quill_cfg::CfgSet::parse_rustc_cfg(value));
            }
            "cargo:rustc-check-cfg"
            | "cargo::rustc-check-cfg"
            | "cargo:rerun-if-env-changed"
            | "cargo::rerun-if-env-changed"
            | "cargo:rerun-if-changed"
            | "cargo::rerun-if-changed" => {}
            _ => {
                return Err(format!(
                    "package build script directive `{directive}` is outside the native subset"
                ));
            }
        }
    }
    Ok(build_cfg)
}

fn compile_cargo_subset_for_target(
    manifest: &str,
    output: &str,
    selected_bin: Option<&str>,
) -> Result<(), String> {
    let workspace = load_cargo_workspace(manifest)?;
    compile_cargo_subset_for_target_with_workspace(manifest, output, selected_bin, &workspace)
}

fn load_cargo_workspace(manifest: &str) -> Result<quill_cargo::CargoWorkspace, String> {
    if let Some((workspace_root, _)) = fast_workspace_paths(manifest) {
        quill_cargo::load_workspace_cached(manifest, cargo_cache_root(&workspace_root))
    } else {
        quill_cargo::load_workspace(manifest)
    }
    .map_err(|error| error.to_string())
}

fn compile_cargo_subset_for_target_with_workspace(
    manifest: &str,
    output: &str,
    selected_bin: Option<&str>,
    workspace: &quill_cargo::CargoWorkspace,
) -> Result<(), String> {
    let mut profile_timer = profile_timer();
    profile_checkpoint("begin target compilation", &mut profile_timer);
    let candidates: Vec<_> = workspace
        .packages
        .iter()
        .filter(|package| package.is_default_member)
        .flat_map(|package| {
            package
                .targets
                .iter()
                .filter(|target| target.kinds.iter().any(|kind| kind == "bin"))
                .filter(|target| selected_bin.is_none_or(|name| target.name == name))
                .map(move |target| (package, target))
        })
        .collect();
    if candidates.len() != 1 {
        return Err(format!(
            "cargo-build-subset requires exactly one matching default-member binary target (found {})",
            candidates.len(),
        ));
    }
    profile_checkpoint("select Cargo binary target", &mut profile_timer);
    let (package, target) = candidates[0];
    let package_build_targets: Vec<_> = package
        .targets
        .iter()
        .filter(|candidate| {
            candidate
                .kinds
                .iter()
                .any(|kind| matches!(kind.as_str(), "bin" | "lib" | "proc-macro" | "custom-build"))
        })
        .collect();
    let package_libraries: Vec<_> = package_build_targets
        .iter()
        .filter(|target| target.kinds.iter().any(|kind| kind == "lib"))
        .copied()
        .collect();
    let package_build_scripts: Vec<_> = package_build_targets
        .iter()
        .filter(|target| target.kinds.iter().any(|kind| kind == "custom-build"))
        .copied()
        .collect();
    let has_unsupported_target = package_build_targets
        .iter()
        .any(|target| target.kinds.iter().any(|kind| kind == "proc-macro"));
    if has_unsupported_target || package_libraries.len() > 1 || package_build_scripts.len() > 1 {
        return Err(
            "cargo-build-subset accepts selected binary targets, at most one library target, and at most one build script per package"
                .into(),
        );
    }
    let mut package_cfg = package.cfg.clone();
    let mut cfg_digest = workspace_cfg_digest(workspace);
    if let Some(build_script) = package_build_scripts.first() {
        let build_cfg = run_cargo_build_script(build_script, package, &package_cfg)?;
        package_cfg.extend(build_cfg);
        let mut hasher = blake3::Hasher::new();
        hasher.update(&cfg_digest);
        hasher.update(&package_cfg.fingerprint());
        cfg_digest = *hasher.finalize().as_bytes();
    }
    let mut roots = vec![(
        CrateTargetId {
            package: package.id.clone(),
            target: target.name.clone(),
            cfg: package_cfg.clone(),
            edition: target.edition.clone(),
        },
        target.source_path.display().to_string(),
    )];
    let mut crate_prefixes = HashMap::new();
    if let Some(library) = package_libraries.first() {
        let owner = CrateTargetId {
            package: package.id.clone(),
            target: library.name.clone(),
            cfg: package_cfg,
            edition: library.edition.clone(),
        };
        crate_prefixes.insert(owner.key(), package.name.replace('-', "_"));
        roots.push((owner, library.source_path.display().to_string()));
    }
    let mut seen_dependencies = HashSet::new();
    let mut pending_dependencies = package.dependencies.clone();
    while let Some(dependency) = pending_dependencies.pop() {
        if !dependency.is_workspace_member {
            return Err(format!(
                "dependency `{}` is outside this workspace and cannot be compiled natively yet",
                dependency.package_name
            ));
        }
        if !seen_dependencies.insert(dependency.package_id.clone()) {
            continue;
        }
        let dependency_package = workspace
            .packages
            .iter()
            .find(|candidate| candidate.id == dependency.package_id)
            .ok_or_else(|| {
                format!(
                    "resolved workspace package `{}` was not found",
                    dependency.package_name
                )
            })?;
        pending_dependencies.extend(dependency_package.dependencies.iter().cloned());
        let libraries: Vec<_> = dependency_package
            .targets
            .iter()
            .filter(|candidate| candidate.kinds.iter().any(|kind| kind == "lib"))
            .collect();
        let other_build_targets = dependency_package.targets.iter().any(|candidate| {
            candidate
                .kinds
                .iter()
                .any(|kind| matches!(kind.as_str(), "bin" | "proc-macro" | "custom-build"))
        });
        if libraries.len() != 1 || other_build_targets {
            return Err(format!(
                "dependency `{}` must have exactly one ordinary library target",
                dependency.package_name
            ));
        }
        let library = libraries[0];
        let owner = CrateTargetId {
            package: dependency_package.id.clone(),
            target: library.name.clone(),
            cfg: dependency_package.cfg.clone(),
            edition: library.edition.clone(),
        };
        crate_prefixes.insert(owner.key(), dependency.crate_name.clone());
        roots.push((owner, library.source_path.display().to_string()));
    }
    let mut pool = None;
    let (parsed_files, module_errors) = parse_cargo_module_trees(&roots, &mut pool, true)
        .map_err(|error| format!("could not read Cargo target sources: {error}"))?;
    profile_checkpoint("parse source graph", &mut profile_timer);
    if !module_errors.is_empty() {
        return Err(module_errors.join("\n"));
    }
    let external_inputs = parsed_files
        .iter()
        .map(|source| source.source.path().to_path_buf())
        .filter(|path| !path.starts_with(&workspace.root))
        .collect::<Vec<_>>();
    let build_cache = if selected_bin.is_none() {
        cargo_build_cache_path(
            &workspace,
            manifest,
            &parsed_files,
            cfg_digest,
            !package_build_scripts.is_empty(),
        )?
    } else {
        None
    };
    profile_checkpoint("compute semantic cache key", &mut profile_timer);
    if let Some(cache_path) = build_cache
        .as_ref()
        .filter(|cache_path| is_cached_executable(&cache_path.path))
    {
        if let Some(parent) = Path::new(output).parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "could not create output directory {}: {error}",
                    parent.display()
                )
            })?;
        }
        if std::fs::copy(&cache_path.path, output).is_ok() {
            let _ = write_build_artifact_index(
                &workspace.root,
                manifest,
                cache_path,
                external_inputs,
                output,
            );
            return Ok(());
        }
    }
    let fast_object_cache_paths = build_cache
        .as_ref()
        .map(|artifact| {
            cargo_source_object_cache_paths(
                &workspace,
                &parsed_files,
                &artifact.semantic_inputs,
                &crate_prefixes,
                cargo_cache_root(&workspace.root).join("package-source-objects-v1"),
                artifact.cfg_digest,
            )
        })
        .transpose()?;
    profile_checkpoint("compute source object cache keys", &mut profile_timer);
    let object_cache_root = cargo_cache_root(&workspace.root).join("package-objects-v1");
    compile_parsed_sources(
        parsed_files,
        output,
        &crate_prefixes,
        Some(&object_cache_root),
        fast_object_cache_paths.as_ref(),
    )?;
    profile_checkpoint("type-check, codegen, and link", &mut profile_timer);
    if let Some(cache_path) = build_cache.filter(|_| selected_bin.is_none()) {
        let _ = cache_compiled_executable(output, &cache_path.path);
        let _ = write_build_artifact_index(
            &workspace.root,
            manifest,
            &cache_path,
            external_inputs,
            output,
        );
    }
    Ok(())
}

fn restore_cached_cargo_build(manifest: &str, output: &str) -> bool {
    let mut profile_timer = profile_timer();
    let Some((workspace_root, manifest_path)) = fast_workspace_paths(manifest) else {
        return false;
    };
    profile_checkpoint("warm locate workspace", &mut profile_timer);
    let cache_root = cargo_cache_root(&workspace_root);
    let index_path = build_artifact_index_path(&cache_root, &manifest_path);
    let Ok(index_bytes) = std::fs::read(index_path) else {
        return false;
    };
    let Ok(index) = serde_json::from_slice::<BuildArtifactIndex>(&index_bytes) else {
        return false;
    };
    profile_checkpoint("warm read and decode index", &mut profile_timer);
    if index.workspace_root != workspace_root || index.manifest != manifest_path {
        return false;
    }
    if index.uses_build_script {
        return false;
    }
    let Ok(executable) = std::env::current_exe() else {
        return false;
    };
    let Some(compiler) = file_stamp(&executable) else {
        return false;
    };
    if index.compiler != compiler || index.environment != build_environment() {
        return false;
    }
    profile_checkpoint("warm validate compiler and environment", &mut profile_timer);
    let Some(inputs) = build_input_stamps(&workspace_root, &index.external_inputs) else {
        return false;
    };
    profile_checkpoint("warm scan input stamps", &mut profile_timer);
    if index.inputs != inputs || !is_cached_executable(&index.artifact) {
        return false;
    }
    profile_checkpoint("warm validate cached executable", &mut profile_timer);
    if let Ok(output_path) = std::fs::canonicalize(output)
        && index.output_path.as_ref() == Some(&output_path)
        && index.output_stamp.as_ref() == file_stamp(&output_path).as_ref()
    {
        profile_checkpoint("warm reuse existing output", &mut profile_timer);
        return true;
    }
    if let Some(parent) = Path::new(output).parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return false;
    }
    let copied = std::fs::copy(&index.artifact, output).is_ok();
    profile_checkpoint("warm copy output", &mut profile_timer);
    if copied {
        let artifact = CargoBuildArtifact {
            path: index.artifact,
            cfg_digest: index.cfg_digest,
            uses_build_script: index.uses_build_script,
            semantic_inputs: index.semantic_inputs,
        };
        let _ = write_build_artifact_index(
            &workspace_root,
            manifest,
            &artifact,
            index.external_inputs,
            output,
        );
    }
    copied
}

fn fast_workspace_paths(manifest: &str) -> Option<(PathBuf, PathBuf)> {
    let manifest_path = std::fs::canonicalize(manifest).ok()?;
    for directory in manifest_path.parent()?.ancestors() {
        let candidate = directory.join("Cargo.toml");
        if let Ok(contents) = std::fs::read_to_string(&candidate)
            && contents.lines().any(|line| line.trim() == "[workspace]")
        {
            return Some((directory.to_path_buf(), manifest_path));
        }
    }
    None
}

fn cargo_cache_root(workspace_root: &Path) -> PathBuf {
    std::env::var_os("QUILL_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target/quill-query-cache"))
}

fn build_artifact_index_path(cache_root: &Path, manifest: &Path) -> PathBuf {
    let digest = blake3::hash(manifest.to_string_lossy().as_bytes());
    cache_root
        .join("build-artifacts")
        .join(format!("index-{}.json", digest.to_hex()))
}

fn build_environment() -> Vec<(String, Option<String>)> {
    [
        "CARGO_BUILD_TARGET",
        "CARGO_HOME",
        "CARGO_TARGET_DIR",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTC",
        "RUSTFLAGS",
    ]
    .into_iter()
    .map(|name| {
        (
            name.to_owned(),
            std::env::var_os(name).map(|value| value.to_string_lossy().into_owned()),
        )
    })
    .collect()
}

fn cargo_source_object_cache_paths(
    workspace: &quill_cargo::CargoWorkspace,
    parsed_files: &[ParsedSource],
    semantic_inputs: &[(PathBuf, [u8; 32])],
    crate_prefixes: &HashMap<String, String>,
    cache_root: PathBuf,
    cfg_digest: [u8; 32],
) -> Result<HashMap<String, PathBuf>, String> {
    let semantic_by_path: HashMap<_, _> = semantic_inputs
        .iter()
        .map(|(path, digest)| (path.as_path(), digest))
        .collect();
    let packages_by_id: HashMap<_, _> = workspace
        .packages
        .iter()
        .map(|package| (package.id.as_str(), package))
        .collect();
    let mut owners = HashMap::<String, &CrateTargetId>::new();
    for file in parsed_files {
        if let Some(owner) = &file.owner {
            owners.insert(owner.key(), owner);
        }
    }
    let compiler = std::env::current_exe()
        .ok()
        .and_then(|path| file_stamp(&path))
        .ok_or_else(|| "could not stamp compiler for package object cache".to_owned())?;
    let environment = build_environment();
    let mut build_config_hasher = blake3::Hasher::new();
    let build_inputs = build_input_stamps(&workspace.root, &[])
        .ok_or_else(|| "could not enumerate build configuration for object cache".to_owned())?;
    for input in build_inputs.iter().filter(|input| {
        !input
            .path
            .extension()
            .is_some_and(|extension| extension == "rs")
    }) {
        hash_component(
            &mut build_config_hasher,
            input.path.to_string_lossy().as_bytes(),
        );
        if hash_file_into(&mut build_config_hasher, &input.path).is_err() {
            return Err(format!(
                "could not read build input `{}` for package object cache",
                input.path.display()
            ));
        }
    }
    let build_config_digest = build_config_hasher.finalize();
    let mut paths = HashMap::new();
    for (target_key, owner) in owners {
        let mut package_closure = HashSet::<String>::new();
        let mut pending = vec![owner.package.clone()];
        while let Some(package_id) = pending.pop() {
            if !package_closure.insert(package_id.clone()) {
                continue;
            }
            if let Some(package) = packages_by_id.get(package_id.as_str()) {
                pending.extend(
                    package
                        .dependencies
                        .iter()
                        .filter(|dependency| dependency.is_workspace_member)
                        .map(|dependency| dependency.package_id.clone()),
                );
            }
        }
        let mut hasher = blake3::Hasher::new();
        hash_component(&mut hasher, b"quill-cargo-source-object-cache-v1");
        hash_component(&mut hasher, target_key.as_bytes());
        hash_component(
            &mut hasher,
            crate_prefixes
                .get(&target_key)
                .map(String::as_bytes)
                .unwrap_or_default(),
        );
        hasher.update(&owner.cfg.fingerprint());
        hasher.update(&cfg_digest);
        hasher.update(build_config_digest.as_bytes());
        hash_component(
            &mut hasher,
            &bincode::serialize(&compiler).map_err(|error| error.to_string())?,
        );
        hash_component(
            &mut hasher,
            &bincode::serialize(&environment).map_err(|error| error.to_string())?,
        );
        let mut package_ids: Vec<_> = package_closure.iter().collect();
        package_ids.sort();
        for package_id in package_ids {
            hash_component(&mut hasher, package_id.as_bytes());
            if let Some(package) = packages_by_id.get(package_id.as_str()) {
                hash_component(
                    &mut hasher,
                    &bincode::serialize(&package.cfg).map_err(|error| error.to_string())?,
                );
                let mut dependencies = package.dependencies.iter().collect::<Vec<_>>();
                dependencies.sort_by(|left, right| {
                    (&left.package_id, &left.crate_name)
                        .cmp(&(&right.package_id, &right.crate_name))
                });
                for dependency in dependencies {
                    hash_component(&mut hasher, dependency.package_id.as_bytes());
                    hash_component(&mut hasher, dependency.crate_name.as_bytes());
                }
                hash_component(
                    &mut hasher,
                    &bincode::serialize(&package.targets).map_err(|error| error.to_string())?,
                );
            }
        }
        let mut files = parsed_files
            .iter()
            .filter(|file| {
                file.owner
                    .as_ref()
                    .is_some_and(|file_owner| package_closure.contains(&file_owner.package))
            })
            .collect::<Vec<_>>();
        files.sort_by(|left, right| left.source.path().cmp(right.source.path()));
        for file in files {
            let digest = semantic_by_path.get(file.source.path()).ok_or_else(|| {
                format!(
                    "semantic digest missing for package source `{}`",
                    file.source.path().display()
                )
            })?;
            hash_component(&mut hasher, file.source.path().to_string_lossy().as_bytes());
            hasher.update(*digest);
            hash_component(
                &mut hasher,
                &bincode::serialize(&file.module_path).map_err(|error| error.to_string())?,
            );
            if let Some(file_owner) = &file.owner {
                hasher.update(&file_owner.cfg.fingerprint());
            }
        }
        let key = hasher.finalize().to_hex().to_string();
        paths.insert(target_key, cache_root.join(format!("{key}.o")));
    }
    Ok(paths)
}

fn write_cached_object(path: &Path, object: &[u8]) {
    if !object.starts_with(b"\x7fELF") {
        return;
    }
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&temporary, object).is_ok() {
        let _ = std::fs::rename(temporary, path);
    }
}

fn build_input_stamps(
    workspace_root: &Path,
    external_inputs: &[PathBuf],
) -> Option<Vec<BuildFileStamp>> {
    let mut paths = Vec::new();
    collect_cargo_build_inputs(workspace_root, &mut paths).ok()?;
    paths.extend(external_inputs.iter().cloned());
    let invocation_directory = std::env::current_dir().ok()?;
    for search_root in [workspace_root, invocation_directory.as_path()] {
        for ancestor in search_root.ancestors() {
            for toolchain_name in ["rust-toolchain.toml", "rust-toolchain"] {
                let toolchain = ancestor.join(toolchain_name);
                if toolchain.is_file() {
                    paths.push(toolchain);
                }
            }
            for config_name in ["config.toml", "config"] {
                let config = ancestor.join(".cargo").join(config_name);
                if config.is_file() {
                    paths.push(config);
                }
            }
        }
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
    if let Some(cargo_home) = cargo_home {
        for config_name in ["config.toml", "config"] {
            let config = cargo_home.join(config_name);
            if config.is_file() {
                paths.push(config);
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths.into_iter().map(|path| file_stamp(&path)).collect()
}

fn file_stamp(path: &Path) -> Option<BuildFileStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(BuildFileStamp {
        path: path.to_path_buf(),
        size: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn write_build_artifact_index(
    workspace_root: &Path,
    manifest: &str,
    artifact: &CargoBuildArtifact,
    external_inputs: Vec<PathBuf>,
    output: &str,
) -> std::io::Result<()> {
    let manifest_path = std::fs::canonicalize(manifest)?;
    let Some(compiler) = std::env::current_exe()
        .ok()
        .and_then(|path| file_stamp(&path))
    else {
        return Ok(());
    };
    let Some(inputs) = build_input_stamps(workspace_root, &external_inputs) else {
        return Ok(());
    };
    let output_path = std::fs::canonicalize(output).ok();
    let output_stamp = output_path.as_deref().and_then(file_stamp);
    let index = BuildArtifactIndex {
        workspace_root: workspace_root.to_path_buf(),
        manifest: manifest_path.clone(),
        compiler,
        environment: build_environment(),
        uses_build_script: artifact.uses_build_script,
        cfg_digest: artifact.cfg_digest,
        inputs,
        external_inputs,
        semantic_inputs: artifact.semantic_inputs.clone(),
        artifact: artifact.path.clone(),
        output_path,
        output_stamp,
    };
    let cache_root = cargo_cache_root(workspace_root);
    let path = build_artifact_index_path(&cache_root, &manifest_path);
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(
        &temporary,
        serde_json::to_vec(&index).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(temporary, path)
}

fn cargo_build_cache_path(
    workspace: &quill_cargo::CargoWorkspace,
    manifest: &str,
    parsed_sources: &[ParsedSource],
    cfg_digest: [u8; 32],
    uses_build_script: bool,
) -> Result<Option<CargoBuildArtifact>, String> {
    let workspace_root = &workspace.root;
    let cache_root = std::env::var_os("QUILL_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target/quill-query-cache"));
    let external_inputs = parsed_sources
        .iter()
        .map(|source| source.source.path().to_path_buf())
        .filter(|path| !path.starts_with(workspace_root))
        .collect::<Vec<_>>();
    let Some(inputs) = build_input_stamps(workspace_root, &external_inputs) else {
        return Ok(None);
    };
    let Ok(executable) = std::env::current_exe() else {
        return Ok(None);
    };
    let Some(compiler_stamp) = file_stamp(&executable) else {
        return Ok(None);
    };
    let environment = build_environment();
    let Ok(manifest_path) = std::fs::canonicalize(manifest) else {
        return Ok(None);
    };

    let index_path = build_artifact_index_path(&cache_root, &manifest_path);
    let previous_index = std::fs::read(&index_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BuildArtifactIndex>(&bytes).ok())
        .filter(|index| {
            index.workspace_root == *workspace_root
                && index.manifest == manifest_path
                && index.compiler == compiler_stamp
                && index.environment == environment
                && index.cfg_digest == cfg_digest
        });
    let previous_stamps = previous_index
        .as_ref()
        .map(|index| {
            index
                .inputs
                .iter()
                .map(|stamp| (stamp.path.clone(), stamp))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let previous_semantics = previous_index
        .as_ref()
        .map(|index| {
            index
                .semantic_inputs
                .iter()
                .cloned()
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let current_stamps = inputs
        .iter()
        .map(|stamp| (stamp.path.clone(), stamp))
        .collect::<HashMap<_, _>>();
    let changed_sources = parsed_sources
        .iter()
        .filter(|source| {
            let path = source.source.path();
            !previous_stamps
                .get(path)
                .zip(current_stamps.get(path))
                .is_some_and(|(previous, current)| *previous == *current)
        })
        .map(|source| source.source.path().to_path_buf())
        .collect::<HashSet<_>>();

    let semantic_digests = {
        let compute_digest = |source: &ParsedSource| {
            let path = source.source.path();
            let unchanged = previous_stamps
                .get(path)
                .zip(current_stamps.get(path))
                .is_some_and(|(previous, current)| *previous == *current);
            if unchanged && let Some(digest) = previous_semantics.get(path) {
                Ok(*digest)
            } else {
                parse_semantic_digest(source)
            }
        };
        if let Some(pool) = quill_parallel::WorkPool::shared_typeck() {
            pool.map(parsed_sources, compute_digest)
        } else {
            parsed_sources.iter().map(compute_digest).collect()
        }
    };
    let mut semantic_inputs = Vec::with_capacity(parsed_sources.len());
    for (source, semantic_digest) in parsed_sources.iter().zip(semantic_digests) {
        let path = source.source.path().to_path_buf();
        semantic_inputs.push((path, semantic_digest?));
    }
    semantic_inputs.sort_by(|left, right| left.0.cmp(&right.0));
    let semantic_by_path = semantic_inputs.iter().cloned().collect::<HashMap<_, _>>();

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"quill-cargo-build-cache-v2");
    hash_component(&mut hasher, manifest_path.to_string_lossy().as_bytes());
    hasher.update(&cfg_digest);
    if hash_file_into(&mut hasher, &executable).is_err() {
        return Ok(None);
    }
    for (name, value) in &environment {
        hash_component(&mut hasher, name.as_bytes());
        if let Some(value) = value {
            hash_component(&mut hasher, value.as_bytes());
        } else {
            hash_component(&mut hasher, &[]);
        }
    }
    let parsed_paths = parsed_sources
        .iter()
        .map(|source| (source.source.path().to_path_buf(), source))
        .collect::<HashMap<_, _>>();
    let mut input_paths = inputs
        .into_iter()
        .map(|stamp| stamp.path)
        .collect::<Vec<_>>();
    input_paths.extend(parsed_paths.keys().cloned());
    input_paths.sort();
    input_paths.dedup();
    for input in input_paths {
        hash_component(
            &mut hasher,
            input
                .strip_prefix(workspace_root)
                .unwrap_or(&input)
                .to_string_lossy()
                .as_bytes(),
        );
        if input.extension().is_some_and(|extension| extension == "rs") {
            if let Some(source) = parsed_paths.get(&input) {
                // A prior valid cache entry proves unchanged Rust files have
                // already passed this conservative include-macro guard. Scan
                // only changed files, using the text already loaded by parser.
                if changed_sources.contains(&input) && source.source.text().contains("include") {
                    return Ok(None);
                }
                if let Some(digest) = semantic_by_path.get(&input) {
                    hasher.update(digest);
                }
            } else {
                if hash_file_into(&mut hasher, &input).is_err() {
                    return Ok(None);
                }
            }
        } else if hash_file_into(&mut hasher, &input).is_err() {
            return Ok(None);
        }
    }
    Ok(Some(CargoBuildArtifact {
        path: cache_root
            .join("build-artifacts")
            .join(format!("{}.elf", hasher.finalize().to_hex())),
        cfg_digest,
        uses_build_script,
        semantic_inputs,
    }))
}

fn workspace_cfg_digest(workspace: &quill_cargo::CargoWorkspace) -> [u8; 32] {
    let mut packages = workspace.packages.iter().collect::<Vec<_>>();
    packages.sort_by(|left, right| left.id.cmp(&right.id));
    let mut hasher = blake3::Hasher::new();
    for package in packages {
        hash_component(&mut hasher, package.id.as_bytes());
        hash_component(&mut hasher, format!("{:?}", package.cfg).as_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn parse_semantic_digest(source: &ParsedSource) -> Result<[u8; 32], String> {
    let mut hasher = blake3::Hasher::new();
    hash_component(
        &mut hasher,
        source.source.path().to_string_lossy().as_bytes(),
    );
    hash_component(
        &mut hasher,
        source
            .owner
            .as_ref()
            .map_or_else(String::new, CrateTargetId::key)
            .as_bytes(),
    );
    for component in &source.module_path {
        hash_component(&mut hasher, component.as_bytes());
    }
    hasher.update(b"parse-tree");
    bincode::serialize_into(HasherWriter(&mut hasher), &source.parse)
        .map_err(|_| "could not fingerprint parsed Rust source".to_owned())?;
    Ok(*hasher.finalize().as_bytes())
}

fn hash_component(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hash_file_into(hasher: &mut blake3::Hasher, path: &Path) -> std::io::Result<()> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    hasher.update(&length.to_le_bytes());
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(())
}

fn collect_cargo_build_inputs(root: &Path, inputs: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(root).map_err(|error| {
        format!(
            "could not scan build input directory {}: {error}",
            root.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_dir() {
            if !matches!(entry.file_name().to_str(), Some("target" | ".git")) {
                collect_cargo_build_inputs(&path, inputs)?;
            }
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("Cargo.toml" | "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml")
        ) || path.extension().is_some_and(|extension| extension == "rs")
            || (path
                .parent()
                .is_some_and(|parent| parent.ends_with(".cargo"))
                && name == "config.toml")
        {
            inputs.push(path);
        }
    }
    Ok(())
}

fn is_cached_executable(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    if !file.metadata().is_ok_and(|metadata| metadata.len() > 64) {
        return false;
    }
    let mut magic = [0; 4];
    file.read_exact(&mut magic).is_ok() && magic == *b"\x7fELF"
}

fn cache_compiled_executable(output: &str, cache_path: &Path) -> std::io::Result<()> {
    let Some(parent) = cache_path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)?;
    let temporary = cache_path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::copy(output, &temporary)?;
    match std::fs::rename(&temporary, cache_path) {
        Ok(()) => Ok(()),
        Err(_error) if cache_path.exists() => {
            let _ = std::fs::remove_file(temporary);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(temporary);
            Err(error)
        }
    }
}

fn compile_subset_main(source_path: &str, output_path: &str) -> Result<(), String> {
    compile_subset_main_with_cfg(source_path, output_path, &quill_cfg::CfgSet::default())
}

fn compile_subset_main_with_cfg(
    source_path: &str,
    output_path: &str,
    cfg: &quill_cfg::CfgSet,
) -> Result<(), String> {
    let mut pool = None;
    let (parsed_files, module_errors) = parse_module_trees_with_cfg(source_path, cfg, &mut pool)
        .map_err(|error| format!("could not read crate source: {error}"))?;
    if !module_errors.is_empty() {
        return Err(module_errors.join("\n"));
    }
    compile_parsed_sources(parsed_files, output_path, &HashMap::new(), None, None)
}

fn compile_parsed_sources(
    mut parsed_files: Vec<ParsedSource>,
    output_path: &str,
    crate_prefixes: &HashMap<String, String>,
    object_cache_root: Option<&Path>,
    fast_object_cache_paths: Option<&HashMap<String, PathBuf>>,
) -> Result<(), String> {
    let mut profile_timer = profile_timer();
    let object_compiler_stamp = std::env::current_exe()
        .ok()
        .and_then(|path| file_stamp(&path));
    let mut errors = Vec::new();
    for file in &parsed_files {
        for diagnostic in &file.parse.diagnostics {
            let (line, column) = file.source.line_column(diagnostic.span.start);
            errors.push(format!(
                "{}:{line}:{column}: {}",
                file.source.path().display(),
                diagnostic.message
            ));
        }
        let import_declaration_count: HashSet<_> = file
            .parse
            .imports
            .iter()
            .map(|import| import.span.clone())
            .collect();
        if file.parse.unsupported_item_count
            > file.parse.modules.len() + import_declaration_count.len()
        {
            errors.push(format!(
                "{}: unsupported item outside the native codegen subset",
                file.source.path().display()
            ));
        }
        if file
            .parse
            .functions
            .iter()
            .any(|function| function.scope.iter().any(|scope| scope.starts_with("fn:")))
        {
            errors.push(format!(
                "{}: nested function items are outside the native codegen subset",
                file.source.path().display()
            ));
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }

    let direct_object_paths = fast_object_cache_paths.is_some_and(|paths| {
        parsed_files
            .iter()
            .filter_map(|file| file.owner.as_ref().map(CrateTargetId::key))
            .all(|target| paths.contains_key(&target))
    });
    let mut preloaded_objects = HashMap::<String, Vec<u8>>::new();
    let mut cached_object_paths = HashSet::<String>::new();
    if let Some(paths) = fast_object_cache_paths {
        for (target, path) in paths {
            if direct_object_paths {
                let mut magic = [0; 4];
                use std::io::Read;
                if std::fs::File::open(path)
                    .and_then(|mut file| file.read_exact(&mut magic))
                    .is_ok()
                    && &magic == b"\x7fELF"
                {
                    cached_object_paths.insert(target.clone());
                }
            } else if let Ok(object) = std::fs::read(path)
                && object.starts_with(b"\x7fELF")
            {
                if std::env::var_os("QUILL_PROFILE").is_some() {
                    eprintln!("quill-profile fast package object cache hit: {target}");
                }
                preloaded_objects.insert(target.clone(), object);
            }
        }
    }
    let cached_target_keys: HashSet<_> = if direct_object_paths {
        cached_object_paths
    } else {
        preloaded_objects.keys().cloned().collect()
    };

    for file in &mut parsed_files {
        let Some(owner) = file.owner.as_ref() else {
            continue;
        };
        if file.summary_only && !cached_target_keys.contains(&owner.key()) {
            let cache = quill_query::ParseCache::for_source(&file.source);
            file.parse =
                cache.parse_file_with_cfg_and_edition(&file.source, &owner.cfg, &owner.edition);
            file.summary_only = false;
        }
    }

    let enum_definitions = parsed_files
        .iter()
        .flat_map(|file| {
            let prefix = file
                .owner
                .as_ref()
                .and_then(|owner| crate_prefixes.get(&owner.key()))
                .map(String::as_str)
                .unwrap_or("");
            file.parse.enums.iter().map(move |item| EnumDefinition {
                path: canonical_enum_path(file, item, prefix),
                variants: item
                    .variants
                    .iter()
                    .map(|variant| (variant.name.clone(), variant.discriminant))
                    .collect(),
            })
        })
        .collect::<Vec<_>>();
    let enum_paths = enum_definitions
        .iter()
        .map(|definition| definition.path.as_str())
        .collect::<HashSet<_>>();
    if enum_paths.len() != enum_definitions.len() {
        return Err("duplicate enum paths are outside the native codegen subset".to_owned());
    }

    // The parsed functions are consumed by the compilation pipeline. Moving
    // them avoids recursively cloning every body just to qualify its path.
    let keyed_functions: Vec<Vec<_>> = parsed_files
        .iter_mut()
        .map(|file| {
            let prefix = file
                .owner
                .as_ref()
                .and_then(|owner| crate_prefixes.get(&owner.key()))
                .map(String::as_str)
                .unwrap_or("");
            std::mem::take(&mut file.parse.functions)
                .into_iter()
                .map(|mut function| {
                    function.name = canonical_function_path(file, &function, prefix);
                    function
                })
                .collect()
        })
        .collect();
    let all_keys: HashSet<_> = keyed_functions
        .iter()
        .flatten()
        .map(|function| function.name.clone())
        .collect();
    let function_count = keyed_functions.iter().map(Vec::len).sum::<usize>();
    if all_keys.len() != function_count {
        return Err("duplicate function paths are outside the native codegen subset".into());
    }
    let mut function_prefixes = HashMap::new();
    for (file, functions) in parsed_files.iter().zip(&keyed_functions) {
        let prefix = file
            .owner
            .as_ref()
            .and_then(|owner| crate_prefixes.get(&owner.key()))
            .cloned()
            .unwrap_or_default();
        for function in functions {
            function_prefixes.insert(function.name.clone(), prefix.clone());
        }
    }
    let mut public_modules = HashMap::new();
    for file in &parsed_files {
        let crate_prefix = file
            .owner
            .as_ref()
            .and_then(|owner| crate_prefixes.get(&owner.key()))
            .map(String::as_str)
            .unwrap_or("");
        for module in &file.parse.modules {
            let mut path = Vec::new();
            if !crate_prefix.is_empty() {
                path.push(crate_prefix.to_owned());
            }
            path.extend(file.module_path.clone());
            path.extend(
                module
                    .parent_scope
                    .iter()
                    .filter_map(|scope| scope.strip_prefix("mod:").map(str::to_owned)),
            );
            path.push(module.name.clone());
            public_modules.insert(path.join("::"), module.is_public);
        }
    }
    let mut exported_paths = HashSet::new();
    for (file, functions) in parsed_files.iter().zip(&keyed_functions) {
        let crate_prefix = file
            .owner
            .as_ref()
            .and_then(|owner| crate_prefixes.get(&owner.key()))
            .map(String::as_str)
            .unwrap_or("");
        for keyed_function in functions {
            if !keyed_function.is_public {
                continue;
            }
            let mut module_path = file.module_path.clone();
            module_path.extend(
                keyed_function
                    .scope
                    .iter()
                    .filter_map(|scope| scope.strip_prefix("mod:").map(str::to_owned)),
            );
            let mut visible = true;
            let mut path = Vec::new();
            if !crate_prefix.is_empty() {
                path.push(crate_prefix.to_owned());
            }
            for component in module_path {
                path.push(component);
                if public_modules.get(&path.join("::")) != Some(&true) {
                    visible = false;
                    break;
                }
            }
            if visible {
                exported_paths.insert(keyed_function.name.clone());
            }
        }
    }
    let import_aliases: Vec<_> = parsed_files
        .iter()
        .map(|file| {
            let prefix = file
                .owner
                .as_ref()
                .and_then(|owner| crate_prefixes.get(&owner.key()))
                .map(String::as_str)
                .unwrap_or("");
            module_import_aliases(file, &all_keys, prefix)
        })
        .collect::<Result<_, _>>()?;
    let caller_prefixes: HashSet<_> = parsed_files
        .iter()
        .map(|file| {
            file.owner
                .as_ref()
                .and_then(|owner| crate_prefixes.get(&owner.key()))
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    let visible_paths_by_prefix: HashMap<_, _> = caller_prefixes
        .into_iter()
        .map(|caller_prefix| {
            let mut visible_paths = exported_paths.clone();
            visible_paths.extend(
                function_prefixes
                    .iter()
                    .filter(|(_, prefix)| *prefix == &caller_prefix)
                    .map(|(path, _)| path.clone()),
            );
            (caller_prefix, visible_paths)
        })
        .collect();
    let mut all_signatures = keyed_functions
        .iter()
        .flatten()
        .map(|function| quill_typeck::ExternalFunctionSignature {
            scope: function.scope.clone(),
            path: function.name.clone(),
            parameter_types: function
                .parameters
                .iter()
                .map(|parameter| parameter.type_text.clone())
                .collect(),
            return_type: function.return_type.clone(),
            generic_parameters: function.generic_parameters.clone(),
            generic_copy_parameters: function.generic_copy_parameters.clone(),
            generic_as_ref_str_parameters: function.generic_as_ref_str_parameters.clone(),
            generic_add_parameters: function.generic_add_parameters.clone(),
            generic_has_bounds: function.generic_has_bounds,
        })
        .collect::<Vec<_>>();
    all_signatures.sort();
    // Package-object hits still need an owner for external-call routing, but
    // their non-generic function bodies and interfaces are already present in
    // the object. Build this small routing index before dropping cached ASTs.
    let mut cached_function_owners = HashMap::new();
    for (file, functions) in parsed_files.iter().zip(&keyed_functions) {
        let owner = file
            .owner
            .as_ref()
            .map(CrateTargetId::key)
            .unwrap_or_default();
        for function in functions {
            if exported_paths.contains(&function.name) {
                cached_function_owners.insert(function.name.clone(), owner.clone());
            }
        }
    }
    let file_pool = quill_parallel::WorkPool::for_tasks(parsed_files.len()).ok();
    let mut rewrite_tasks = keyed_functions
        .into_iter()
        .enumerate()
        .map(|(file_index, mut functions)| {
            let cached_target = parsed_files[file_index]
                .owner
                .as_ref()
                .is_some_and(|owner| cached_target_keys.contains(&owner.key()));
            if cached_target {
                // Keep the exported interface for downstream HIR lookup.
                // Non-generic bodies live in the cached object; generic bodies
                // remain available for downstream monomorphization.
                functions.retain(|function| exported_paths.contains(&function.name));
                for function in &mut functions {
                    if !function.is_generic {
                        function.body = None;
                        function.body_span = None;
                    }
                }
            }
            (file_index, functions)
        })
        .collect::<Vec<_>>();
    let rewrite_file = |task: &mut (usize, Vec<quill_syntax::FunctionItem>)| {
        let (file_index, functions) = task;
        let file = &parsed_files[*file_index];
        let crate_prefix = file
            .owner
            .as_ref()
            .and_then(|owner| crate_prefixes.get(&owner.key()))
            .map(String::as_str)
            .unwrap_or("");
        let visible_paths = &visible_paths_by_prefix[crate_prefix];
        let mut resolution_cache = CallResolutionCache::new();
        for function in functions.iter_mut() {
            if function.body.is_none() {
                continue;
            }
            // The parser's call index is collected from the same Rust syntax
            // tree as the body. Bodies without path calls need no resolver
            // traversal; method calls remain untouched by this pass anyway.
            if function.calls.is_empty() && function.qualified_calls.is_empty() {
                continue;
            }
            let mut module_path = file.module_path.clone();
            if let Some(prefix) = file
                .owner
                .as_ref()
                .and_then(|owner| crate_prefixes.get(&owner.key()))
            {
                module_path.insert(0, prefix.clone());
            }
            module_path.extend(
                function
                    .scope
                    .iter()
                    .filter_map(|scope| scope.strip_prefix("mod:").map(str::to_owned)),
            );
            rewrite_calls(
                function,
                &module_path,
                &visible_paths,
                &import_aliases[*file_index],
                &mut resolution_cache,
            )?;
        }
        Ok::<_, String>(())
    };
    let rewrite_results = if let Some(pool) = &file_pool {
        pool.map_mut(&mut rewrite_tasks, rewrite_file)
    } else {
        rewrite_tasks.iter_mut().map(rewrite_file).collect()
    };
    for result in rewrite_results {
        result?;
    }
    let lowered_by_file = rewrite_tasks
        .into_iter()
        .map(|(_, functions)| functions)
        .collect::<Vec<_>>();
    profile_checkpoint("resolve imports and calls", &mut profile_timer);
    let mut all_structs = parsed_files
        .iter()
        .flat_map(|candidate| {
            let prefix = candidate
                .owner
                .as_ref()
                .and_then(|owner| crate_prefixes.get(&owner.key()))
                .map(String::as_str)
                .unwrap_or("");
            typeck_struct_signatures_with_prefix(candidate, prefix)
        })
        .collect::<Vec<_>>();
    all_structs.sort();
    let type_check_interface_digest =
        quill_query::type_check_interface_digest(&all_signatures, &all_structs);
    let check_file = |index: &usize| {
        let file = &parsed_files[*index];
        if file
            .owner
            .as_ref()
            .is_some_and(|owner| cached_target_keys.contains(&owner.key()))
        {
            return Vec::new();
        }
        let caller_prefix = file
            .owner
            .as_ref()
            .and_then(|owner| crate_prefixes.get(&owner.key()))
            .map(String::as_str)
            .unwrap_or("");
        let local_keys: HashSet<_> = lowered_by_file[*index]
            .iter()
            .map(|function| function.name.as_str())
            .collect();
        let external: Vec<_> = all_signatures
            .iter()
            .filter(|signature| !local_keys.contains(signature.path.as_str()))
            .filter(|signature| {
                function_prefixes
                    .get(&signature.path)
                    .is_some_and(|prefix| prefix == caller_prefix)
                    || exported_paths.contains(&signature.path)
            })
            .cloned()
            .collect();
        let cache = quill_query::TypeCheckCache::for_source(&file.source);
        let diagnostics = cache.check_functions_per_body_with_known_interface(
            &file.source,
            &lowered_by_file[*index],
            true,
            &external,
            &all_structs,
            type_check_interface_digest,
        );
        report_type_check_cache_stats(&cache);
        diagnostics
    };
    let type_check_tasks = (0..parsed_files.len()).collect::<Vec<_>>();
    let type_check_results = if let Some(pool) = &file_pool {
        pool.map(&type_check_tasks, check_file)
    } else {
        type_check_tasks.iter().map(check_file).collect()
    };
    let mut type_check_errors = Vec::new();
    for (file, diagnostics) in parsed_files.iter().zip(type_check_results) {
        for diagnostic in diagnostics {
            let (line, column) = file.source.line_column(diagnostic.span.start);
            type_check_errors.push(format!(
                "{}:{line}:{column}: {}",
                file.source.path().display(),
                diagnostic.message
            ));
        }
    }
    if !type_check_errors.is_empty() {
        return Err(type_check_errors.join("\n"));
    }
    profile_checkpoint("parallel type-check", &mut profile_timer);

    let struct_paths: HashSet<_> = parsed_files
        .iter()
        .flat_map(|file| {
            let prefix = file
                .owner
                .as_ref()
                .and_then(|owner| crate_prefixes.get(&owner.key()))
                .map(String::as_str)
                .unwrap_or("");
            file.parse
                .structs
                .iter()
                .map(move |structure| canonical_struct_path(file, structure, prefix))
        })
        .collect();
    let mut canonicalization_tasks = parsed_files.iter().zip(lowered_by_file).collect::<Vec<_>>();
    let canonicalize_file = |task: &mut (&ParsedSource, Vec<quill_syntax::FunctionItem>)| {
        let (file, functions) = task;
        let crate_prefix = file
            .owner
            .as_ref()
            .and_then(|owner| crate_prefixes.get(&owner.key()))
            .map(String::as_str)
            .unwrap_or("");
        for function in functions {
            let mut function_module = Vec::new();
            if !crate_prefix.is_empty() {
                function_module.push(crate_prefix.to_owned());
            }
            function_module.extend(file.module_path.clone());
            function_module.extend(
                function
                    .scope
                    .iter()
                    .filter_map(|scope| scope.strip_prefix("mod:").map(str::to_owned)),
            );
            let impl_owner = function
                .scope
                .iter()
                .find_map(|scope| scope.strip_prefix("impl:"));
            for parameter in &mut function.parameters {
                rewrite_enum_type(&mut parameter.type_text, &function_module, &enum_paths);
            }
            if let Some(return_type) = &mut function.return_type {
                rewrite_enum_type(return_type, &function_module, &enum_paths);
            }
            if let Some(impl_owner) = impl_owner
                && let Some(self_type) =
                    resolve_impl_owner_type(&function_module, impl_owner, &struct_paths)
            {
                for parameter in &mut function.parameters {
                    parameter.type_text = substitute_self_type(&parameter.type_text, &self_type);
                }
                if let Some(return_type) = &mut function.return_type {
                    *return_type = substitute_self_type(return_type, &self_type);
                }
            }
            if let Some(body) = &mut function.body {
                rewrite_enum_references(
                    body,
                    &function_module,
                    &enum_definitions,
                    &enum_paths,
                    impl_owner,
                    &struct_paths,
                );
            }
            // Keep the impl marker until HIR reachability has identified method bodies.
            function.scope = impl_owner
                .map(|owner| vec![format!("impl:{owner}")])
                .unwrap_or_default();
        }
        file.parse
            .structs
            .iter()
            .map(|structure| {
                let canonical_path = canonical_struct_path(file, structure, crate_prefix);
                let mut structure = structure.clone();
                structure.name = canonical_path;
                structure.scope.clear();
                let enum_module: Vec<String> = structure
                    .name
                    .rsplit_once("::")
                    .map(|(module, _)| module.split("::").map(str::to_owned).collect())
                    .unwrap_or_default();
                for field in &mut structure.fields {
                    rewrite_enum_type(&mut field.type_text, &enum_module, &enum_paths);
                }
                structure
            })
            .collect::<Vec<_>>()
    };
    let canonical_structs_by_file = if let Some(pool) = &file_pool {
        pool.map_mut(&mut canonicalization_tasks, canonicalize_file)
    } else {
        canonicalization_tasks
            .iter_mut()
            .map(canonicalize_file)
            .collect()
    };
    let lowered_by_file = canonicalization_tasks
        .into_iter()
        .map(|(_, functions)| functions)
        .collect::<Vec<_>>();
    let mut combined = quill_syntax::Parse::default();
    let mut target_parses = HashMap::<String, quill_syntax::Parse>::new();
    let mut target_order = Vec::<String>::new();
    let mut target_seen = HashSet::<String>::new();
    let mut target_source_fingerprints =
        HashMap::<String, Vec<(String, Vec<String>, [u8; 32], Option<[u8; 32]>)>>::new();
    let mut function_owners = cached_function_owners;
    let mut functions_by_path = HashMap::<String, quill_syntax::FunctionItem>::new();
    let mut target_objects = Vec::<Vec<u8>>::new();
    for ((file, functions), canonical_structs) in parsed_files
        .iter()
        .zip(lowered_by_file)
        .zip(canonical_structs_by_file)
    {
        let owner_key = file
            .owner
            .as_ref()
            .map(CrateTargetId::key)
            .unwrap_or_default();
        if target_seen.insert(owner_key.clone()) {
            target_order.push(owner_key.clone());
            if !cached_target_keys.contains(&owner_key) {
                target_parses.insert(owner_key.clone(), quill_syntax::Parse::default());
            }
        }
        target_source_fingerprints
            .entry(owner_key.clone())
            .or_default()
            .push((
                file.source.path().to_string_lossy().into_owned(),
                file.module_path.clone(),
                *blake3::hash(file.source.text().as_bytes()).as_bytes(),
                file.owner.as_ref().map(|owner| owner.cfg.fingerprint()),
            ));
        let import_count: HashSet<_> = file
            .parse
            .imports
            .iter()
            .map(|import| import.span.clone())
            .collect();
        combined.unsupported_item_count += file
            .parse
            .unsupported_item_count
            .saturating_sub(file.parse.modules.len() + import_count.len());
        for function in &functions {
            if exported_paths.contains(&function.name) {
                function_owners.insert(function.name.clone(), owner_key.clone());
                let mut interface_function = function.clone();
                if !interface_function.is_generic {
                    interface_function.body = None;
                    interface_function.body_span = None;
                }
                functions_by_path.insert(function.name.clone(), interface_function);
            }
        }
        if !cached_target_keys.contains(&owner_key) {
            target_parses
                .get_mut(&owner_key)
                .expect("target parse was initialized above")
                .functions
                .extend(functions);
        }
        combined.structs.extend(canonical_structs);
    }
    profile_checkpoint("prepare combined HIR input", &mut profile_timer);
    if !target_order.is_empty() {
        let all_structs = combined.structs.clone();
        let mut target_objects_by_key = preloaded_objects;
        let mut target_tasks = Vec::new();
        for target in &target_order {
            if cached_target_keys.contains(target) {
                continue;
            }
            let mut parse = target_parses
                .remove(target)
                .expect("target parse order is consistent");
            parse.structs = all_structs.clone();
            target_tasks.push((target.clone(), parse));
        }
        let prepare_target = |task: &mut (String, quill_syntax::Parse)| {
            let (target, parse) = task;
            let mut called_targets = function_call_targets(&parse.functions);
            loop {
                let previous_count = called_targets.len();
                for called in called_targets.clone() {
                    if let Some(generic) = functions_by_path
                        .get(&called)
                        .filter(|function| function.is_generic && function.body.is_some())
                    {
                        called_targets.extend(function_call_targets(std::slice::from_ref(generic)));
                    }
                }
                if called_targets.len() == previous_count {
                    break;
                }
            }
            let mut imports = functions_by_path
                .iter()
                .filter(|(path, _)| {
                    called_targets.contains(*path)
                        && function_owners
                            .get(*path)
                            .is_some_and(|owner| owner != target)
                        && exported_paths.contains(*path)
                })
                .map(|(path, function)| {
                    let mut declaration = function.clone();
                    declaration.is_public = false;
                    if !declaration.is_generic {
                        declaration.body = None;
                        declaration.body_span = None;
                    }
                    quill_codegen::ExternalFunctionImport {
                        declaration,
                        symbol: if function.is_generic {
                            String::new()
                        } else {
                            quill_codegen::quill_export_symbol(path)
                        },
                    }
                })
                .collect::<Vec<_>>();
            imports.sort_by(|left, right| left.declaration.name.cmp(&right.declaration.name));
            let source_fingerprints = target_source_fingerprints
                .get(target)
                .expect("every target has source fingerprints");
            let function_headers = parse
                .functions
                .iter()
                .map(|function| {
                    (
                        &function.name,
                        &function.scope,
                        &function.parameters,
                        &function.return_type,
                        &function.calls,
                        &function.qualified_calls,
                        (
                            function.is_public,
                            function.is_generic,
                            &function.generic_parameters,
                            &function.generic_copy_parameters,
                            &function.generic_as_ref_str_parameters,
                            &function.generic_add_parameters,
                            function.generic_has_bounds,
                            function.is_async,
                            function.is_const,
                            function.is_unsafe,
                            function.has_abi,
                        ),
                    )
                })
                .collect::<Vec<_>>();
            let mut object_hasher = blake3::Hasher::new();
            bincode::serialize_into(
                HasherWriter(&mut object_hasher),
                &(
                    &*target,
                    source_fingerprints,
                    &function_headers,
                    &parse.structs,
                    &imports,
                    object_compiler_stamp.as_ref(),
                ),
            )
            .map_err(|error| format!("could not fingerprint package object: {error}"))?;
            let object_fingerprint = *object_hasher.finalize().as_bytes();
            let (object, cache_hit) = emit_target_object_cached(
                object_fingerprint,
                std::mem::take(parse),
                &imports,
                object_cache_root,
            )?;
            if cache_hit && std::env::var_os("QUILL_PROFILE").is_some() {
                eprintln!("quill-profile package object cache hit: {target}");
            }
            if let Some(path) = fast_object_cache_paths.and_then(|paths| paths.get(target.as_str()))
            {
                write_cached_object(path, &object);
            }
            Ok::<_, String>((target.clone(), object))
        };
        let prepared_objects = if let Some(pool) = &file_pool {
            pool.map_mut(&mut target_tasks, prepare_target)
        } else {
            target_tasks.iter_mut().map(prepare_target).collect()
        };
        for result in prepared_objects {
            let (target, object) = result?;
            if !direct_object_paths {
                target_objects_by_key.insert(target, object);
            }
        }
        if !direct_object_paths {
            for target in &target_order {
                target_objects.push(
                    target_objects_by_key
                        .remove(target)
                        .expect("every target has a prepared or cached object"),
                );
            }
        }
        profile_checkpoint("prepare and fetch package objects", &mut profile_timer);
        if direct_object_paths {
            let paths =
                fast_object_cache_paths.expect("direct object paths require a complete path map");
            let paths = target_order
                .iter()
                .map(|target| {
                    paths
                        .get(target)
                        .expect("every target has a direct object cache path")
                })
                .collect::<Vec<_>>();
            quill_link::link_elf_object_paths(&paths, output_path, "_start")
                .map_err(|error| error.to_string())?;
        } else {
            let object_refs = target_objects.iter().map(Vec::as_slice).collect::<Vec<_>>();
            quill_link::link_elf_objects(&object_refs, output_path, "_start")
                .map_err(|error| error.to_string())?;
        }
        profile_checkpoint("link package objects", &mut profile_timer);
        return Ok(());
    }
    let object_bytes =
        quill_codegen::emit_subset_main_object(&combined).map_err(|error| error.to_string())?;
    profile_checkpoint("emit native object", &mut profile_timer);
    quill_link::link_elf_executable(&object_bytes, output_path, "_start")
        .map_err(|error| error.to_string())?;
    profile_checkpoint("link executable", &mut profile_timer);
    Ok(())
}

fn emit_target_object_cached(
    fingerprint: [u8; 32],
    parse: quill_syntax::Parse,
    imports: &[quill_codegen::ExternalFunctionImport],
    cache_root: Option<&Path>,
) -> Result<(Vec<u8>, bool), String> {
    let cache_path = if let Some(root) = cache_root {
        let key = blake3::Hash::from_bytes(fingerprint).to_hex().to_string();
        Some((root.join(format!("{key}.o")), root))
    } else {
        None
    };
    if let Some((path, _)) = &cache_path
        && let Ok(bytes) = std::fs::read(path)
        && bytes.starts_with(b"\x7fELF")
    {
        return Ok((bytes, true));
    }

    let has_main = parse
        .functions
        .iter()
        .any(|function| function.name == "main");
    let object = if has_main {
        quill_codegen::emit_subset_main_object_owned_with_imports(parse, imports)
    } else {
        quill_codegen::emit_subset_library_object_owned_with_imports(parse, imports)
    }
    .map_err(|error| error.to_string())?;

    if let Some((path, root)) = cache_path {
        if std::fs::create_dir_all(root).is_ok() {
            let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
            if std::fs::write(&temporary, &object).is_ok() {
                let _ = std::fs::rename(temporary, path);
            }
        }
    }
    Ok((object, false))
}

#[derive(Clone, Copy)]
struct ProfileTimer {
    wall: Instant,
    process_cpu: Option<Duration>,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct ClockTimespec {
    seconds: std::os::raw::c_long,
    nanoseconds: std::os::raw::c_long,
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn clock_gettime(
        clock_id: std::os::raw::c_int,
        time: *mut ClockTimespec,
    ) -> std::os::raw::c_int;
}

fn process_cpu_clock() -> Option<Duration> {
    #[cfg(target_os = "linux")]
    {
        // CLOCK_PROCESS_CPUTIME_ID includes CPU time spent by this process's
        // worker threads, which is the useful measure for parallel phases.
        const CLOCK_PROCESS_CPUTIME_ID: std::os::raw::c_int = 2;
        let mut time = ClockTimespec {
            seconds: 0,
            nanoseconds: 0,
        };
        // SAFETY: `time` points to writable storage with the POSIX timespec ABI.
        if unsafe { clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &mut time) } != 0 {
            return None;
        }
        return Some(Duration::new(time.seconds as u64, time.nanoseconds as u32));
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn profile_timer() -> Option<ProfileTimer> {
    std::env::var_os("QUILL_PROFILE").map(|_| ProfileTimer {
        wall: Instant::now(),
        process_cpu: process_cpu_clock(),
    })
}

fn profile_checkpoint(stage: &str, previous: &mut Option<ProfileTimer>) {
    if let Some(started) = previous.as_mut() {
        let wall_now = Instant::now();
        let cpu_now = process_cpu_clock();
        let wall_ms = wall_now.duration_since(started.wall).as_secs_f64() * 1000.0;
        if let (Some(cpu_start), Some(cpu_end)) = (started.process_cpu, cpu_now) {
            let cpu_ms = cpu_end.saturating_sub(cpu_start).as_secs_f64() * 1000.0;
            eprintln!("quill-profile {stage}: wall={wall_ms:.3} ms cpu={cpu_ms:.3} ms");
        } else {
            eprintln!("quill-profile {stage}: wall={wall_ms:.3} ms");
        }
        *started = ProfileTimer {
            wall: wall_now,
            process_cpu: cpu_now,
        };
    }
}

fn canonical_function_path(
    file: &ParsedSource,
    function: &quill_syntax::FunctionItem,
    crate_prefix: &str,
) -> String {
    let mut path = Vec::new();
    if !crate_prefix.is_empty() {
        path.push(crate_prefix.to_owned());
    }
    path.extend(file.module_path.clone());
    path.extend(function.scope.iter().filter_map(|scope| {
        scope
            .strip_prefix("mod:")
            .or_else(|| scope.strip_prefix("impl:"))
            .map(str::to_owned)
    }));
    path.push(function.name.clone());
    path.join("::")
}

fn canonical_struct_path(
    file: &ParsedSource,
    structure: &quill_syntax::StructItem,
    crate_prefix: &str,
) -> String {
    let mut path = Vec::new();
    if !crate_prefix.is_empty() {
        path.push(crate_prefix.to_owned());
    }
    path.extend(file.module_path.clone());
    path.extend(
        structure
            .scope
            .iter()
            .filter_map(|scope| scope.strip_prefix("mod:").map(str::to_owned)),
    );
    path.push(structure.name.clone());
    path.join("::")
}

fn canonical_enum_path(
    file: &ParsedSource,
    item: &quill_syntax::EnumItem,
    crate_prefix: &str,
) -> String {
    let mut path = Vec::new();
    if !crate_prefix.is_empty() {
        path.push(crate_prefix.to_owned());
    }
    path.extend(file.module_path.clone());
    path.extend(
        item.scope
            .iter()
            .filter_map(|scope| scope.strip_prefix("mod:").map(str::to_owned)),
    );
    path.push(item.name.clone());
    path.join("::")
}

fn rewrite_enum_type(type_text: &mut String, module_path: &[String], enum_paths: &HashSet<&str>) {
    let normalized = type_text.trim();
    if resolve_enum_path(normalized, module_path, enum_paths).is_some() {
        *type_text = "i32".to_owned();
    }
}

fn resolve_enum_path<'a>(
    name: &str,
    module_path: &[String],
    enum_paths: &HashSet<&'a str>,
) -> Option<&'a str> {
    if enum_paths.contains(name) {
        return enum_paths.get(name).copied();
    }
    let local_candidate = if module_path.is_empty() {
        name.to_owned()
    } else {
        format!("{}::{name}", module_path.join("::"))
    };
    if let Some(local) = enum_paths.get(local_candidate.as_str()) {
        return Some(*local);
    }
    let mut suffix_matches = enum_paths.iter().copied().filter(|path| {
        path.ends_with(&format!("::{name}")) || path.rsplit("::").next() == name.rsplit("::").next()
    });
    let found = suffix_matches.next()?;
    suffix_matches.next().is_none().then_some(found)
}

fn resolve_enum_variant<'a>(
    path: &str,
    module_path: &[String],
    enums: &'a [EnumDefinition],
) -> Option<(&'a EnumDefinition, i32)> {
    let (requested_enum, requested_variant) = path.rsplit_once("::")?;
    let requested_enum_name = requested_enum.rsplit("::").next().unwrap_or(requested_enum);
    let mut matches = enums.iter().filter_map(|definition| {
        definition
            .variants
            .iter()
            .find_map(|(variant, discriminant)| {
                let full_path = format!("{}::{variant}", definition.path);
                (full_path == path
                    || full_path.ends_with(&format!("::{path}"))
                    || (variant == requested_variant
                        && definition.path.rsplit("::").next() == Some(requested_enum_name)))
                .then_some((definition, *discriminant))
            })
    });
    let found = matches.next()?;
    if matches.next().is_none() {
        return Some(found);
    }
    let local_prefix = module_path.join("::");
    let mut local_matches = enums.iter().filter_map(|definition| {
        if !definition.path.starts_with(&format!("{local_prefix}::")) {
            return None;
        }
        definition
            .variants
            .iter()
            .find_map(|(variant, discriminant)| {
                let full_path = format!("{}::{variant}", definition.path);
                (full_path == path
                    || full_path.ends_with(&format!("::{path}"))
                    || (variant == requested_variant
                        && definition.path.rsplit("::").next() == Some(requested_enum_name)))
                .then_some((definition, *discriminant))
            })
    });
    let local = local_matches.next()?;
    local_matches.next().is_none().then_some(local)
}

fn rewrite_enum_references(
    expression: &mut quill_syntax::BodyExpr,
    module_path: &[String],
    enums: &[EnumDefinition],
    enum_paths: &HashSet<&str>,
    impl_owner: Option<&str>,
    struct_paths: &HashSet<String>,
) {
    use quill_syntax::{BodyExpr, BodyExprKind, BodyStatement, LiteralKind, MatchPattern};

    fn rewrite_pattern(
        pattern: &mut MatchPattern,
        module_path: &[String],
        enums: &[EnumDefinition],
    ) {
        match pattern {
            MatchPattern::Path(path) => {
                if let Some((definition, discriminant)) =
                    resolve_enum_variant(path, module_path, enums)
                {
                    *pattern = MatchPattern::EnumVariant {
                        path: path.clone(),
                        discriminant,
                        variant_count: definition.variants.len(),
                    };
                }
            }
            MatchPattern::Or(patterns) => {
                for pattern in patterns {
                    rewrite_pattern(pattern, module_path, enums);
                }
            }
            MatchPattern::Wildcard
            | MatchPattern::Literal(_)
            | MatchPattern::Binding(_)
            | MatchPattern::Range { .. }
            | MatchPattern::EnumVariant { .. }
            | MatchPattern::Destructure { .. }
            | MatchPattern::Unsupported => {}
        }
    }

    fn resolve_struct_path(
        path: &str,
        module_path: &[String],
        impl_owner: Option<&str>,
        struct_paths: &HashSet<String>,
    ) -> String {
        let candidate = if path == "Self" {
            impl_owner.map(str::to_owned)
        } else {
            Some(path.to_owned())
        };
        let Some(candidate) = candidate else {
            return path.to_owned();
        };
        let mut relative = module_path.to_vec();
        relative.push(candidate.clone());
        let relative = relative.join("::");
        if struct_paths.contains(&relative) {
            return relative;
        }
        if struct_paths.contains(&candidate) {
            return candidate;
        }
        let suffix = format!("::{candidate}");
        let mut matches = struct_paths.iter().filter(|path| path.ends_with(&suffix));
        if let Some(found) = matches.next()
            && matches.next().is_none()
        {
            return found.clone();
        }
        path.to_owned()
    }

    fn rewrite(
        expression: &mut BodyExpr,
        module_path: &[String],
        enums: &[EnumDefinition],
        enum_paths: &HashSet<&str>,
        impl_owner: Option<&str>,
        struct_paths: &HashSet<String>,
    ) {
        match &mut expression.kind {
            BodyExprKind::Path(path) => {
                if let Some((_, discriminant)) = resolve_enum_variant(path, module_path, enums) {
                    expression.kind = BodyExprKind::Literal(LiteralKind::Integer {
                        text: discriminant.to_string(),
                        suffix: "i32".to_owned(),
                    });
                }
            }
            BodyExprKind::Call {
                generic_arguments,
                arguments,
                ..
            } => {
                for type_text in generic_arguments {
                    rewrite_enum_type(type_text, module_path, enum_paths);
                }
                for argument in arguments {
                    rewrite(
                        argument,
                        module_path,
                        enums,
                        enum_paths,
                        impl_owner,
                        struct_paths,
                    );
                }
            }
            BodyExprKind::MethodCall {
                receiver,
                arguments,
                ..
            } => {
                rewrite(
                    receiver,
                    module_path,
                    enums,
                    enum_paths,
                    impl_owner,
                    struct_paths,
                );
                for argument in arguments {
                    rewrite(
                        argument,
                        module_path,
                        enums,
                        enum_paths,
                        impl_owner,
                        struct_paths,
                    );
                }
            }
            BodyExprKind::Closure { body, .. } => rewrite(
                body,
                module_path,
                enums,
                enum_paths,
                impl_owner,
                struct_paths,
            ),
            BodyExprKind::Tuple(elements)
            | BodyExprKind::ArrayLiteral(elements)
            | BodyExprKind::FixedArrayLiteral(elements) => {
                for element in elements {
                    rewrite(
                        element,
                        module_path,
                        enums,
                        enum_paths,
                        impl_owner,
                        struct_paths,
                    );
                }
            }
            BodyExprKind::ArrayRepeat { element, .. } => rewrite(
                element,
                module_path,
                enums,
                enum_paths,
                impl_owner,
                struct_paths,
            ),
            BodyExprKind::Index { base, index, .. } => {
                rewrite(
                    base,
                    module_path,
                    enums,
                    enum_paths,
                    impl_owner,
                    struct_paths,
                );
                rewrite(
                    index,
                    module_path,
                    enums,
                    enum_paths,
                    impl_owner,
                    struct_paths,
                );
            }
            BodyExprKind::StructLiteral { path, fields } => {
                *path = resolve_struct_path(path, module_path, impl_owner, struct_paths);
                for (_, value) in fields {
                    rewrite(
                        value,
                        module_path,
                        enums,
                        enum_paths,
                        impl_owner,
                        struct_paths,
                    );
                }
            }
            BodyExprKind::FieldAccess { base, .. }
            | BodyExprKind::Negate(base)
            | BodyExprKind::Not(base)
            | BodyExprKind::Parenthesized(base)
            | BodyExprKind::Try(base)
            | BodyExprKind::Reference {
                expression: base, ..
            }
            | BodyExprKind::Dereference(base)
            | BodyExprKind::Cast {
                expression: base, ..
            } => rewrite(
                base,
                module_path,
                enums,
                enum_paths,
                impl_owner,
                struct_paths,
            ),
            BodyExprKind::Binary { left, right, .. } => {
                rewrite(
                    left,
                    module_path,
                    enums,
                    enum_paths,
                    impl_owner,
                    struct_paths,
                );
                rewrite(
                    right,
                    module_path,
                    enums,
                    enum_paths,
                    impl_owner,
                    struct_paths,
                );
            }
            BodyExprKind::Block { statements, tail } => {
                for statement in statements {
                    match statement {
                        BodyStatement::Let {
                            type_text,
                            initializer,
                            ..
                        } => {
                            if let Some(type_text) = type_text {
                                rewrite_enum_type(type_text, module_path, enum_paths);
                            }
                            if let Some(initializer) = initializer {
                                rewrite(
                                    initializer,
                                    module_path,
                                    enums,
                                    enum_paths,
                                    impl_owner,
                                    struct_paths,
                                );
                            }
                        }
                        BodyStatement::LetTuple { initializer, .. }
                        | BodyStatement::Assign {
                            value: initializer, ..
                        }
                        | BodyStatement::Expression {
                            expression: initializer,
                            ..
                        } => rewrite(
                            initializer,
                            module_path,
                            enums,
                            enum_paths,
                            impl_owner,
                            struct_paths,
                        ),
                        BodyStatement::While {
                            condition, body, ..
                        } => {
                            rewrite(
                                condition,
                                module_path,
                                enums,
                                enum_paths,
                                impl_owner,
                                struct_paths,
                            );
                            rewrite(
                                body,
                                module_path,
                                enums,
                                enum_paths,
                                impl_owner,
                                struct_paths,
                            );
                        }
                        BodyStatement::Loop { body, .. } => rewrite(
                            body,
                            module_path,
                            enums,
                            enum_paths,
                            impl_owner,
                            struct_paths,
                        ),
                        BodyStatement::ForRange {
                            start, end, body, ..
                        } => {
                            rewrite(
                                start,
                                module_path,
                                enums,
                                enum_paths,
                                impl_owner,
                                struct_paths,
                            );
                            rewrite(
                                end,
                                module_path,
                                enums,
                                enum_paths,
                                impl_owner,
                                struct_paths,
                            );
                            rewrite(
                                body,
                                module_path,
                                enums,
                                enum_paths,
                                impl_owner,
                                struct_paths,
                            );
                        }
                        BodyStatement::ForEach { iterator, body, .. } => {
                            rewrite(
                                iterator,
                                module_path,
                                enums,
                                enum_paths,
                                impl_owner,
                                struct_paths,
                            );
                            rewrite(
                                body,
                                module_path,
                                enums,
                                enum_paths,
                                impl_owner,
                                struct_paths,
                            );
                        }
                        BodyStatement::MacroInvocation { arguments, .. } => {
                            for argument in arguments {
                                rewrite(
                                    argument,
                                    module_path,
                                    enums,
                                    enum_paths,
                                    impl_owner,
                                    struct_paths,
                                );
                            }
                        }
                        BodyStatement::Break { .. }
                        | BodyStatement::Continue { .. }
                        | BodyStatement::Unsupported => {}
                    }
                }
                if let Some(tail) = tail {
                    rewrite(
                        tail,
                        module_path,
                        enums,
                        enum_paths,
                        impl_owner,
                        struct_paths,
                    );
                }
            }
            BodyExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                rewrite(
                    condition,
                    module_path,
                    enums,
                    enum_paths,
                    impl_owner,
                    struct_paths,
                );
                rewrite(
                    then_branch,
                    module_path,
                    enums,
                    enum_paths,
                    impl_owner,
                    struct_paths,
                );
                if let Some(branch) = else_branch {
                    rewrite(
                        branch,
                        module_path,
                        enums,
                        enum_paths,
                        impl_owner,
                        struct_paths,
                    );
                }
            }
            BodyExprKind::Match { scrutinee, arms } => {
                rewrite(
                    scrutinee,
                    module_path,
                    enums,
                    enum_paths,
                    impl_owner,
                    struct_paths,
                );
                for arm in arms {
                    rewrite_pattern(&mut arm.pattern, module_path, enums);
                    rewrite(
                        &mut arm.body,
                        module_path,
                        enums,
                        enum_paths,
                        impl_owner,
                        struct_paths,
                    );
                }
            }
            BodyExprKind::MacroInvocation(macro_expression) => {
                for argument in &mut macro_expression.arguments {
                    rewrite(
                        argument,
                        module_path,
                        enums,
                        enum_paths,
                        impl_owner,
                        struct_paths,
                    );
                }
            }
            BodyExprKind::Return(value) => {
                if let Some(value) = value {
                    rewrite(
                        value,
                        module_path,
                        enums,
                        enum_paths,
                        impl_owner,
                        struct_paths,
                    );
                }
            }
            BodyExprKind::Unit
            | BodyExprKind::Literal(_)
            | BodyExprKind::EmptySliceReference
            | BodyExprKind::Unsupported => {}
        }
    }

    rewrite(
        expression,
        module_path,
        enums,
        enum_paths,
        impl_owner,
        struct_paths,
    );
}

fn resolve_impl_owner_type(
    module_path: &[String],
    impl_owner: &str,
    struct_paths: &HashSet<String>,
) -> Option<String> {
    let mut candidate = module_path.to_vec();
    candidate.push(impl_owner.to_owned());
    let candidate = candidate.join("::");
    if struct_paths.contains(&candidate) {
        return Some(candidate);
    }
    let suffix = format!("::{impl_owner}");
    let mut matches = struct_paths.iter().filter(|path| path.ends_with(&suffix));
    let found = matches.next()?.clone();
    matches.next().is_none().then_some(found)
}

fn substitute_self_type(type_text: &str, self_type: &str) -> String {
    let mut output = String::with_capacity(type_text.len() + self_type.len());
    let mut remaining = type_text;
    while let Some(index) = remaining.find("Self") {
        let before_is_identifier = remaining[..index]
            .chars()
            .next_back()
            .is_some_and(|character| character == '_' || character.is_ascii_alphanumeric());
        let after_index = index + "Self".len();
        let after_is_identifier = remaining[after_index..]
            .chars()
            .next()
            .is_some_and(|character| character == '_' || character.is_ascii_alphanumeric());
        if before_is_identifier || after_is_identifier {
            let split = index + "Self".len();
            output.push_str(&remaining[..split]);
            remaining = &remaining[split..];
        } else {
            output.push_str(&remaining[..index]);
            output.push_str(self_type);
            remaining = &remaining[after_index..];
        }
    }
    output.push_str(remaining);
    output
}

fn resolve_cached_call(
    callee: &str,
    context: &str,
    prefixes: &[String],
    function_paths: &HashSet<String>,
    import_aliases: &ImportAliasMap,
    cache: &mut CallResolutionCache,
) -> Result<Option<String>, String> {
    let key = (context.to_owned(), callee.to_owned());
    if let Some(resolution) = cache.get(&key) {
        return Ok(resolution.clone());
    }
    let resolution = resolve_module_call(callee, prefixes, function_paths, import_aliases)?;
    cache.insert(key, resolution.clone());
    Ok(resolution)
}

fn rewrite_calls(
    function: &mut quill_syntax::FunctionItem,
    module_path: &[String],
    function_paths: &HashSet<String>,
    import_aliases: &ImportAliasMap,
    resolution_cache: &mut CallResolutionCache,
) -> Result<(), String> {
    use quill_syntax::{BodyExpr, BodyExprKind, BodyStatement};
    // The canonical function path is the authoritative crate/module scope. The
    // source walk may have loaded a root file through more than one Cargo
    // target, so prefer this identity when resolving sibling calls.
    let canonical_scope = function
        .name
        .rsplit_once("::")
        .map(|(parent, _)| parent.split("::").map(str::to_owned).collect::<Vec<_>>())
        .unwrap_or_else(|| module_path.to_vec());
    let canonical_prefixes = scope_prefixes(&canonical_scope);
    let module_prefixes = scope_prefixes(module_path);
    let canonical_context = canonical_scope.join("::");
    let module_context = module_path.join("::");
    fn visit(
        expr: &mut BodyExpr,
        prefixes: &[String],
        paths: &HashSet<String>,
        aliases: &ImportAliasMap,
        resolution_cache: &mut CallResolutionCache,
        context: &str,
    ) -> Result<(), String> {
        match &mut expr.kind {
            BodyExprKind::Call {
                callee, arguments, ..
            } => {
                let resolution = resolve_cached_call(
                    callee,
                    context,
                    prefixes,
                    paths,
                    aliases,
                    resolution_cache,
                )?;
                if let Some(resolved) = resolution {
                    *callee = resolved;
                }
                for argument in arguments {
                    visit(
                        argument,
                        prefixes,
                        paths,
                        aliases,
                        resolution_cache,
                        context,
                    )?;
                }
            }
            BodyExprKind::MacroInvocation(macro_expression) => {
                for argument in &mut macro_expression.arguments {
                    visit(
                        argument,
                        prefixes,
                        paths,
                        aliases,
                        resolution_cache,
                        context,
                    )?;
                }
            }
            BodyExprKind::Tuple(arguments)
            | BodyExprKind::ArrayLiteral(arguments)
            | BodyExprKind::FixedArrayLiteral(arguments) => {
                for argument in arguments {
                    visit(
                        argument,
                        prefixes,
                        paths,
                        aliases,
                        resolution_cache,
                        context,
                    )?;
                }
            }
            BodyExprKind::ArrayRepeat { element, .. } => {
                visit(
                    element,
                    prefixes,
                    paths,
                    aliases,
                    resolution_cache,
                    context,
                )?;
            }
            BodyExprKind::Index { base, index, .. } => {
                visit(base, prefixes, paths, aliases, resolution_cache, context)?;
                visit(index, prefixes, paths, aliases, resolution_cache, context)?;
            }
            BodyExprKind::MethodCall {
                receiver,
                arguments,
                ..
            } => {
                visit(
                    receiver,
                    prefixes,
                    paths,
                    aliases,
                    resolution_cache,
                    context,
                )?;
                for argument in arguments {
                    visit(
                        argument,
                        prefixes,
                        paths,
                        aliases,
                        resolution_cache,
                        context,
                    )?;
                }
            }
            BodyExprKind::FieldAccess { base, .. }
            | BodyExprKind::Reference {
                expression: base, ..
            }
            | BodyExprKind::Cast {
                expression: base, ..
            } => {
                visit(base, prefixes, paths, aliases, resolution_cache, context)?;
            }
            BodyExprKind::Closure { body, .. } => {
                visit(body, prefixes, paths, aliases, resolution_cache, context)?
            }
            BodyExprKind::Binary { left, right, .. } => {
                visit(left, prefixes, paths, aliases, resolution_cache, context)?;
                visit(right, prefixes, paths, aliases, resolution_cache, context)?;
            }
            BodyExprKind::Negate(value) | BodyExprKind::Parenthesized(value) => {
                visit(value, prefixes, paths, aliases, resolution_cache, context)?
            }
            BodyExprKind::Block { statements, tail } => {
                for statement in statements {
                    match statement {
                        BodyStatement::Let {
                            initializer: Some(value),
                            ..
                        } => visit(value, prefixes, paths, aliases, resolution_cache, context)?,
                        BodyStatement::LetTuple { initializer, .. } => visit(
                            initializer,
                            prefixes,
                            paths,
                            aliases,
                            resolution_cache,
                            context,
                        )?,
                        BodyStatement::Assign { value, .. } => {
                            visit(value, prefixes, paths, aliases, resolution_cache, context)?
                        }
                        BodyStatement::While {
                            condition, body, ..
                        } => {
                            visit(
                                condition,
                                prefixes,
                                paths,
                                aliases,
                                resolution_cache,
                                context,
                            )?;
                            visit(body, prefixes, paths, aliases, resolution_cache, context)?;
                        }
                        BodyStatement::Loop { body, .. } => {
                            visit(body, prefixes, paths, aliases, resolution_cache, context)?
                        }
                        BodyStatement::ForRange {
                            start, end, body, ..
                        } => {
                            visit(start, prefixes, paths, aliases, resolution_cache, context)?;
                            visit(end, prefixes, paths, aliases, resolution_cache, context)?;
                            visit(body, prefixes, paths, aliases, resolution_cache, context)?;
                        }
                        BodyStatement::ForEach { iterator, body, .. } => {
                            visit(
                                iterator,
                                prefixes,
                                paths,
                                aliases,
                                resolution_cache,
                                context,
                            )?;
                            visit(body, prefixes, paths, aliases, resolution_cache, context)?;
                        }
                        BodyStatement::Expression { expression, .. } => visit(
                            expression,
                            prefixes,
                            paths,
                            aliases,
                            resolution_cache,
                            context,
                        )?,
                        _ => {}
                    }
                }
                if let Some(tail) = tail {
                    visit(tail, prefixes, paths, aliases, resolution_cache, context)?;
                }
            }
            BodyExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                visit(
                    condition,
                    prefixes,
                    paths,
                    aliases,
                    resolution_cache,
                    context,
                )?;
                visit(
                    then_branch,
                    prefixes,
                    paths,
                    aliases,
                    resolution_cache,
                    context,
                )?;
                if let Some(branch) = else_branch {
                    visit(branch, prefixes, paths, aliases, resolution_cache, context)?;
                }
            }
            BodyExprKind::Match { scrutinee, arms } => {
                visit(
                    scrutinee,
                    prefixes,
                    paths,
                    aliases,
                    resolution_cache,
                    context,
                )?;
                for arm in arms {
                    visit(
                        &mut arm.body,
                        prefixes,
                        paths,
                        aliases,
                        resolution_cache,
                        context,
                    )?;
                }
            }
            BodyExprKind::Return(Some(value)) => {
                visit(value, prefixes, paths, aliases, resolution_cache, context)?
            }
            _ => {}
        }
        Ok(())
    }
    if let Some(body) = function.body.as_mut() {
        visit(
            body,
            &canonical_prefixes,
            function_paths,
            import_aliases,
            resolution_cache,
            &canonical_context,
        )?;
    }
    for call in &mut function.calls {
        if let Some(resolved) = resolve_cached_call(
            call,
            &canonical_context,
            &canonical_prefixes,
            function_paths,
            import_aliases,
            resolution_cache,
        )? {
            *call = resolved;
        }
    }
    for call in function.qualified_calls.clone() {
        if let Some(resolved) = resolve_cached_call(
            &call,
            &module_context,
            &module_prefixes,
            function_paths,
            import_aliases,
            resolution_cache,
        )? && !function.calls.contains(&resolved)
        {
            function.calls.push(resolved);
        }
    }
    Ok(())
}

fn function_call_targets(functions: &[quill_syntax::FunctionItem]) -> HashSet<String> {
    use quill_syntax::{BodyExpr, BodyExprKind, BodyStatement};

    fn visit(expression: &BodyExpr, targets: &mut HashSet<String>) {
        macro_rules! walk {
            ($child:expr) => {
                visit($child, targets)
            };
        }
        match &expression.kind {
            BodyExprKind::Call {
                callee, arguments, ..
            } => {
                targets.insert(callee.clone());
                for argument in arguments {
                    walk!(argument);
                }
            }
            BodyExprKind::MethodCall {
                receiver,
                arguments,
                ..
            } => {
                walk!(receiver);
                for argument in arguments {
                    walk!(argument);
                }
            }
            BodyExprKind::MacroInvocation(macro_call) => {
                for argument in &macro_call.arguments {
                    walk!(argument);
                }
            }
            BodyExprKind::Reference { expression, .. }
            | BodyExprKind::Dereference(expression)
            | BodyExprKind::Cast { expression, .. }
            | BodyExprKind::Closure {
                body: expression, ..
            }
            | BodyExprKind::FieldAccess {
                base: expression, ..
            }
            | BodyExprKind::Negate(expression)
            | BodyExprKind::Not(expression)
            | BodyExprKind::Try(expression)
            | BodyExprKind::Parenthesized(expression) => walk!(expression),
            BodyExprKind::Index { base, index, .. } => {
                walk!(base);
                walk!(index);
            }
            BodyExprKind::Tuple(items)
            | BodyExprKind::ArrayLiteral(items)
            | BodyExprKind::FixedArrayLiteral(items) => {
                for item in items {
                    walk!(item);
                }
            }
            BodyExprKind::ArrayRepeat { element, .. } => walk!(element),
            BodyExprKind::StructLiteral { fields, .. } => {
                for (_, value) in fields {
                    walk!(value);
                }
            }
            BodyExprKind::Binary { left, right, .. } => {
                walk!(left);
                walk!(right);
            }
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
                        | BodyStatement::Assign { value, .. }
                        | BodyStatement::Expression {
                            expression: value, ..
                        } => walk!(value),
                        BodyStatement::While {
                            condition, body, ..
                        } => {
                            walk!(condition);
                            walk!(body);
                        }
                        BodyStatement::Loop { body, .. } => walk!(body),
                        BodyStatement::ForRange {
                            start, end, body, ..
                        } => {
                            walk!(start);
                            walk!(end);
                            walk!(body);
                        }
                        BodyStatement::ForEach { iterator, body, .. } => {
                            walk!(iterator);
                            walk!(body);
                        }
                        BodyStatement::MacroInvocation { arguments, .. } => {
                            for argument in arguments {
                                walk!(argument);
                            }
                        }
                        BodyStatement::Let {
                            initializer: None, ..
                        }
                        | BodyStatement::Break { .. }
                        | BodyStatement::Continue { .. }
                        | BodyStatement::Unsupported => {}
                    }
                }
                if let Some(tail) = tail {
                    walk!(tail);
                }
            }
            BodyExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                walk!(condition);
                walk!(then_branch);
                if let Some(branch) = else_branch {
                    walk!(branch);
                }
            }
            BodyExprKind::Match { scrutinee, arms } => {
                walk!(scrutinee);
                for arm in arms {
                    walk!(&arm.body);
                }
            }
            BodyExprKind::Return(value) => {
                if let Some(value) = value {
                    walk!(value);
                }
            }
            BodyExprKind::Unit
            | BodyExprKind::Literal(_)
            | BodyExprKind::EmptySliceReference
            | BodyExprKind::Path(_)
            | BodyExprKind::Unsupported => {}
        }
    }

    let mut targets = HashSet::new();
    for function in functions {
        if let Some(body) = &function.body {
            visit(body, &mut targets);
        }
    }
    targets
}

fn resolve_module_call(
    callee: &str,
    scope_prefixes: &[String],
    function_paths: &HashSet<String>,
    import_aliases: &ImportAliasMap,
) -> Result<Option<String>, String> {
    if callee == "std::process::exit" {
        return Ok(None);
    }
    let normalized = callee
        .strip_prefix("crate::")
        .or_else(|| callee.strip_prefix("self::"))
        .unwrap_or(callee);
    if normalized.split("::").any(|part| part == "super") {
        return Err("`super::` calls are not supported by the native module subset".into());
    }
    if normalized.contains("::") {
        if function_paths.contains(normalized) {
            return Ok(Some(normalized.to_owned()));
        }
        let (head, tail) = normalized.split_once("::").unwrap();
        for scope in scope_prefixes.iter().rev() {
            if let Some(target) = import_aliases
                .get(scope)
                .and_then(|aliases| aliases.get(head))
            {
                let candidate = format!("{target}::{tail}");
                if function_paths.contains(&candidate) {
                    return Ok(Some(candidate));
                }
            }
            let candidate = join_scope_and_name(scope, normalized);
            if function_paths.contains(&candidate) {
                return Ok(Some(candidate));
            }
        }
        return Ok(None);
    }
    for scope in scope_prefixes.iter().rev() {
        if let Some(target) = import_aliases
            .get(scope)
            .and_then(|aliases| aliases.get(normalized))
        {
            return Ok(function_paths.contains(target).then(|| target.clone()));
        }
        let candidate = join_scope_and_name(scope, normalized);
        if function_paths.contains(&candidate) {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn scope_prefixes(scope: &[String]) -> Vec<String> {
    (0..=scope.len())
        .map(|depth| scope[..depth].join("::"))
        .collect()
}

fn join_scope_and_name(scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.to_owned()
    } else {
        format!("{scope}::{name}")
    }
}

fn module_import_aliases(
    file: &ParsedSource,
    function_paths: &HashSet<String>,
    crate_prefix: &str,
) -> Result<ImportAliasMap, String> {
    let mut aliases = HashMap::new();
    for import in &file.parse.imports {
        let error = || {
            format!(
                "{}: this use declaration is outside the native codegen subset",
                file.source.path().display()
            )
        };
        if import.is_glob || import.is_public || import.path.is_empty() {
            return Err(error());
        }
        // A standard-library type import does not introduce a callable symbol
        // into this resolver. Keep it out of the function alias table while
        // allowing later type/layout stages to issue the relevant diagnostic.
        if is_supported_std_type_import(&import.path) {
            continue;
        }
        let mut scope = file.module_path.clone();
        if !crate_prefix.is_empty() {
            scope.insert(0, crate_prefix.to_owned());
        }
        scope.extend(
            import
                .scope
                .iter()
                .filter_map(|part| part.strip_prefix("mod:").map(str::to_owned)),
        );
        let mut target = import.path.clone();
        match target.first().map(String::as_str) {
            Some("crate") => {
                target.remove(0);
                if !crate_prefix.is_empty() {
                    target.insert(0, crate_prefix.to_owned());
                }
            }
            Some("self") => {
                target.remove(0);
                target.splice(0..0, scope.clone());
            }
            _ => {}
        }
        if target.iter().any(|part| part == "super") {
            return Err(error());
        }
        let target = target.join("::");
        if !function_paths.contains(&target) {
            return Err(error());
        }
        let name = import
            .alias
            .clone()
            .unwrap_or_else(|| import.path.last().unwrap().clone());
        if aliases
            .entry(scope.join("::"))
            .or_insert_with(HashMap::new)
            .insert(name, target)
            .is_some()
        {
            return Err(error());
        }
    }
    Ok(aliases)
}

fn is_supported_std_type_import(path: &[String]) -> bool {
    matches!(
        path.iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
        ["std", "collections", "BTreeMap"]
            | ["std", "collections", "BTreeSet"]
            | ["std", "collections", "HashMap"]
            | ["std", "collections", "HashSet"]
            | ["std", "string", "String"]
            | ["std", "vec", "Vec"]
            | ["alloc", "collections", "BTreeMap"]
            | ["alloc", "collections", "BTreeSet"]
            | ["alloc", "collections", "BinaryHeap"]
            | ["alloc", "string", "String"]
            | ["alloc", "vec", "Vec"]
    )
}

fn check_subset_files(paths: Vec<String>) -> ExitCode {
    if paths.is_empty() {
        eprintln!("usage: quillc check-subset <crate-root.rs> [more-root.rs ...]");
        return ExitCode::from(2);
    }
    let mut pool = None;
    let (parsed_files, module_errors) = match parse_module_trees(&paths, &mut pool) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("quillc: {error}");
            return ExitCode::FAILURE;
        }
    };
    let pool = match pool {
        Some(pool) => pool,
        None => match quill_parallel::WorkPool::for_tasks(parsed_files.len()) {
            Ok(pool) => pool,
            Err(error) => {
                eprintln!("quillc: could not start type-check workers: {error}");
                return ExitCode::FAILURE;
            }
        },
    };
    let type_check_cache = parsed_files
        .first()
        .map(|file| quill_query::TypeCheckCache::for_source(&file.source));
    let check_results = pool.map(&parsed_files, |file| {
        if file.parse.had_error {
            (0, Vec::new())
        } else {
            let checked = file
                .parse
                .functions
                .iter()
                .filter(|function| function.body.is_some())
                .count();
            (
                checked,
                type_check_cache
                    .as_ref()
                    .expect("parsed crate has a source file")
                    .check_functions(&file.source, &file.parse.functions, false),
            )
        }
    });
    let mut had_error = false;
    let mut checked = 0;
    for (file, (file_checked, type_diagnostics)) in parsed_files.iter().zip(check_results) {
        let path = file.source.path().display().to_string();
        for diagnostic in &file.parse.diagnostics {
            had_error = true;
            let (line, column) = file.source.line_column(diagnostic.span.start);
            eprintln!("{path}:{line}:{column}: {}", diagnostic.message);
        }
        checked += file_checked;
        if file.parse.had_error {
            had_error = true;
            continue;
        }
        for diagnostic in type_diagnostics {
            had_error = true;
            let (line, column) = file.source.line_column(diagnostic.span.start);
            if diagnostic.message.contains(" has type `")
                && diagnostic.message.contains(", expected `")
            {
                eprintln!("{path}:{line}:{column}: error[E0308]: mismatched types");
                eprintln!("  = note: {}", diagnostic.message);
            } else {
                eprintln!("{path}:{line}:{column}: {}", diagnostic.message);
            }
        }
    }
    for error in module_errors {
        had_error = true;
        eprintln!("quillc: {error}");
    }
    if let Some(cache) = &type_check_cache {
        report_type_check_cache_stats(cache);
    }
    println!("checked {checked} function bodies with quill-typeck's primitive subset");
    if had_error {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn report_type_check_cache_stats(cache: &quill_query::TypeCheckCache) {
    if std::env::var_os("QUILL_CACHE_STATS").is_some() {
        let stats = cache.stats();
        eprintln!(
            "quill-typeck-query: hits={} misses={} invalid={} write_failures={}",
            stats.hits, stats.misses, stats.invalid_entries, stats.write_failures
        );
    }
}

fn process_files(paths: Vec<String>, parse: bool) -> ExitCode {
    if paths.is_empty() {
        eprintln!("usage: quillc lex|parse <file.rs> [more.rs ...]");
        return ExitCode::from(2);
    }
    let mut had_error = false;
    if parse {
        let mut pool = None;
        let (parsed_files, module_errors) = match parse_module_trees(&paths, &mut pool) {
            Ok(result) => result,
            Err(error) => {
                eprintln!("quillc: {error}");
                return ExitCode::FAILURE;
            }
        };
        for source in &parsed_files {
            let path = source.source.path().display().to_string();
            let parsed = &source.parse;
            had_error |= parsed.had_error;
            for diagnostic in &parsed.diagnostics {
                let (line, column) = source.source.line_column(diagnostic.span.start);
                eprintln!("{path}:{line}:{column}: {}", diagnostic.message);
            }
        }
        for error in module_errors {
            had_error = true;
            eprintln!("quillc: {error}");
        }
        let crate_name = paths
            .first()
            .and_then(|path| std::path::Path::new(path).file_stem())
            .and_then(|stem| stem.to_str())
            .unwrap_or("input");
        let module_inputs: Vec<_> = parsed_files
            .iter()
            .map(|file| (file.module_path.clone(), file.parse.clone()))
            .collect();
        let hir = quill_hir::lower_module_files(crate_name, &module_inputs);
        let roots: Vec<_> = hir
            .functions
            .iter()
            .filter(|function| function.def_path.name == "main")
            .map(|function| function.def_path.clone())
            .collect();
        let reachable = quill_hir::codegen_reachable(&hir, &roots);
        println!(
            "{} files: {} modules, {} function items, {} codegen candidates (prototype)",
            parsed_files.len(),
            hir.modules.len(),
            hir.functions.len(),
            reachable.len()
        );
        for function in hir.functions {
            println!(
                "  {}{}{}{}",
                function.def_path,
                if function.generic { " [generic]" } else { "" },
                if function.externally_visible {
                    " [public]"
                } else {
                    ""
                },
                if reachable.contains(&function.def_path) {
                    " [reachable candidate]"
                } else {
                    " [unreachable candidate]"
                }
            );
        }
    } else {
        let sources: Vec<_> = match paths
            .iter()
            .map(quill_source::SourceFile::load)
            .collect::<Result<_, _>>()
        {
            Ok(sources) => sources,
            Err(error) => {
                eprintln!("quillc: {error}");
                return ExitCode::FAILURE;
            }
        };
        let pool = match quill_parallel::WorkPool::for_tasks(sources.len()) {
            Ok(pool) => pool,
            Err(error) => {
                eprintln!("quillc: could not start worker pool: {error}");
                return ExitCode::FAILURE;
            }
        };
        let lexed_files = quill_syntax::lex_files(&pool, &sources);
        for ((path, source), lexed) in paths.iter().zip(&sources).zip(lexed_files) {
            had_error |= lexed.had_error;
            for token in lexed.tokens {
                let text = source.slice(token.span.clone());
                println!(
                    "{}:{}..{}\t{:?}\t{:?}",
                    path, token.span.start, token.span.end, token.kind, text
                );
            }
            for diagnostic in lexed.diagnostics {
                let (line, column) = source.line_column(diagnostic.span.start);
                eprintln!("{path}:{line}:{column}: {}", diagnostic.message);
            }
        }
    }
    if had_error {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Parse crate roots breadth-first. Each module frontier is parsed in parallel,
/// then its external module declarations determine the next frontier.
fn parse_module_trees(
    roots: &[String],
    pool: &mut Option<quill_parallel::WorkPool>,
) -> std::io::Result<(Vec<ParsedSource>, Vec<String>)> {
    let roots: Vec<_> = roots.iter().map(|path| (None, path.clone())).collect();
    parse_module_roots(&roots, pool, false)
}

fn parse_module_trees_with_cfg(
    root: &str,
    cfg: &quill_cfg::CfgSet,
    pool: &mut Option<quill_parallel::WorkPool>,
) -> std::io::Result<(Vec<ParsedSource>, Vec<String>)> {
    let owner = CrateTargetId {
        package: "standalone".into(),
        target: root.to_owned(),
        cfg: cfg.clone(),
        edition: "2024".into(),
    };
    parse_module_roots(&[(Some(owner), root.to_owned())], pool, false)
}

fn parse_cargo_module_trees(
    roots: &[(CrateTargetId, String)],
    pool: &mut Option<quill_parallel::WorkPool>,
    summaries: bool,
) -> std::io::Result<(Vec<ParsedSource>, Vec<String>)> {
    let roots: Vec<_> = roots
        .iter()
        .map(|(owner, path)| (Some(owner.clone()), path.clone()))
        .collect();
    parse_module_roots(&roots, pool, summaries)
}

fn parse_module_roots(
    roots: &[(Option<CrateTargetId>, String)],
    pool: &mut Option<quill_parallel::WorkPool>,
    summaries: bool,
) -> std::io::Result<(Vec<ParsedSource>, Vec<String>)> {
    let root_requests = roots
        .iter()
        .map(|(owner, path)| (path.clone(), owner.clone()))
        .collect::<Vec<_>>();
    if root_requests.len() > 1 && pool.is_none() {
        *pool = Some(quill_parallel::WorkPool::available().map_err(std::io::Error::other)?);
    }
    let load_root = |(path, owner): &(String, Option<CrateTargetId>)| {
        quill_source::SourceFile::load(path)
            .map(|source| (source, Vec::new(), false, owner.clone()))
    };
    let loaded_roots = if root_requests.len() > 1 {
        pool.as_ref()
            .expect("root loading initialized the pool")
            .map(&root_requests, load_root)
    } else {
        root_requests.iter().map(load_root).collect()
    };
    let mut frontier = loaded_roots
        .into_iter()
        .collect::<std::io::Result<Vec<_>>>()?;
    let parse_cache = frontier
        .first()
        .map(|(source, _, _, _)| quill_query::ParseCache::for_source(source));
    let mut visited = HashSet::new();
    let mut parsed_sources = Vec::new();
    let mut errors = Vec::new();
    // Module frontiers are discovered sequentially, but parsing within each
    // frontier is independent. Keep the caller's pool alive for later phases.
    while !frontier.is_empty() {
        frontier.retain(|(source, module_path, path_is_override, owner)| {
            let identity = (
                source.path().to_path_buf(),
                module_path.clone(),
                *path_is_override,
                owner.clone(),
            );
            visited.insert(identity)
        });
        if frontier.is_empty() {
            break;
        }
        let inputs: Vec<_> = frontier
            .iter()
            .map(|(source, _, _, owner)| {
                (
                    source.clone(),
                    owner
                        .as_ref()
                        .map(|owner| owner.cfg.clone())
                        .unwrap_or_default(),
                    owner
                        .as_ref()
                        .map(|owner| owner.edition.clone())
                        .unwrap_or_else(|| "2024".into()),
                )
            })
            .collect();
        let cache = parse_cache.as_ref().expect("a root source was loaded");
        let parses = if inputs.len() <= 1 {
            inputs
                .iter()
                .map(|(source, cfg, edition)| {
                    if summaries {
                        let (parse, compact) =
                            cache.parse_file_summary_with_cfg_and_edition(source, cfg, edition);
                        (parse, compact)
                    } else {
                        (
                            cache.parse_file_with_cfg_and_edition(source, cfg, edition),
                            false,
                        )
                    }
                })
                .collect()
        } else {
            if pool.is_none() {
                *pool = Some(quill_parallel::WorkPool::available().map_err(std::io::Error::other)?);
            }
            pool.as_ref()
                .expect("pool initialized")
                .map(&inputs, |(source, cfg, edition)| {
                    if summaries {
                        cache.parse_file_summary_with_cfg_and_edition(source, cfg, edition)
                    } else {
                        (
                            cache.parse_file_with_cfg_and_edition(source, cfg, edition),
                            false,
                        )
                    }
                })
        };
        let mut next_requests = Vec::new();
        for ((source, module_path, path_is_override, owner), (parse, summary_only)) in
            frontier.into_iter().zip(parses)
        {
            let (requests, file_errors) =
                discover_module_requests(&source, &module_path, path_is_override, &owner, &parse);
            next_requests.extend(requests);
            errors.extend(file_errors);
            parsed_sources.push(ParsedSource {
                source,
                module_path,
                parse,
                owner,
                summary_only,
            });
        }
        let load_module = |(path, module_path, path_is_override, owner): &(
            PathBuf,
            Vec<String>,
            bool,
            Option<CrateTargetId>,
        )| {
            quill_source::SourceFile::load(path).map(|source| {
                (
                    source,
                    module_path.clone(),
                    *path_is_override,
                    owner.clone(),
                )
            })
        };
        let loaded_modules = if next_requests.len() > 1 {
            if pool.is_none() {
                *pool = Some(quill_parallel::WorkPool::available().map_err(std::io::Error::other)?);
            }
            pool.as_ref()
                .expect("module loading initialized the pool")
                .map(&next_requests, load_module)
        } else {
            next_requests.iter().map(load_module).collect()
        };
        frontier = loaded_modules
            .into_iter()
            .collect::<std::io::Result<Vec<_>>>()?;
    }
    parsed_sources.sort_by(|left, right| left.source.path().cmp(right.source.path()));
    if std::env::var_os("QUILL_CACHE_STATS").is_some()
        && let Some(cache) = parse_cache
    {
        let stats = cache.stats();
        eprintln!(
            "quill-query: hits={} misses={} invalid={} write_failures={}",
            stats.hits, stats.misses, stats.invalid_entries, stats.write_failures
        );
    }
    Ok((parsed_sources, errors))
}

fn module_directory(file: &Path, is_root: bool, path_is_override: bool) -> PathBuf {
    let parent = file.parent().unwrap_or_else(|| Path::new("."));
    if is_root || path_is_override || file.file_name().is_some_and(|name| name == "mod.rs") {
        parent.to_path_buf()
    } else {
        let stem = file.file_stem().unwrap_or_default();
        parent.join(stem)
    }
}

type ModuleLoadRequest = (PathBuf, Vec<String>, bool, Option<CrateTargetId>);

fn discover_module_requests(
    source: &quill_source::SourceFile,
    module_path: &[String],
    path_is_override: bool,
    owner: &Option<CrateTargetId>,
    parse: &quill_syntax::Parse,
) -> (Vec<ModuleLoadRequest>, Vec<String>) {
    let file_path = source.path().to_path_buf();
    let base = module_directory(&file_path, module_path.is_empty(), path_is_override);
    let mut requests = Vec::new();
    let mut errors = Vec::new();
    for module in parse
        .modules
        .iter()
        .filter(|module| module.body_span.is_none())
    {
        let inline_scope: Vec<_> = module
            .parent_scope
            .iter()
            .filter_map(|part| part.strip_prefix("mod:").map(str::to_owned))
            .collect();
        let mut disk_dir = base.clone();
        for component in &inline_scope {
            disk_dir.push(component);
        }
        let flat = disk_dir.join(format!("{}.rs", module.name));
        let nested = disk_dir.join(&module.name).join("mod.rs");
        let selected = if let Some(override_path) = &module.path_override {
            let path = disk_dir.join(override_path);
            if path.is_file() {
                Some(path)
            } else {
                errors.push(format!(
                    "{} declares module `{}` at #[path = {:?}], but {} does not exist",
                    file_path.display(),
                    module.name,
                    override_path,
                    path.display()
                ));
                None
            }
        } else {
            match (flat.is_file(), nested.is_file()) {
                (true, false) => Some(flat),
                (false, true) => Some(nested),
                (true, true) => {
                    errors.push(format!(
                        "{} declares module `{}` but both {} and {} exist",
                        file_path.display(),
                        module.name,
                        flat.display(),
                        nested.display()
                    ));
                    None
                }
                (false, false) => {
                    errors.push(format!(
                        "{} declares module `{}` but neither {} nor {} exists",
                        file_path.display(),
                        module.name,
                        flat.display(),
                        nested.display()
                    ));
                    None
                }
            }
        };
        if let Some(path) = selected {
            let mut child_module_path = module_path.to_vec();
            child_module_path.extend(inline_scope);
            child_module_path.push(module.name.clone());
            requests.push((
                path,
                child_module_path,
                module.path_override.is_some(),
                owner.clone(),
            ));
        }
    }
    (requests, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_self_struct_literals_to_their_canonical_module_type() {
        let source = quill_source::SourceFile::from_text(
            "node.rs",
            "struct Node { value: i32 } impl Node { fn make() -> Self { Self { value: 7 } } }",
        );
        let parsed = quill_syntax::parse(&source);
        let method = parsed
            .functions
            .iter()
            .find(|function| function.name == "make")
            .unwrap();
        let mut body = method.body.clone().unwrap();
        let paths = HashSet::from(["pkg::tree::Node".to_owned()]);
        rewrite_enum_references(
            &mut body,
            &["pkg".to_owned(), "tree".to_owned()],
            &[],
            &HashSet::new(),
            Some("Node"),
            &paths,
        );
        let quill_syntax::BodyExprKind::Block {
            tail: Some(value), ..
        } = body.kind
        else {
            panic!("expected a tail struct literal");
        };
        let quill_syntax::BodyExprKind::StructLiteral { path, .. } = value.kind else {
            panic!("expected a struct literal");
        };
        assert_eq!(path, "pkg::tree::Node");
    }

    #[test]
    fn rewrites_self_in_method_parameter_and_return_types() {
        let source = quill_source::SourceFile::from_text(
            "node.rs",
            "struct Node { value: i32 } impl Node { fn echo(self) -> Self { self } }",
        );
        let parsed = quill_syntax::parse(&source);
        let mut method = parsed
            .functions
            .iter()
            .find(|function| function.name == "echo")
            .unwrap()
            .clone();
        let paths = HashSet::from(["pkg::tree::Node".to_owned()]);
        let self_type =
            resolve_impl_owner_type(&["pkg".to_owned(), "tree".to_owned()], "Node", &paths)
                .unwrap();
        for parameter in &mut method.parameters {
            parameter.type_text = substitute_self_type(&parameter.type_text, &self_type);
        }
        if let Some(return_type) = &mut method.return_type {
            *return_type = substitute_self_type(return_type, &self_type);
        }
        assert_eq!(method.parameters[0].type_text, "pkg::tree::Node");
        assert_eq!(method.return_type.as_deref(), Some("pkg::tree::Node"));
    }

    #[test]
    fn canonical_struct_paths_include_crate_and_module_identity() {
        let source = quill_source::SourceFile::from_text("tree.rs", "struct Node { value: i32 }");
        let structure = quill_syntax::StructItem {
            name: "Node".to_owned(),
            scope: vec!["mod:tree".to_owned()],
            fields: Vec::new(),
            generic_parameters: Vec::new(),
            derived_traits: Vec::new(),
            repr_c: false,
            repr_layout_supported: true,
            span: 0..0,
            is_public: true,
        };
        let file = ParsedSource {
            source,
            module_path: vec!["api".to_owned()],
            parse: quill_syntax::Parse::default(),
            owner: None,
            summary_only: false,
        };
        assert_eq!(
            canonical_struct_path(&file, &structure, "catalog"),
            "catalog::api::tree::Node"
        );
    }

    #[test]
    fn named_word_struct_field_execution_matches_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-u64-struct-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        for integer_type in ["u64", "usize"] {
            let program = format!(
                "struct Pair {{ count: {integer_type} }} fn main() {{ let pair = Pair {{ count: 0x1_0000_0000{integer_type} }}; std::process::exit(if pair.count == 0x1_0000_0000{integer_type} {{ 42 }} else {{ 1 }}); }}"
            );
            std::fs::write(&source, program).unwrap();
            let source_file = quill_source::SourceFile::load(&source).unwrap();
            let parsed = quill_syntax::parse(&source_file);
            let object = quill_codegen::emit_subset_main_object(&parsed).unwrap();
            quill_link::link_elf_executable(&object, &quill_output, "_start").unwrap();
            let rustc = std::process::Command::new("rustc")
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .status()
                .unwrap();
            assert!(rustc.success());
            assert_eq!(
                std::process::Command::new(&quill_output)
                    .status()
                    .unwrap()
                    .code(),
                Some(42)
            );
            assert_eq!(
                std::process::Command::new(&rustc_output)
                    .status()
                    .unwrap()
                    .code(),
                Some(42)
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn three_field_aggregate_return_execution_matches_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-three-field-return-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        let program = "struct Triple { first: u64, answer: i32, last: usize } fn make(ignored: u64) -> Triple { Triple { first: 100u64, answer: 42, last: 9usize } } fn main() { let result = make(7u64); std::process::exit(result.answer); }";
        std::fs::write(&source, program).unwrap();
        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .status()
            .unwrap();
        assert!(rustc.success());
        assert_eq!(
            std::process::Command::new(&quill_output)
                .status()
                .unwrap()
                .code(),
            Some(42)
        );
        assert_eq!(
            std::process::Command::new(&rustc_output)
                .status()
                .unwrap()
                .code(),
            Some(42)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn string_and_scalar_aggregate_return_execution_matches_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-string-aggregate-return-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        let program = "struct Mixed { key: String, answer: i32, id: u64 } fn make(key: impl Into<String>, id: u64) -> Mixed { Mixed { key: key.into(), answer: 42, id: 9u64 } } fn main() { let record = make(\"hello\", 7u64); std::process::exit(if record.key.len() == 5 { record.answer } else { 1 }); }";
        std::fs::write(&source, program).unwrap();
        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .status()
            .unwrap();
        assert!(rustc.success());
        assert_eq!(
            std::process::Command::new(&quill_output)
                .status()
                .unwrap()
                .code(),
            Some(42)
        );
        assert_eq!(
            std::process::Command::new(&rustc_output)
                .status()
                .unwrap()
                .code(),
            Some(42)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn string_vec_and_btreemap_record_initialization_matches_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-record-containers-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        let program = "use std::collections::BTreeMap; struct Record { key: String, revision: u64, labels: Vec<String>, fields: BTreeMap<String, String> } fn make_record(key: impl Into<String>, revision: u64) -> Record { Record { key: key.into(), revision, labels: Vec::new(), fields: BTreeMap::new() } } fn main() { let record = make_record(\"hello\", 42u64); std::process::exit(if record.key.len() == 5 && record.revision == 42u64 && record.labels.len() == 0 && record.fields.len() == 0 { 42 } else { 1 }); }";
        std::fs::write(&source, program).unwrap();
        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .status()
            .unwrap();
        assert!(rustc.success());
        assert_eq!(
            std::process::Command::new(&quill_output)
                .status()
                .unwrap()
                .code(),
            Some(42)
        );
        assert_eq!(
            std::process::Command::new(&rustc_output)
                .status()
                .unwrap()
                .code(),
            Some(42)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn identifies_only_known_standard_library_type_imports() {
        assert!(is_supported_std_type_import(&[
            "std".to_owned(),
            "collections".to_owned(),
            "BTreeMap".to_owned(),
        ]));
        assert!(is_supported_std_type_import(&[
            "alloc".to_owned(),
            "vec".to_owned(),
            "Vec".to_owned(),
        ]));
        assert!(!is_supported_std_type_import(&[
            "std".to_owned(),
            "collections".to_owned(),
            "UnknownMap".to_owned(),
        ]));
        assert!(!is_supported_std_type_import(&[
            "other_crate".to_owned(),
            "collections".to_owned(),
            "BTreeMap".to_owned(),
        ]));
    }

    #[test]
    fn standard_library_type_imports_do_not_enter_the_function_alias_table() {
        let source = quill_source::SourceFile::from_text(
            "model.rs",
            "use std::collections::BTreeMap; fn main() {}",
        );
        let file = ParsedSource {
            parse: quill_syntax::parse_ast(&source),
            source,
            module_path: Vec::new(),
            owner: None,
            summary_only: false,
        };
        assert!(
            module_import_aliases(&file, &HashSet::new(), "")
                .unwrap()
                .is_empty()
        );

        let source = quill_source::SourceFile::from_text(
            "model.rs",
            "use other_crate::BTreeMap; fn main() {}",
        );
        let file = ParsedSource {
            parse: quill_syntax::parse_ast(&source),
            source,
            module_path: Vec::new(),
            owner: None,
            summary_only: false,
        };
        assert!(module_import_aliases(&file, &HashSet::new(), "").is_err());
    }

    #[test]
    fn cargo_checker_rechecks_when_a_workspace_dependency_signature_changes() {
        let root = std::env::temp_dir().join(format!(
            "quillc-cargo-interface-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let app_src = root.join("app/src");
        let model_src = root.join("model/src");
        std::fs::create_dir_all(&app_src).unwrap();
        std::fs::create_dir_all(&model_src).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\", \"model\"]\ndefault-members = [\"app\"]\nresolver = \"3\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\nmodel_alias = { package = \"model\", path = \"../model\" }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("model/Cargo.toml"),
            "[package]\nname = \"model\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            app_src.join("main.rs"),
            "fn main() { model_alias::api::check(true); }\n",
        )
        .unwrap();
        std::fs::write(model_src.join("lib.rs"), "pub mod api;\n").unwrap();
        let interface_path = model_src.join("api.rs");
        std::fs::write(
            &interface_path,
            "pub fn check(value: bool) -> bool { value }\n",
        )
        .unwrap();

        let manifest = root.join("Cargo.toml").display().to_string();
        assert_eq!(
            cargo_workspace_command(vec![manifest.clone()], true),
            ExitCode::SUCCESS
        );

        std::fs::write(
            &interface_path,
            "pub fn check(value: i32) -> bool { value > 0 }\n",
        )
        .unwrap();
        assert_eq!(
            cargo_workspace_command(vec![manifest], true),
            ExitCode::FAILURE
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovers_nested_files_and_preserves_module_paths() {
        let root = std::env::temp_dir().join(format!(
            "quillc-module-tree-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let nested_dir = root.join("net/client");
        std::fs::create_dir_all(&nested_dir).unwrap();
        std::fs::write(root.join("lib.rs"), "mod net; fn root_fn() {}\n").unwrap();
        std::fs::write(
            root.join("net.rs"),
            "pub fn server() {} mod client { mod codec; fn inline_fn() {} }\n",
        )
        .unwrap();
        std::fs::write(nested_dir.join("codec.rs"), "pub fn decode() {}\n").unwrap();

        let mut pool = None;
        let (files, errors) =
            parse_module_trees(&[root.join("lib.rs").display().to_string()], &mut pool).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(files.len(), 3);
        let inputs: Vec<_> = files
            .iter()
            .map(|file| (file.module_path.clone(), file.parse.clone()))
            .collect();
        let hir = quill_hir::lower_module_files("fixture", &inputs);
        let scopes: Vec<_> = hir
            .functions
            .iter()
            .map(|function| {
                (
                    function.def_path.name.as_str(),
                    function.def_path.scope.clone(),
                )
            })
            .collect();
        assert!(scopes.contains(&("root_fn", vec![])));
        assert!(scopes.contains(&("server", vec!["mod:net".into()])));
        assert!(scopes.contains(&("inline_fn", vec!["mod:net".into(), "mod:client".into()])));
        assert!(scopes.contains(&(
            "decode",
            vec!["mod:net".into(), "mod:client".into(), "mod:codec".into()]
        )));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn follows_path_attributes_using_rust_module_directory_rules() {
        let root = std::env::temp_dir().join(format!(
            "quillc-module-path-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("lib.rs"),
            "#[path = \"implementation.rs\"] mod api;\n",
        )
        .unwrap();
        std::fs::write(root.join("implementation.rs"), "pub mod client;\n").unwrap();
        std::fs::write(root.join("client.rs"), "pub fn request() {}\n").unwrap();

        let mut pool = None;
        let (files, errors) =
            parse_module_trees(&[root.join("lib.rs").display().to_string()], &mut pool).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(files.len(), 3);
        assert!(
            files
                .iter()
                .any(|file| file.module_path == ["api", "client"])
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn builds_and_runs_an_empty_main_without_a_rust_backend() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-subset-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(&source, "fn main() {}\n").unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        let result = std::process::Command::new(&output).status().unwrap();
        assert_eq!(result.code(), Some(0));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cargo_build_subset_matches_cargo_for_a_binary_package() {
        let root = std::env::temp_dir().join(format!(
            "quillc-cargo-build-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let manifest = root.join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"mini-cargo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\ndefault = [\"fast\"]\nfast = []\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "#[cfg(feature = \"fast\")] fn answer() -> i32 { 40 + 2 } #[cfg(not(feature = \"fast\"))] fn answer() -> i32 { 0 } fn main() { std::process::exit(answer()); }\n",
        )
        .unwrap();
        let quill_output = root.join("quill-program");
        let cargo_target = root.join("cargo-target");
        compile_cargo_subset(
            &manifest.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let cargo = std::process::Command::new("cargo")
            .args(["build", "--manifest-path"])
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&cargo_target)
            .output()
            .unwrap();
        assert!(
            cargo.status.success(),
            "{}",
            String::from_utf8_lossy(&cargo.stderr)
        );
        let cargo_output = cargo_target.join("debug/mini-cargo");
        assert_eq!(
            std::process::Command::new(&quill_output)
                .status()
                .unwrap()
                .code(),
            Some(42)
        );
        assert_eq!(
            std::process::Command::new(&cargo_output)
                .status()
                .unwrap()
                .code(),
            Some(42)
        );
        std::fs::write(
            root.join("src/main.rs"),
            "#[cfg(feature = \"fast\")] fn answer() -> i32 { 43 } #[cfg(not(feature = \"fast\"))] fn answer() -> i32 { 0 } fn main() { std::process::exit(answer()); }\n",
        )
        .unwrap();
        compile_cargo_subset(
            &manifest.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        assert_eq!(
            std::process::Command::new(&quill_output)
                .status()
                .unwrap()
                .code(),
            Some(43),
            "a semantic source edit must invalidate the executable cache"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_rust_include_text_disables_semantic_artifact_cache() {
        let root = std::env::temp_dir().join(format!(
            "quillc-cargo-include-cache-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let manifest = root.join("Cargo.toml");
        let source = root.join("src/main.rs");
        std::fs::write(
            &manifest,
            "[workspace]\nmembers = [\".\"]\nresolver = \"3\"\n\n[package]\nname = \"include-cache-check\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(&source, "fn main() {}\n").unwrap();
        let output = root.join("quill-program");
        compile_cargo_subset(
            &manifest.display().to_string(),
            &output.display().to_string(),
        )
        .unwrap();
        assert!(restore_cached_cargo_build(
            &manifest.display().to_string(),
            &output.display().to_string()
        ));

        std::fs::write(&source, "fn main() {}\n// include marker\n").unwrap();
        assert!(!restore_cached_cargo_build(
            &manifest.display().to_string(),
            &output.display().to_string()
        ));
        compile_cargo_subset(
            &manifest.display().to_string(),
            &output.display().to_string(),
        )
        .unwrap();
        assert!(!restore_cached_cargo_build(
            &manifest.display().to_string(),
            &output.display().to_string()
        ));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cargo_build_subset_links_transitive_workspace_library_dependencies() {
        let root = std::env::temp_dir().join(format!(
            "quillc-cargo-workspace-build-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let app = root.join("app");
        let util = root.join("util");
        let base = root.join("base");
        std::fs::create_dir_all(app.join("src")).unwrap();
        std::fs::create_dir_all(util.join("src")).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\", \"util\", \"base\"]\ndefault-members = [\"app\"]\nresolver = \"3\"\n",
        )
        .unwrap();
        std::fs::write(
            app.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\nutil = { path = \"../util\" }\n",
        )
        .unwrap();
        std::fs::write(
            util.join("Cargo.toml"),
            "[package]\nname = \"util\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\ncore_base = { package = \"base\", path = \"../base\" }\n",
        )
        .unwrap();
        std::fs::write(
            base.join("Cargo.toml"),
            "[package]\nname = \"base\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            app.join("src/main.rs"),
            "use util::api::answer; fn main() { std::process::exit(answer()); }\n",
        )
        .unwrap();
        std::fs::write(util.join("src/lib.rs"), "pub mod api;\n").unwrap();
        std::fs::write(
            util.join("src/api.rs"),
            "pub fn answer() -> i32 { core_base::value() }\n",
        )
        .unwrap();
        std::fs::write(base.join("src/lib.rs"), "pub fn value() -> i32 { 42 }\n").unwrap();
        let manifest = root.join("Cargo.toml");
        let quill_output = root.join("quill-program");
        let cargo_target = root.join("cargo-target");
        compile_cargo_subset(
            &manifest.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let cargo = std::process::Command::new("cargo")
            .args(["build", "--manifest-path"])
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&cargo_target)
            .output()
            .unwrap();
        assert!(
            cargo.status.success(),
            "{}",
            String::from_utf8_lossy(&cargo.stderr)
        );
        let cargo_output = cargo_target.join("debug/app");
        assert_eq!(
            std::process::Command::new(&quill_output)
                .status()
                .unwrap()
                .code(),
            Some(42)
        );
        assert_eq!(
            std::process::Command::new(&cargo_output)
                .status()
                .unwrap()
                .code(),
            Some(42)
        );
        std::fs::write(base.join("src/lib.rs"), "pub fn value() -> i32 { 43 }\n").unwrap();
        compile_cargo_subset(
            &manifest.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        assert_eq!(
            std::process::Command::new(&quill_output)
                .status()
                .unwrap()
                .code(),
            Some(43),
            "a semantic edit in a transitive dependency must invalidate the executable cache"
        );
        std::fs::write(
            app.join("src/main.rs"),
            "fn main() { std::process::exit(util::api::helper()); }\n",
        )
        .unwrap();
        assert!(
            compile_cargo_subset(
                &manifest.display().to_string(),
                &quill_output.display().to_string()
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn builds_and_runs_supported_external_module_calls() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-module-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let module = root.join("math.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        std::fs::write(
            &source,
            "mod math; fn main() { std::process::exit(math::answer()); }\n",
        )
        .unwrap();
        std::fs::write(
            &module,
            "pub fn answer() -> i32 { compute() } fn compute() -> i32 { 42 }\n",
        )
        .unwrap();
        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .args(["--edition=2024", "-C", "overflow-checks=off"])
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .unwrap();
        assert!(
            rustc.status.success(),
            "{}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        assert_eq!(
            std::process::Command::new(&quill_output)
                .status()
                .unwrap()
                .code(),
            Some(42)
        );
        assert_eq!(
            std::process::Command::new(&rustc_output)
                .status()
                .unwrap()
                .code(),
            Some(42)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolves_qualified_calls_relative_to_the_current_module() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-relative-qualified-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(root.join("nested")).unwrap();
        let source = root.join("main.rs");
        let nested = root.join("nested.rs");
        let shard = root.join("nested/shard.rs");
        let output = root.join("quill-program");
        std::fs::write(
            &source,
            "mod nested; fn main() { std::process::exit(nested::shard::answer()); }\n",
        )
        .unwrap();
        std::fs::write(&nested, "pub mod shard;\n").unwrap();
        std::fs::write(&shard, "pub fn answer() -> i32 { 42 }\n").unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        assert_eq!(
            std::process::Command::new(&output).status().unwrap().code(),
            Some(42)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn builds_and_runs_explicit_module_import_aliases() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-use-alias-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "mod math; use math::answer as calculate; fn main() { std::process::exit(calculate()); }\n",
        )
        .unwrap();
        std::fs::write(root.join("math.rs"), "pub fn answer() -> i32 { 42 }\n").unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        assert_eq!(
            std::process::Command::new(&output).status().unwrap().code(),
            Some(42)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolves_same_named_functions_in_distinct_inline_modules() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-inline-module-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "mod left { fn value() -> i32 { 40 } } mod right { fn value() -> i32 { 2 } } fn main() { std::process::exit(left::value() + right::value()); }\n",
        )
        .unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        assert_eq!(
            std::process::Command::new(&output).status().unwrap().code(),
            Some(42)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn main_executes_locals_branches_loops_and_helper_calls() {
        let root = std::env::temp_dir().join(format!("quillc-main-body-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        let cases = [
            (
                "fn main() { let mut i = 0; let mut sum = 0; for i in 1..=5 { sum = sum + i; } std::process::exit(sum); }",
                15,
            ),
            (
                "fn id(x: i32) -> i32 { x } fn main() { let mut value = 4; if value == 4 { value = value + 3; } std::process::exit(id(value)); }",
                7,
            ),
            (
                "fn main() { let mut value = 4; if value == 5 { value = 2; } else { value = 9; } std::process::exit(value); }",
                9,
            ),
            (
                "fn main() { let mut enabled = false; if enabled { enabled = false; } else { enabled = true; } std::process::exit(if enabled { 42 } else { 1 }); }",
                42,
            ),
            (
                "fn count_chars(input: &str) -> usize { let mut count = 0usize; for ch in input.chars() { count = count + 1usize; } count } fn main() { std::process::exit(if count_chars(\"aé🦀\") == 3usize { 42 } else { 1 }); }",
                42,
            ),
            (
                "fn count_ascii(input: &str) -> usize { let mut count = 0usize; for ch in input.chars() { if ch.is_ascii_alphanumeric() { count = count + 1usize; } } count } fn main() { std::process::exit(if count_ascii(\"a-é2\") == 2usize { 42 } else { 1 }); }",
                42,
            ),
            (
                "fn main() { let mut value = 0; loop { value = value + 1; if value == 6 { break; } } std::process::exit(value); }",
                6,
            ),
            (
                "fn check(value: i32) -> Result<(), &'static str> { if value == 0 { return Err(\"empty\"); } Ok(()) } fn main() { std::process::exit(if check(1).is_ok() { if check(0).is_ok() { 1 } else { 42 } } else { 2 }); }",
                42,
            ),
            (
                "struct Row { revision: u64 } fn check(row: &Row) -> Result<(), &'static str> { if row.revision == 0 { return Err(\"revision\"); } Ok(()) } fn main() { let row = Row { revision: 7u64 }; std::process::exit(if \"x\".is_empty() { 1 } else if check(&row).is_ok() { 42 } else { 2 }); }",
                42,
            ),
            (
                "struct Row { revision: u64 } impl Row { fn revision(&self) -> u64 { self.revision } } fn main() { let row = Row { revision: 42u64 }; std::process::exit(if row.revision() == 42u64 { 42 } else { 1 }); }",
                42,
            ),
            (
                "struct Row { revision: u64 } impl Row { fn into_revision(self) -> u64 { self.revision } } fn main() { let row = Row { revision: 42u64 }; std::process::exit(if row.into_revision() == 42u64 { 42 } else { 1 }); }",
                42,
            ),
            (
                "struct Row { revision: u64 } impl Row { fn into_revision(self) -> u64 { self.revision } } fn read(row: Row) -> u64 { row.into_revision() } fn main() { let row = Row { revision: 42u64 }; std::process::exit(if read(row) == 42u64 { 42 } else { 1 }); }",
                42,
            ),
            (
                "struct Counter { low: u64, high: u64 } impl Counter { fn bumped(mut self) -> Self { self.low = self.low + 1u64; self } fn total(&self) -> u64 { self.low + self.high } } fn main() { let counter = Counter { low: 40u64, high: 2u64 }; let updated = counter.bumped(); std::process::exit(if updated.total() == 43u64 { 42 } else { 1 }); }",
                42,
            ),
            (
                "struct Labels { values: Vec<String>, revision: u64 } impl Labels { fn push(mut self, value: impl Into<String>) -> Self { self.values.push(value.into()); self } fn count(&self) -> usize { self.values.len() } } fn main() { let labels = Labels { values: Vec::new(), revision: 1u64 }; let labels = labels.push(\"a\"); let labels = labels.push(\"bc\"); std::process::exit(if labels.count() == 2usize { 42 } else { 1 }); }",
                42,
            ),
            (
                "struct Labels { values: Vec<String> } fn push(labels: &mut Labels, value: impl Into<String>) { labels.values.push(value.into()); } fn count(labels: &Labels) -> usize { labels.values.len() } fn main() { let mut labels = Labels { values: Vec::new() }; push(&mut labels, \"a\"); push(&mut labels, \"bc\"); std::process::exit(if count(&labels) == 2usize { 42 } else { 1 }); }",
                42,
            ),
            (
                "struct Row { name: String, id: u64, labels: Vec<String> } impl Row { fn new(name: impl Into<String>, id: u64) -> Self { Self { name: name.into(), id, labels: Vec::new() } } } fn total(rows: &[Row]) -> u64 { let mut sum = 0u64; for row in rows.iter() { sum = sum + row.id; } sum } fn main() { let mut rows = Vec::new(); let row = Row::new(\"x\", 42u64); rows.push(row); std::process::exit(if total(&rows) == 42u64 { 42 } else { 1 }); }",
                42,
            ),
            (
                "use std::collections::BTreeMap; struct Map { values: BTreeMap<String, String> } impl Map { fn put(&mut self, key: impl Into<String>, value: impl Into<String>) { self.values.insert(key.into(), value.into()); } fn count(&self) -> usize { self.values.len() } } fn main() { let mut map = Map { values: BTreeMap::new() }; map.put(\"key\", \"old\"); map.put(\"key\", \"new\"); map.put(\"a\", \"1\"); map.put(\"b\", \"2\"); map.put(\"c\", \"3\"); map.put(\"d\", \"4\"); map.put(\"e\", \"5\"); std::process::exit(if map.count() == 6usize { 42 } else { 1 }); }",
                42,
            ),
            (
                "struct Row { revision: u64 } impl Row { fn revision(&self) -> u64 { self.revision } } fn read(row: &Row) -> u64 { row.revision() } fn main() { let row = Row { revision: 42u64 }; std::process::exit(if read(&row) == 42u64 { 42 } else { 1 }); }",
                42,
            ),
            (
                "struct Counter { value: u64 } impl Counter { fn increment(&mut self) { self.value = self.value + 1u64; } fn value(&self) -> u64 { self.value } } fn increment(counter: &mut Counter) { counter.increment(); } fn main() { let mut counter = Counter { value: 41u64 }; increment(&mut counter); std::process::exit(if counter.value() == 42u64 { 42 } else { 1 }); }",
                42,
            ),
            (
                "fn text(input: &str) -> &str { input } fn length(input: &str) -> u64 { text(input).len() as u64 } fn main() { std::process::exit(if length(\"abc\") == 3u64 { 42 } else { 1 }); }",
                42,
            ),
            (
                "fn hash(input: &str) -> u64 { input.bytes().fold(0u64, |state, byte| state.wrapping_mul(33).wrapping_add(u64::from(byte))) } fn main() { std::process::exit(if hash(\"abc\") == 108966u64 { 42 } else { 1 }); }",
                42,
            ),
            (
                "fn total(values: &[u64]) -> u64 { values.iter().fold(0u64, |sum, value| sum + value) } fn main() { std::process::exit(if total(&[10u64, 20u64, 12u64]) == 42u64 { 42 } else { 1 }); }",
                42,
            ),
            (
                "fn stats(values: &[u64]) -> (i32, u64) { values.iter().fold((0i32, 0u64), |(count, hash), value| (count + 1i32, hash ^ value)) } fn main() { let (count, hash) = stats(&[10u64, 20u64, 12u64]); std::process::exit(if count == 3i32 && hash == 18u64 { 42 } else { 1 }); }",
                42,
            ),
            (
                "fn pair() -> (usize, u64) { (3usize, 39u64) } fn second() -> u64 { pair().1 } fn main() { std::process::exit(if second() == 39u64 { 42 } else { 1 }); }",
                42,
            ),
            (
                "fn main() { let mut values: Vec<u64> = Vec::new(); values.push(19); values.push(23); values.push(17); values.push(29); values.push(12); let mut total = 0u64; for value in values { total = total + value; } std::process::exit(if total == 100u64 { 42 } else { 1 }); }",
                42,
            ),
            (
                "fn main() { let mut values = Vec::new(); values.push(19u64); values.push(23u64); let mut total = 0u64; for value in values { total = total + value; } std::process::exit(if total == 42u64 { 42 } else { 1 }); }",
                42,
            ),
            (
                "fn main() { let mut values: Vec<i32> = Vec::new(); values.push(19); values.push(23); let mut total = 0; for value in values { total = total + value; } std::process::exit(if total == 42 { 42 } else { 1 }); }",
                42,
            ),
            (
                "#[derive(Clone, PartialEq)] struct Marker { value: u64 } fn main() { let first = Marker { value: 42u64 }; let second = first.clone(); if first == second { println!(\"same\"); } else { println!(\"different\"); } }",
                0,
            ),
        ];
        for (program, expected) in cases {
            std::fs::write(&source, format!("{program}\n")).unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "rustc failed: {}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code(), "{program}");
            assert_eq!(quill_status.code(), Some(expected), "{program}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn borrowed_integer_slice_for_each_matches_rustc() {
        let root = std::env::temp_dir().join(format!("quillc-array-slice-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        let programs = [
            "fn total(values: &[i32]) -> i32 { let mut sum = 0; for value in values { sum = sum + value; } sum } fn main() { std::process::exit(total(&[11, 13, 18])); }",
            "fn total(values: &[u64]) -> i32 { let mut sum = 0u64; for value in values { sum = sum + value; } if sum == 42u64 { 42 } else { 1 } } fn main() { std::process::exit(total(&[11u64, 13u64, 18u64])); }",
            "fn total(values: &[(i32, i32)]) -> i32 { let mut sum = 0; for (left, right) in values { sum = sum + left + right; } sum } fn main() { std::process::exit(total(&[(11, 13), (18, 0)])); }",
            "fn check(values: &[(i32, u64)]) -> i32 { for (_left, right) in values { if right + 29u64 == 42u64 { return 42; } } 1 } fn main() { std::process::exit(check(&[(11, 13u64), (18, 0u64)])); }",
        ];
        for program in programs {
            std::fs::write(&source, program).unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "rustc failed: {}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill = std::process::Command::new(&quill_output).output().unwrap();
            let rustc = std::process::Command::new(&rustc_output).output().unwrap();
            assert_eq!(quill.status.code(), rustc.status.code(), "{program}");
            assert_eq!(quill.status.code(), Some(42), "{program}");
            assert_eq!(quill.stdout, rustc.stdout, "{program}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn builds_and_runs_main_with_initialized_locals() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-reject-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        std::fs::write(&source, "fn main() { let answer = 42; }\n").unwrap();
        let output = root.join("program");
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        assert_eq!(
            std::process::Command::new(output).status().unwrap().code(),
            Some(0)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tuple_literal_bindings_execute_like_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-tuple-binding-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "fn main() { let (count, digest) = (40i32, 2i32); std::process::exit(count + digest); }\n",
        )
        .unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        assert_eq!(
            std::process::Command::new(&output).status().unwrap().code(),
            Some(42)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pair_return_calls_match_rustc_across_sysv_return_registers() {
        let root = std::env::temp_dir().join(format!(
            "quillc-pair-return-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        let programs = [
            "fn pair(value: u64) -> (u64, i32) { (value, 42) } fn main() { let (whole, answer) = pair(100u64); std::process::exit(answer); }",
            "fn pair(value: u64) -> (i32, u64) { (42, value) } fn main() { let (answer, whole) = pair(100u64); std::process::exit(answer); }",
        ];
        for program in programs {
            std::fs::write(&source, program).unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .status()
                .unwrap();
            assert!(rustc.success());
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code());
            assert_eq!(quill_status.code(), Some(42));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plain_two_word_struct_return_and_field_projection_match_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-record-return-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        for representation in ["", "#[repr(C)] "] {
            for body in [
                "fn project_x() -> u64 { point().x } fn project_y() -> u64 { point().y } fn main() { let x = project_x(); let y = project_y(); println!(\"{x} {y}\"); }",
                "fn main() { let point = point(); let x = point.x; let y = point.y; println!(\"{x} {y}\"); }",
            ] {
                std::fs::write(
                    &source,
                    format!(
                        "{representation}struct Point {{ x: u64, y: u64 }} fn point() -> Point {{ Point {{ x: 40u64, y: 42u64 }} }} {body}"
                    ),
                )
                .unwrap();
                compile_subset_main(
                    &source.display().to_string(),
                    &quill_output.display().to_string(),
                )
                .unwrap();
                let rustc = std::process::Command::new("rustc")
                    .args(["--edition=2024"])
                    .arg(&source)
                    .arg("-o")
                    .arg(&rustc_output)
                    .status()
                    .unwrap();
                assert!(rustc.success());
                let quill_result = std::process::Command::new(&quill_output).output().unwrap();
                let rustc_result = std::process::Command::new(&rustc_output).output().unwrap();
                assert_eq!(quill_result.status.code(), rustc_result.status.code());
                assert_eq!(quill_result.stdout, rustc_result.stdout);
                assert_eq!(quill_result.stdout, b"40 42\n");
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn named_decimal_and_padded_hex_println_match_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-println-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        std::fs::write(
            &source,
            "fn report() -> (usize, u64) { (123usize, 0xabcu64) } fn main() { let (records, digest) = report(); let zero = 0usize; println!(\"{{records}}={records} digest={digest:016x} zero={zero}\"); }",
        )
        .unwrap();
        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .args(["--edition=2024"])
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .status()
            .unwrap();
        assert!(rustc.success());
        let quill = std::process::Command::new(&quill_output).output().unwrap();
        let rustc = std::process::Command::new(&rustc_output).output().unwrap();
        assert_eq!(quill.status.code(), rustc.status.code());
        assert_eq!(quill.stdout, rustc.stdout);
        assert_eq!(
            quill.stdout,
            b"{records}=123 digest=0000000000000abc zero=0\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn u64_arithmetic_preserves_high_bits_and_matches_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-u64-arithmetic-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        std::fs::write(
            &source,
            "fn sub(a: u64, b: u64) -> u64 { a - b } fn div(a: u64, b: u64) -> u64 { a / b } fn rem(a: u64, b: u64) -> u64 { a % b } fn main() { let sum = 4294967296u64 + 9u64; let difference = sub(sum, 9u64); let quotient = div(18446744073709551614u64, 2u64); let remainder = rem(18446744073709551614u64, 10u64); println!(\"{sum} {difference} {quotient} {remainder}\"); }\n",
        )
        .unwrap();
        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .args(["--edition=2024", "-C", "overflow-checks=off"])
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .status()
            .unwrap();
        assert!(rustc.success());
        let quill = std::process::Command::new(&quill_output).output().unwrap();
        let rustc = std::process::Command::new(&rustc_output).output().unwrap();
        assert_eq!(quill.status.code(), rustc.status.code());
        assert_eq!(quill.stdout, rustc.stdout);
        assert_eq!(
            quill.stdout,
            b"4294967305 4294967296 9223372036854775807 4\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn passes_empty_slice_pointer_and_length_in_sysv_registers() {
        let root = std::env::temp_dir().join(format!(
            "quillc-slice-abi-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        std::fs::write(
            &source,
            "fn consume(values: &[u64]) -> i32 { if values.len() == 0 { 42 } else { 1 } } fn main() { std::process::exit(consume(&[])); }",
        )
        .unwrap();
        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .args(["--edition=2024"])
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .status()
            .unwrap();
        assert!(rustc.success());
        let quill_status = std::process::Command::new(&quill_output).status().unwrap();
        let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
        assert_eq!(quill_status.code(), rustc_status.code());
        assert_eq!(quill_status.code(), Some(42));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn named_i32_struct_literals_and_field_reads_match_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-struct-field-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        std::fs::write(
            &source,
            "struct Pair { left: i32, right: i32 } fn value() -> i32 { 42 } fn main() { let pair = Pair { right: value(), left: 3 }; std::process::exit(pair.right); }\n",
        )
        .unwrap();
        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .args(["--edition=2024"])
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .unwrap();
        assert!(
            rustc.status.success(),
            "{}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let quill_status = std::process::Command::new(&quill_output).status().unwrap();
        let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
        assert_eq!(quill_status.code(), rustc_status.code());
        assert_eq!(quill_status.code(), Some(42));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_mismatched_struct_field_type_during_checking() {
        let root = std::env::temp_dir().join(format!(
            "quillc-struct-type-error-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "struct Pair { value: i32 } fn main() { let pair = Pair { value: true }; std::process::exit(0); }\n",
        )
        .unwrap();
        let error =
            compile_subset_main(&source.display().to_string(), &output.display().to_string())
                .unwrap_err();
        assert!(error.contains("struct field initializer"), "{error}");
        assert!(!output.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn builds_a_program_with_constant_i32_arithmetic() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-exit-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(&source, "fn main() { std::process::exit(20 + 17); }\n").unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        let result = std::process::Command::new(&output).status().unwrap();
        assert_eq!(result.code(), Some(37));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dynamic_i32_arithmetic_matches_rustc_with_overflow_checks_disabled() {
        let root = std::env::temp_dir().join(format!(
            "quillc-arithmetic-differential-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        let cases = [
            ("a + b", 17, 6),
            ("a - b", 6, 17),
            ("a * b", 7, 8),
            ("a / b", -81, 7),
            ("a % b", -81, 7),
            ("a + b", 2_147_483_647, 1),
            ("a - b", -2_147_483_647, 2),
            ("a * b", 1_073_741_824, 4),
            ("let product = a * b; product / 2 + (a - b) % 5", 37, 8),
        ];
        for (expression, left, right) in cases {
            let program = format!(
                "fn calc(a: i32, b: i32) -> i32 {{ {expression} }} fn main() {{ std::process::exit(calc({left}, {right})); }}\n"
            );
            std::fs::write(&source, program).unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "rustc failed: {}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(
                quill_status.code(),
                rustc_status.code(),
                "behavior differs for `{expression}` with {left}, {right}"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn u64_wrapping_methods_match_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-u64-wrapping-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        std::fs::write(
            &source,
            "fn base() -> u64 { 1u64 } fn mix(state: u64, byte: u64) -> u64 { state.wrapping_mul(33).wrapping_add(byte) } fn sum7(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) -> u64 { a.wrapping_add(b).wrapping_add(c).wrapping_add(d).wrapping_add(e).wrapping_add(f).wrapping_add(g) } fn status() -> i32 { if mix(18446744073709551615u64, 1) == 18446744073709551584u64 && mix(base(), 2) > 34u64 && sum7(1, 2, 3, 4, 5, 6, 7) == 28u64 { 42 } else { 41 } } fn main() { std::process::exit(status()); }\n",
        )
        .unwrap();
        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .args(["--edition=2024"])
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .expect("rustc must be available for differential compiler tests");
        assert!(
            rustc.status.success(),
            "rustc failed: {}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let quill_status = std::process::Command::new(&quill_output).status().unwrap();
        let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
        assert_eq!(quill_status.code(), Some(42));
        assert_eq!(quill_status.code(), rustc_status.code());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn utf8_string_literals_pass_as_str_slices_and_match_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-str-literal-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        std::fs::write(
            &source,
            "fn byte_len(value: &str) -> usize { let copy = value; copy.len() } fn main() { std::process::exit(if byte_len(\"café 🦀\") == 10usize && byte_len(\"\\0x\") == 2usize && byte_len(\"\") == 0usize { 42 } else { 41 }); }\n",
        )
        .unwrap();
        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .args(["--edition=2024"])
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .expect("rustc must be available for differential compiler tests");
        assert!(
            rustc.status.success(),
            "rustc failed: {}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let quill_status = std::process::Command::new(&quill_output).status().unwrap();
        let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
        assert_eq!(quill_status.code(), Some(42));
        assert_eq!(quill_status.code(), rustc_status.code());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn impl_into_string_uses_native_owned_storage_and_matches_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-owned-string-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        std::fs::write(
            &source,
            "fn own(value: impl Into<String>, revision: u64) -> String { value.into() } fn byte_len(value: String) -> usize { let copy = value; copy.len() } fn main() { let owned = own(\"café 🦀\", 7u64); std::process::exit(if byte_len(owned) == 10usize { 42 } else { 41 }); }\n",
        )
        .unwrap();
        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .args(["--edition=2024"])
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .expect("rustc must be available for differential compiler tests");
        assert!(
            rustc.status.success(),
            "rustc failed: {}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let quill_status = std::process::Command::new(&quill_output).status().unwrap();
        let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
        assert_eq!(quill_status.code(), Some(42));
        assert_eq!(quill_status.code(), rustc_status.code());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutable_string_return_matches_rustc_output() {
        let root = std::env::temp_dir().join(format!(
            "quillc-string-return-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        std::fs::write(
            &source,
            "fn normalize(input: &str) -> String { let mut output = String::with_capacity(input.len()); let mut separator = false; for ch in input.chars() { if ch.is_ascii_alphanumeric() { if separator && !output.is_empty() { output.push('-'); } output.extend(ch.to_lowercase()); separator = false; } else { separator = true; } } output } fn main() { let output = normalize(\"Catalog record 42\"); println!(\"{output}\"); }\n",
        )
        .unwrap();
        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .args(["--edition=2024"])
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .expect("rustc must be available for differential compiler tests");
        assert!(
            rustc.status.success(),
            "rustc failed: {}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let quill_result = std::process::Command::new(&quill_output).output().unwrap();
        let rustc_result = std::process::Command::new(&rustc_output).output().unwrap();
        assert_eq!(quill_result.status.code(), Some(0));
        assert_eq!(quill_result.stdout, b"catalog-record-42\n");
        assert_eq!(quill_result.stdout, rustc_result.stdout);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn named_decimal_format_captures_create_owned_strings_and_match_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-format-string-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        std::fs::write(
            &source,
            "fn label(revision: u64) -> String { format!(\"item-{revision}\") } fn signed(value: i32) -> String { format!(\"n{value}\") } fn own(value: impl Into<String>, version: u64) -> String { value.into() } fn byte_len(value: String) -> usize { let copy = value; copy.len() } fn main() { let owned = own(label(128u64), 7u64); let zero = label(0u64); let max = label(18446744073709551615u64); let negative = signed(-42); let positive = signed(42); println!(\"{owned}|{zero}|{max}\"); println!(\"{negative}\"); println!(\"{positive}\"); std::process::exit(if byte_len(owned) == 8usize && byte_len(zero) == 6usize && byte_len(max) == 25usize && byte_len(negative) == 4usize { 42 } else { 41 }); }\n",
        )
        .unwrap();
        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .args(["--edition=2024"])
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .expect("rustc must be available for differential compiler tests");
        assert!(
            rustc.status.success(),
            "rustc failed: {}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let quill_result = std::process::Command::new(&quill_output).output().unwrap();
        let rustc_result = std::process::Command::new(&rustc_output).output().unwrap();
        assert_eq!(quill_result.status.code(), Some(42));
        assert_eq!(quill_result.status.code(), rustc_result.status.code());
        assert_eq!(
            quill_result.stdout,
            b"item-128|item-0|item-18446744073709551615\nn-42\nn42\n"
        );
        assert_eq!(quill_result.stdout, rustc_result.stdout);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn boolean_conditions_short_circuit_and_match_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-boolean-differential-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        let cases = [
            ("a != 0 && 1 / a > 0", 0, 7),
            ("a != 0 && 1 / a > 0", 1, 42),
            ("a != 0 && 1 / a > 0", -1, 7),
            ("a == 0 || 1 / a > 0", 0, 42),
            ("a == 0 || 1 / a > 0", -1, 7),
            ("a < 5 && (a == 2 || a == 3)", 2, 42),
            ("a < 5 && (a == 2 || a == 3)", 3, 42),
            ("a < 5 && (a == 2 || a == 3)", 7, 7),
            ("a == 0 || a > 4 && a < 8", 6, 42),
        ];
        for (condition, input, expected) in cases {
            let program = format!(
                "fn choose(a: i32) -> i32 {{ if {condition} {{ 42 }} else {{ 7 }} }} fn main() {{ std::process::exit(choose({input})); }}\n"
            );
            std::fs::write(&source, program).unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "rustc failed: {}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code());
            assert_eq!(quill_status.code(), Some(expected));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scalar_match_expressions_match_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-match-differential-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        let cases = [
            (
                "fn choose(value: i32) -> i32 { match value { 0 => 42, 1 => 10, _ => 7 } } fn main() { std::process::exit(choose(0)); }",
                42,
            ),
            (
                "fn choose(value: i32) -> i32 { match value { 0 => 42, 1 => 10, _ => 7 } } fn main() { std::process::exit(choose(1)); }",
                10,
            ),
            (
                "fn choose(value: i32) -> i32 { match value { 0 => 42, 1 => 10, _ => 7 } } fn main() { std::process::exit(choose(9)); }",
                7,
            ),
            (
                "fn choose(value: i32) -> i32 { match value { 0 | 1 => 42, 2 | 3 => 10, _ => 7 } } fn main() { std::process::exit(choose(3)); }",
                10,
            ),
            (
                "fn choose(value: i32) -> i32 { match value { 3..=5 => 42, _ => 7 } } fn main() { std::process::exit(choose(5)); }",
                42,
            ),
            (
                "fn choose(value: i32) -> i32 { match value { 3..5 => 42, _ => 7 } } fn main() { std::process::exit(choose(5)); }",
                7,
            ),
            (
                "fn choose(value: i32) -> i32 { match value { -2..=1 => 42, _ => 7 } } fn main() { std::process::exit(choose(-1)); }",
                42,
            ),
            (
                "fn main() { let value = 5u64; std::process::exit(match value { 3u64..=5u64 => 42, _ => 7 }); }",
                42,
            ),
            (
                "fn main() { let value = 'c'; std::process::exit(match value { 'b'..='d' => 42, _ => 7 }); }",
                42,
            ),
            (
                "fn main() { let value = 41; std::process::exit(match value { captured => captured + 1 }); }",
                42,
            ),
            (
                "fn main() { let value = 10; std::process::exit(match value { value => value + 32 }); }",
                42,
            ),
            (
                "enum State { Ready } fn main() { let state = State::Ready; std::process::exit(match state { current => if let State::Ready = current { 42 } else { 7 } }); }",
                42,
            ),
            (
                "fn main() { let value = false; std::process::exit(match value { true => 42, false => 7 }); }",
                7,
            ),
        ];
        for (program, expected) in cases {
            std::fs::write(&source, program).unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "rustc failed: {}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code());
            assert_eq!(quill_status.code(), Some(expected));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn if_let_patterns_match_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-if-let-differential-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        let cases = [
            (
                "fn main() { let value = 2; std::process::exit(if let 1..=3 = value { 42 } else { 7 }); }",
                42,
            ),
            (
                "fn main() { let value = 4; std::process::exit(if let 1 | 4 = value { 42 } else { 7 }); }",
                42,
            ),
            (
                "enum State { Ready, Done } fn main() { let state = State::Done; std::process::exit(if let State::Ready | State::Done = state { 42 } else { 7 }); }",
                42,
            ),
        ];
        for (program, expected) in cases {
            std::fs::write(&source, program).unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "rustc failed: {}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code());
            assert_eq!(quill_status.code(), Some(expected));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fieldless_enum_matches_match_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-enum-differential-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        let program = "mod model { #[repr(i32)] #[derive(Clone, Copy, PartialEq, Eq)] pub enum State { Start = -2, Ready = 4, End } } fn choose(value: model::State) -> i32 { match value { model::State::Start | model::State::End => 42, model::State::Ready => 10 } } fn main() { let state: model::State = model::State::End; std::process::exit(if state == model::State::Ready { choose(state) } else { choose(state) }); }";
        std::fs::write(&source, program).unwrap();
        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .args(["--edition=2024", "-C", "overflow-checks=off"])
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .expect("rustc must be available for differential compiler tests");
        assert!(
            rustc.status.success(),
            "rustc failed: {}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let quill_status = std::process::Command::new(&quill_output).status().unwrap();
        let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
        assert_eq!(quill_status.code(), rustc_status.code());
        assert_eq!(quill_status.code(), Some(42));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn while_loops_with_mutable_locals_match_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-while-differential-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        for limit in [0, 1, 10, 20] {
            std::fs::write(
                &source,
                format!(
                    "fn identity(value: i32) -> i32 {{ value }} fn sum(limit: i32) -> i32 {{ let mut i = 0; let mut total = 0; while identity(i) < limit && i < 10 {{ total = total + i; i = i + 1; }} total }} fn main() {{ std::process::exit(sum({limit})); }}\n"
                ),
            )
            .unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "rustc failed: {}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code());
        }
        std::fs::write(
            &source,
            "fn bad() -> i32 { let value = 0; value = 1; value } fn main() { std::process::exit(bad()); }\n",
        )
        .unwrap();
        let error = compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap_err();
        assert!(error.contains("immutable variable"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn integer_for_ranges_match_rustc_including_inclusive_maximum() {
        let root = std::env::temp_dir().join(format!(
            "quillc-for-range-differential-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        let cases = [
            ("for i in identity(start)..identity(end)", 0, 0, 0),
            ("for i in identity(start)..identity(end)", 0, 10, 45),
            ("for i in identity(start)..identity(end)", 3, 5, 7),
            ("for i in identity(start)..=identity(end)", 3, 5, 12),
            ("for i in identity(start)..=identity(end)", 5, 3, 0),
            (
                "for i in identity(start)..=identity(end)",
                2_147_483_647,
                2_147_483_647,
                255,
            ),
        ];
        for (range, start, end, expected) in cases {
            std::fs::write(
                &source,
                format!(
                    "fn identity(value: i32) -> i32 {{ value }} fn sum(start: i32, end: i32) -> i32 {{ let mut total = 0; {range} {{ total = total + i; }} total }} fn main() {{ std::process::exit(sum({start}, {end})); }}\n"
                ),
            )
            .unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "rustc failed: {}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code());
            assert_eq!(quill_status.code(), Some(expected));
        }
        std::fs::write(
            &source,
            "fn bad() -> i32 { let mut sum = 0; for i in 0..3 { i = 1; sum = sum + i; } sum } fn main() { std::process::exit(bad()); }\n",
        )
        .unwrap();
        let error = compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap_err();
        assert!(error.contains("immutable variable `i`"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recursive_helpers_match_rustc_for_factorial_and_fibonacci() {
        let root = std::env::temp_dir().join(format!(
            "quillc-recursion-differential-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        let cases = [
            ("fact(0)", 1),
            ("fact(1)", 1),
            ("fact(5)", 120),
            ("fact(8)", 40_320),
            ("fib(0)", 0),
            ("fib(1)", 1),
            ("fib(10)", 55),
            ("fact(8) + fib(10)", 40_375),
        ];
        for (expression, expected) in cases {
            std::fs::write(
                &source,
                format!(
                    "fn fact(n: i32) -> i32 {{ if n <= 1 {{ 1 }} else {{ n * fact(n - 1) }} }} fn fib(n: i32) -> i32 {{ if n <= 1 {{ n }} else {{ fib(n - 1) + fib(n - 2) }} }} fn main() {{ std::process::exit({expression}); }}\n"
                ),
            )
            .unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "rustc failed: {}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code(), "{expression}");
            assert_eq!(quill_status.code(), Some(expected % 256), "{expression}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_early_returns_match_rustc() {
        let root = std::env::temp_dir().join(format!("quillc-return-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        let cases = [
            (
                "fn run(n: i32) -> i32 { if n == 0 { return 9; } n + 1 }",
                0,
                9,
            ),
            (
                "fn run(n: i32) -> i32 { if n < 0 { return 3; } return n * 2; }",
                -4,
                3,
            ),
            (
                "fn run(n: i32) -> i32 { if n < 0 { return 3; } return n * 2; }",
                7,
                14,
            ),
        ];
        for (body, argument, expected) in cases {
            std::fs::write(
                &source,
                format!("{body} fn main() {{ std::process::exit(run({argument})); }}\n"),
            )
            .unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "rustc failed: {}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code(), "{body}");
            assert_eq!(quill_status.code(), Some(expected), "{body}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn try_propagation_matches_rustc_for_niche_result_calls() {
        let root = std::env::temp_dir().join(format!("quillc-try-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        for (inner, propagation, expected) in [
            ("Err(\"bad\")", "inner()?;", 0),
            ("Ok(())", "inner()?;", 7),
            ("Err(\"bad\")", "let value = inner()?;", 0),
            ("Ok(())", "let value = inner()?;", 7),
        ] {
            std::fs::write(
                &source,
                format!(
                    "fn inner() -> Result<(), &'static str> {{ {inner} }} fn outer() -> Result<(), &'static str> {{ {propagation} Ok(()) }} fn main() {{ std::process::exit(if outer().is_ok() {{ {expected} }} else {{ 0 }}); }}\n"
                ),
            )
            .unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "{}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code());
            assert_eq!(quill_status.code(), Some(expected));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn option_try_propagation_matches_rustc_for_unit_values() {
        let root = std::env::temp_dir().join(format!("quillc-option-try-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        for (inner, expected) in [("None", 0), ("Some(())", 7)] {
            std::fs::write(
                &source,
                format!(
                    "fn inner() -> Option<()> {{ {inner} }} fn outer() -> Option<()> {{ let unit = inner()?; Some(()) }} fn main() {{ std::process::exit(if outer().is_some() {{ 7 }} else {{ 0 }}); }}\n"
                ),
            )
            .unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "{}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code());
            assert_eq!(quill_status.code(), Some(expected));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn option_i32_try_propagation_matches_rustc_for_tagged_returns() {
        let root = std::env::temp_dir().join(format!("quillc-option-i32-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        for (inner, expected) in [("None", 0), ("Some(42)", 7)] {
            std::fs::write(
                &source,
                format!(
                    "fn inner() -> Option<i32> {{ {inner} }} fn outer() -> Option<i32> {{ inner()?; Some(7) }} fn main() {{ std::process::exit(if outer().is_some() {{ 7 }} else {{ 0 }}); }}\n"
                ),
            )
            .unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "{}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code());
            assert_eq!(quill_status.code(), Some(expected));
        }
        for (inner, expected) in [("None", 9), ("Some(42)", 42)] {
            std::fs::write(
                &source,
                format!(
                    "fn inner() -> Option<i32> {{ {inner} }} fn main() {{ std::process::exit(inner().unwrap_or(9)); }}\n"
                ),
            )
            .unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "{}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code());
            assert_eq!(quill_status.code(), Some(expected));
        }
        for (inner, expected) in [("None", 0), ("Some(42)", 43)] {
            std::fs::write(
                &source,
                format!(
                    "fn inner() -> Option<i32> {{ {inner} }} fn outer() -> Option<i32> {{ let value = inner()?; Some(value + 1) }} fn main() {{ std::process::exit(outer().unwrap_or(0)); }}\n"
                ),
            )
            .unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "{}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code());
            assert_eq!(quill_status.code(), Some(expected));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn result_i32_try_propagation_matches_rustc_for_tagged_returns() {
        let root = std::env::temp_dir().join(format!("quillc-result-i32-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        for (inner, outer_body, expected) in [
            ("Err(5)", "inner()?; Ok(7)", 9),
            ("Ok(42)", "inner()?; Ok(7)", 7),
            ("Err(5)", "let value = inner()?; Ok(value + 1)", 9),
            ("Ok(42)", "let value = inner()?; Ok(value + 1)", 43),
        ] {
            std::fs::write(&source, format!(
                "fn inner() -> Result<i32, i32> {{ {inner} }} fn outer() -> Result<i32, i32> {{ {outer_body} }} fn main() {{ std::process::exit(outer().unwrap_or(9)); }}\n"
            )).unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .unwrap();
            assert!(
                rustc.status.success(),
                "{}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code());
            assert_eq!(quill_status.code(), Some(expected));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn result_is_err_matches_rustc_for_niche_and_tagged_layouts() {
        let root =
            std::env::temp_dir().join(format!("quillc-result-is-err-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        for (result_type, value, expected) in [
            ("Result<(), &'static str>", "Ok(())", 0),
            ("Result<(), &'static str>", "Err(\"bad\")", 1),
            ("Result<i32, i32>", "Ok(42)", 0),
            ("Result<i32, i32>", "Err(5)", 1),
        ] {
            std::fs::write(
                &source,
                format!(
                    "fn check() -> {result_type} {{ {value} }} fn main() {{ std::process::exit(if check().is_err() {{ 1 }} else {{ 0 }}); }}\n"
                ),
            )
            .unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .unwrap();
            assert!(
                rustc.status.success(),
                "{}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code());
            assert_eq!(quill_status.code(), Some(expected));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn result_word_payload_try_and_unwrap_match_rustc() {
        let root = std::env::temp_dir().join(format!("quillc-result-word-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        for (ok_type, err_type, ok_value, err_value, fallback) in [
            ("u64", "u64", "4294967296u64", "5u64", "9u64"),
            ("u64", "usize", "4294967296u64", "5usize", "9u64"),
            ("usize", "u64", "4294967296usize", "5u64", "9usize"),
            ("usize", "usize", "4294967296usize", "5usize", "9usize"),
        ] {
            for (inner, body, expected) in [
                (
                    format!("Ok({ok_value})"),
                    format!("inner()?; Ok(7{ok_type})"),
                    7u64,
                ),
                (
                    format!("Ok({ok_value})"),
                    "let value = inner()?; Ok(value)".to_owned(),
                    4_294_967_296u64,
                ),
                (
                    format!("Err({err_value})"),
                    format!("inner()?; Ok(7{ok_type})"),
                    9u64,
                ),
                (
                    format!("Err({err_value})"),
                    "let value = inner()?; Ok(value)".to_owned(),
                    9u64,
                ),
            ] {
                std::fs::write(&source, format!(
                    "fn inner() -> Result<{ok_type}, {err_type}> {{ {inner} }} fn outer() -> Result<{ok_type}, {err_type}> {{ {body} }} fn main() {{ let observed = outer().unwrap_or({fallback}); println!(\"{{observed}}\"); }}\n"
                )).unwrap();
                compile_subset_main(
                    &source.display().to_string(),
                    &quill_output.display().to_string(),
                )
                .unwrap_or_else(|error| panic!("{inner} / {body}: {error}"));
                let rustc = std::process::Command::new("rustc")
                    .args(["--edition=2024", "-C", "overflow-checks=off"])
                    .arg(&source)
                    .arg("-o")
                    .arg(&rustc_output)
                    .output()
                    .unwrap();
                assert!(
                    rustc.status.success(),
                    "{}",
                    String::from_utf8_lossy(&rustc.stderr)
                );
                let quill = std::process::Command::new(&quill_output).output().unwrap();
                let rustc = std::process::Command::new(&rustc_output).output().unwrap();
                assert_eq!(quill.status.code(), rustc.status.code(), "{inner} / {body}");
                assert_eq!(quill.stdout, rustc.stdout, "{inner} / {body}");
                assert_eq!(
                    quill.stdout,
                    format!("{expected}\n").as_bytes(),
                    "{inner} / {body}"
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn break_continue_and_nested_loop_targets_match_rustc() {
        let root = std::env::temp_dir().join(format!("quillc-loop-control-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        let cases = [
            (
                "fn run() -> i32 { let mut i = 0; let mut sum = 0; loop { i = i + 1; if i == 3 { continue; } sum = sum + i; if i == 5 { break; } } sum }",
                12,
            ),
            (
                "fn run() -> i32 { let mut outer = 0; let mut total = 0; loop { outer = outer + 1; let mut inner = 0; loop { inner = inner + 1; break; } total = total + inner; if outer == 3 { break; } } total }",
                3,
            ),
            (
                "fn run() -> i32 { let mut i = 0; let mut total = 0; while i < 10 { i = i + 1; if i == 3 { continue; } total = total + i; if i == 6 { break; } } total }",
                18,
            ),
            (
                "fn run() -> i32 { let mut i = 0; while i < 8 { i = i + 1; continue; } i }",
                8,
            ),
            (
                "fn run() -> i32 { let mut i = 0; while i < 8 { i = i + 1; break; } i }",
                1,
            ),
            (
                "fn run() -> i32 { let mut total = 0; for i in 0..8 { total = total + i; continue; total = total + 100; } total }",
                28,
            ),
            (
                "fn run() -> i32 { let mut total = 0; for i in 0..8 { total = total + i; break; } total }",
                0,
            ),
            (
                "fn run() -> i32 { let mut i = 0; let mut total = 0; while i < 3 { let mut j = 0; while j < 5 { j = j + 1; break; } i = i + 1; total = total + j; } total }",
                3,
            ),
        ];
        for (body, expected) in cases {
            std::fs::write(
                &source,
                format!("{body} fn main() {{ std::process::exit(run()); }}\n"),
            )
            .unwrap();
            compile_subset_main(
                &source.display().to_string(),
                &quill_output.display().to_string(),
            )
            .unwrap();
            let rustc = std::process::Command::new("rustc")
                .args(["--edition=2024", "-C", "overflow-checks=off"])
                .arg(&source)
                .arg("-o")
                .arg(&rustc_output)
                .output()
                .expect("rustc must be available for differential compiler tests");
            assert!(
                rustc.status.success(),
                "rustc failed: {}",
                String::from_utf8_lossy(&rustc.stderr)
            );
            let quill_status = std::process::Command::new(&quill_output).status().unwrap();
            let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
            assert_eq!(quill_status.code(), rustc_status.code(), "{body}");
            assert_eq!(quill_status.code(), Some(expected), "{body}");
        }
        std::fs::write(&source, "fn main() { break; }\n").unwrap();
        let error = compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap_err();
        assert!(error.contains("outside of a loop"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn links_reachable_helper_calls_and_prunes_private_dead_functions() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-call-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "fn main() { std::process::exit(answer()); } fn answer() -> i32 { value() } fn value() -> i32 { 41 } fn unused() -> i32 { 99 }\n",
        )
        .unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        let result = std::process::Command::new(&output).status().unwrap();
        assert_eq!(result.code(), Some(41));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expands_literal_concat_macro_like_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-concat-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        std::fs::write(
            &source,
            "fn answer() -> i32 { if concat!(\"qu\", 'i', 1, true).len() == 8 { 42 } else { 7 } } fn main() { std::process::exit(answer()); }\n",
        )
        .unwrap();

        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .arg("+nightly-2026-08-23")
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .expect("pinned rustc must be available for differential compiler tests");
        assert!(
            rustc.status.success(),
            "rustc rejected the concat fixture: {}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let quill_status = std::process::Command::new(&quill_output).status().unwrap();
        let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
        assert_eq!(quill_status.code(), rustc_status.code());
        assert_eq!(quill_status.code(), Some(42));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn indexes_fixed_array_values_like_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-array-index-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        std::fs::write(
            &source,
            "fn answer() -> i32 { let values: [u64; 3] = [7, 42, 13]; if values[1] == 42 { 42 } else { 7 } } fn main() { std::process::exit(answer()); }\n",
        )
        .unwrap();

        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .arg("+nightly-2026-08-23")
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .expect("pinned rustc must be available for differential compiler tests");
        assert!(
            rustc.status.success(),
            "rustc rejected the array fixture: {}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let quill_status = std::process::Command::new(&quill_output).status().unwrap();
        let rustc_status = std::process::Command::new(&rustc_output).status().unwrap();
        assert_eq!(quill_status.code(), rustc_status.code());
        assert_eq!(quill_status.code(), Some(42));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn constructs_an_owned_string_from_a_literal_like_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-string-from-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let quill_output = root.join("quill-program");
        let rustc_output = root.join("rustc-program");
        std::fs::write(
            &source,
            "fn main() { let value = String::from(\"quill\"); println!(\"{value}\"); }\n",
        )
        .unwrap();

        compile_subset_main(
            &source.display().to_string(),
            &quill_output.display().to_string(),
        )
        .unwrap();
        let rustc = std::process::Command::new("rustc")
            .arg("+nightly-2026-08-23")
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .expect("pinned rustc must be available for differential compiler tests");
        assert!(
            rustc.status.success(),
            "rustc rejected the String::from fixture: {}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let quill_output = std::process::Command::new(&quill_output).output().unwrap();
        let rustc_output = std::process::Command::new(&rustc_output).output().unwrap();
        assert_eq!(quill_output.status.code(), rustc_output.status.code());
        assert_eq!(quill_output.stdout, rustc_output.stdout);
        assert_eq!(quill_output.stderr, rustc_output.stderr);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn type_checks_dead_functions_before_pruning_their_codegen() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-dead-invalid-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        std::fs::write(&source, "fn main() {} fn unused() -> i32 { true }\n").unwrap();
        let result = compile_subset_main(&source.display().to_string(), "unused-output");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("function body"));

        std::fs::write(&source, "fn main() {} fn unused() -> i32 { missing() }\n").unwrap();
        let result = compile_subset_main(&source.display().to_string(), "unused-output");
        assert!(
            result
                .unwrap_err()
                .contains("unresolved function `missing`")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn passes_i32_arguments_through_nested_helper_calls() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-params-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "fn identity(value: i32) -> i32 { value } fn forward(value: i32) -> i32 { identity(value) } fn main() { std::process::exit(forward(identity(42))); }\n",
        )
        .unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        let result = std::process::Command::new(&output).status().unwrap();
        assert_eq!(result.code(), Some(42));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn monomorphizes_an_i32_identity_generic_and_executes_it() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-generic-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "fn identity<T>(value: T) -> T { value } fn main() { std::process::exit(identity(identity(42))); }\n",
        )
        .unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        let result = std::process::Command::new(&output).status().unwrap();
        assert_eq!(result.code(), Some(42));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn monomorphizes_generic_statement_bodies_and_unit_helpers_like_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-generic-locals-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "fn local_copy<T: Copy>(value: T) -> T { let local: T = value; local } fn observe<T: Copy>(value: T) -> () { let local: T = value; } fn main() { observe(1u64); local_copy(4294967296u64); std::process::exit(local_copy(42)); }\n",
        )
        .unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        let quill_result = std::process::Command::new(&output).status().unwrap();
        assert_eq!(quill_result.code(), Some(42));

        let rustc_output = root.join("rustc-program");
        let rustc = std::process::Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .expect("rustc must be available for differential compiler tests");
        assert!(
            rustc.status.success(),
            "{}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let rustc_result = std::process::Command::new(rustc_output).status().unwrap();
        assert_eq!(quill_result.code(), rustc_result.code());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn monomorphizes_add_output_generic_for_i32_and_u64_like_rustc() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-generic-add-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "fn plus<T: std::ops::Add<Output = T>>(left: T, right: T) -> T { let sum: T = left + right; sum } fn verify(value: u64) -> i32 { if value == 8589934592u64 { plus(20i32, 22i32) } else { 0 } } fn main() { std::process::exit(verify(plus(4294967296u64, 4294967296u64))); }\n",
        )
        .unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        let quill_result = std::process::Command::new(&output).status().unwrap();
        assert_eq!(quill_result.code(), Some(42));

        let rustc_output = root.join("rustc-program");
        let rustc = std::process::Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .expect("rustc must be available for differential compiler tests");
        assert!(
            rustc.status.success(),
            "{}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let rustc_result = std::process::Command::new(rustc_output).status().unwrap();
        assert_eq!(quill_result.code(), rustc_result.code());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_generic_arguments_select_a_runtime_u64_instance() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-generic-explicit-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "fn identity<T: Copy>(value: T) -> T { value } fn check(value: u64) -> i32 { if value == 4294967296u64 { 42 } else { 0 } } fn main() { std::process::exit(check(identity::<u64>(4294967296))); }\n",
        )
        .unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        let quill_result = std::process::Command::new(&output).status().unwrap();
        assert_eq!(quill_result.code(), Some(42));

        let rustc_output = root.join("rustc-program");
        let rustc = std::process::Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .expect("rustc must be available for differential compiler tests");
        assert!(
            rustc.status.success(),
            "{}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let rustc_result = std::process::Command::new(rustc_output).status().unwrap();
        assert_eq!(quill_result.code(), rustc_result.code());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reuses_one_generic_definition_for_distinct_scalar_instances() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-generic-types-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "fn identity<T: Copy>(value: T) -> T { value } fn relay<T: Copy>(value: T) -> T { identity(value) } fn check(value: u64) -> i32 { if value == 4294967296u64 { 42 } else { 0 } } fn check_native(value: usize) -> i32 { if value == 42usize { 42 } else { 0 } } fn main() { let _small = relay(7i32); let _wide = check(relay(4294967296u64)); std::process::exit(check_native(relay(42usize))); }\n",
        )
        .unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();

        let quill_result = std::process::Command::new(&output).status().unwrap();
        assert_eq!(quill_result.code(), Some(42));

        let rustc_output = root.join("rustc-program");
        let rustc = std::process::Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .expect("rustc must be available for differential compiler tests");
        assert!(
            rustc.status.success(),
            "{}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let rustc_result = std::process::Command::new(rustc_output).status().unwrap();
        assert_eq!(quill_result.code(), rustc_result.code());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn monomorphizes_copy_generic_over_a_tuple_slice() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-generic-tuple-slice-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "fn count_pairs<T: Copy>(pairs: &[(T, T)]) -> usize { let mut count = 0usize; for (_left, _right) in pairs { count = count + 1usize; } count } fn answer() -> i32 { if count_pairs(&[(10u64, 11u64), (10u64, 11u64)]) == 2usize { 42 } else { 0 } } fn main() { std::process::exit(answer()); }\n",
        )
        .unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        let quill_result = std::process::Command::new(&output).status().unwrap();
        assert_eq!(quill_result.code(), Some(42));

        let rustc_output = root.join("rustc-program");
        let rustc = std::process::Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .expect("rustc must be available for differential compiler tests");
        assert!(
            rustc.status.success(),
            "{}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let rustc_result = std::process::Command::new(rustc_output).status().unwrap();
        assert_eq!(quill_result.code(), rustc_result.code());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn monomorphizes_as_ref_string_generic_over_a_tuple_slice() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-generic-string-tuple-slice-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "fn total_lengths<T: AsRef<str>>(fields: &[(T, T)]) -> usize { let mut total = 0usize; for (key, value) in fields { total = total + key.as_ref().len() + value.as_ref().len(); } total } fn answer() -> i32 { if total_lengths(&[(\"ab\", \"c\"), (\"d\", \"ef\")]) == 6usize { 42 } else { 0 } } fn main() { std::process::exit(answer()); }\n",
        )
        .unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        let quill_result = std::process::Command::new(&output).status().unwrap();
        assert_eq!(quill_result.code(), Some(42));

        let rustc_output = root.join("rustc-program");
        let rustc = std::process::Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(&rustc_output)
            .output()
            .expect("rustc must be available for differential compiler tests");
        assert!(
            rustc.status.success(),
            "{}",
            String::from_utf8_lossy(&rustc.stderr)
        );
        let rustc_result = std::process::Command::new(rustc_output).status().unwrap();
        assert_eq!(quill_result.code(), rustc_result.code());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn monomorphizes_two_type_parameters_and_executes_the_second_argument() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-generic-pair-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "fn second<T, U>(first: T, value: U) -> U { value } fn main() { std::process::exit(second(17, 42)); }\n",
        )
        .unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        let result = std::process::Command::new(&output).status().unwrap();
        assert_eq!(result.code(), Some(42));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn passes_nested_dynamic_results_to_two_argument_generic() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-generic-dynamic-pair-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "fn identity(value: i32) -> i32 { value } fn second<T, U>(first: T, value: U) -> U { value } fn relay(input: i32) -> i32 { second(input, identity(42)) } fn main() { std::process::exit(relay(7)); }\n",
        )
        .unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        let result = std::process::Command::new(&output).status().unwrap();
        assert_eq!(result.code(), Some(42));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn passes_eight_nested_dynamic_arguments_using_sysv_stack_and_registers() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-six-args-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        std::fs::write(
            &source,
            "fn identity(value: i32) -> i32 { value } fn eighth(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32, g: i32, h: i32) -> i32 { h } fn relay(value: i32) -> i32 { eighth(identity(1), identity(2), identity(3), identity(4), identity(5), identity(6), identity(7), identity(value)) } fn main() { std::process::exit(relay(42)); }\n",
        )
        .unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        let result = std::process::Command::new(&output).status().unwrap();
        assert_eq!(result.code(), Some(42));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn handles_large_argument_frames_and_disp32_offsets() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-many-args-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        let parameters = (0..64)
            .map(|index| format!("arg{index}: i32"))
            .collect::<Vec<_>>()
            .join(", ");
        let arguments = (0..63)
            .map(|index| format!("identity({})", index + 1))
            .chain(std::iter::once("identity(value)".to_owned()))
            .collect::<Vec<_>>()
            .join(", ");
        let program = format!(
            "fn identity(value: i32) -> i32 {{ value }} fn last({parameters}) -> i32 {{ arg63 }} fn relay(value: i32) -> i32 {{ last({arguments}) }} fn main() {{ std::process::exit(relay(42)); }}\n"
        );
        std::fs::write(&source, program).unwrap();
        compile_subset_main(&source.display().to_string(), &output.display().to_string()).unwrap();
        let result = std::process::Command::new(&output).status().unwrap();
        assert_eq!(result.code(), Some(42));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lowers_nested_if_else_and_signed_comparisons() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-if-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        for (value, expected) in [("42", 42), ("7", 7), ("0 - 1", 7)] {
            std::fs::write(
                &source,
                format!(
                    "fn identity(value: i32) -> i32 {{ value }} fn choose(n: i32) -> i32 {{ if identity(n) >= identity(40) {{ if identity(n) > identity(50) {{ 99 }} else {{ identity(42) }} }} else {{ identity(7) }} }} fn main() {{ std::process::exit(choose({value})); }}\n"
                ),
            )
            .unwrap();
            compile_subset_main(&source.display().to_string(), &output.display().to_string())
                .unwrap();
            let result = std::process::Command::new(&output).status().unwrap();
            assert_eq!(result.code(), Some(expected), "input {value}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lowers_each_signed_comparison_condition() {
        let root = std::env::temp_dir().join(format!(
            "quillc-build-comparisons-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("main.rs");
        let output = root.join("program");
        let cases = [
            ("n == 42", 42, 42),
            ("n == 42", 7, 7),
            ("n != 42", 7, 42),
            ("n != 42", 42, 7),
            ("n < 42", 7, 42),
            ("n < 42", 42, 7),
            ("n <= 42", 42, 42),
            ("n <= 42", 50, 7),
            ("n > 42", 50, 42),
            ("n > 42", 42, 7),
            ("n >= 42", 42, 42),
            ("n >= 42", 7, 7),
        ];
        for (condition, input, expected) in cases {
            std::fs::write(
                &source,
                format!(
                    "fn choose(n: i32) -> i32 {{ if {condition} {{ 42 }} else {{ 7 }} }} fn main() {{ std::process::exit(choose({input})); }}\n"
                ),
            )
            .unwrap();
            compile_subset_main(&source.display().to_string(), &output.display().to_string())
                .unwrap();
            let result = std::process::Command::new(&output).status().unwrap();
            assert_eq!(result.code(), Some(expected), "{condition} with {input}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
