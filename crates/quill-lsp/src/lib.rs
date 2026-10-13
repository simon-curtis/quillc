use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::str::FromStr;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};

use ra_ap_syntax::{Edition, SourceFile, TextRange, TextSize};
use serde_json::{Value, json};

const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

struct Document {
    text: String,
    line_starts: Vec<usize>,
    version: i64,
    generation: u64,
    parsed: ra_ap_syntax::Parse<SourceFile>,
    symbols: Option<Vec<Symbol>>,
    symbol_response: Option<Vec<Value>>,
}

struct LiveDocument {
    text: String,
    line_starts: Vec<usize>,
    version: i64,
    generation: u64,
    edition: Edition,
    edition_warning: Option<String>,
}

struct ParseJob {
    uri: String,
    version: i64,
    generation: u64,
    text: String,
    line_starts: Vec<usize>,
    edition: Edition,
    edition_warning: Option<String>,
}

struct EditionResolution {
    edition: Edition,
    warning: Option<String>,
}

struct AnalysisResult {
    uri: String,
    version: i64,
    generation: u64,
    text: String,
    line_starts: Vec<usize>,
    params: Value,
    parsed: ra_ap_syntax::Parse<SourceFile>,
}

struct DeferredRequest {
    id: Value,
    method: String,
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
    fn from_result(result: AnalysisResult) -> Self {
        Self {
            text: result.text,
            line_starts: result.line_starts,
            version: result.version,
            generation: result.generation,
            parsed: result.parsed,
            symbols: None,
            symbol_response: None,
        }
    }
}

impl LiveDocument {
    fn new(text: String, version: i64, resolution: EditionResolution, generation: u64) -> Self {
        let line_starts = line_starts(&text);
        Self {
            text,
            line_starts,
            version,
            generation,
            edition: resolution.edition,
            edition_warning: resolution.warning,
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
            self.line_starts = line_starts(&self.text);
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
        update_line_starts(&mut self.line_starts, start, end, replacement);
        self.text.replace_range(start..end, replacement);
    }
}

pub fn run_stdio() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut output = BufWriter::new(stdout.lock());
    let (input_tx, input_rx) = unbounded::<io::Result<Option<Value>>>();
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

    let (analysis_result_tx, analysis_result_rx) = unbounded::<AnalysisResult>();
    let pending_analysis = Arc::new((Mutex::new(HashMap::new()), Condvar::new()));
    start_analysis_worker(pending_analysis.clone(), analysis_result_tx);

    let mut documents = HashMap::<String, Document>::new();
    let mut live_documents = HashMap::<String, LiveDocument>::new();
    let mut deferred_requests = Vec::<DeferredRequest>::new();
    let mut next_generation = 1u64;
    let mut running = true;

