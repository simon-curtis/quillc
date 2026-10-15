# Develop and publish the website

Use this guide for website content, statistics, assets, and publication. For
composition work, read [the design record](design.md) and apply
[the review criteria](../../CODING_STANDARDS.md).

## Local preview

1. Use the Node version required by [package.json](../../package.json). From
   the repository root, install locked dependencies and start development:

   ```sh
   npm ci --ignore-scripts
   npm run dev
   ```

2. Open the URL printed by Vite+. [vite.config.mjs](../../vite.config.mjs)
   owns the host and port. Preview is ready when the requested page loads and
   an edit appears after Vite updates it. CSS updates in place; HTML, docs,
   reports, scripts, and Git revision changes reload pages.

`serve.py` launches the same development command. For generated output, use
`npm run build` followed by `npm run preview`; choose another `--port` if the
development server occupies the configured port.

## Change content

1. Find the authoritative source for each claim. Use the implementation and
   [architecture guide](../architecture.md) for compiler behavior,
   [the inventory workflow](../rust-compatibility-inventory.md) for feature
   scope, and [statistics and publication](#statistics-and-publication) for
   measurements.
2. Update every affected page. Link detailed contracts and evidence; preserve
   the workload, revision, and limits needed to interpret each claim.
3. Inspect the served page and its links. Finish when the rendered claim matches
   its source and related pages agree. Layout, theme, and asset changes also
   require [visual review](#review-a-change).

### Document publication

The document reader serves eligible repository Markdown and JSON at their
original paths; `?raw=1` returns source. Relative links and images retain their
original base. Project notes indexes newly added documents automatically.

[publicFiles](../../website/data.mjs) owns eligibility: Git-ignored paths,
dot-directories, build output, and paths resolving outside the repository are
excluded. Keep public documentation and reports within that boundary. Reloading
updates displayed evidence; it does not generate a new compiler measurement.

## Statistics and publication

[website/data.mjs](../../website/data.mjs) selects reports and renders figures
for both development and production, including HTML readable without JavaScript.
Inspect `/site-data.json` to identify the reports and revision actually displayed.

| Claim | Authoritative input | Interpretation |
| --- | --- | --- |
| Build time and generated-binary runtime | [Catalog Flow report](../../benchmarks/catalog-flow/results.json) | Keep build modes, runtime, workload, host, and compiler provenance together. |
| Differential compiler checks | Reports under [compatibility/results](../../benchmarks/compatibility/results) | Case outcomes describe that corpus and compiler state. |
| Editor latency and syntax | Matching benchmark IDs under [benchmarks/results](../../benchmarks/results) | Each report covers its own operations and syntax scope. |
| Compiler parser acceptance | `quill-compiler-parser-compatibility` reports under [benchmarks/results](../../benchmarks/results) | Counts rustc-accepted sources from the named corpora, separately from editor syntax. |
| Compiler parser coverage | `quill-compiler-parser-compatibility` reports under [benchmarks/results](../../benchmarks/results) | Pass rate over rustc-accepted fixtures and library files; not all Rust syntax. |
| Stage coverage | Curated [compatibility rows](../../compatibility.html) and feature IDs in `siteData` | Areas with partial or full support are counted equally; disclose the denominator and gaps. |
| Real-crate probes | [Crate report](../../benchmarks/crates/results.json) | Parsing, subset checking, native builds, and rustc checking require their own evidence. |

Report selection uses recorded run/artifact timestamps, filename dates,
filesystem modification times, then filenames. Include timestamps in new report
formats so selection is reproducible across machines. The compiler-parsing stage
selects its own report and shows “unmeasured” when none is available. Generate
the report through
[the compiler acceptance gates](../compiler-parser-migration.md#rustc-acceptance-gates).
Editor syntax reports measure a different path.

To change a measured section:

1. Record or select evidence through [the benchmark workflow](../../benchmarks/README.md).
   Update the source report; figures in source HTML are not a second data store.
2. If markup or report structure changes, update `renderStats` and affected
   controls to consume the same fields. New stages require explicit evidence.
3. Inspect `/site-data.json` and the rendered pages. Check missing-report behavior
   shows an unavailable state and malformed reports surface an error. Completion
   means every displayed figure traces to its selected report and scope.

### Build and deploy

`npm run build` writes the static snapshot, documents, JSON, and referenced media
to `target/website`. `scripts/build-website.py` is a compatibility entry point.

[The website workflow](../../.github/workflows/website.yml) owns CI triggers,
bundle checks, deployment commands, credentials, and deployed-revision checking.
Use it for publication; a local build alone establishes only the generated
snapshot. A deployment is complete when its checks pass and the live
`/project-status.json` reports the intended revision. Package hosting has a
separate [release workflow](../arch-packaging.md#hosting-boundaries).

## Review a change

1. Capture the current section before changing its structure or stylesheet.
2. In T3, call `preview_status` and use an agent-owned tab or open one. Wait for
   Vite to apply the edit.
3. Wait for `document.fonts.ready` and two animation frames. Scroll the section
   into view and let entrance animations settle. For geometry comparisons,
   finish finite animations and leave infinite animations running.
4. Capture the section with its neighbours at 1280 × 800 and 390 × 844; add
   320px when width sensitivity matters. Check overflow, wrapping, hit targets,
   and halftone boundaries.
5. Exercise affected controls, keyboard focus, and reduced motion. For shared
   CSS, also inspect the compatibility page and Markdown reader.
6. Apply [the review criteria](../../CODING_STANDARDS.md). Record inspected
   views, behavior, evidence, and unresolved limits. Finish when each applicable
   criterion has evidence or a named open issue.

Keep temporary captures outside the tracked tree unless intentionally updating
an approved reference. Follow [AGENTS.md](../../AGENTS.md) when staging work.

## Color themes

The header cycles System → Dark → Light. `quill-theme` stores the explicit
choice and synchronizes tabs; without storage, the control still works for the
current page. Each template needs the inline initializer before styles paint
and [theme.js](../../assets/theme.js) for the control and system changes.

Use palette tokens from [site.css](../../assets/site.css). Halftone canvases
repaint on `quill:themechange`, including under reduced motion. Theme changes
are complete when text, controls, code/JSON, statuses, and artwork work in both
palettes, the preference survives navigation, and narrow headers remain usable.

## CSS changes

Use the format and check scripts in [package.json](../../package.json).
Keep a component's base and responsive rules together. Remove obsolete selectors
when replacing a component; intentional media-query overrides stay with it.

## Implementation map

- `index.html`, `compatibility.html`, `project.html`: page content and structure.
- `assets/site.css`: shared foundation and homepage components;
  `assets/compatibility.css` and `assets/document.css`: page-specific styles.
- `assets/print-fields.js`, `assets/motion.js`: halftone artwork and motion.
- `website/content-plugin.mjs`: document routes, live reload, and publication.
- `website/data.mjs`: report selection and statistics rendering.
