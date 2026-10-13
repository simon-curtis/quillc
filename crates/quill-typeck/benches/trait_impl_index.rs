use std::hint::black_box;
use std::time::Instant;

use quill_source::SourceFile;
use quill_syntax::parse;
use quill_typeck::{check_functions_with_declarations_and_structs, StructSignature};

fn workload(count: usize) -> String {
    let mut source = String::with_capacity(count.saturating_mul(220));
    for index in 0..count {
        source.push_str(&format!(
            "trait Trait{index} {{ fn method{index}(&self) -> u64; }}\n"
        ));
    }
    source.push_str("struct Item;\n");
    for index in 0..count {
        source.push_str(&format!(
            "impl Trait{index} for Item {{ fn method{index}(&self) -> u64 {{ {index} }} }}\n"
        ));
    }
    for index in 0..count {
        source.push_str(&format!(
            "fn read_item{index}<T: Trait{index}>(item: &T) -> u64 {{ item.method{index}() }}\n"
        ));
    }
    source.push_str("fn main() { let item = Item;\n");
    for index in 0..count {
        source.push_str(&format!("let result{index} = read_item{index}(&item);\n"));
    }
    source.push_str("}\n");
    source
}

fn main() {
    let count = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(1_000usize);
    let source = SourceFile::from_text("trait-index-bench.rs", workload(count));
    let parsed = parse(&source);
    assert_eq!(parsed.functions.len(), count * 3 + 1);
    let structs = [StructSignature {
        path: "Item".to_owned(),
        ..StructSignature::default()
    }];

    let check = || {
        check_functions_with_declarations_and_structs(
            &parsed.functions,
            &parsed.functions,
            false,
            &[],
            &structs,
        )
    };
    assert!(check().is_empty());
    if count > 0 {
        let invalid = source.text().replacen(
            "fn method0(&self) -> u64 { 0 }",
            "fn method0(&self) -> i32 { 0 }",
            1,
        );
        let invalid_source = SourceFile::from_text("trait-index-invalid.rs", invalid);
        let invalid_parsed = parse(&invalid_source);
        let diagnostics = check_functions_with_declarations_and_structs(
            &invalid_parsed.functions,
            &invalid_parsed.functions,
            false,
            &[],
            &structs,
        );
        assert!(diagnostics.iter().any(|diagnostic| diagnostic.message.contains("does not implement")),
            "the workload must exercise trait implementation checks: {diagnostics:?}; generics={:?}",
            invalid_parsed.functions.iter().filter(|function| !function.generic_parameters.is_empty()).take(3).map(|function| (&function.name, &function.generic_trait_bounds)).collect::<Vec<_>>());
    }
    let mut samples = Vec::with_capacity(7);
    for _ in 0..7 {
        let start = Instant::now();
        let diagnostics = black_box(check());
        assert!(diagnostics.is_empty());
        samples.push(start.elapsed().as_secs_f64());
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "traits={count} functions={} samples_seconds={samples:?} median_seconds={:.6}",
        parsed.functions.len(),
        samples[samples.len() / 2]
    );
}
