# Website work

Read [visual direction](design.md) before design changes and
[review standards](../../CODING_STANDARDS.md) before reviewing them.

## File map

- `index.html`: homepage; `compatibility.html`: compatibility index.
- `project.html`: project documentation landing page.
- `assets/site.css`: shared foundation and homepage components, grouped by section.
- `assets/compatibility.css` and `assets/document.css`: page-specific styles.
- `assets/print-fields.js`: halftone rendering; `assets/motion.js`: shared motion.
- `vite.config.mjs`: Vite+ development and production configuration.
- `website/content-plugin.mjs`: Markdown routes, live reload, and publication.
- `website/data.mjs`: report selection and server-rendered statistics.

## Local preview

From the repository root (Node version requirements are in `package.json`):

```sh
npm ci --ignore-scripts
npm run dev
```

Vite+ listens on `0.0.0.0:4173`. Open `http://localhost:4173` locally, or the
host's LAN address on another device. CSS updates in place; changes to docs,
reports, scripts, and HTML reload open pages. Git revision changes also reload.
`serve.py` is a compatibility launcher for this same command.

Every non-ignored repository `.md` file is served in the existing document reader
at its original URL; `?raw=1` returns Markdown. Project notes lists all documents,
including newly added files. Relative links and images keep their original base.
Dot-directories, build output, and files resolving outside the repository are
excluded from publication.

## Statistics and publication

Edit or regenerate source reports; page figures are generated automatically.
`website/data.mjs` supplies the same data to development pages and production
HTML, including readable figures with JavaScript disabled. `/site-data.json`
records the selected reports, documentation index, and current revision.

- Catalog Flow uses `benchmarks/catalog-flow/results.json` for times, ratios,
  workload size, run count, machine, and source revision. The metric switch reads
  that same report.
- Compiler checks select the newest report in `benchmarks/compatibility/results/`.
  Report category coverage remains distinct from the current documentation index.
- LSP latency and syntax coverage select reports by their `benchmark` identifier
  under `benchmarks/results/`. Adding a matching report makes it eligible automatically.
- Selection prefers recorded run/artifact timestamps, then filename date,
  filesystem modification time, and filename. Record timestamps in new report
  formats for unambiguous ordering across machines.
- Feature availability is still curated in `compatibility.html`; the homepage
  counts those entries automatically. Passing a probe never changes a feature's
  status automatically.

Malformed reports surface an error. Missing LSP or compiler reports display an
unavailable state instead of retaining copied numbers. Reloading displays new
measurements; it does not run compiler benchmarks or infer new results from code.

```sh
npm run build
npm run preview
```

The build writes `target/website`, including Markdown, report JSON, and referenced
public media. Preview serves that built snapshot; use a different `--port` if the
development server is already running. The existing deployment workflow now
installs locked dependencies and builds with Vite+. `scripts/build-website.py`
remains a compatibility entry point. Source HTML provides the layout; modify
`renderStats` when changing a measured section's markup. Do not copy new numbers
into HTML or create another benchmark snapshot under `assets/`.

## Review a change

1. Capture the existing section before changing its structure or stylesheet.
2. In T3, call `preview_status` first and use an owned tab. Open a preview if none
   exists. Wait for Vite to apply the edit; do not assume an old screenshot is current.
3. Wait for `document.fonts.ready`, then two animation frames. Scroll the section
   into view and let its finite entrance animation settle. Do not wait on infinite
   animations. For geometry comparisons, finish only finite animations.
4. Capture the changed section with neighbouring content at 1280 × 800 and
   390 × 844. Check 320px too when changing width-sensitive content. Check for
   overflow, wrapping, hit targets, and the placement of the halftone impressions.
5. Compare against [the design record](design.md). Exercise affected controls,
   keyboard focus, and reduced-motion behavior when those areas change. Review
   compatibility and Markdown pages when changing shared CSS.
6. Record what was inspected and any limitations. Commit website files separately
   from compiler work; inspect the staged diff before committing.

Screenshots are review evidence, not replacements for source. Keep temporary
captures outside the tracked tree unless intentionally updating this reference.

## CSS tooling

```sh
npm ci --ignore-scripts
npm run format:css
npm run check:css
```

Prettier formats the three stylesheets. Stylelint rejects duplicate selectors,
duplicate properties, empty blocks, unknown properties, and invalid hex colours.
Responsive overrides in different media conditions are intentional. Keep each
component's rules together instead of adding a later duplicate to win the cascade.

Tooling is development-only. At setup, npm reported a transitive `braces`
stack-exhaustion advisory through Stylelint; the available audit fix downgraded
Stylelint to an obsolete major. Our commands use fixed local globs. Reassess the
advisory when updating dependencies; do not apply that downgrade automatically.
