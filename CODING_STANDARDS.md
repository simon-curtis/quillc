# Website review criteria

Use these criteria for page, browser-asset, and preview-server changes. Follow
[the website workflow](docs/website/README.md#review-a-change) for review steps.

| Area | Completion criterion |
| --- | --- |
| Composition | The changed section fits its neighbours and [the visual direction](docs/website/design.md); the reference informs print treatment, not subject matter. |
| Responsive behavior | Important content, controls, and halftone boundaries remain readable and usable at desktop and phone widths. |
| Component replacement | The new component owns its base and responsive styles; obsolete selectors and behavior are removed. |
| Numeric claims | Figures come from report data and retain workload and revision context. Build speed, binary runtime, and editor latency are distinguishable. |
| Compatibility claims | Wording separates [the target](benchmarks/compatibility/TARGET.md), documented feature status, and measured probes. |
| Handoff | State what was inspected and any remaining limitations; keep the commit scoped to the work. |

User instructions override these defaults. Shared-checkout commit guidance lives
in [AGENTS.md](AGENTS.md).