    while running {
        let message = crossbeam_channel::select_biased! {
            recv(input_rx) -> incoming => match incoming {
                Ok(Ok(Some(message))) => message,
                Ok(Ok(None)) | Err(_) => break,
                Ok(Err(error)) => return Err(error),
            },
            recv(analysis_result_rx) -> result => {
                let Ok(result) = result else { break };
                publish_analysis_result(
                    &mut output,
                    &mut documents,
                    &live_documents,
                    result,
                )?;
                publish_ready_analysis(
                    &mut output,
                    &mut documents,
                    &live_documents,
                    &analysis_result_rx,
                )?;
                respond_ready_requests(
                    &mut output,
                    &mut deferred_requests,
                    &mut documents,
                    &live_documents,
                )?;
                continue;
            }
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
                    let state =
                        LiveDocument::new(text.to_owned(), version, edition, next_generation);
                    next_generation = next_generation.wrapping_add(1).max(1);
                    queue_parse_job(&pending_analysis, state.parse_job(uri));
                    live_documents.insert(uri.to_owned(), state);
                }
            }
            "textDocument/didChange" => {
                if let Some(document) = params.get("textDocument") {
                    let uri = document
                        .get("uri")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if let Some(state) = live_documents.get_mut(uri) {
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
                        queue_parse_job(&pending_analysis, state.parse_job(uri));
                    }
                }
            }
            "textDocument/didClose" => {
                if let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) {
                    documents.remove(uri);
                    live_documents.remove(uri);
                    queue_close_job(&pending_analysis, uri);
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
                if request_analysis_is_current(params, &documents, &live_documents) {
                    let symbols = params
                        .pointer("/textDocument/uri")
                        .and_then(Value::as_str)
                        .and_then(|uri| documents.get_mut(uri))
                        .map(document_symbols)
                        .unwrap_or_default();
                    response = Some(Value::Array(symbols));
                } else if let Some(id) = id.as_ref() {
                    deferred_requests.push(DeferredRequest {
                        id: id.clone(),
                        method: method.to_owned(),
                        params: params.clone(),
                    });
                }
            }
            "textDocument/completion" => {
                if request_analysis_is_current(params, &documents, &live_documents) {
                    let live = params
                        .pointer("/textDocument/uri")
                        .and_then(Value::as_str)
                        .and_then(|uri| live_documents.get(uri));
                    response = Some(completions(params, &mut documents, live, false));
                } else if let Some(live) = params
                    .pointer("/textDocument/uri")
                    .and_then(Value::as_str)
                    .and_then(|uri| {
                        documents
                            .contains_key(uri)
                            .then(|| live_documents.get(uri))
                            .flatten()
                    })
                {
                    // Reuse the last completed symbol index while deriving the prefix from live
                    // text. Mark this provisional list incomplete so the client can refresh it.
                    response = Some(completions(params, &mut documents, Some(live), true));
                } else if let Some(id) = id.as_ref() {
                    deferred_requests.push(DeferredRequest {
                        id: id.clone(),
                        method: method.to_owned(),
                        params: params.clone(),
                    });
                }
            }
            "textDocument/definition" => {
                if request_analysis_is_current(params, &documents, &live_documents) {
                    response = Some(definition(params, &mut documents));
                } else if let Some(id) = id.as_ref() {
                    deferred_requests.push(DeferredRequest {
                        id: id.clone(),
                        method: method.to_owned(),
                        params: params.clone(),
                    });
                }
            }
            "textDocument/hover" => {
                if request_analysis_is_current(params, &documents, &live_documents) {
                    response = Some(hover(params, &mut documents));
                } else if let Some(id) = id.as_ref() {
                    deferred_requests.push(DeferredRequest {
                        id: id.clone(),
                        method: method.to_owned(),
                        params: params.clone(),
                    });
                }
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
        respond_ready_requests(
            &mut output,
            &mut deferred_requests,
            &mut documents,
            &live_documents,
        )?;
    }
    Ok(())
}

impl LiveDocument {
    fn parse_job(&self, uri: &str) -> ParseJob {
        ParseJob {
            uri: uri.to_owned(),
            version: self.version,
            generation: self.generation,
            text: self.text.clone(),
            line_starts: self.line_starts.clone(),
            edition: self.edition,
            edition_warning: self.edition_warning.clone(),
        }
    }
}

impl ParseJob {
    fn run(
        self,
        previous: Option<(u64, String, ra_ap_syntax::Parse<SourceFile>)>,
    ) -> (AnalysisResult, String, ra_ap_syntax::Parse<SourceFile>) {
        let parsed = match previous {
            Some((generation, old_text, old_parse)) if generation == self.generation => {
                reparse_latest(&old_text, &self.text, old_parse, self.edition)
            }
            _ => SourceFile::parse(&self.text, self.edition),
        };
        let mut syntax_errors = parsed.errors();
        syntax_errors.sort_by_key(|error| {
            let range = error.range();
            (
                u32::from(range.start()),
                u32::from(range.end()),
                error.to_string(),
            )
        });
        let mut diagnostics = syntax_errors
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
        if let Some(warning) = self.edition_warning {
            diagnostics.push(json!({
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
                "severity": 2,
                "source": "quill",
                "message": warning,
            }));
        }
        let uri = self.uri;
        let version = self.version;
        let generation = self.generation;
        let params = json!({ "uri": uri.clone(), "version": version, "diagnostics": diagnostics });
        let result = AnalysisResult {
            uri,
            version,
            generation,
            text: self.text.clone(),
            line_starts: self.line_starts.clone(),
            params,
            parsed: parsed.clone(),
        };
        (result, self.text, parsed)
    }
}

