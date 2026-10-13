# Website visual direction

## Print treatment

![Original lithographic style reference](references/lithographic-print.png)

The reference is for halftone ink, paper, and typographic confidence. The animals,
knot, and three-card arrangement are not Quill motifs or a requested layout.

Use warm paper, charcoal type, and restrained rust-red halftone artwork. The
palette and local Geist fonts are defined in `assets/site.css`; keep that file
the source of truth for exact values. Geist Mono supports code and small labels.
Halftone rendering lives in `assets/print-fields.js`.

## Decisions from the design review

- Explain the compiler in plain language. Its ambition is full Rust compatibility
  on Linux and Windows; its current implementation is still developing.
- Keep hero text left aligned, with a compact vertical getting-started section
  on its right at desktop sizes. The large hero feather was removed; small
  flame-feather identity marks remain in navigation and footer.
- Section separators are small, consistent halftone impressions constrained to
  the same content container as the text. They should not expand into large
  landscape sweeps or bleed across the viewport.
- Preserve useful density. Avoid giant slogans paired with small text across
  mostly empty space, generic feature-card grids, and decorative technical labels.
- Arbitrary animals and abstract shapes were rejected. The printing-press and
  factory concepts were explored and scrapped. Do not revive them as defaults.
- The “Taking shape” section was removed. Tinted section bands and isolated
  straight rules were rejected as the main way to separate homepage sections.

## Current implementation baseline

The architecture section uses a text-based Parse / Check / Generate / Link
interaction. Inspiration pairs halftone language names with concise research
notes. Performance shows measured data with provenance. Compatibility keeps
documented feature status separate from differential test evidence.

These are the current baseline, not permanent approvals of every detail. For a
new design request, change the named area deliberately and carry forward the
explicit decisions above unless the user changes direction.

## Visual review questions

Does the visitor understand what Quill is? Is the important content immediately
readable? Does the halftone treatment belong to the composition? Are separators
equally compact and aligned to the content edges? Does the section still work
with its neighbours and on a phone? Is every numeric claim described accurately?
