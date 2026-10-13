//! Differential tests: the rust-analyzer collector must reproduce the model
//! the Syn collector produced for the same source, cfg and edition.

use std::path::{Path, PathBuf};
use std::process::Command;

use quill_source::SourceFile;

use crate::{Parse, parse_ast_with_cfg_and_edition, syn_oracle};

fn model(parse: &Parse) -> serde_json::Value {
    let mut value = serde_json::to_value(parse).expect("parse model serializes");
    // Syn and rust-analyzer word syntax errors differently; compare the
    // presence of an error rather than its message.
    if let Some(diagnostics) = value.get_mut("diagnostics") {
        *diagnostics = serde_json::Value::Bool(parse.had_error);
    }
    value
}

/// First differing JSON path between two models.
fn first_difference(left: &serde_json::Value, right: &serde_json::Value, path: &str) -> Option<String> {
    use serde_json::Value;
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, value) in a {
                let other = b.get(key).unwrap_or(&Value::Null);
                if let Some(found) = first_difference(value, other, &format!("{path}.{key}")) {
                    return Some(found);
                }
            }
            None
        }
        (Value::Array(a), Value::Array(b)) => {
            for (index, (x, y)) in a.iter().zip(b).enumerate() {
                if let Some(found) = first_difference(x, y, &format!("{path}[{index}]")) {
                    return Some(found);
                }
            }
            (a.len() != b.len()).then(|| format!("{path}: length {} vs {}", a.len(), b.len()))
        }
        _ => (left != right).then(|| format!("{path}: syn={left} ra={right}")),
    }
}

/// Compare functions by source identity before their list positions. A single
/// parser-only item can shift every later function index and hide the actual
/// differing function body.
fn model_difference(expected: &serde_json::Value, actual: &serde_json::Value) -> Option<String> {
    let expected_functions = expected["functions"].as_array()?;
    let actual_functions = actual["functions"].as_array()?;
    let mut matched_actual = vec![false; actual_functions.len()];
    let mut missing = Vec::new();
    for expected_function in expected_functions {
        let key = function_identity(expected_function);
        let Some((index, actual_function)) = actual_functions
            .iter()
            .enumerate()
            .find(|(index, function)| {
                !matched_actual[*index] && function_identity(function) == key
            })
        else {
            missing.push(key);
            continue;
        };
        matched_actual[index] = true;
        if let Some(difference) = first_difference(
            expected_function,
            actual_function,
            &format!(".functions[{key}]"),
        ) {
            return Some(difference);
        }
    }
    let extra = actual_functions
        .iter()
        .enumerate()
        .filter(|(index, _)| !matched_actual[*index])
        .map(|(_, function)| function_identity(function))
        .collect::<Vec<_>>();
    if !missing.is_empty() || !extra.is_empty() {
        return Some(format!(
            ".functions missing from RA: {missing:?}; extra in RA: {extra:?}"
        ));
    }
    let expected_order = expected_functions
        .iter()
        .map(function_identity)
        .collect::<Vec<_>>();
    let actual_order = actual_functions
        .iter()
        .map(function_identity)
        .collect::<Vec<_>>();
    if expected_order != actual_order {
        return Some(format!(
            ".functions order: Syn={expected_order:?}; RA={actual_order:?}"
        ));
    }

    let mut expected_without_functions = expected.clone();
    let mut actual_without_functions = actual.clone();
    if let Some(object) = expected_without_functions.as_object_mut() {
        object.remove("functions");
    }
    if let Some(object) = actual_without_functions.as_object_mut() {
        object.remove("functions");
    }
    first_difference(&expected_without_functions, &actual_without_functions, "")
}

fn function_identity(function: &serde_json::Value) -> String {
    format!(
        "{}::{}@{}",
        function["scope"], function["name"], function["name_span"]["start"]
    )
}

