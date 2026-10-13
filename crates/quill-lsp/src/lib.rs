use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::str::FromStr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use ra_ap_syntax::{Edition, SourceFile, TextRange, TextSize};
use serde_json::{Value, json};

const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

struct Document {
    text: String,
    line_starts: Vec<usize>,
    version: i64,
    edition: Edition,
    parsed: ra_ap_syntax::Parse<SourceFile>,
    symbols: Option<Vec<Symbol>>,
}

struct DiagnosticJob {
    uri: String,
    version: i64,
    text: String,
    line_starts: Vec<usize>,
    parsed: ra_ap_syntax::Parse<SourceFile>,
}

struct DiagnosticResult {
    uri: String,
    version: i64,
    params: Value,
}

#[derive(Clone)]
struct Symbol {
    name: String,
    kind: u32,
    range: TextRange,
    selection: TextRange,
}

impl Document {
    fn new(text: String, version: i64, edition: Edition) -> Self {
        let parsed = SourceFile::parse(&text, edition);
        let line_starts = line_starts(&text);
        Self {
            text,
            line_starts,
            version,
            edition,
            parsed,
            symbols: None,
        }
    }

    fn apply_change(&mut self, change: &Value) {
        let replacement = change
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(range) = change.get("range") else {
            self.text.clear();
            self.text.push_str(replacement);
            self.parsed = SourceFile::parse(&self.text, self.edition);
            self.line_starts = line_starts(&self.text);
            self.symbols = None;
            return;
        };

        let start = position_to_offset(
            &self.text,
            &self.line_starts,
            range.get("start").unwrap_or(&Value::Null),
        );
        let end = position_to_offset(
            &self.text,
            &self.line_starts,
            range.get("end").unwrap_or(&Value::Null),
        );
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let deleted = TextRange::new(TextSize::from(start as u32), TextSize::from(end as u32));
        self.parsed = self.parsed.reparse(deleted, replacement, self.edition);
        self.text.replace_range(start..end, replacement);
        self.line_starts = line_starts(&self.text);
        self.symbols = None;
    }
}

pub fn run_stdio() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut output = BufWriter::new(stdout.lock());
    let (input_tx, input_rx) = mpsc::channel::<io::Result<Option<Value>>>();
    thread::spawn(move || {
        let mut input = BufReader::new(stdin.lock());
        loop {
            let message = read_message(&mut input);
            let finished = !matches!(&message, Ok(Some(_)));
            if input_tx.send(message).is_err() || finished {
                break;
            }
        }
    });

    let (diagnostic_job_tx, diagnostic_job_rx) = mpsc::channel::<DiagnosticJob>();
    let (diagnostic_result_tx, diagnostic_result_rx) = mpsc::channel::<DiagnosticResult>();
    start_diagnostic_worker(diagnostic_job_rx, diagnostic_result_tx);

    let mut documents = HashMap::<String, Document>::new();
    let mut running = true;

    while running {
        let message = match input_rx.recv_timeout(Duration::from_millis(5)) {
            Ok(Ok(Some(message))) => message,
            Ok(Ok(None)) => break,
            Ok(Err(error)) => return Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                publish_ready_diagnostics(&mut output, &documents, &diagnostic_result_rx)?;
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            continue;
        };
        let id = message.get("id").cloned();
        let params = message.get("params").unwrap_or(&Value::Null);
        let mut response = None;

        match method {
            "initialize" => {
                response = Some(json!({
                    "capabilities": {
                        "textDocumentSync": { "openClose": true, "change": 2 },
                        "documentSymbolProvider": true,
                        "completionProvider": { "resolveProvider": false },
                        "definitionProvider": true,
                        "hoverProvider": true,
                    },
                    "serverInfo": { "name": "quill-check", "version": env!("CARGO_PKG_VERSION") },
                }));
            }
            "shutdown" => response = Some(Value::Null),
            "exit" => running = false,
            "textDocument/didOpen" => {
                if let Some(document) = params.get("textDocument") {
                    let uri = document
                        .get("uri")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let text = document
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let version = document.get("version").and_then(Value::as_i64).unwrap_or(0);
                    let edition = edition_for_uri(uri);
                    let state = Document::new(text.to_owned(), version, edition);
                    let _ = diagnostic_job_tx.send(state.diagnostic_job(uri));
                    documents.insert(uri.to_owned(), state);
                }
            }
            "textDocument/didChange" => {
                if let Some(document) = params.get("textDocument") {
                    let uri = document
                        .get("uri")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if let Some(state) = documents.get_mut(uri) {
                        let version = document
                            .get("version")
                            .and_then(Value::as_i64)
                            .unwrap_or(state.version);
                        if version < state.version {
                            continue;
                        }
                        if let Some(changes) =
                            params.get("contentChanges").and_then(Value::as_array)
                        {
                            for change in changes {
                                state.apply_change(change);
                            }
                        }
                        state.version = version;
                        let _ = diagnostic_job_tx.send(state.diagnostic_job(uri));
                    }
                }
            }
            "textDocument/didClose" => {
                if let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) {
                    documents.remove(uri);
                    send_notification(
                        &mut output,
                        "textDocument/publishDiagnostics",
                        json!({
                            "uri": uri,
                            "diagnostics": [],
                        }),
                    )?;
                }
            }
            "textDocument/documentSymbol" => {
                let symbols = params
                    .pointer("/textDocument/uri")
                    .and_then(Value::as_str)
                    .and_then(|uri| documents.get_mut(uri))
                    .map(document_symbols)
                    .unwrap_or_default();
                response = Some(Value::Array(symbols));
            }
            "textDocument/completion" => {
                response = Some(completions(params, &mut documents));
            }
            "textDocument/definition" => {
                response = Some(definition(params, &mut documents));
            }
            "textDocument/hover" => {
                response = Some(hover(params, &mut documents));
            }
            "$/cancelRequest" | "initialized" | "workspace/didChangeConfiguration" => {}
            _ if id.is_some() => {
                send_error(
                    &mut output,
                    id.as_ref().unwrap(),
                    -32601,
                    "Method not implemented",
                )?;
            }
            _ => {}
        }

        if let (Some(id), Some(result)) = (id, response) {
            send_message(
                &mut output,
                &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            )?;
        }
        publish_ready_diagnostics(&mut output, &documents, &diagnostic_result_rx)?;
    }
    Ok(())
}

