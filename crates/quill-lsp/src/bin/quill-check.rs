fn main() {
    if let Err(error) = quill_lsp::run_stdio() {
        eprintln!("quill-check: {error}");
        std::process::exit(1);
    }
}
