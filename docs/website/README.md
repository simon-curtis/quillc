# Website work

Read [visual direction](design.md) before design changes and
[review standards](../../CODING_STANDARDS.md) before reviewing them.

## File map

- `index.html`: homepage; `compatibility.html`: compatibility index.
- `project.html`: project documentation landing page.
- `assets/site.css`: shared foundation and homepage components, grouped by section.
- `assets/compatibility.css` and `assets/document.css`: page-specific styles.
- `assets/print-fields.js`: halftone rendering; `assets/motion.js`: shared motion.
- `serve.py`: local preview, Markdown rendering, and live project status.

## Local preview

From the repository root:

```sh
python3 serve.py --bind 0.0.0.0 --port 4173
```

Open `http://localhost:4173` locally, or the host's LAN address on another device.
The development server returns `Cache-Control: no-store` for every response and
ignores conditional request validators. Restart it after editing Python code.
If switching from an older caching server, perform one hard reload. Asset URLs
should stay stable; do not accumulate temporary version query strings.

## Review a change

1. Capture the existing section before changing its structure or stylesheet.
2. In T3, call `preview_status` first and use an owned tab. Open a preview if none
   exists. Reload the page after edits; do not assume an old screenshot is current.
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