fn reparse_latest(
    old_text: &str,
    new_text: &str,
    old_parse: ra_ap_syntax::Parse<SourceFile>,
    edition: Edition,
) -> ra_ap_syntax::Parse<SourceFile> {
    let mut prefix = old_text
        .bytes()
        .zip(new_text.bytes())
        .take_while(|(old, new)| old == new)
        .count();
    while !old_text.is_char_boundary(prefix) || !new_text.is_char_boundary(prefix) {
        prefix -= 1;
    }

    let max_suffix = old_text.len().min(new_text.len()) - prefix;
    let mut suffix = old_text
        .bytes()
        .rev()
        .zip(new_text.bytes().rev())
        .take(max_suffix)
        .take_while(|(old, new)| old == new)
        .count();
    while !old_text.is_char_boundary(old_text.len() - suffix)
        || !new_text.is_char_boundary(new_text.len() - suffix)
    {
        suffix -= 1;
    }

    let old_end = old_text.len() - suffix;
    let new_end = new_text.len() - suffix;
    old_parse.reparse(
        TextRange::new(
            TextSize::from(prefix as u32),
            TextSize::from(old_end as u32),
        ),
        &new_text[prefix..new_end],
        edition,
    )
}

type PendingAnalysis = Arc<(Mutex<HashMap<String, Option<ParseJob>>>, Condvar)>;

fn queue_parse_job(pending: &PendingAnalysis, job: ParseJob) {
    let (jobs, wake) = &**pending;
    if let Ok(mut jobs) = jobs.lock() {
        jobs.insert(job.uri.clone(), Some(job));
        wake.notify_one();
    }
}

fn queue_close_job(pending: &PendingAnalysis, uri: &str) {
    let (jobs, wake) = &**pending;
    if let Ok(mut jobs) = jobs.lock() {
        jobs.insert(uri.to_owned(), None);
        wake.notify_one();
    }
}

fn start_analysis_worker(pending: PendingAnalysis, results: Sender<AnalysisResult>) {
    thread::spawn(move || {
        let mut worker_documents =
            HashMap::<String, (u64, String, ra_ap_syntax::Parse<SourceFile>)>::new();
        loop {
            let (jobs, wake) = &*pending;
            let mut jobs = match jobs.lock() {
                Ok(jobs) => jobs,
                Err(_) => break,
            };
            while jobs.is_empty() {
                jobs = match wake.wait(jobs) {
                    Ok(jobs) => jobs,
                    Err(_) => return,
                };
            }
            let Some(uri) = jobs.keys().next().cloned() else {
                continue;
            };
            let Some(job) = jobs.remove(&uri) else {
                continue;
            };
            drop(jobs);
            let Some(job) = job else {
                worker_documents.remove(&uri);
                continue;
            };
            let previous = worker_documents
                .get(&uri)
                .map(|(generation, text, parsed)| (*generation, text.clone(), parsed.clone()));
            let (result, text, parsed) = job.run(previous);
            worker_documents.insert(uri.clone(), (result.generation, text, parsed));
            if result.uri != uri || results.send(result).is_err() {
                break;
            }
        }
    });
}

fn publish_ready_analysis(
    output: &mut impl Write,
    documents: &mut HashMap<String, Document>,
    live_documents: &HashMap<String, LiveDocument>,
    results: &Receiver<AnalysisResult>,
) -> io::Result<()> {
    while let Ok(result) = results.try_recv() {
        publish_analysis_result(output, documents, live_documents, result)?;
    }
    Ok(())
}

fn publish_analysis_result(
    output: &mut impl Write,
    documents: &mut HashMap<String, Document>,
    live_documents: &HashMap<String, LiveDocument>,
    result: AnalysisResult,
) -> io::Result<()> {
    let current = live_documents.get(&result.uri).is_some_and(|document| {
        document.version == result.version && document.generation == result.generation
    });
    if current {
        let uri = result.uri.clone();
        let params = result.params.clone();
        let document = Document::from_result(result);
        documents.insert(uri, document);
        send_notification(output, "textDocument/publishDiagnostics", params)?;
    }
    Ok(())
}