/// The rust-analyzer collector resolves `Self { .. }` struct literals inside
/// impls to the impl's type, which the Syn collector never did. Apply the
/// same rule to the oracle model before comparing.
fn resolve_oracle_self_literals(model: &mut serde_json::Value) {
    fn rewrite(value: &mut serde_json::Value, owner: &str) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::Object(literal)) = map.get_mut("StructLiteral")
                    && let Some(serde_json::Value::String(path)) = literal.get_mut("path")
                {
                    if path == "Self" {
                        *path = owner.to_owned();
                    } else if let Some(suffix) = path.strip_prefix("Self::") {
                        *path = format!("{owner}::{suffix}");
                    }
                }
                map.values_mut().for_each(|value| rewrite(value, owner));
            }
            serde_json::Value::Array(values) => {
                values.iter_mut().for_each(|value| rewrite(value, owner));
            }
            _ => {}
        }
    }
    let Some(serde_json::Value::Array(functions)) = model.get_mut("functions") else {
        return;
    };
    for function in functions {
        let owner = function["scope"].as_array().and_then(|scope| {
            scope
                .iter()
                .rev()
                .find_map(|scope| scope.as_str()?.strip_prefix("impl:").map(str::to_owned))
        });
        if let Some(owner) = owner {
            rewrite(&mut function["body"], &owner);
        }
    }
}

pub(crate) fn compare(source: &SourceFile, cfg: &quill_cfg::CfgSet, edition: &str) -> Option<String> {
    let expected = syn_oracle::parse_ast_with_cfg_and_edition(source, cfg, edition);
    let actual = parse_ast_with_cfg_and_edition(source, cfg, edition);
    let parse_error_differs = expected.had_error != actual.had_error;
    let actual_diagnostics = actual.diagnostics.clone();
    let mut expected = model(&expected);
    resolve_oracle_self_literals(&mut expected);
    let actual = model(&actual);
    model_difference(&expected, &actual).map(|difference| {
        if parse_error_differs {
            format!("{difference}; Quill diagnostics: {actual_diagnostics:?}")
        } else {
            difference
        }
    })
}

fn rust_files(root: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // rust-analyzer's parser fixtures are deliberately malformed or
            // use rust-analyzer-only syntax (`builtin #`); they are not Rust
            // sources the compiler receives.
            if path
                .file_name()
                .is_some_and(|name| name == "target" || name == "test_data")
            {
                continue;
            }
            rust_files(&path, output);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.push(path);
        }
    }
}

/// Edition from the nearest enclosing `Cargo.toml`, defaulting like Cargo.
fn package_edition(path: &Path) -> String {
    for directory in path.ancestors().skip(1) {
        let Ok(manifest) = std::fs::read_to_string(directory.join("Cargo.toml")) else {
            continue;
        };
        if !manifest.contains("[package]") {
            continue;
        }
        return manifest
            .lines()
            .map(str::trim)
            .find_map(|line| {
                let value = line.strip_prefix("edition")?.trim_start().strip_prefix('=')?;
                Some(value.trim().trim_matches('"').to_owned())
            })
            .filter(|edition| matches!(edition.as_str(), "2015" | "2018" | "2021" | "2024"))
            .unwrap_or_else(|| "2015".to_owned());
    }
    "2021".to_owned()
}

fn compare_tree(root: &Path, limit: usize) -> (usize, Vec<String>) {
    let mut files = Vec::new();
    rust_files(root, &mut files);
    files.sort();
    let mut mismatches = Vec::new();
    let mut compared = 0;
    for path in files {
        let Ok(source) = SourceFile::load(&path) else {
            continue;
        };
        // Syn reports errors at a line/column; skip inputs it cannot place.
        let edition = package_edition(&path);
        let syn_parse = std::panic::catch_unwind(|| {
            syn_oracle::parse_ast_with_cfg_and_edition(&source, &quill_cfg::CfgSet::default(), &edition)
        });
        if syn_parse.is_err() {
            continue;
        }
        compared += 1;
        if let Some(difference) = compare(&source, &quill_cfg::CfgSet::default(), &edition) {
            mismatches.push(format!("{} ({edition}): {difference}", path.display()));
            if mismatches.len() >= limit {
                break;
            }
        }
    }
    (compared, mismatches)
}

#[test]
fn matches_syn_model_for_workspace_sources() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (compared, mismatches) = compare_tree(&root, 50);
    assert!(compared > 20, "only {compared} workspace files compared");
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