impl Document {
    fn diagnostic_job(&self, uri: &str) -> DiagnosticJob {
        DiagnosticJob {
            uri: uri.to_owned(),
            version: self.version,
            text: self.text.clone(),
            line_starts: self.line_starts.clone(),
            parsed: self.parsed.clone(),
        }
    }
}

impl DiagnosticJob {
    fn run(self) -> Value {
        let diagnostics = self
            .parsed
            .errors()
            .into_iter()
            .map(|error| {
                let range = error.range();
                let start = u32::from(range.start()) as usize;
                let end = u32::from(range.end()) as usize;
                json!({
                    "range": {
                        "start": offset_to_position(&self.text, &self.line_starts, start),
                        "end": offset_to_position(&self.text, &self.line_starts, end),
                    },
                    "severity": 1,
                    "source": "quill",
                    "message": error.to_string(),
                })
            })
            .collect::<Vec<_>>();
        json!({ "uri": self.uri, "version": self.version, "diagnostics": diagnostics })
    }
}

fn start_diagnostic_worker(jobs: Receiver<DiagnosticJob>, results: Sender<DiagnosticResult>) {
    thread::spawn(move || {
        let mut pending = HashMap::<String, DiagnosticJob>::new();
        while let Ok(first) = jobs.recv() {
            pending.insert(first.uri.clone(), first);
            while let Ok(job) = jobs.try_recv() {
                pending.insert(job.uri.clone(), job);
            }
            let Some(uri) = pending.keys().next().cloned() else {
                continue;
            };
            let Some(job) = pending.remove(&uri) else {
                continue;
            };
            let version = job.version;
            let params = job.run();
            if results
                .send(DiagnosticResult {
                    uri,
                    version,
                    params,
                })
                .is_err()
            {
                break;
            }
        }
    });
}

fn publish_ready_diagnostics(
    output: &mut impl Write,
    documents: &HashMap<String, Document>,
    results: &Receiver<DiagnosticResult>,
) -> io::Result<()> {
    while let Ok(result) = results.try_recv() {
        let current = documents
            .get(&result.uri)
            .is_some_and(|document| document.version == result.version);
        if current {
            send_notification(output, "textDocument/publishDiagnostics", result.params)?;
        }
    }
    Ok(())
}

fn document_symbols(document: &mut Document) -> Vec<Value> {
    let symbols = document_symbols_index(document).to_vec();
    symbols
        .iter()
        .map(|symbol| {
            json!({
                "name": symbol.name,
                "kind": symbol.kind,
                "range": text_range_to_lsp(&document.text, &document.line_starts, symbol.range),
                "selectionRange": text_range_to_lsp(
                    &document.text,
                    &document.line_starts,
                    symbol.selection,
                ),
            })
        })
        .collect()
}