fn analysis_is_current(
    uri: &str,
    documents: &HashMap<String, Document>,
    live_documents: &HashMap<String, LiveDocument>,
) -> bool {
    documents
        .get(uri)
        .zip(live_documents.get(uri))
        .is_some_and(|(analysis, live)| {
            analysis.version == live.version && analysis.generation == live.generation
        })
}

fn request_analysis_is_current(
    params: &Value,
    documents: &HashMap<String, Document>,
    live_documents: &HashMap<String, LiveDocument>,
) -> bool {
    params
        .pointer("/textDocument/uri")
        .and_then(Value::as_str)
        .is_some_and(|uri| analysis_is_current(uri, documents, live_documents))
}

fn respond_ready_requests(
    output: &mut impl Write,
    pending: &mut Vec<DeferredRequest>,
    documents: &mut HashMap<String, Document>,
    live_documents: &HashMap<String, LiveDocument>,
) -> io::Result<()> {
    let mut waiting = Vec::new();
    for request in pending.drain(..) {
        let uri = request
            .params
            .pointer("/textDocument/uri")
            .and_then(Value::as_str);
        let Some(uri) = uri else {
            send_error(output, &request.id, -32602, "Missing text document URI")?;
            continue;
        };
        if live_documents.contains_key(uri) && !analysis_is_current(uri, documents, live_documents)
        {
            waiting.push(request);
            continue;
        }
        let result = match request.method.as_str() {
            "textDocument/documentSymbol" => Value::Array(
                documents
                    .get_mut(uri)
                    .map(document_symbols)
                    .unwrap_or_default(),
            ),
            "textDocument/completion" => {
                completions(&request.params, documents, live_documents.get(uri), false)
            }
            "textDocument/definition" => definition(&request.params, documents),
            "textDocument/hover" => hover(&request.params, documents),
            _ => Value::Null,
        };
        send_message(
            output,
            &json!({ "jsonrpc": "2.0", "id": request.id, "result": result }),
        )?;
    }
    *pending = waiting;
    Ok(())
}

