# Rust syntax coverage contract

Use this contract when changing the LSP parser, edition selection, recovery,
or the pinned parser dependency. It defines syntax fidelity; compiler semantic
compatibility has a separate [completion target](../benchmarks/compatibility/TARGET.md).

## Scope

The LSP uses the full-fidelity `ra_ap_syntax` parser, currently pinned to
`0.0.358` in [its manifest](../crates/quill-lsp/Cargo.toml). Preserve source,
comments, attributes, and token trees, including syntax that Quill's compiler
does not understand. A complete syntax result is a lossless tree with grammar
errors at the affected ranges. It does not establish name resolution, type
checking, `cfg` evaluation, or semantic validity.

### Editions

| Input | Parser edition |
| --- | --- |
| Explicit package edition | That edition |
| Inherited workspace package edition | The workspace value |
| Cargo package with no edition field | 2015 |
| Standalone file | The parser's current edition |
| Manifest edition unknown to the pinned parser | Newest supported edition, with a warning that the parse is not exact for the requested edition |

The target covers 2015, 2018, 2021, and 2024. See [Cargo's edition rules](https://doc.rust-lang.org/cargo/reference/manifest.html#the-edition-field).

### Macros

Parse invocation paths, delimiters, and token trees without interpreting the
payload as Rust expressions, items, or patterns. The payload may be a macro's
own language. Declarative expansion and procedural execution are separate
capabilities. A future expansion provider must supply the fragment kind when
passing expanded text to a Rust fragment parser.

## Grammar inventory

The pinned `rust.ungram` contains 190 productions:

| Syntax area | Included forms |
| --- | --- |
| Tokens and source structure | Identifiers, lifetimes, literals, comments, doc comments, shebangs, source files, frontmatter. |
| Paths and generics | Qualified paths, type anchors, generic parameters/arguments, associated types, const arguments, where clauses. |
| Attributes and macros | Inner/outer attributes, metadata, `cfg` forms, calls/definitions, token trees, macro item/statement bodies. |
| Items and declarations | Modules, functions, structs and fields, enums, unions, traits, impls, aliases, constants, statics, imports, extern declarations, assembly. |
| Statements and expressions | Let/let-else, blocks, control flow, closures, calls, operators, casts, async/await, ranges, match, returns, labels, literals, macro expressions. |
| Types and bounds | References, pointers, tuples, arrays, slices, function pointers, `impl`/`dyn` traits, higher-ranked and trait bounds. |
| Patterns | Identifier, wildcard, literal, range, reference, tuple, slice, record, tuple-struct, or-pattern, rest, macro patterns. |

The corpus and assembly witness observe all 170 concrete `SourceFile` node
productions. The remaining 20 are 18 AST union categories and the `MacroItems`
and `MacroStmts` fragment roots, which do not occur in a `SourceFile` tree.
This inventory does not claim that completion, symbols, or name lookup
understand every node.

## Evidence and upgrade gate

Run the release server through the Quill-owned gate:

```sh
python3 benchmarks/rust_syntax_compat.py --quillc target/release/quillc
```

The runner uses the pinned `ra_ap_parser` corpus, honoring fixture edition
comments when creating Cargo manifests. Read [the raw report](../benchmarks/results/lsp-rust-syntax-compat-20261013.json)
for the recorded run. Its evidence comprises:

| Check | Recorded coverage |
| --- | --- |
| Upstream source fixtures | 440: 326 accepted and 114 expected to report grammar errors; parsed through `SourceFile`, including `inline` fixtures. |
| Edition/manifest and recovery probes | Nine edition/manifest probes plus seven malformed buffers: 456 LSP cases in total. |
| Incremental diagnostics | Ranged EOF edit, then fresh parse, for all 456 cases; diagnostics match. |
| Recovery | All seven malformed buffers survive repair and re-break sequences; final diagnostics match a fresh parse. |
| Source and tree fidelity | Exact reconstruction and incremental/full-parse structural equivalence for all 440 upstream fixtures. |
| Varied transitions | Five edits per fixture, 2,200 in total: comments, item insertion, malformed delimiters, Unicode/raw strings, deletion; trees, errors, and diagnostics match fresh parses. |
| Additional regressions | Token-boundary edits across all four editions; non-Rust macro payloads round-trip without false syntax errors. |

Before upgrading the parser:

1. Review grammar, fixture, and edition changes against the pinned version.
2. Expand the gate for new forms and changed recovery behavior. Broader
   incomplete-buffer stress and longer malformed-delimiter sequences remain open.
3. Require the existing source-fidelity, grammar-node, diagnostic, edition, and
   incremental checks to pass with the new parser.
4. Record the parser version, coverage changes, remaining exclusions, and results.
   Claim only the coverage established by that evidence; keep the dependency pinned.

## Performance boundary

Parse versioned document snapshots in the background. Keep macro payloads opaque
on the syntax worker. Symbols, definition, hover, and diagnostics use the current
parse; [provisional completion](lsp.md#performance-design) may use live text and
the last completed index with an incomplete marker. Layer semantic analysis,
workspace indexing, expansion, and compiler diagnostics above this contract.
