# Website review standards

Scope: static pages, browser assets, and the local website server.

- Review the rendered result against [the visual direction](docs/website/design.md).
  A reference conveys a treatment; its pictured objects are not a content brief.
- Preserve the hierarchy and density of the section being changed. Review it with
  its neighbours and at narrow widths, not only as an isolated crop.
- When replacing a component, remove its unused styles and behavior. Keep base
  rules together and make responsive overrides explicit; do not stack successive
  redesigns at the end of the stylesheet.
- Distinguish the goal of full Rust compatibility on Linux and Windows from
  implemented capabilities. Keep performance claims tied to their workload,
  measurement source, and revision. A curated probe pass rate is not a language
  compatibility percentage.
- Keep website commits separate from compiler work. Stage named files or hunks;
  preserve concurrent edits in shared files.

Use the [preview and CSS workflow](docs/website/README.md) for the concrete review
steps. User instructions take precedence over these defaults.