fn document_symbols_index(document: &mut Document) -> &[Symbol] {
    if document.symbols.is_none() {
        document.symbols = Some(collect_symbols(&document.parsed));
    }
    document.symbols.as_deref().unwrap_or_default()
}

fn collect_symbols(parsed: &ra_ap_syntax::Parse<SourceFile>) -> Vec<Symbol> {
    use ra_ap_syntax::ast::HasModuleItem;

    let mut symbols = Vec::new();
    collect_items(parsed.tree().items(), &mut symbols);
    symbols
}

fn collect_items(items: impl Iterator<Item = ra_ap_syntax::ast::Item>, symbols: &mut Vec<Symbol>) {
    use ra_ap_syntax::ast::{self, HasModuleItem};

    for item in items {
        match item {
            ast::Item::Fn(item) => push_named(&item, 12, symbols),
            ast::Item::Struct(item) => {
                push_named(&item, 23, symbols);
                collect_fields(item.field_list(), symbols);
            }
            ast::Item::Union(item) => {
                push_named(&item, 23, symbols);
                collect_record_fields(item.record_field_list(), symbols);
            }
            ast::Item::Enum(item) => {
                push_named(&item, 10, symbols);
                if let Some(variants) = item.variant_list() {
                    for variant in variants.variants() {
                        push_named(&variant, 22, symbols);
                        collect_fields(variant.field_list(), symbols);
                    }
                }
            }
            ast::Item::Trait(item) => {
                push_named(&item, 11, symbols);
                if let Some(items) = item.assoc_item_list() {
                    collect_assoc_items(items.assoc_items(), symbols);
                }
            }
            ast::Item::Module(item) => {
                push_named(&item, 2, symbols);
                if let Some(items) = item.item_list() {
                    collect_items(items.items(), symbols);
                }
            }
            ast::Item::TypeAlias(item) => push_named(&item, 5, symbols),
            ast::Item::Const(item) => push_named(&item, 14, symbols),
            ast::Item::Static(item) => push_named(&item, 13, symbols),
            ast::Item::MacroRules(item) => push_named(&item, 12, symbols),
            ast::Item::MacroDef(item) => push_named(&item, 12, symbols),
            ast::Item::Impl(item) => {
                if let Some(items) = item.assoc_item_list() {
                    collect_assoc_items(items.assoc_items(), symbols);
                }
            }
            ast::Item::ExternBlock(item) => {
                if let Some(items) = item.extern_item_list() {
                    for item in items.extern_items() {
                        match item {
                            ast::ExternItem::Fn(item) => push_named(&item, 12, symbols),
                            ast::ExternItem::Static(item) => push_named(&item, 13, symbols),
                            ast::ExternItem::TypeAlias(item) => push_named(&item, 5, symbols),
                            ast::ExternItem::MacroCall(_) => {}
                        }
                    }
                }
            }
            ast::Item::AsmExpr(_)
            | ast::Item::ExternCrate(_)
            | ast::Item::MacroCall(_)
            | ast::Item::Use(_) => {}
        }
    }
}

fn collect_assoc_items(
    items: impl Iterator<Item = ra_ap_syntax::ast::AssocItem>,
    symbols: &mut Vec<Symbol>,
) {
    use ra_ap_syntax::ast;

    for item in items {
        match item {
            ast::AssocItem::Const(item) => push_named(&item, 14, symbols),
            ast::AssocItem::Fn(item) => push_named(&item, 12, symbols),
            ast::AssocItem::TypeAlias(item) => push_named(&item, 5, symbols),
            ast::AssocItem::MacroCall(_) => {}
        }
    }
}

fn collect_fields(fields: Option<ra_ap_syntax::ast::FieldList>, symbols: &mut Vec<Symbol>) {
    use ra_ap_syntax::ast::FieldList;

    if let Some(FieldList::RecordFieldList(fields)) = fields {
        for field in fields.fields() {
            push_named(&field, 8, symbols);
        }
    }
}

fn collect_record_fields(
    fields: Option<ra_ap_syntax::ast::RecordFieldList>,
    symbols: &mut Vec<Symbol>,
) {
    if let Some(fields) = fields {
        for field in fields.fields() {
            push_named(&field, 8, symbols);
        }
    }
}

fn push_named<N>(item: &N, kind: u32, symbols: &mut Vec<Symbol>)
where
    N: ra_ap_syntax::AstNode + ra_ap_syntax::ast::HasName,
{
    use ra_ap_syntax::AstNode;

    let Some(name) = item.name() else { return };
    let name_range = name.syntax().text_range();
    symbols.push(Symbol {
        name: name.text().to_string(),
        kind,
        range: item.syntax().text_range(),
        selection: name_range,
    });
}