fn document_symbols(document: &mut Document) -> Vec<Value> {
    if document.symbol_response.is_none() {
        let symbols = document_symbols_index(document).to_vec();
        document.symbol_response = Some(
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
                .collect(),
        );
    }
    document.symbol_response.clone().unwrap_or_default()
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

fn completions(
    params: &Value,
    documents: &mut HashMap<String, Document>,
    live_document: Option<&LiveDocument>,
    is_incomplete: bool,
) -> Value {
    let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
        return json!({ "isIncomplete": true, "items": [] });
    };
    let Some(document) = documents.get(uri) else {
        return json!({ "isIncomplete": true, "items": [] });
    };
    let (text, line_starts) = live_document
        .map(|live| (live.text.as_str(), live.line_starts.as_slice()))
        .unwrap_or((&document.text, &document.line_starts));
    let prefix = identifier_prefix(
        text,
        line_starts,
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
    json!({ "isIncomplete": is_incomplete, "items": items })
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

fn edition_for_uri(uri: &str) -> EditionResolution {
    let Ok(url) = url::Url::parse(uri) else {
        return EditionResolution {
            edition: Edition::CURRENT,
            warning: None,
        };
    };
    let Ok(mut path) = url.to_file_path() else {
        return EditionResolution {
            edition: Edition::CURRENT,
            warning: None,
        };
    };
    if path.is_file() {
        path.pop();
    }
    let mut inherited_edition_missing = false;
    for directory in path.ancestors() {
        let manifest = directory.join("Cargo.toml");
        let Ok(contents) = std::fs::read_to_string(manifest) else {
            continue;
        };
        let value = match toml::from_str::<toml::Value>(&contents) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(package) = value.get("package") {
            if let Some(edition) = package.get("edition") {
                if edition.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                    inherited_edition_missing = true;
                    if let Some(workspace_edition) = value
                        .get("workspace")
                        .and_then(|workspace| workspace.get("package"))
                        .and_then(|package| package.get("edition"))
                    {
                        return edition_resolution(workspace_edition);
                    }
                    continue;
                }
                return edition_resolution(edition);
            }
            return EditionResolution {
                edition: Edition::Edition2015,
                warning: None,
            };
        }
        if let Some(edition) = value
            .get("workspace")
            .and_then(|w| w.get("package"))
            .and_then(|p| p.get("edition"))
        {
            return edition_resolution(edition);
        }
    }
    if inherited_edition_missing {
        return EditionResolution {
            edition: Edition::CURRENT,
            warning: Some(format!(
                "The package inherits its Rust edition, but no workspace edition was found; using edition {}.",
                Edition::CURRENT
            )),
        };
    }
    EditionResolution {
        edition: Edition::CURRENT,
        warning: None,
    }
}

fn edition_resolution(value: &toml::Value) -> EditionResolution {
    if let Some(name) = value.as_str() {
        if let Ok(edition) = Edition::from_str(name) {
            return EditionResolution {
                edition,
                warning: None,
            };
        }
        return EditionResolution {
            edition: Edition::CURRENT,
            warning: Some(format!(
                "Rust edition '{name}' is not supported by this parser; using edition {}. Supported editions: 2015, 2018, 2021, 2024.",
                Edition::CURRENT
            )),
        };
    }

    EditionResolution {
        edition: Edition::CURRENT,
        warning: Some(format!(
            "Rust edition value '{value}' is invalid; using edition {}. Supported editions: 2015, 2018, 2021, 2024.",
            Edition::CURRENT
        )),
    }
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = Vec::with_capacity(text.bytes().filter(|byte| *byte == b'\n').count() + 1);
    starts.push(0);
    starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
    starts
}

fn update_line_starts(starts: &mut Vec<usize>, start: usize, end: usize, inserted: &str) {
    let prefix_end = starts.partition_point(|line_start| *line_start <= start);
    let suffix_start = starts.partition_point(|line_start| *line_start <= end);
    let removed_len = end - start;
    let delta = inserted.len() as isize - removed_len as isize;
    let inserted_newlines = inserted.bytes().filter(|byte| *byte == b'\n').count();
    let mut updated =
        Vec::with_capacity(prefix_end + inserted_newlines + starts.len() - suffix_start);
    updated.extend_from_slice(&starts[..prefix_end]);
    updated.extend(
        inserted
            .match_indices('\n')
            .map(|(index, _)| start + index + 1),
    );
    updated.extend(
        starts[suffix_start..]
            .iter()
            .map(|old| (*old as isize + delta) as usize),
    );
    *starts = updated;
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

#[cfg(test)]
mod syntax_contract_tests {
    use super::{ParseJob, reparse_latest};
    use ra_ap_syntax::{AstNode, Edition, SourceFile};
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn tree_signature(parse: &ra_ap_syntax::Parse<SourceFile>) -> Vec<String> {
        parse
            .tree()
            .syntax()
            .descendants_with_tokens()
            .map(|element| {
                let text = element
                    .as_node()
                    .map(|node| node.text().to_string())
                    .or_else(|| element.as_token().map(|token| token.text().to_owned()))
                    .unwrap_or_default();
                format!(
                    "{:?}|{:?}|{}|{}",
                    element.kind(),
                    element.text_range(),
                    element.as_node().is_some(),
                    text,
                )
            })
            .collect()
    }

    fn error_signature(parse: &ra_ap_syntax::Parse<SourceFile>) -> Vec<String> {
        let mut errors = parse
            .errors()
            .into_iter()
            .map(|error| format!("{:?}|{}", error.range(), error))
            .collect::<Vec<_>>();
        errors.sort();
        errors
    }

    fn collect_source_files(directory: &Path, files: &mut Vec<PathBuf>) {
        let entries = fs::read_dir(directory).expect("read parser fixture directory");
        for entry in entries {
            let path = entry.expect("read parser fixture entry").path();
            if path.is_dir() {
                collect_source_files(&path, files);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }

    fn fixture_edition(source: &str) -> Edition {
        let first_line = source.lines().next().unwrap_or_default();
        let marker = first_line
            .strip_prefix("//@ edition: ")
            .or_else(|| first_line.strip_prefix("// "));
        match marker.map(|marker| marker.get(..4).unwrap_or_default()) {
            Some("2015") => Edition::Edition2015,
            Some("2018") => Edition::Edition2018,
            Some("2021") => Edition::Edition2021,
            _ => Edition::Edition2024,
        }
    }

    #[test]
    fn source_file_tree_round_trips_source_exactly() {
        let sources = [
            "#![allow(dead_code)]\n/// docs\n#[cfg(any())]\nfn r#type<'a>(x: &'a str) {\n    macro_rules! nested { ($($t:tt)*) => { $($t)* } }\n    nested!({ let café = r##\"raw // text\"##; /* outer /* nested */ */ });\n}\n",
            "fn main() { let x = \"unfinished;\n",
            "macro_rules! m { ($x:expr) => { $x }\n",
            "/* comment only */\n",
            "fn main() { builtin # asm(\"\", clobber_abi(\"C\"), options(nostack)); }\n",
        ];

        for edition in [
            Edition::Edition2015,
            Edition::Edition2018,
            Edition::Edition2021,
            Edition::Edition2024,
        ] {
            for source in sources {
                let parse = SourceFile::parse(source, edition);
                assert_eq!(parse.tree().syntax().text().to_string(), source);
            }
        }
    }

    #[test]
    fn macro_payloads_remain_opaque_token_trees() {
        // Macro arguments have a macro-defined grammar. This payload is not a
        // valid Rust expression, but it is valid Rust macro-call syntax and
        // must not produce a parser diagnostic before expansion.
        let source = concat!(
            "macro_rules! accepts_tokens { ($($t:tt)*) => { $($t)* }; }\n",
            "fn main() { accepts_tokens!({ let = \"macro DSL\"; ??? }); }\n",
        );
        let (result, _, parsed) = ParseJob {
            uri: "file:///macro-payload.rs".to_owned(),
            version: 1,
            generation: 1,
            text: source.to_owned(),
            line_starts: super::line_starts(source),
            edition: Edition::Edition2024,
            edition_warning: None,
        }
        .run(None);

        assert_eq!(parsed.tree().syntax().text().to_string(), source);
        assert!(
            result.params["diagnostics"].as_array().unwrap().is_empty(),
            "opaque macro payload produced a Rust syntax diagnostic: {}",
            result.params["diagnostics"],
        );
    }

    #[test]
    fn upstream_parser_fixtures_round_trip_and_incrementally_match() {
        let Some(fixture_root) = std::env::var_os("QUILL_RUST_PARSER_FIXTURES") else {
            eprintln!("skipping upstream fixture corpus: QUILL_RUST_PARSER_FIXTURES is unset");
            return;
        };
        let fixture_root = PathBuf::from(fixture_root);
        let mut files = Vec::new();
        for category in ["ok", "err", "inline"] {
            let directory = fixture_root.join(category);
            if directory.is_dir() {
                collect_source_files(&directory, &mut files);
            }
        }
        files.sort();
        assert!(
            !files.is_empty(),
            "no parser fixtures found under {fixture_root:?}"
        );
        let mut observed_kinds = BTreeSet::new();

        for path in &files {
            let source = fs::read_to_string(path).expect("read Rust parser fixture");
            let edition = fixture_edition(&source);
            let mut current_text = source.clone();
            let mut current_parse = SourceFile::parse(&source, edition);
            observed_kinds.extend(
                current_parse
                    .tree()
                    .syntax()
                    .descendants_with_tokens()
                    .filter_map(|element| {
                        element.as_node().map(|_| format!("{:?}", element.kind()))
                    }),
            );
            assert_eq!(
                current_parse.tree().syntax().text().to_string(),
                source,
                "source did not round-trip: {}",
                path.display(),
            );

            let probe_prefix = format!("{source}\n// Quill incremental probe\n");
            let valid_probe =
                "fn __quill_incremental_probe() { let __quill_probe_value_7f3 = 1; }\n";
            let malformed_probe =
                "fn __quill_incremental_probe() { let __quill_probe_value_7f3 = (1; }\n";
            let repaired_probe =
                "fn __quill_incremental_probe() { let __quill_probe_value_7f3 = r##\"λ\"##; }\n";
            let edits = [
                probe_prefix.clone(),
                format!("{probe_prefix}{valid_probe}"),
                format!("{probe_prefix}{malformed_probe}"),
                format!("{probe_prefix}{repaired_probe}"),
                source.clone(),
            ];

            for (version, edited_text) in edits.into_iter().enumerate() {
                let uri = path.to_string_lossy().into_owned();
                let (incremental_result, _, incremental) = ParseJob {
                    uri: uri.clone(),
                    version: version as i64 + 1,
                    generation: 1,
                    text: edited_text.clone(),
                    line_starts: super::line_starts(&edited_text),
                    edition,
                    edition_warning: None,
                }
                .run(Some((1, current_text.clone(), current_parse)));
                let (fresh_result, _, reparsed) = ParseJob {
                    uri,
                    version: version as i64 + 1,
                    generation: 1,
                    text: edited_text.clone(),
                    line_starts: super::line_starts(&edited_text),
                    edition,
                    edition_warning: None,
                }
                .run(None);
                assert_eq!(
                    incremental.tree().syntax().text().to_string(),
                    edited_text,
                    "incremental tree lost source text: {}",
                    path.display(),
                );
                assert_eq!(
                    tree_signature(&incremental),
                    tree_signature(&reparsed),
                    "incremental tree differs from full parse: {}",
                    path.display(),
                );
                assert_eq!(
                    error_signature(&incremental),
                    error_signature(&reparsed),
                    "incremental errors differ from full parse: {}",
                    path.display(),
                );
                assert_eq!(
                    incremental_result.params["diagnostics"],
                    fresh_result.params["diagnostics"],
                    "incremental diagnostics differ from full parse: {}",
                    path.display(),
                );
                current_text = edited_text;
                current_parse = incremental;
            }
        }
        let grammar_witnesses =
            ["fn main() { builtin # asm(\"\", clobber_abi(\"C\"), options(nostack)); }\n"];
        for source in grammar_witnesses {
            let parse = SourceFile::parse(source, Edition::Edition2024);
            assert_eq!(
                parse.tree().syntax().text().to_string(),
                source,
                "grammar witness source did not round-trip",
            );
            observed_kinds.extend(
                parse
                    .tree()
                    .syntax()
                    .descendants_with_tokens()
                    .filter_map(|element| {
                        element.as_node().map(|_| format!("{:?}", element.kind()))
                    }),
            );
        }
        eprintln!("verified upstream parser fixtures: {}", files.len());
        eprintln!("verified varied incremental edits: {}", files.len() * 5);
        eprintln!(
            "observed syntax node kinds: {}",
            observed_kinds.into_iter().collect::<Vec<_>>().join(",")
        );
    }

    #[test]
    fn incremental_reparse_matches_fresh_tree_and_errors() {
        let edit_pairs = [
            (
                "fn main() { let value = 1; }\n",
                "fn main() { let value = 10; }\n",
            ),
            (
                "fn main() { let value = (1 + 2); }\n",
                "fn main() { let value = (1 + 2; }\n",
            ),
            (
                "fn main() { /* note */ value(); }\n",
                "fn main() { /* note */ value :: call(); }\n",
            ),
            (
                "fn main() { let café = r#type; }\n",
                "fn main() { let café = r#type; let λ = 2; }\n",
            ),
            (
                "macro_rules! m { ($($t:tt)*) => { $($t)* } }\n",
                "macro_rules! m { ($($t:tt)*) => { ($($t)*) } }\n",
            ),
            (
                "fn main() { let value = \"text\"; }\n",
                "fn main() { let value = r##\"text\"##; }\n",
            ),
        ];

        for edition in [
            Edition::Edition2015,
            Edition::Edition2018,
            Edition::Edition2021,
            Edition::Edition2024,
        ] {
            for (old_text, new_text) in edit_pairs {
                let old_parse = SourceFile::parse(old_text, edition);
                let incremental = reparse_latest(old_text, new_text, old_parse, edition);
                let fresh = SourceFile::parse(new_text, edition);

                assert_eq!(incremental.tree().syntax().text().to_string(), new_text);
                assert_eq!(tree_signature(&incremental), tree_signature(&fresh));
                assert_eq!(error_signature(&incremental), error_signature(&fresh));
            }
        }
    }
}
