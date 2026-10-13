use std::hint::black_box;
use std::time::{Duration, Instant};

use ra_ap_syntax::{Edition, SourceFile, TextRange, TextSize};

const ROUNDS: usize = 31;

fn workload() -> String {
    let mut lines = vec![
        "#![allow(dead_code)]".to_owned(),
        "macro_rules! identity { ($e:expr) => { $e }; }".to_owned(),
    ];
    for module in 0..40 {
        lines.push(format!("pub mod module_{module} {{"));
        lines.push("    pub trait Convert<T> { fn convert(&self) -> T; }".to_owned());
        for function in 0..50 {
            let name = format!("function_{module}_{function}");
            lines.extend([
                format!("    pub async fn {name}(input: &[u8]) -> usize {{"),
                "        let mut total = 0usize;".to_owned(),
                "        for byte in input.iter().copied() {".to_owned(),
                "            total += identity!(usize::from(byte));".to_owned(),
                "        }".to_owned(),
                "        total".to_owned(),
            ]);
            if module == 39 && function == 49 {
                lines.push("        // EDIT000000".to_owned());
            }
            lines.push("    }".to_owned());
        }
        lines.push("}".to_owned());
        lines.push(String::new());
    }
    lines.join("\n")
}

fn syn_parse(source: &str) -> usize {
    let tokens = source
        .parse::<proc_macro2::TokenStream>()
        .expect("benchmark source tokenizes with proc_macro2");
    let file = syn::parse2::<syn::File>(tokens).expect("benchmark source parses with Syn");
    let item_count = file.items.len();
    black_box(file);
    item_count
}

fn ra_parse(source: &str) -> usize {
    let parsed = SourceFile::parse(source, Edition::Edition2024);
    let error_count = parsed.errors().len();
    black_box(parsed);
    error_count
}

fn median(samples: &[Duration]) -> Duration {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

fn p95(samples: &[Duration]) -> Duration {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[((sorted.len() * 95).div_ceil(100) - 1).min(sorted.len() - 1)]
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn main() {
    let source = workload();
    let changed = source.replacen("EDIT000000", "EDIT000001", 1);
    let edit_start = source.find("EDIT000000").expect("edit marker") as u32;
    let previous = SourceFile::parse(&source, Edition::Edition2024);
    assert!(
        previous.errors().is_empty(),
        "RA rejects the benchmark source"
    );
    assert!(
        syn::parse_file(&source).is_ok(),
        "Syn rejects the benchmark source"
    );
    assert_eq!(syn_parse(&source), 41);

    let mut ra_full = Vec::with_capacity(ROUNDS);
    let mut syn_full = Vec::with_capacity(ROUNDS);
    let mut ra_incremental = Vec::with_capacity(ROUNDS);
    let mut syn_after_edit = Vec::with_capacity(ROUNDS);

    for round in 0..ROUNDS {
        if round % 2 == 0 {
            let started = Instant::now();
            black_box(ra_parse(&source));
            ra_full.push(started.elapsed());

            let started = Instant::now();
            black_box(syn_parse(&source));
            syn_full.push(started.elapsed());
        } else {
            let started = Instant::now();
            black_box(syn_parse(&source));
            syn_full.push(started.elapsed());

            let started = Instant::now();
            black_box(ra_parse(&source));
            ra_full.push(started.elapsed());
        }

        let started = Instant::now();
        black_box(previous.reparse(
            TextRange::new(TextSize::from(edit_start), TextSize::from(edit_start + 10)),
            "EDIT000001",
            Edition::Edition2024,
        ));
        ra_incremental.push(started.elapsed());

        let started = Instant::now();
        black_box(syn_parse(&changed));
        syn_after_edit.push(started.elapsed());
    }

    let sample_array = |samples: &[Duration]| {
        samples
            .iter()
            .map(|sample| milliseconds(*sample))
            .collect::<Vec<_>>()
    };
    let summary = |samples: &[Duration]| {
        serde_json::json!({
            "samples_ms": sample_array(samples),
            "median_ms": milliseconds(median(samples)),
            "p95_ms": milliseconds(p95(samples)),
        })
    };
    let report = serde_json::json!({
        "benchmark": "quill-rust-parser-comparison",
        "ra_ap_syntax": "0.0.358",
        "syn": "3.0.9",
        "arch": std::env::consts::ARCH,
        "workload_bytes": source.len(),
        "workload_lines": source.matches('\n').count() + 1,
        "rounds": ROUNDS,
        "measurements": {
            "ra_full_parse": summary(&ra_full),
            "syn_full_parse": summary(&syn_full),
            "ra_incremental_edit": summary(&ra_incremental),
            "syn_full_parse_after_edit": summary(&syn_after_edit),
        },
        "ratios": {
            "ra_full_over_syn": median(&ra_full).as_secs_f64() / median(&syn_full).as_secs_f64(),
            "syn_full_over_ra_incremental": median(&syn_after_edit).as_secs_f64() / median(&ra_incremental).as_secs_f64(),
        }
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("serialize benchmark results")
    );
}