fn completions(params: &Value, documents: &mut HashMap<String, Document>) -> Value {
    let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
        return json!({ "isIncomplete": true, "items": [] });
    };
    let Some(document) = documents.get(uri) else {
        return json!({ "isIncomplete": true, "items": [] });
    };
    let prefix = identifier_prefix(
        &document.text,
        &document.line_starts,
        params.pointer("/position").unwrap_or(&Value::Null),
    );
    let mut candidates = HashMap::<String, (u32, String)>::new();
    for candidate in [
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
        "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait",
        "true", "type", "unsafe", "use", "where", "while",
    ] {
        candidates.insert(candidate.to_owned(), (14, "Rust keyword".to_owned()));
    }
    for (candidate_uri, candidate_doc) in documents.iter_mut() {
        for symbol in document_symbols_index(candidate_doc).to_vec() {
            candidates
                .entry(symbol.name.clone())
                .or_insert_with(|| (symbol.kind, candidate_uri.clone()));
        }
    }
    let mut names = candidates.into_iter().collect::<Vec<_>>();
    names.sort_by(|left, right| left.0.cmp(&right.0));
    let items = names
        .into_iter()
        .filter(|(name, _)| name.starts_with(&prefix) && name != &prefix)
        .map(|(name, (kind, detail))| {
            json!({ "label": name, "kind": kind, "detail": detail, "insertText": name })
        })
        .collect::<Vec<_>>();
    json!({ "isIncomplete": false, "items": items })
}

fn definition(params: &Value, documents: &mut HashMap<String, Document>) -> Value {
    let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
        return Value::Null;
    };
    let Some(document) = documents.get(uri) else {
        return Value::Null;
    };
    let name = identifier_at(
        &document.text,
        &document.line_starts,
        params.pointer("/position").unwrap_or(&Value::Null),
    );
    let Some(name) = name else { return Value::Null };
    let mut locations = Vec::new();
    let mut uris = documents.keys().cloned().collect::<Vec<_>>();
    uris.sort();
    uris.sort_by_key(|candidate| candidate != uri);
    for candidate_uri in uris {
        let Some(candidate_doc) = documents.get_mut(&candidate_uri) else {
            continue;
        };
        for symbol in document_symbols_index(candidate_doc).to_vec() {
            if symbol.name == name {
                locations.push(json!({
                    "uri": candidate_uri,
                    "range": text_range_to_lsp(
                        &candidate_doc.text,
                        &candidate_doc.line_starts,
                        symbol.selection,
                    ),
                }));
            }
        }
    }
    if locations.is_empty() {
        Value::Null
    } else {
        Value::Array(locations)
    }
}

fn hover(params: &Value, documents: &mut HashMap<String, Document>) -> Value {
    let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
        return Value::Null;
    };
    let Some(document) = documents.get(uri) else {
        return Value::Null;
    };
    let name = identifier_at(
        &document.text,
        &document.line_starts,
        params.pointer("/position").unwrap_or(&Value::Null),
    );
    let Some(name) = name else { return Value::Null };
    let mut uris = documents.keys().cloned().collect::<Vec<_>>();
    uris.sort();
    uris.sort_by_key(|candidate| candidate != uri);
    for candidate_uri in uris {
        let Some(candidate_doc) = documents.get_mut(&candidate_uri) else {
            continue;
        };
        for symbol in document_symbols_index(candidate_doc).to_vec() {
            if symbol.name == name {
                return json!({ "contents": {
                    "kind": "markdown",
                    "value": format!("`{}` **{}**\\n\\nDefined in `{}`", symbol_kind_name(symbol.kind), symbol.name, candidate_uri),
                }});
            }
        }
    }
    Value::Null
}

fn symbol_kind_name(kind: u32) -> &'static str {
    match kind {
        2 => "module",
        5 => "type",
        8 => "field",
        10 => "enum",
        11 => "trait",
        12 => "function",
        13 => "static",
        14 => "constant",
        22 => "enum variant",
        23 => "struct",
        _ => "symbol",
    }
}

fn identifier_prefix(text: &str, line_starts: &[usize], position: &Value) -> String {
    let cursor = position_to_offset(text, line_starts, position);
    let Some(line_start) = text[..cursor]
        .rfind('\n')
        .map_or(Some(0), |offset| Some(offset + 1))
    else {
        return String::new();
    };
    let start = text[line_start..cursor]
        .char_indices()
        .rev()
        .take_while(|(_, character)| is_identifier_continue(*character))
        .last()
        .map(|(relative, _)| line_start + relative)
        .unwrap_or(cursor);
    text[start..cursor].to_owned()
}

