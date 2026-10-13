use std::borrow::Cow;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use bincode::Options;
use quill_source::SourceFile;
use quill_syntax::{Diagnostic, FunctionItem, ImportItem, ModuleItem, Parse};
use quill_typeck::{ExternalFunctionSignature, StructSignature, TypeDiagnostic};
use serde::{Deserialize, Serialize};

// Increment this when parse-cache contents or parser interpretation changes.
const CACHE_SCHEMA: u32 = 28;
// Increment when checker semantics or the cached diagnostic format changes.
const TYPE_CHECK_CACHE_SCHEMA: u32 = 47;
const MAX_CACHE_BYTES: u64 = 128 * 1024 * 1024;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Persistent parsed-file results keyed by source path and validated by content.
/// Cache failures fall back to parsing and never change compiler acceptance.
pub struct ParseCache {
    root: PathBuf,
    hits: AtomicUsize,
    misses: AtomicUsize,
    invalid_entries: AtomicUsize,
    write_failures: AtomicUsize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParseCacheStats {
    pub hits: usize,
    pub misses: usize,
    pub invalid_entries: usize,
    pub write_failures: usize,
}

/// Persistent diagnostics keyed by source content and checker mode.
pub struct TypeCheckCache {
    root: PathBuf,
    hits: AtomicUsize,
    misses: AtomicUsize,
    invalid_entries: AtomicUsize,
    write_failures: AtomicUsize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TypeCheckCacheStats {
    pub hits: usize,
    pub misses: usize,
    pub invalid_entries: usize,
    pub write_failures: usize,
}

#[derive(Serialize, Deserialize)]
struct CacheRecord {
    schema: u32,
    source_digest: [u8; 32],
    cfg_digest: [u8; 32],
    parsed: CachedParse,
}

#[derive(Serialize, Deserialize)]
struct CachedParse {
    functions: Vec<FunctionItem>,
    structs: Vec<quill_syntax::StructItem>,
    enums: Vec<quill_syntax::EnumItem>,
    modules: Vec<ModuleItem>,
    imports: Vec<ImportItem>,
    unsupported_item_count: usize,
    diagnostics: Vec<Diagnostic>,
    had_error: bool,
}

#[derive(Serialize, Deserialize)]
struct TypeCheckRecord {
    schema: u32,
    source_digest: [u8; 32],
    interface_digest: [u8; 32],
    strict_calls: bool,
    diagnostics: Vec<TypeDiagnostic>,
}

#[derive(Clone, Serialize, Deserialize)]
struct BodyTypeCheckEntry {
    body_digest: [u8; 32],
    diagnostics: Vec<TypeDiagnostic>,
}

#[derive(Serialize, Deserialize)]
struct BodyTypeCheckFile {
    schema: u32,
    interface_digest: [u8; 32],
    strict_calls: bool,
    entries: std::collections::BTreeMap<String, BodyTypeCheckEntry>,
}

fn interface_digest(signatures: &[ExternalFunctionSignature]) -> [u8; 32] {
    let signatures = if signatures.is_sorted() {
        Cow::Borrowed(signatures)
    } else {
        let mut sorted = signatures.to_vec();
        sorted.sort();
        Cow::Owned(sorted)
    };
    let mut hasher = blake3::Hasher::new();
    for signature in signatures.iter() {
        hash_field(&mut hasher, signature.path.as_bytes());
        for parameter in &signature.parameter_types {
            hash_field(&mut hasher, parameter.as_bytes());
        }
        hash_field(&mut hasher, &[0xff]);
        match &signature.return_type {
            Some(return_type) => hash_field(&mut hasher, return_type.as_bytes()),
            None => hash_field(&mut hasher, &[0]),
        }
        for generic in &signature.generic_parameters {
            hash_field(&mut hasher, generic.as_bytes());
        }
        for generic in &signature.generic_copy_parameters {
            hash_field(&mut hasher, generic.as_bytes());
        }
        for generic in &signature.generic_as_ref_str_parameters {
            hash_field(&mut hasher, generic.as_bytes());
        }
        for generic in &signature.generic_add_parameters {
            hash_field(&mut hasher, generic.as_bytes());
        }
        hash_field(&mut hasher, &[u8::from(signature.generic_has_bounds)]);
    }
    *hasher.finalize().as_bytes()
}

fn interface_digest_with_structs(
    signatures: &[ExternalFunctionSignature],
    structs: &[StructSignature],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&interface_digest(signatures));
    let structs = if structs.is_sorted() {
        Cow::Borrowed(structs)
    } else {
        let mut sorted = structs.to_vec();
        sorted.sort();
        Cow::Owned(sorted)
    };
    for structure in structs.iter() {
        hash_field(&mut hasher, structure.path.as_bytes());
        hash_field(&mut hasher, &[u8::from(structure.is_enum)]);
        for parameter in &structure.generic_parameters {
            hash_field(&mut hasher, parameter.as_bytes());
        }
        for derived_trait in &structure.derived_traits {
            hash_field(&mut hasher, derived_trait.as_bytes());
        }
        hash_field(&mut hasher, &[0xfe]);
        for field in &structure.fields {
            hash_field(&mut hasher, field.name.as_bytes());
            hash_field(&mut hasher, field.type_text.as_bytes());
        }
        hash_field(&mut hasher, &[0xfd]);
        for variant in &structure.enum_variants {
            hash_field(&mut hasher, variant.name.as_bytes());
            hash_field(&mut hasher, &variant.discriminant.to_le_bytes());
        }
        hash_field(&mut hasher, &[0xfc]);
    }
    *hasher.finalize().as_bytes()
}

/// Compute the cache identity for a complete visible type-check interface.
/// Callers checking many source files can compute it once and share it.
pub fn type_check_interface_digest(
    signatures: &[ExternalFunctionSignature],
    structs: &[StructSignature],
) -> [u8; 32] {
    interface_digest_with_structs(signatures, structs)
}

fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hex_digest(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl ParseCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            hits: AtomicUsize::new(0),
            misses: AtomicUsize::new(0),
            invalid_entries: AtomicUsize::new(0),
            write_failures: AtomicUsize::new(0),
        }
    }

    /// Choose a target-local cache unless the caller supplies QUILL_CACHE_DIR.
    pub fn for_source(source: &SourceFile) -> Self {
        let root = std::env::var_os("QUILL_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_cache_root(source.path()));
        Self::new(root)
    }

    pub fn parse_files(
        &self,
        pool: &quill_parallel::WorkPool,
        sources: &[SourceFile],
    ) -> Vec<Parse> {
        pool.map(sources, |source| self.parse_file(source))
    }

    pub fn parse_file(&self, source: &SourceFile) -> Parse {
        self.parse_file_with_cfg(source, &quill_cfg::CfgSet::default())
    }

    pub fn parse_file_with_cfg(&self, source: &SourceFile, cfg: &quill_cfg::CfgSet) -> Parse {
        self.parse_file_with_cfg_and_edition(source, cfg, "2024")
    }

    pub fn parse_file_with_cfg_and_edition(
        &self,
        source: &SourceFile,
        cfg: &quill_cfg::CfgSet,
        edition: &str,
    ) -> Parse {
        let source_digest = *blake3::hash(source.text().as_bytes()).as_bytes();
        let mut hasher = blake3::Hasher::new();
        hasher.update(&cfg.fingerprint());
        hasher.update(edition.as_bytes());
        let cfg_digest = *hasher.finalize().as_bytes();
        let cache_path = self.cache_path(source.path(), &cfg_digest);
        if let Some(parsed) = self.read_cache(&cache_path, &source_digest, &cfg_digest) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return parsed;
        }

        self.misses.fetch_add(1, Ordering::Relaxed);
        let parsed = quill_syntax::parse_ast_with_cfg_and_edition(source, cfg, edition);
        if self
            .write_cache(&cache_path, source_digest, cfg_digest, &parsed)
            .is_err()
        {
            self.write_failures.fetch_add(1, Ordering::Relaxed);
        }
        parsed
    }

    /// Parse a file using a compact cache entry that omits non-generic
    /// function bodies. Generic bodies remain available for monomorphization.
    /// A missing compact entry is populated from the ordinary parse cache.
    pub fn parse_file_summary_with_cfg_and_edition(
        &self,
        source: &SourceFile,
        cfg: &quill_cfg::CfgSet,
        edition: &str,
    ) -> Parse {
        let source_digest = *blake3::hash(source.text().as_bytes()).as_bytes();
        let mut hasher = blake3::Hasher::new();
        hasher.update(&cfg.fingerprint());
        hasher.update(edition.as_bytes());
        let cfg_digest = *hasher.finalize().as_bytes();
        let full_path = self.cache_path(source.path(), &cfg_digest);
        let summary_path = self.summary_cache_path(source.path(), &cfg_digest);
        if let Some(parsed) = self.read_cache(&summary_path, &source_digest, &cfg_digest) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return parsed;
        }

        let parsed = if let Some(parsed) = self.read_cache(&full_path, &source_digest, &cfg_digest)
        {
            self.hits.fetch_add(1, Ordering::Relaxed);
            parsed
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            let parsed = quill_syntax::parse_ast_with_cfg_and_edition(source, cfg, edition);
            if self
                .write_cache(&full_path, source_digest, cfg_digest, &parsed)
                .is_err()
            {
                self.write_failures.fetch_add(1, Ordering::Relaxed);
            }
            parsed
        };
        let summary = CachedParse::from_parse_summary(&parsed).into_parse();
        if self
            .write_cache(&summary_path, source_digest, cfg_digest, &summary)
            .is_err()
        {
            self.write_failures.fetch_add(1, Ordering::Relaxed);
        }
        summary
    }

    pub fn stats(&self) -> ParseCacheStats {
        ParseCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            invalid_entries: self.invalid_entries.load(Ordering::Relaxed),
            write_failures: self.write_failures.load(Ordering::Relaxed),
        }
    }

    fn cache_path(&self, source_path: &Path, cfg_digest: &[u8; 32]) -> PathBuf {
        let namespace = format!("parse-{}", hex_digest(cfg_digest));
        cache_path(&self.root, source_path, &namespace)
    }

    fn summary_cache_path(&self, source_path: &Path, cfg_digest: &[u8; 32]) -> PathBuf {
        let namespace = format!("parse-summary-{}", hex_digest(cfg_digest));
        cache_path(&self.root, source_path, &namespace)
    }

    fn read_cache(
        &self,
        path: &Path,
        source_digest: &[u8; 32],
        cfg_digest: &[u8; 32],
    ) -> Option<Parse> {
        let bytes = read_cache_bytes(path, &self.invalid_entries)?;
        let Ok(record) = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(MAX_CACHE_BYTES)
            .deserialize::<CacheRecord>(&bytes)
        else {
            self.invalid_entries.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        if record.schema != CACHE_SCHEMA
            || &record.source_digest != source_digest
            || &record.cfg_digest != cfg_digest
        {
            self.invalid_entries.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(record.parsed.into_parse())
    }

    fn write_cache(
        &self,
        path: &Path,
        source_digest: [u8; 32],
        cfg_digest: [u8; 32],
        parsed: &Parse,
    ) -> io::Result<()> {
        let record = CacheRecord {
            schema: CACHE_SCHEMA,
            source_digest,
            cfg_digest,
            parsed: CachedParse::from_parse(parsed),
        };
        let bytes = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(MAX_CACHE_BYTES)
            .serialize(&record)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        write_cache_bytes(path, &bytes)
    }
}

impl TypeCheckCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            hits: AtomicUsize::new(0),
            misses: AtomicUsize::new(0),
            invalid_entries: AtomicUsize::new(0),
            write_failures: AtomicUsize::new(0),
        }
    }

    pub fn for_source(source: &SourceFile) -> Self {
        Self::for_source_with_cfg(source, &quill_cfg::CfgSet::default())
    }

    pub fn for_source_with_cfg(source: &SourceFile, cfg: &quill_cfg::CfgSet) -> Self {
        let root = std::env::var_os("QUILL_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_cache_root(source.path()));
        Self::new(root.join(format!("cfg-{}", hex_digest(&cfg.fingerprint()))))
    }

    pub fn new_with_cfg(root: impl Into<PathBuf>, cfg: &quill_cfg::CfgSet) -> Self {
        Self::new(
            root.into()
                .join(format!("cfg-{}", hex_digest(&cfg.fingerprint()))),
        )
    }

    pub fn check_functions(
        &self,
        source: &SourceFile,
        functions: &[FunctionItem],
        strict_calls: bool,
    ) -> Vec<TypeDiagnostic> {
        self.check_functions_with_interfaces(source, functions, strict_calls, &[])
    }

    pub fn check_functions_with_interfaces(
        &self,
        source: &SourceFile,
        functions: &[FunctionItem],
        strict_calls: bool,
        external_signatures: &[ExternalFunctionSignature],
    ) -> Vec<TypeDiagnostic> {
        self.check_functions_with_interfaces_and_structs(
            source,
            functions,
            strict_calls,
            external_signatures,
            &[],
        )
    }

    pub fn check_functions_with_interfaces_and_structs(
        &self,
        source: &SourceFile,
        functions: &[FunctionItem],
        strict_calls: bool,
        external_signatures: &[ExternalFunctionSignature],
        structs: &[StructSignature],
    ) -> Vec<TypeDiagnostic> {
        let source_digest = *blake3::hash(source.text().as_bytes()).as_bytes();
        let interface_digest = interface_digest_with_structs(external_signatures, structs);
        let namespace = if strict_calls {
            "typeck-strict"
        } else {
            "typeck"
        };
        let path = cache_path(&self.root, source.path(), namespace);
        if let Some(diagnostics) =
            self.read_cache(&path, &source_digest, &interface_digest, strict_calls)
        {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return diagnostics;
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let diagnostics = match (
            strict_calls,
            external_signatures.is_empty(),
            structs.is_empty(),
        ) {
            (true, true, true) => quill_typeck::check_functions_strict(functions),
            (false, true, true) => quill_typeck::check_functions(functions),
            (true, false, true) => quill_typeck::check_functions_strict_with_external_signatures(
                functions,
                external_signatures,
            ),
            (false, false, true) => quill_typeck::check_functions_with_external_signatures(
                functions,
                external_signatures,
            ),
            (false, true, false) => quill_typeck::check_functions_with_structs(functions, structs),
            (false, false, false) => {
                quill_typeck::check_functions_with_external_signatures_and_structs(
                    functions,
                    external_signatures,
                    structs,
                )
            }
            (true, true, false) => {
                quill_typeck::check_functions_strict_with_structs(functions, structs)
            }
            (true, false, false) => {
                quill_typeck::check_functions_strict_with_external_signatures_and_structs(
                    functions,
                    external_signatures,
                    structs,
                )
            }
        };
        if self
            .write_cache(
                &path,
                source_digest,
                interface_digest,
                strict_calls,
                &diagnostics,
            )
            .is_err()
        {
            self.write_failures.fetch_add(1, Ordering::Relaxed);
        }
        diagnostics
    }

    /// Check/cache each function independently. An unrelated body edit reuses
    /// all other body results; changing any callable signature invalidates all
    /// bodies in this source unit because call resolution/type inference can
    /// depend on every declaration.
    pub fn check_functions_per_body(
        &self,
        source: &SourceFile,
        functions: &[FunctionItem],
        strict_calls: bool,
        external_signatures: &[ExternalFunctionSignature],
    ) -> Vec<TypeDiagnostic> {
        self.check_functions_per_body_with_structs(
            source,
            functions,
            strict_calls,
            external_signatures,
            &[],
        )
    }

    pub fn check_functions_per_body_with_structs(
        &self,
        source: &SourceFile,
        functions: &[FunctionItem],
        strict_calls: bool,
        external_signatures: &[ExternalFunctionSignature],
        structs: &[StructSignature],
    ) -> Vec<TypeDiagnostic> {
        self.check_functions_per_body_with_interface_digest(
            source,
            functions,
            strict_calls,
            external_signatures,
            structs,
            None,
        )
    }

    pub fn check_functions_per_body_with_known_interface(
        &self,
        source: &SourceFile,
        functions: &[FunctionItem],
        strict_calls: bool,
        external_signatures: &[ExternalFunctionSignature],
        structs: &[StructSignature],
        interface_digest: [u8; 32],
    ) -> Vec<TypeDiagnostic> {
        self.check_functions_per_body_with_interface_digest(
            source,
            functions,
            strict_calls,
            external_signatures,
            structs,
            Some(interface_digest),
        )
    }

    fn check_functions_per_body_with_interface_digest(
        &self,
        source: &SourceFile,
        functions: &[FunctionItem],
        strict_calls: bool,
        external_signatures: &[ExternalFunctionSignature],
        structs: &[StructSignature],
        known_interface_digest: Option<[u8; 32]>,
    ) -> Vec<TypeDiagnostic> {
        let aggregate_interface_digest = known_interface_digest
            .unwrap_or_else(|| interface_digest_with_structs(external_signatures, structs));
        let source_digest = *blake3::hash(source.text().as_bytes()).as_bytes();
        let aggregate_namespace = if strict_calls {
            "typeck-strict"
        } else {
            "typeck"
        };
        let aggregate_path = cache_path(&self.root, source.path(), aggregate_namespace);
        if let Some(cached) = self.read_cache(
            &aggregate_path,
            &source_digest,
            &aggregate_interface_digest,
            strict_calls,
        ) {
            let body_count = functions
                .iter()
                .filter(|function| function.body.is_some())
                .count();
            self.hits.fetch_add(body_count, Ordering::Relaxed);
            return cached;
        }
        let interface_digest = if let Some(digest) = known_interface_digest {
            digest
        } else {
            let mut interfaces = external_signatures.to_vec();
            interfaces.extend(functions.iter().map(|function| {
                ExternalFunctionSignature {
                    path: function.name.clone(),
                    parameter_types: function
                        .parameters
                        .iter()
                        .map(|p| p.type_text.clone())
                        .collect(),
                    return_type: function.return_type.clone(),
                    generic_parameters: function.generic_parameters.clone(),
                    generic_copy_parameters: function.generic_copy_parameters.clone(),
                    generic_as_ref_str_parameters: function.generic_as_ref_str_parameters.clone(),
                    generic_add_parameters: function.generic_add_parameters.clone(),
                    generic_has_bounds: function.generic_has_bounds,
                }
            }));
            interfaces.sort();
            interface_digest_with_structs(&interfaces, structs)
        };
        let namespace = if strict_calls {
            "typeck-bodies-strict"
        } else {
            "typeck-bodies"
        };
        let path = cache_path(&self.root, source.path(), namespace);
        let mut entries = self
            .read_body_cache(&path, &interface_digest, strict_calls)
            .unwrap_or_default();
        let mut had_miss = false;
        let mut diagnostics = Vec::new();
        let mut misses = Vec::new();
        for (function_index, function) in functions.iter().enumerate() {
            if function.body.is_none() {
                continue;
            }
            let snippet = source.slice(function.span.clone());
            let body_digest = *blake3::hash(snippet.as_bytes()).as_bytes();
            let identity = format!("{}::{}", function.scope.join("/"), function.name);
            if let Some(entry) = entries
                .get(&identity)
                .filter(|entry| entry.body_digest == body_digest)
            {
                self.hits.fetch_add(1, Ordering::Relaxed);
                let mut cached = entry.diagnostics.clone();
                for diagnostic in &mut cached {
                    diagnostic.span.start =
                        diagnostic.span.start.saturating_add(function.span.start);
                    diagnostic.span.end = diagnostic.span.end.saturating_add(function.span.start);
                }
                diagnostics.extend(cached);
                continue;
            }
            self.misses.fetch_add(1, Ordering::Relaxed);
            had_miss = true;
            misses.push((identity, function_index, body_digest));
        }
        if !misses.is_empty() {
            let selected: Vec<_> = misses
                .iter()
                .map(|(_, function_index, _)| &functions[*function_index])
                .collect();
            // Keep body batches sequential here; Cargo queries parallelize
            // across source files without creating nested worker pools.
            let checked = quill_typeck::check_function_refs_with_declarations_and_structs(
                &selected,
                functions,
                strict_calls,
                external_signatures,
                structs,
            );
            for (identity, function_index, body_digest) in misses {
                let function = &functions[function_index];
                let mut function_diagnostics: Vec<_> = checked
                    .iter()
                    .filter(|diagnostic| function.span.contains(&diagnostic.span.start))
                    .cloned()
                    .collect();
                for diagnostic in &mut function_diagnostics {
                    diagnostic.span.start =
                        diagnostic.span.start.saturating_sub(function.span.start);
                    diagnostic.span.end = diagnostic.span.end.saturating_sub(function.span.start);
                }
                entries.insert(
                    identity,
                    BodyTypeCheckEntry {
                        body_digest,
                        diagnostics: function_diagnostics.clone(),
                    },
                );
                for diagnostic in &mut function_diagnostics {
                    diagnostic.span.start =
                        diagnostic.span.start.saturating_add(function.span.start);
                    diagnostic.span.end = diagnostic.span.end.saturating_add(function.span.start);
                }
                diagnostics.extend(function_diagnostics);
            }
        }
        if had_miss
            && self
                .write_body_cache(&path, interface_digest, strict_calls, entries)
                .is_err()
        {
            self.write_failures.fetch_add(1, Ordering::Relaxed);
        }
        diagnostics.sort_by_key(|diagnostic| diagnostic.span.start);
        if self
            .write_cache(
                &aggregate_path,
                source_digest,
                aggregate_interface_digest,
                strict_calls,
                &diagnostics,
            )
            .is_err()
        {
            self.write_failures.fetch_add(1, Ordering::Relaxed);
        }
        diagnostics
    }

    fn read_body_cache(
        &self,
        path: &Path,
        interface_digest: &[u8; 32],
        strict_calls: bool,
    ) -> Option<std::collections::BTreeMap<String, BodyTypeCheckEntry>> {
        let bytes = read_cache_bytes(path, &self.invalid_entries)?;
        let record = match bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(MAX_CACHE_BYTES)
            .deserialize::<BodyTypeCheckFile>(&bytes)
        {
            Ok(record) => record,
            Err(_) => {
                self.invalid_entries.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        if record.schema != TYPE_CHECK_CACHE_SCHEMA
            || &record.interface_digest != interface_digest
            || record.strict_calls != strict_calls
        {
            self.invalid_entries.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(record.entries)
    }

    fn write_body_cache(
        &self,
        path: &Path,
        interface_digest: [u8; 32],
        strict_calls: bool,
        entries: std::collections::BTreeMap<String, BodyTypeCheckEntry>,
    ) -> io::Result<()> {
        let record = BodyTypeCheckFile {
            schema: TYPE_CHECK_CACHE_SCHEMA,
            interface_digest,
            strict_calls,
            entries,
        };
        let bytes = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(MAX_CACHE_BYTES)
            .serialize(&record)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        write_cache_bytes(path, &bytes)
    }

    pub fn stats(&self) -> TypeCheckCacheStats {
        TypeCheckCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            invalid_entries: self.invalid_entries.load(Ordering::Relaxed),
            write_failures: self.write_failures.load(Ordering::Relaxed),
        }
    }

    fn read_cache(
        &self,
        path: &Path,
        source_digest: &[u8; 32],
        interface_digest: &[u8; 32],
        strict_calls: bool,
    ) -> Option<Vec<TypeDiagnostic>> {
        let bytes = read_cache_bytes(path, &self.invalid_entries)?;
        let Ok(record) = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(MAX_CACHE_BYTES)
            .deserialize::<TypeCheckRecord>(&bytes)
        else {
            self.invalid_entries.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        if record.schema != TYPE_CHECK_CACHE_SCHEMA
            || &record.source_digest != source_digest
            || &record.interface_digest != interface_digest
            || record.strict_calls != strict_calls
        {
            self.invalid_entries.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(record.diagnostics)
    }

    fn write_cache(
        &self,
        path: &Path,
        source_digest: [u8; 32],
        interface_digest: [u8; 32],
        strict_calls: bool,
        diagnostics: &[TypeDiagnostic],
    ) -> io::Result<()> {
        let record = TypeCheckRecord {
            schema: TYPE_CHECK_CACHE_SCHEMA,
            source_digest,
            interface_digest,
            strict_calls,
            diagnostics: diagnostics.to_vec(),
        };
        let bytes = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(MAX_CACHE_BYTES)
            .serialize(&record)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        write_cache_bytes(path, &bytes)
    }
}

fn cache_path(root: &Path, source_path: &Path, namespace: &str) -> PathBuf {
    let identity = fs::canonicalize(source_path).unwrap_or_else(|_| source_path.to_path_buf());
    let key = blake3::hash(identity.to_string_lossy().as_bytes());
    let mut hexadecimal = String::with_capacity(64);
    for byte in key.as_bytes() {
        use std::fmt::Write as _;
        write!(&mut hexadecimal, "{byte:02x}").expect("writing to String cannot fail");
    }
    root.join(namespace)
        .join(&hexadecimal[..2])
        .join(format!("{}.bin", &hexadecimal[2..]))
}

fn read_cache_bytes(path: &Path, invalid_entries: &AtomicUsize) -> Option<Vec<u8>> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > MAX_CACHE_BYTES {
        invalid_entries.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(_) => {
            invalid_entries.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

fn write_cache_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cache path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temp_path = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(bytes)?;
        drop(file);
        fs::rename(&temp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

impl CachedParse {
    fn from_parse(parse: &Parse) -> Self {
        Self {
            functions: parse.functions.clone(),
            structs: parse.structs.clone(),
            enums: parse.enums.clone(),
            modules: parse.modules.clone(),
            imports: parse.imports.clone(),
            unsupported_item_count: parse.unsupported_item_count,
            diagnostics: parse.diagnostics.clone(),
            had_error: parse.had_error,
        }
    }

    fn from_parse_summary(parse: &Parse) -> Self {
        Self {
            functions: parse
                .functions
                .iter()
                .map(|function| FunctionItem {
                    name: function.name.clone(),
                    scope: function.scope.clone(),
                    calls: function.calls.clone(),
                    qualified_calls: function.qualified_calls.clone(),
                    name_span: function.name_span.clone(),
                    span: function.span.clone(),
                    body_span: function
                        .is_generic
                        .then(|| function.body_span.clone())
                        .flatten(),
                    parameters: function.parameters.clone(),
                    return_type: function.return_type.clone(),
                    body: function.is_generic.then(|| function.body.clone()).flatten(),
                    is_public: function.is_public,
                    is_generic: function.is_generic,
                    generic_parameters: function.generic_parameters.clone(),
                    generic_copy_parameters: function.generic_copy_parameters.clone(),
                    generic_as_ref_str_parameters: function.generic_as_ref_str_parameters.clone(),
                    generic_add_parameters: function.generic_add_parameters.clone(),
                    generic_trait_bounds: function.generic_trait_bounds.clone(),
                    generic_has_bounds: function.generic_has_bounds,
                    is_async: function.is_async,
                    is_const: function.is_const,
                    is_unsafe: function.is_unsafe,
                    has_abi: function.has_abi,
                })
                .collect(),
            structs: parse.structs.clone(),
            enums: parse.enums.clone(),
            modules: parse.modules.clone(),
            imports: parse.imports.clone(),
            unsupported_item_count: parse.unsupported_item_count,
            diagnostics: parse.diagnostics.clone(),
            had_error: parse.had_error,
        }
    }

    fn into_parse(self) -> Parse {
        Parse {
            functions: self.functions,
            structs: self.structs,
            enums: self.enums,
            modules: self.modules,
            imports: self.imports,
            unsupported_item_count: self.unsupported_item_count,
            diagnostics: self.diagnostics,
            had_error: self.had_error,
            ..Parse::default()
        }
    }
}

fn default_cache_root(source_path: &Path) -> PathBuf {
    let source_dir = source_path.parent().unwrap_or_else(|| Path::new("."));
    let workspace = source_dir
        .ancestors()
        .find(|ancestor| ancestor.join("Cargo.toml").is_file())
        .unwrap_or(source_dir);
    workspace.join("target").join("quill-query-cache")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use quill_parallel::WorkPool;
    use quill_source::SourceFile;
    use quill_typeck::{ExternalFunctionSignature, StructFieldSignature, StructSignature};

    use super::ParseCache;
    use super::TypeCheckCache;
    use super::cache_path;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "quill-query-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn equivalent_parse(left: &quill_syntax::Parse, right: &quill_syntax::Parse) -> bool {
        left.functions == right.functions
            && left.structs == right.structs
            && left.modules == right.modules
            && left.imports == right.imports
            && left.unsupported_item_count == right.unsupported_item_count
            && left.diagnostics == right.diagnostics
            && left.had_error == right.had_error
            && left.items.is_empty()
            && right.items.is_empty()
    }

    #[test]
    fn reuses_unchanged_parse_and_invalidates_changed_source() {
        let root = temp_root();
        let source_path = root.join("lib.rs");
        fs::create_dir_all(&root).unwrap();
        let source =
            SourceFile::from_text(&source_path, "struct Record { id: i32 } fn first() {}\n");
        let pool = WorkPool::new(1).unwrap();
        let cache = ParseCache::new(root.join("cache"));

        let cold = cache.parse_files(&pool, std::slice::from_ref(&source));
        assert_eq!(cache.stats().misses, 1);
        let warm = cache.parse_files(&pool, std::slice::from_ref(&source));
        assert_eq!(cache.stats().hits, 1);
        assert!(equivalent_parse(&cold[0], &warm[0]));
        assert_eq!(warm[0].structs[0].name, "Record");

        let changed = SourceFile::from_text(&source_path, "fn second() {}\n");
        let incremental = cache.parse_files(&pool, &[changed]);
        assert_eq!(cache.stats().misses, 2);
        assert_eq!(incremental[0].functions[0].name, "second");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parse_cache_separates_cfg_configurations() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let source = SourceFile::from_text(
            root.join("lib.rs"),
            "#[cfg(feature = \"on\")] fn selected() {}\n",
        );
        let cache = ParseCache::new(root.join("cache"));
        let mut enabled = quill_cfg::CfgSet::default();
        enabled.insert_value("feature", "on");
        assert_eq!(
            cache.parse_file_with_cfg(&source, &enabled).functions.len(),
            1
        );
        assert_eq!(cache.parse_file(&source).functions.len(), 0);
        assert_eq!(
            cache.parse_file_with_cfg(&source, &enabled).functions.len(),
            1
        );
        assert_eq!(cache.stats().misses, 2);
        assert_eq!(cache.stats().hits, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupted_cache_falls_back_to_parser_and_is_replaced() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let source_path = root.join("main.rs");
        let source = SourceFile::from_text(&source_path, "fn main() {}\n");
        let cache = ParseCache::new(root.join("cache"));
        let pool = WorkPool::new(1).unwrap();
        let expected = quill_syntax::parse_ast(&source);
        cache.parse_file(&source);
        let mut cfg_hasher = blake3::Hasher::new();
        cfg_hasher.update(&quill_cfg::CfgSet::default().fingerprint());
        cfg_hasher.update(b"2024");
        let cfg_digest = *cfg_hasher.finalize().as_bytes();
        let cache_path = cache.cache_path(&source_path, &cfg_digest);
        fs::write(&cache_path, b"not a valid parse cache").unwrap();

        let recovered = cache.parse_file(&source);
        assert!(equivalent_parse(&expected, &recovered));
        assert_eq!(cache.stats().invalid_entries, 1);
        assert_eq!(cache.stats().write_failures, 0);
        assert!(
            cache.parse_files(&pool, std::slice::from_ref(&source))[0]
                .functions
                .len()
                == 1
        );
        assert_eq!(cache.stats().hits, 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn type_check_cache_keys_source_content_and_strictness() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let source_path = root.join("lib.rs");
        let source = SourceFile::from_text(&source_path, "fn caller() { missing(); }\n");
        let parsed = quill_syntax::parse_ast(&source);
        let cache = TypeCheckCache::new(root.join("cache"));

        let permissive = cache.check_functions(&source, &parsed.functions, false);
        assert!(permissive.is_empty());
        let strict = cache.check_functions(&source, &parsed.functions, true);
        assert!(
            strict
                .iter()
                .any(|diagnostic| diagnostic.message.contains("unresolved"))
        );
        assert!(
            cache
                .check_functions(&source, &parsed.functions, false)
                .is_empty()
        );
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 2);

        let changed =
            SourceFile::from_text(&source_path, "fn caller() { found(); } fn found() {}\n");
        let changed_parse = quill_syntax::parse_ast(&changed);
        assert!(
            cache
                .check_functions(&changed, &changed_parse.functions, true)
                .is_empty()
        );
        assert_eq!(cache.stats().misses, 3);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn per_body_cache_reuses_unedited_functions_and_invalidates_signature_changes() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let path = root.join("lib.rs");
        let cache = TypeCheckCache::new(root.join("cache"));
        let first_source = SourceFile::from_text(
            &path,
            "fn helper(x: i32) -> i32 { x }\nfn caller() -> i32 { helper(1) }\n",
        );
        let first_functions = quill_syntax::parse_ast(&first_source).functions;
        assert!(
            cache
                .check_functions_per_body(&first_source, &first_functions, false, &[])
                .is_empty()
        );
        assert_eq!(cache.stats().misses, 2);

        let edited_source = SourceFile::from_text(
            &path,
            "fn helper(x: i32) -> i32 { x }\nfn caller() -> i32 { helper(true) }\n",
        );
        let edited_functions = quill_syntax::parse_ast(&edited_source).functions;
        assert!(
            !cache
                .check_functions_per_body(&edited_source, &edited_functions, false, &[])
                .is_empty()
        );
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 3);

        let signature_source = SourceFile::from_text(
            &path,
            "fn helper(x: bool) -> i32 { 1 }\nfn caller() -> i32 { helper(true) }\n",
        );
        let signature_functions = quill_syntax::parse_ast(&signature_source).functions;
        assert!(
            cache
                .check_functions_per_body(&signature_source, &signature_functions, false, &[])
                .is_empty()
        );
        assert_eq!(cache.stats().misses, 5);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn type_check_cache_isolated_by_cfg_fingerprint() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let source = SourceFile::from_text(
            root.join("lib.rs"),
            "#[cfg(feature = \"enabled\")] fn selected() -> i32 { true }\n",
        );
        let mut enabled = quill_cfg::CfgSet::default();
        enabled.insert_value("feature", "enabled");
        let enabled_functions = quill_syntax::parse_ast_with_cfg(&source, &enabled).functions;
        let disabled = quill_cfg::CfgSet::default();
        let disabled_functions = quill_syntax::parse_ast_with_cfg(&source, &disabled).functions;
        let enabled_cache = TypeCheckCache::new_with_cfg(root.join("cache"), &enabled);
        let disabled_cache = TypeCheckCache::new_with_cfg(root.join("cache"), &disabled);
        assert_eq!(
            enabled_cache
                .check_functions_per_body(&source, &enabled_functions, false, &[])
                .len(),
            1
        );
        assert!(
            disabled_cache
                .check_functions_per_body(&source, &disabled_functions, false, &[])
                .is_empty()
        );
        assert_eq!(enabled_cache.stats().misses, 1);
        assert_eq!(disabled_cache.stats().misses, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn type_check_cache_invalidates_when_dependency_interface_changes() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let source_path = root.join("lib.rs");
        let source =
            SourceFile::from_text(&source_path, "fn caller() { model_alias::count(true); }\n");
        let parsed = quill_syntax::parse_ast(&source);
        let cache = TypeCheckCache::new(root.join("cache"));
        let bool_signature = ExternalFunctionSignature {
            path: "model_alias::count".to_owned(),
            parameter_types: vec!["bool".to_owned()],
            return_type: Some("u64".to_owned()),
            generic_parameters: Vec::new(),
            generic_copy_parameters: Vec::new(),
            generic_as_ref_str_parameters: Vec::new(),
            generic_add_parameters: Vec::new(),
            generic_has_bounds: false,
        };
        assert!(
            cache
                .check_functions_with_interfaces(
                    &source,
                    &parsed.functions,
                    false,
                    std::slice::from_ref(&bool_signature),
                )
                .is_empty()
        );

        let changed_signature = ExternalFunctionSignature {
            parameter_types: vec!["i32".to_owned()],
            ..bool_signature.clone()
        };
        let diagnostics = cache.check_functions_with_interfaces(
            &source,
            &parsed.functions,
            false,
            std::slice::from_ref(&changed_signature),
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("expected `i32`"))
        );
        assert_eq!(cache.stats().invalid_entries, 1);

        assert_eq!(
            cache.check_functions_with_interfaces(
                &source,
                &parsed.functions,
                false,
                &[changed_signature],
            ),
            diagnostics
        );
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn per_body_cache_invalidates_when_struct_field_type_changes() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let source = SourceFile::from_text(
            root.join("main.rs"),
            "fn main() { let pair = Pair { value: true }; if pair.value {} }",
        );
        let parsed = quill_syntax::parse_ast(&source);
        let cache = TypeCheckCache::new(root.join("cache"));
        let bool_struct = StructSignature {
            path: "Pair".to_owned(),
            generic_parameters: Vec::new(),
            derived_traits: Vec::new(),
            fields: vec![StructFieldSignature {
                name: "value".to_owned(),
                type_text: "bool".to_owned(),
            }],
            ..StructSignature::default()
        };
        assert!(
            cache
                .check_functions_per_body_with_structs(
                    &source,
                    &parsed.functions,
                    false,
                    &[],
                    &[bool_struct],
                )
                .is_empty()
        );
        assert_eq!(cache.stats().misses, 1);

        let integer_struct = StructSignature {
            path: "Pair".to_owned(),
            generic_parameters: Vec::new(),
            derived_traits: Vec::new(),
            fields: vec![StructFieldSignature {
                name: "value".to_owned(),
                type_text: "i32".to_owned(),
            }],
            ..StructSignature::default()
        };
        let diagnostics = cache.check_functions_per_body_with_structs(
            &source,
            &parsed.functions,
            false,
            &[],
            &[integer_struct],
        );
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.message.contains("if condition")
                    && diagnostic.message.contains("expected `bool`")
            }),
            "{diagnostics:?}"
        );
        assert_eq!(cache.stats().misses, 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupted_type_check_cache_falls_back_without_losing_diagnostics() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let source_path = root.join("lib.rs");
        let source = SourceFile::from_text(&source_path, "fn caller() { missing(); }\n");
        let parsed = quill_syntax::parse_ast(&source);
        let cache_root = root.join("cache");
        let cache = TypeCheckCache::new(&cache_root);
        let first = cache.check_functions(&source, &parsed.functions, true);
        assert!(!first.is_empty());

        let path = cache_path(&cache_root, &source_path, "typeck-strict");
        fs::write(path, b"not a valid type-check cache").unwrap();
        let recovered = cache.check_functions(&source, &parsed.functions, true);
        assert_eq!(recovered, first);
        assert_eq!(cache.stats().invalid_entries, 1);
        assert_eq!(cache.stats().write_failures, 0);
        assert_eq!(
            cache.check_functions(&source, &parsed.functions, true),
            first
        );
        assert_eq!(cache.stats().hits, 1);

        fs::remove_dir_all(root).unwrap();
    }
}
