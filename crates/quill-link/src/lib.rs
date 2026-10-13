use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_OBJECT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    InvalidElfObject,
    TemporaryObjectIo(String),
    LinkerUnavailable(String),
    LinkFailed(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidElfObject => write!(formatter, "input is not an ELF object file"),
            Self::TemporaryObjectIo(error) => {
                write!(formatter, "could not prepare temporary object: {error}")
            }
            Self::LinkerUnavailable(error) => {
                write!(formatter, "could not start the system linker `ld`: {error}")
            }
            Self::LinkFailed(error) => write!(formatter, "system linker failed: {error}"),
        }
    }
}

impl std::error::Error for LinkError {}

/// Link an ELF relocatable object into a native executable using GNU-compatible
/// `ld`. Quill owns object generation; this layer only performs final layout
/// and emits the executable. It does not invoke a compiler driver or LLVM.
pub fn link_elf_executable(
    object_bytes: &[u8],
    output_path: impl AsRef<Path>,
    entry_symbol: &str,
) -> Result<(), LinkError> {
    link_elf_objects(&[object_bytes], output_path, entry_symbol)
}

/// Link several Quill-generated ELF relocatable objects into one executable.
/// Cross-object symbols are resolved by `ld`; code generation remains owned by
/// Quill. Object order is preserved to keep linker behavior deterministic.
pub fn link_elf_objects(
    objects: &[&[u8]],
    output_path: impl AsRef<Path>,
    entry_symbol: &str,
) -> Result<(), LinkError> {
    if objects.is_empty() || objects.iter().any(|object| !object.starts_with(b"\x7fELF")) {
        return Err(LinkError::InvalidElfObject);
    }
    let mut temporary_objects = Vec::with_capacity(objects.len());
    let mut object_paths = Vec::with_capacity(objects.len());
    for object in objects {
        let path = TempObject::new_path();
        fs::write(&path, object)
            .map_err(|error| LinkError::TemporaryObjectIo(error.to_string()))?;
        object_paths.push(path.clone());
        temporary_objects.push(TempObject(path));
    }
    link_elf_object_paths(&object_paths, output_path, entry_symbol)
}

/// Link existing ELF object files without copying their contents through
/// temporary files. Callers retain ownership of the input paths.
pub fn link_elf_object_paths(
    object_paths: &[impl AsRef<Path>],
    output_path: impl AsRef<Path>,
    entry_symbol: &str,
) -> Result<(), LinkError> {
    if object_paths.is_empty() {
        return Err(LinkError::InvalidElfObject);
    }
    for path in object_paths {
        let mut file = fs::File::open(path.as_ref())
            .map_err(|error| LinkError::TemporaryObjectIo(error.to_string()))?;
        let mut magic = [0; 4];
        use std::io::Read;
        file.read_exact(&mut magic)
            .map_err(|error| LinkError::TemporaryObjectIo(error.to_string()))?;
        if &magic != b"\x7fELF" {
            return Err(LinkError::InvalidElfObject);
        }
    }
    let mut command = Command::new("ld");
    command
        .arg("-o")
        .arg(output_path.as_ref())
        .arg("-e")
        .arg(entry_symbol);
    command.args(object_paths.iter().map(AsRef::as_ref));
    let output = command
        .output()
        .map_err(|error| LinkError::LinkerUnavailable(error.to_string()))?;
    if !output.status.success() {
        return Err(LinkError::LinkFailed(format!(
            "{}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        )));
    }
    Ok(())
}

struct TempObject(PathBuf);

impl TempObject {
    fn new_path() -> PathBuf {
        let id = TEMP_OBJECT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("quill-link-{}-{id}.o", std::process::id()))
    }
}

impl Drop for TempObject {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object::write::{Object, StandardSection, Symbol, SymbolSection};
    use object::{Architecture, BinaryFormat, Endianness, SymbolFlags, SymbolKind, SymbolScope};

    fn object_with_symbol(name: &str, bytes: &[u8], undefined: Option<&str>) -> Vec<u8> {
        let mut object = Object::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let text = object.section_id(StandardSection::Text);
        let symbol = object.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: 0,
            size: bytes.len() as u64,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        });
        object.add_symbol_data(symbol, text, bytes, 1);
        if let Some(target) = undefined {
            let target_symbol = object.add_symbol(Symbol {
                name: target.as_bytes().to_vec(),
                value: 0,
                size: 0,
                kind: SymbolKind::Text,
                scope: SymbolScope::Linkage,
                weak: false,
                section: SymbolSection::Undefined,
                flags: SymbolFlags::None,
            });
            object
                .add_relocation(
                    text,
                    object::write::Relocation {
                        offset: 1,
                        symbol: target_symbol,
                        addend: -4,
                        flags: object::RelocationFlags::Generic {
                            kind: object::RelocationKind::Relative,
                            encoding: object::RelocationEncoding::X86Branch,
                            size: 32,
                        },
                    },
                )
                .unwrap();
        }
        object.write().unwrap()
    }

    #[test]
    fn rejects_non_elf_input_before_invoking_linker() {
        let result = link_elf_executable(b"not an ELF object", "/tmp/quill-link-unused", "_start");
        assert_eq!(result, Err(LinkError::InvalidElfObject));
    }

    #[test]
    fn links_cross_object_call_and_runs_result() {
        let caller = object_with_symbol(
            "_start",
            &[
                0xe8, 0, 0, 0, 0, 0x89, 0xc7, 0xb8, 60, 0, 0, 0, 0x0f, 0x05, 0x0f, 0x0b,
            ],
            Some("answer"),
        );
        let callee = object_with_symbol("answer", &[0xb8, 42, 0, 0, 0, 0xc3], None);
        let output = std::env::temp_dir().join(format!("quill-link-test-{}", std::process::id()));
        link_elf_objects(&[&caller, &callee], &output, "_start").unwrap();
        let result = Command::new(&output).status().unwrap();
        let _ = fs::remove_file(output);
        assert_eq!(result.code(), Some(42));
    }
}