/// Run with `QUILL_SYNTAX_DIFF_DIR=~/.cargo/registry/src cargo test -p
/// quill-syntax -- --ignored differential_corpus` for a large real-world run.
#[test]
#[ignore]
fn differential_corpus() {
    let root = std::env::var_os("QUILL_SYNTAX_DIFF_DIR").expect("QUILL_SYNTAX_DIFF_DIR is set");
    let limit = std::env::var("QUILL_SYNTAX_DIFF_LIMIT")
        .ok()
        .and_then(|limit| limit.parse().ok())
        .unwrap_or(200);
    let (compared, mismatches) = compare_tree(Path::new(&root), limit);
    eprintln!("compared {compared} files, {} mismatches", mismatches.len());
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

/// Focused fixtures for forms where the parsers' trees differ in shape.
const PARITY_FIXTURES: &[&str] = &[
    // Items, visibility, generics, bounds and doc-comment attribute counts.
    "//! crate docs\n#![allow(dead_code)]\n/// Docs count as attributes.\n/// Twice.\npub fn map<T: Copy, U: AsRef<str>, V: std::ops::Add<Output = V>>(a: T, b: U, c: V) -> T where T: Clone { a }",
    "pub(crate) fn hidden() {} pub(in crate::a) fn scoped() {} fn r#match(r#type: u8) -> u8 { r#type }",
    "trait Value { fn value(&self) -> u64; } struct N; impl Value for N { fn value(&self) -> u64 { 1 } } fn read<T: Value + ?Sized, 'a>(x: &'a T) -> u64 { x.value() }",
    "#[derive(Clone, Copy, Debug)] #[repr(C)] pub struct Point<T> { pub x: T, #[allow(x)] y: f64 } #[repr(C, packed)] struct P(u8); #[derive(Eq)] struct F(f64); union U { a: u8 }",
    "/// docs\n#[repr(i32)] #[derive(PartialEq)] enum E { A = -3, B, C = 0x10, D(u8), E = 1u8, F = (2) } enum Dup { A = 1, B = 1 }",
    "impl<T: Copy> Wrapper<T> { pub fn get(&self) -> T { self.0 } fn take(mut self: Box<Self>) {} fn r(&mut self) {} } impl std::fmt::Display for W { fn fmt(&self) {} }",
    "extern \"C\" { pub fn abs(x: i32) -> i32; fn printf(f: *const u8, ...) -> i32; static X: u8; } const C: u8 = { fn nested() {} 1 }; static S: u8 = 0; type A = u8;",
    "mod a { pub mod b; use super::{self as s, c::*, d::{e, f as g}}; use ::std::fmt; use x as _; } pub use crate::a::b;",
    "#[path = \"x.rs\"] mod m; fn f() { use std::io; struct Inner; fn nested() { helper(); } }",
    // Expressions and statements.
    "fn f(mut x: u64, v: &mut [u8]) -> u64 { x += 1; x -= 2; x = x * 2 % 3 ^ 1 << 2; v[0] = 1; *v.first_mut().unwrap() = 2; let y: u64 = 1f32 as u64 + 2.5e3f64 as u64 + 1_000u32 as u64 + 0xff_u8 as u64; y }",
    "fn f(o: Option<u8>) -> u8 { match o { Some(v) if v > 1 => v, None => 0, _ if true => 1, 1 | 2 => 3, -4..=5 => 6, 'a'..'z' => 7, x @ 1 => x, ref r => *r, } }",
    "fn f(x: u8) { if let 1 | 2 = x { } else if let 3..=4 = (x) { } else if let Some(y) = g() { } if x > 1 && let 2 = x {} while let Some(_) = None::<u8> {} }",
    "fn f() { 'outer: loop { break 'outer; } loop { continue; } for i in 0..10 { } for (a, b) in pairs { } for &x in v { } for i in (0..=3) {} let (a, b) = (1, 2); let (a, mut b) = t; let _: u8 = 0; let Some(x) = o else { return; }; }",
    "fn f() -> u8 { let s = S { a: 1, b, ..Default::default() }; let t = T { a: 1, .. }; let u = U { 0: 1 }; let c = |a, (b, c)| a + b; let d = |x: u8| x; let e = move || 1; s.a }",
    "fn f() { let a = [1, 2]; let b = [0; 3]; let c = &[]; let d = &mut [1]; let e = &raw const x; let g = <T as Tr>::f(); let h = Vec::<u8>::new(); let i = f::<u8, Vec<u8>>(1); let j = (); let k = (1,); unsafe { } async {}; x? }",
    "fn f() { println!(\"{} {x}\", a, x = 1); let s = format!(\"v\"); vec![1; 3]; m! { a } concat!(\"a\", 1, 'c', 2.0, true); write!(f, \"x\")?; assert_eq!(a, b,); let t = stringify!(a + b); }",
    // Local macro_rules expansion.
    "macro_rules! answer { () => { 42u64 }; } macro_rules! sum { ($($v:expr),+ $(,)?) => {{ let mut t = 0; $(t = t + $v;)+ t }}; } fn main() { let a = answer!(); let b = sum!(call(1), 2 * 3,); answer!(); }",
    "unsafe impl X for Foo where (): Check<Self, { #[repr(C)] pub enum Tag { Bar } 1 }> { fn f() {} } trait T: Check<{ fn g() {} 1 }> { fn h(&self); }",
    "macro_rules! tag { () => { \"t\" }; } fn main() { assert_eq!(tag!().len(), 1); let x = tag!(); }",
    "macro_rules! twice { ($($v:expr);*) => { ($($v),*) }; } fn main() { let x = twice!(1; 2); } impl X { fn m() { let y = twice!(); } }",
    "fn f() { quote!(#input_type#ty_generics); }",
    "/// é\nfn naïve(ünïcode: &str) -> &str { let ß = 'α'; let t = \"日本\"; ß }",
    "struct S; impl S { fn r<'a>(&'a mut self) {} fn s(&'static self) {} fn t(&'_ self) {} }",
    "fn f() -> bool { let a = -1f64; let b = 1f32; let c = 2.0f64; let d = 0x1F_u8; d == 1 }",
    "fn f(x: i32) -> i32 { match x { -1 => 0, -5..=-2 => 1, 0 ... 9 => 2, '0'...'9' => 3, _ => 4 } }",
    "fn decode(b0: u8) -> u8 { match b0 { b'a'..=b'f' => 10 + (b0 - b'a'), _ => 0 } }",
    "fn decode(b0: u8) -> u8 { let mut ch = 0; ch += 0x10 * match b0 { b'0'..=b'9' => b0 - b'0', b'a'..=b'f' => 10 + (b0 - b'a'), b'A'..=b'F' => 10 + (b0 - b'A'), _ => return 0 }; ch }",
    "fn call<T>(x: T, y: i32, z: i32) { g::<T>(y + (y + z), x); }",
    "fn f() { while 'w: { let mut c = 0; if c == 1 { break 'w false; } true } {} let v = 'b: { 1 }; let u = unsafe { 2 }; }",
    "fn f() -> f64 { 3.402e+38 + 1e-3 + 2E+5_f64 }",
    "fn f() -> &'static str {\r\n    let s = \"a\\\r\n        b\";\r\n    let t = \"x\r\ny\";\r\n    println!(\"one\\\r\n  two\");\r\n    concat!(\"p\r\nq\", 1)\r\n}\r\n",
    "\u{feff}fn bom() -> u8 { 1 }",
    "fn f() { m!(/// doc\n a, //! inner\n b /** block */); }",
    // Malformed input.
    "fn broken() { let value = ; }",
    "fn f( {",
    "struct S { a: u8,, }",
];

#[test]
fn matches_syn_model_for_focused_fixtures() {
    let mut cfg = quill_cfg::CfgSet::default();
    cfg.insert_flag("unix");
    let mut mismatches = Vec::new();
    for fixture in PARITY_FIXTURES {
        let source = SourceFile::from_text("fixture.rs", *fixture);
        for edition in ["2018", "2021", "2024"] {
            for cfg in [&quill_cfg::CfgSet::default(), &cfg] {
                if let Some(difference) = compare(&source, cfg, edition) {
                    mismatches.push(format!("{edition} {fixture}\n    {difference}"));
                }
            }
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn matches_syn_cfg_filtering() {
    let source = SourceFile::from_text(
        "cfg.rs",
        "#[cfg(all(unix, not(feature = \"off\")))] fn a() {} #[cfg(any())] fn b() {} #[cfg_attr(unix, cfg(feature = \"on\"))] fn c() {} #[cfg_attr(not(unix), path = \"w.rs\")] #[cfg_attr(unix, path = \"u.rs\")] mod m; impl S { #[cfg(windows)] fn d() {} fn e() {} } trait T { #[cfg(unix)] fn f(); }",
    );
    for flags in [&[][..], &["unix"][..]] {
        let mut cfg = quill_cfg::CfgSet::default();
        for flag in flags {
            cfg.insert_flag(*flag);
        }
        cfg.insert_value("feature", "on");
        assert_eq!(compare(&source, &cfg, "2021"), None, "flags {flags:?}");
    }
}

#[test]
fn matches_syn_edition_rules() {
    let cases = [
        ("2015", "trait V {} fn a(_: &dyn V) {} fn main() { let async = 1; let await = 2; let dyn = 3; }"),
        ("2015", "pub async fn answer() -> u64 { 42 }"),
        ("2024", "fn main() { let gen = 1u64; }"),
        ("2024", "extern \"C\" { fn abs(value: i32) -> i32; }"),
        ("2024", "#[no_mangle] pub extern \"C\" fn exported() {}"),
        ("2024", "#[export_name = \"x\"] fn exported() {}"),
        ("2024", "#[unsafe(no_mangle)] pub extern \"C\" fn e() {} unsafe extern \"C\" { fn abs(v: i32) -> i32; } macro_rules! d { ($i:tt) => {}; } fn main() { let r#gen = 1u64; d!(gen); }"),
        ("2021", "fn main() { let gen = 1; } extern \"C\" {} #[no_mangle] fn e() {}"),
    ];
    for (edition, text) in cases {
        let source = SourceFile::from_text("edition.rs", text);
        let expected = syn_oracle::parse_ast_with_cfg_and_edition(&source, &quill_cfg::CfgSet::default(), edition);
        let actual = parse_ast_with_cfg_and_edition(&source, &quill_cfg::CfgSet::default(), edition);
        assert_eq!(expected.had_error, actual.had_error, "{edition}: {text}: {:?}", actual.diagnostics);
        assert_eq!(compare(&source, &quill_cfg::CfgSet::default(), edition), None, "{edition}: {text}");
    }
}

#[test]
fn matches_rustc_parser_acceptance_for_item_macro_paths() {
    let cases = [
        ("2021", "safe! { fn inside_macro() {} } fn after() {}", true),
        ("2021", "safe ! { fn inside_macro() {} } fn after() {}", true),
        ("2021", "safe!(); fn after() {}", true),
        ("2024", "safe! { fn inside_macro() {} } fn after() {}", true),
        ("2021", "fn main() { safe! {} }", true),
        ("2021", "fn broken( {", false),
    ];
    for (edition, text, expected_acceptance) in cases {
        let source = SourceFile::from_text("rustc-parser.rs", text);
        let actual =
            parse_ast_with_cfg_and_edition(&source, &quill_cfg::CfgSet::default(), edition);
        let rustc_accepts = rustc_parser_accepts(text, edition);
        assert_eq!(rustc_accepts, expected_acceptance, "rustc fixture: {text}");
        assert_eq!(
            !actual.had_error,
            rustc_accepts,
            "edition {edition}, source {text:?}, diagnostics {:?}",
            actual.diagnostics
        );
        if expected_acceptance && text.contains("fn after()") {
            assert!(
                actual.functions.iter().any(|function| function.name == "after"),
                "following item disappeared after macro parse: {text}"
            );
        }
    }
}

#[test]
fn matches_rustc_parser_acceptance_for_upstream_recovery_cases() {
    let cases = [
        (
            "2021",
            "fn f<T: (Copy) + (?Sized) + (for<'a> Trait<'a>)>() {}",
        ),
        (
            "2021",
            "fn main() { let _: Box<(Copy) + (?Sized) + (for<'a> Trait<'a>)>; }",
        ),
        ("2021", "fn bounds<T: (Copy) + (Send)>() {}"),
        ("2021", "fn multiline<T: (Copy)\n    + (Send)>() {}"),
        ("2021", "fn where_bounds<T>() where T: (Copy) + (Send) {}"),
        (
            "2021",
            "fn bounds<T: (Copy) + (Send)>() {}\n/// The range is 10^lo <= V < 10^hi.\n#[doc = \"where x < y\"]\nfn tuple(dec_bounds: (i16, i16)) {}",
        ),
        ("2021", "type LegacyObject = Box<(Send) + (Sync)>;"),
        ("2021", "fn object() { let _: Box<(Send)>; }"),
        (
            "2024",
            "#![feature(const_closures)] fn closure() { const move |a| wrap(a); }",
        ),
        (
            "2024",
            "#![feature(const_closures)] impl S { const fn one(f: F) { const move |a| f(a) } const fn two(f: F) { const move |a, b| f(a, b) } }",
        ),
        (
            "2024",
            "#![feature(final_associated_functions)] pub trait Share { final fn share(&self) -> Self { self.clone() } }",
        ),
        (
            "2024",
            "#![feature(final_associated_functions)] final fn top_level() {}",
        ),
        (
            "2024",
            "#![feature(min_specialization)] trait SizeHint { fn lower(&self) -> usize; } impl<T: ?Sized> SizeHint for T { default fn lower(&self) -> usize { 0 } }",
        ),
        ("2021", "impl impl NotType {}"),
        ("2021", "impl Trait2 for impl NotType {}"),
        ("2021", "impl impl AnotherType {}"),
        ("2021", "impl OtherTrait for impl AnotherType {}"),
        ("2021", "#![cfg(key = value)]"),
        ("2021", "struct S(i32, i32); fn f() { let s = S(1, 2); let a = s.1e0; }"),
    ];
    for (edition, text) in cases {
        assert!(rustc_parser_accepts(text, edition), "rustc fixture: {text}");
        let source = SourceFile::from_text("upstream-parser-recovery.rs", text);
        let actual =
            parse_ast_with_cfg_and_edition(&source, &quill_cfg::CfgSet::default(), edition);
        assert!(
            !actual.had_error,
            "edition {edition}, source {text:?}, diagnostics {:?}",
            actual.diagnostics
        );
    }

    let malformed = "fn main() { let _: Box<(Copy) + (?Sized)>; let broken = ; }";
    assert!(!rustc_parser_accepts(malformed, "2021"));
    let source = SourceFile::from_text("malformed-with-recovery-context.rs", malformed);
    let actual = parse_ast_with_cfg_and_edition(
        &source,
        &quill_cfg::CfgSet::default(),
        "2021",
    );
    assert!(actual.had_error, "malformed syntax was accepted: {malformed}");

    let malformed_impl = "impl OtherTrait for impl AnotherType { fn broken( {} }";
    assert!(!rustc_parser_accepts(malformed_impl, "2021"));
    let source = SourceFile::from_text("malformed-impl-recovery-context.rs", malformed_impl);
    let actual = parse_ast_with_cfg_and_edition(
        &source,
        &quill_cfg::CfgSet::default(),
        "2021",
    );
    assert!(
        actual.had_error,
        "malformed impl body was accepted: {malformed_impl}"
    );
}

fn rustc_parser_accepts(text: &str, edition: &str) -> bool {
    use std::io::Write;

    let mut rustc = Command::new("rustc")
        .args(["--edition", edition, "-Z", "unpretty=ast-tree", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("pinned rustc is available for syntax compatibility tests");
    rustc
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(text.as_bytes())
        .expect("write Rust fixture");
    rustc.wait().expect("wait for rustc").success()
}

/// Compare rustc-accepted parser fixtures with the compiler parser. Rustc's
/// AST dump also runs early AST validation, so only status-zero sources count
/// as evidence. Run with `cargo test -p quill-syntax rustc_parser_fixture_corpus
/// -- --ignored --nocapture`.
#[test]
#[ignore]
fn rustc_parser_fixture_corpus() {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").expect("HOME is set")).join(".cargo")
        });
    let roots = std::fs::read_dir(cargo_home.join("registry/src"))
        .expect("Cargo registry sources are available")
        .flatten()
        .map(|entry| entry.path().join("ra_ap_parser-0.0.358/test_data/parser"))
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    let root = roots
        .first()
        .expect("pinned ra_ap_parser fixtures are installed");
    let mut fixtures = Vec::new();
    rust_files(root, &mut fixtures);
    fixtures.retain(|path| {
        let relative = path
            .strip_prefix(root)
            .expect("fixture is below corpus root");
        matches!(
            relative
                .components()
                .next()
                .and_then(|part| part.as_os_str().to_str()),
            Some("ok" | "err" | "inline")
        )
    });
    fixtures.sort();

    let mut mismatches = Vec::new();
    let mut rustc_accepted = 0;
    for path in &fixtures {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let edition = text
            .lines()
            .next()
            .and_then(|line| {
                ["//@ edition: ", "// "]
                    .into_iter()
                    .find_map(|prefix| line.strip_prefix(prefix))
            })
            .map(|value| value.trim().chars().take(4).collect::<String>())
            .filter(|value| matches!(value.as_str(), "2015" | "2018" | "2021" | "2024"))
            .unwrap_or_else(|| "2024".to_owned());
        let source = SourceFile::from_text(path, text.clone());
        let rustc_accepts = rustc_parser_accepts(&text, &edition);
        if !rustc_accepts {
            continue;
        }
        rustc_accepted += 1;
        if let Err(diagnostic) =
            crate::collect::collect(&source, &quill_cfg::CfgSet::default(), &edition, false)
        {
            mismatches.push(format!(
                "{} ({edition}): Quill rejected rustc-accepted syntax: {} at {:?}",
                path.strip_prefix(root).unwrap_or(path).display(),
                diagnostic.message,
                diagnostic.span
            ));
            if mismatches.len() >= 100 {
                break;
            }
        }
    }
    eprintln!(
        "checked {rustc_accepted} rustc-accepted fixtures of {} complete-source fixtures",
        fixtures.len()
    );
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

/// Run with `cargo test -p quill-syntax --lib rustc_library_source_corpus
/// -- --ignored --nocapture` to compare rustc's accepted standard-library
/// sources with the syntax collector. Rustc's AST dump can reject sources
/// during early AST validation, so rejected files are not parser evidence.
#[test]
#[ignore]
fn rustc_library_source_corpus() {
    let sysroot = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .expect("rustc sysroot is available");
    let library_root =
        PathBuf::from(String::from_utf8(sysroot.stdout).expect("sysroot is UTF-8").trim())
            .join("lib/rustlib/src/rust/library");
    let roots = [
        "core",
        "alloc",
        "std",
        "test",
        "proc_macro",
        "panic_abort",
        "panic_unwind",
        "std_detect",
    ]
    .map(|crate_name| library_root.join(crate_name).join("src"))
    .into_iter()
    .filter(|path| path.is_dir())
    .collect::<Vec<_>>();
    if roots.is_empty() {
        eprintln!("rust-src library sources are not installed; skipping {}", library_root.display());
        return;
    }

    let mut files = Vec::new();
    for root in &roots {
        rust_files(root, &mut files);
    }
    files.sort();
    let mut rustc_accepted = 0;
    let mut mismatches = Vec::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let edition = package_edition(&path);
        if !rustc_parser_accepts(&text, &edition) {
            continue;
        }
        rustc_accepted += 1;
        let source = SourceFile::from_text(&path, text);
        if let Err(diagnostic) = crate::collect::collect(
            &source,
            &quill_cfg::CfgSet::default(),
            &edition,
            true,
        ) {
            mismatches.push(format!(
                "{} ({edition}): Quill rejected rustc-accepted syntax: {} at {:?}",
                path.strip_prefix(&library_root).unwrap_or(&path).display(),
                diagnostic.message,
                diagnostic.span
            ));
            if mismatches.len() >= 100 {
                break;
            }
        }
    }
    eprintln!(
        "checked {rustc_accepted} rustc-accepted files from {} crates under {}",
        roots.len(),
        library_root.display()
    );
    assert!(rustc_accepted > 0, "no rustc-accepted core sources found");
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

/// Intentional divergences where the Syn collector was wrong.
#[test]
fn fixes_syn_lifetime_receivers_and_multibyte_literal_spans() {
    let source = SourceFile::from_text(
        "fixed.rs",
        "struct S; impl S { fn r<'a>(&'a mut self) {} } fn f() -> &'static str { let ß = \"日本\"; ß }",
    );
    let parsed = crate::parse_ast(&source);
    assert!(!parsed.had_error, "{:?}", parsed.diagnostics);
    assert_eq!(parsed.functions[0].parameters[0].type_text, "&'a mut Self");
    let Some(crate::BodyExpr {
        kind: crate::BodyExprKind::Block { statements, .. },
        ..
    }) = &parsed.functions[1].body
    else {
        panic!("expected a block body");
    };
    let crate::BodyStatement::Let {
        initializer: Some(initializer),
        span,
        ..
    } = &statements[0]
    else {
        panic!("expected a local");
    };
    assert_eq!(source.slice(initializer.span.clone()), "\"日本\"");
    assert_eq!(source.slice(span.clone()), "let ß = \"日本\";");
}