fn identifier_at(text: &str, line_starts: &[usize], position: &Value) -> Option<String> {
    let cursor = position_to_offset(text, line_starts, position);
    let before = &text[..cursor];
    let after = &text[cursor..];
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, character)| is_identifier_continue(*character))
        .last()
        .map(|(offset, _)| offset)
        .unwrap_or(cursor);
    let end = after
        .char_indices()
        .take_while(|(_, character)| is_identifier_continue(*character))
        .last()
        .map(|(offset, character)| cursor + offset + character.len_utf8())
        .unwrap_or(cursor);
    if start == end {
        None
    } else {
        Some(text[start..end].to_owned())
    }
}

fn is_identifier_continue(character: char) -> bool {
    character == '_' || unicode_ident::is_xid_continue(character)
}

fn text_range_to_lsp(text: &str, line_starts: &[usize], range: TextRange) -> Value {
    json!({
        "start": offset_to_position(text, line_starts, u32::from(range.start()) as usize),
        "end": offset_to_position(text, line_starts, u32::from(range.end()) as usize),
    })
}

fn edition_for_uri(uri: &str) -> Edition {
    let Ok(url) = url::Url::parse(uri) else {
        return Edition::CURRENT;
    };
    let Ok(mut path) = url.to_file_path() else {
        return Edition::CURRENT;
    };
    if path.is_file() {
        path.pop();
    }
    for directory in path.ancestors() {
        let manifest = directory.join("Cargo.toml");
        let Ok(contents) = std::fs::read_to_string(manifest) else {
            continue;
        };
        let Ok(value) = contents.parse::<toml::Value>() else {
            continue;
        };
        if let Some(edition) = value
            .get("package")
            .and_then(|p| p.get("edition"))
            .and_then(toml::Value::as_str)
        {
            if let Ok(edition) = Edition::from_str(edition) {
                return edition;
            }
        }
        if let Some(edition) = value
            .get("workspace")
            .and_then(|w| w.get("package"))
            .and_then(|p| p.get("edition"))
            .and_then(toml::Value::as_str)
        {
            if let Ok(edition) = Edition::from_str(edition) {
                return edition;
            }
        }
    }
    Edition::CURRENT
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = Vec::with_capacity(text.bytes().filter(|byte| *byte == b'\n').count() + 1);
    starts.push(0);
    starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
    starts
}

fn position_to_offset(text: &str, line_starts: &[usize], position: &Value) -> usize {
    let line = position.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;
    let character = position
        .get("character")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let Some(&line_start) = line_starts.get(line) else {
        return text.len();
    };
    let line_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |i| line_start + i);
    let content_end = if line_end > line_start && text.as_bytes()[line_end - 1] == b'\r' {
        line_end - 1
    } else {
        line_end
    };
    let mut units = 0;
    for (relative, character_value) in text[line_start..content_end].char_indices() {
        if units >= character {
            return line_start + relative;
        }
        let next = units + character_value.len_utf16();
        if character < next {
            return line_start + relative + character_value.len_utf8();
        }
        units = next;
    }
    content_end
}

fn offset_to_position(text: &str, line_starts: &[usize], offset: usize) -> Value {
    let offset = offset.min(text.len());
    let line = line_starts
        .partition_point(|start| *start <= offset)
        .saturating_sub(1);
    let line_start = line_starts.get(line).copied().unwrap_or(0);
    let line_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |relative| line_start + relative);
    let line_end = if line_end > line_start && text.as_bytes()[line_end - 1] == b'\r' {
        line_end - 1
    } else {
        line_end
    };
    let offset = offset.min(line_end);
    let character = text[line_start..offset].encode_utf16().count();
    json!({ "line": line, "character": character })
}

fn read_message(input: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }
    let length = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
    })?;
    if length > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LSP message exceeds size limit",
        ));
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    let message = serde_json::from_slice(&body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(Some(message))
}

fn send_notification(output: &mut impl Write, method: &str, params: Value) -> io::Result<()> {
    send_message(
        output,
        &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
    )
}

fn send_error(output: &mut impl Write, id: &Value, code: i64, message: &str) -> io::Result<()> {
    send_message(
        output,
        &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    )
}

fn send_message(output: &mut impl Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    write!(output, "Content-Length: {}\r\n\r\n", body.len())?;
    output.write_all(&body)?;
    output.flush()
}
