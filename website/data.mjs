import { execFileSync } from "node:child_process";
import { readFile, stat, realpath } from "node:fs/promises";
import path from "node:path";
import { load } from "cheerio";

export const root = path.resolve(import.meta.dirname, "..");
const read = (name) => readFile(path.join(root, name), "utf8");
export function revision() {
  try {
    return execFileSync("git", ["rev-parse", "HEAD"], {
      cwd: root,
      encoding: "utf8",
    }).trim();
  } catch {
    return null;
  }
}
// Include new, uncommitted docs/reports during development, honouring gitignore.
export async function publicFiles() {
  const names = execFileSync(
    "git",
    ["ls-files", "--cached", "--others", "--exclude-standard", "-z"],
    { cwd: root, encoding: "utf8" },
  ).split("\0");
  const selected = [...new Set(names)].filter(
    (name) =>
      name &&
      !name.split("/").some((part) => part.startsWith(".")) &&
      (/\.(md|json)$/i.test(name) ||
        name === "LICENSE" ||
        name.startsWith("assets/") ||
        (/^(docs|benchmarks)\//.test(name) &&
          /\.(json|png|jpe?g|svg|webp|gif|pdf)$/i.test(name))),
  );
  const files = await Promise.all(
    selected.map(async (name) => {
      try {
        const resolved = await realpath(path.join(root, name));
        if (
          !resolved.startsWith(root + path.sep) ||
          !(await stat(resolved)).isFile()
        )
          return null;
        return name;
      } catch {
        return null;
      }
    }),
  );
  return files.filter(Boolean).sort();
}
async function report(name) {
  const value = JSON.parse(await read(name));
  return { path: name, value };
}
async function latest(files, prefix, accepts) {
  const reports = [];
  for (const name of files.filter(
    (n) => n.startsWith(prefix) && n.endsWith(".json"),
  )) {
    // Incomplete writes surface as an error instead of silently showing old numbers.
    const item = await report(name);
    if (!accepts(item.value)) continue;
    const info = await stat(path.join(root, name));
    const recorded =
      item.value.generated_at ||
      item.value.timestamp ||
      item.value.quillc_artifact_mtime;
    // Prefer recorded run/artifact time; filename dates keep old reports from
    // becoming current just because a fresh checkout changed their filesystem times.
    const date = name
      .match(/\d{4}-?\d{2}-?\d{2}/)?.[0]
      .replace(/^(\d{4})(\d{2})(\d{2})$/, "$1-$2-$3");
    item.order = Date.parse(recorded || date || "") || 0;
    item.modified = info.mtimeMs;
    reports.push(item);
  }
  reports.sort(
    (a, b) =>
      b.order - a.order ||
      b.modified - a.modified ||
      b.path.localeCompare(a.path),
  );
  return reports[0] || null;
}
export async function siteData() {
  const files = await publicFiles();
  const $ = load(await read("compatibility.html"));
  const counts = { available: 0, partial: 0, planned: 0, unassessed: 0 };
  const languageCounts = { available: 0, partial: 0, planned: 0, unassessed: 0 };
  const languageGroups = new Set(["Language", "Checking", "Macros"]);
  const generatingIds = new Set([
    "functions-and-scalar-arithmetic", "control-flow", "enums-and-match", "if-let",
    "structs-and-inherent-methods", "tuples--arrays--and-slices", "generics",
    "primitive-types-and-literals", "floating-point-semantics", "operators-casts-and-overflow",
    "closures-and-capture", "destructuring-patterns", "let-else-and-pattern-conditions",
    "references-and-raw-pointers", "function-types-and-abi", "static-items-and-mutability",
    "unsafe-rust", "unions-and-layout", "async-await-and-futures", "inline-assembly",
    "foreign-items-and-ffi", "trait-objects-and-dynamic-dispatch",
  ]);
  const stageAreas = { checking: [], generating: [] };

  $(".compatibility-row[data-status]").each((_, el) => {
    const status = $(el).attr("data-status");
    if (status in counts) counts[status]++;
    const area = { id: $(el).attr("id"), name: $(el).find("h2").text(), status };
    if ($(el).find(".feature-category").text().trim() === "Checking") stageAreas.checking.push(area);
    if (generatingIds.has(area.id)) stageAreas.generating.push(area);

    if (status in languageCounts && languageGroups.has($(el).find(".feature-category").text().trim()))
      languageCounts[status]++;
  });
  const docs = await Promise.all(
    files
      .filter((n) => /\.(md|json)$/i.test(n))
      .map(async (name) => ({
        path: name,
        title: /\.md$/i.test(name)
          ? (await read(name)).match(/^#\s+(.+)$/m)?.[1] || name
          : name,
      })),
  );
  const [catalog, compatibility, lsp, syntax, crates, parser] = await Promise.all([
    report("benchmarks/catalog-flow/results.json"),
    latest(
      files,
      "benchmarks/compatibility/results/",
      (d) =>
        Number.isFinite(d.total) &&
        Number.isFinite(d.passed) &&
        Array.isArray(d.results),
    ),
    latest(
      files,
      "benchmarks/results/",
      (d) => d.benchmark === "quill-lsp-syntax-latency" && d.measurements,
    ),
    latest(
      files,
      "benchmarks/results/",
      (d) => d.benchmark === "quill-lsp-rust-syntax-compatibility",
    ),
    files.includes("benchmarks/crates/results.json")
      ? report("benchmarks/crates/results.json")
      : null,
    latest(
      files,
      "benchmarks/results/",
      (d) =>
        d.benchmark === "quill-compiler-parser-compatibility" &&
        d.compiler_parser_api === "parse_ast_with_cfg_and_edition",
    ),
  ]);
  return {
    revision: revision(),
    counts,
    languageCounts,
    stageAreas,
    crates,
    docs,
    catalog,
    compatibility,
    parser,
    lsp,
    syntax,
  };
}
const ms = (value) => `${Math.round(value * 1000).toLocaleString("en-US")} ms`;
const number = (value) => Number(value).toLocaleString("en-US");
const decimal = (value) => Number(value).toFixed(1);
const escape = (text) =>
  String(text).replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        c
      ],
  );
const link = (url, label) => `<a href="/${escape(url)}">${escape(label)}</a>`;
const provenance = (data) => {
  const rev = data.quillc_source_revision || data.quill_source_revision || data.revision;
  const dirty = data.quillc_source_dirty ?? data.quill_source_dirty;
  return rev
    ? `Recorded revision ${rev.slice(0, 8)}${dirty === true ? " · modified checkout" : dirty === false ? " · clean checkout" : " · checkout cleanliness not recorded"}`
    : "Revision not recorded";
};

export function renderStats(html, data) {
  const $ = load(html);
  const { catalog, compatibility, parser, lsp, counts } = data;
  const total = Object.values(counts).reduce((a, b) => a + b, 0);
  for (const stage of ["checking", "generating"]) {
    const areas = data.stageAreas[stage];
    const complete = areas.filter(a => a.status === "available").length;
    const partial = areas.filter(a => a.status === "partial").length;
    const supported = complete + partial;
    const percentage = areas.length ? Math.round(supported / areas.length * 100) : null;
    $(`[data-stage="${stage}"] [data-stage-value]`).text(percentage === null ? "—" : `${percentage}%`);
    $(`[data-stage="${stage}"] [data-stage-count]`).text(`${supported} of ${areas.length} tracked areas have support`);
    $(`[data-stage="${stage}"] [data-stage-detail]`).text(`${complete} full · ${partial} partial · ${areas.length - supported} without established support`);
    $(`[data-stage="${stage}"] [data-stage-areas]`).html(areas.map(a =>
      `<li><a href="/compatibility.html#${escape(a.id)}">${escape(a.name)}</a> — ${a.status === "planned" ? "not yet" : escape(a.status)}</li>`
    ).join(""));
  }

  const parserStage = $("[data-stage=\"parsing\"]");
  if (parser) {
    const d = parser.value;
    if (
      !Number.isFinite(d.cases_checked) ||
      !Number.isFinite(d.passed) ||
      d.cases_checked <= 0 ||
      d.passed < 0 ||
      d.passed > d.cases_checked
    )
      throw new Error(`Invalid compiler parser report: ${parser.path}`);
    const percentage = Math.round((d.passed / d.cases_checked) * 100);
    parserStage.find("[data-stage-value]").text(`${percentage}%`);
    parserStage
      .find("[data-stage-count]")
      .text(`${number(d.passed)} of ${number(d.cases_checked)} rustc-accepted sources parsed`);
    parserStage
      .find("[data-stage-detail]")
      .text(
        `${number(d.corpora.upstream_parser_fixtures.passed)} syntax fixtures · ${number(d.corpora.rust_library_sources.passed)} compiler source files`,
      );
    parserStage
      .find("[data-stage-limit]")
      .text("Corpus coverage is a regression measure, not proof of every Rust grammar form.");
    parserStage
      .find("[data-stage-provenance]")
      .text(`${provenance(d)} · ${d.target || "target not recorded"}`);
    parserStage.find("[data-stage-report]").attr("href", `/${parser.path}`);
  } else {
    parserStage.find("[data-stage-count]").text("Compiler parser corpus not measured");
    parserStage
      .find("[data-stage-limit]")
      .text("The editor syntax report does not measure the compiler parser path.");
    parserStage
      .find("[data-stage-provenance]")
      .text("Compiler revision and target not recorded.");
    parserStage.find("[data-stage-report]").remove();
  }

  $("#compatibility-score").text("—");
  $(".overview-score .score-unit").text("");
  const summary = `${counts.available} available · ${counts.partial} partial · ${counts.planned} not yet · ${counts.unassessed} unassessed`;
  $("#coverage-counts").text(`Capability inventory · ${total} entries: ${summary}.`);
  $("#coverage-bar").attr("aria-label", `${total} tracked entries: ${summary}`);
  for (const [status, count] of Object.entries(counts)) {
    $(`#coverage-bar .coverage-${status}`)
      .attr("style", `flex:${count}`)
      .attr("hidden", count ? null : "");
  }
  $("#feature-count").text(`${total} grouped entries · not a score`);
  if (data.revision)
    $("#checkout-revision").html(
      `<a href="https://github.com/simon-curtis/quillc/commit/${data.revision}">${data.revision.slice(0, 8)}</a>`,
    );

  if (compatibility) {
    const d = compatibility.value;
    if (d.passed < 0 || d.total < d.passed)
      throw new Error(`Invalid counts: ${compatibility.path}`);
    $("#compatibility-score").text(d.passed);
    $(".overview-score .score-unit").text(` / ${d.total}`);
    $(".differential-number").html(`${d.passed} <span>/ ${d.total}</span>`);
    $(".differential-heading .text-link").attr(
      "href",
      "/" + compatibility.path,
    );
    const coverage = d.feature_coverage || [];
    // Locate values by their labels; optional inventory rows must not shift them.
    $(".differential-stats div").each((_, el) => {
      const row = $(el);
      const label = row.find("dt").text().trim();
      if (label === "Cases with mismatches") row.find("dd").text(d.total - d.passed);
      if (label === "Windows output") row.find("dd").text(
        coverage.find((f) => f.id === "windows-output")?.probed ? "Probed" : "Not probed",
      );
    });
    $(".differential-source p").text(
      `${(d.rustc || d.toolchain || "").split("\n")[0]} · ${provenance(d)}. See report for exact provenance.`,
    );
    $(".coverage-evidence").html(
      `${escape(provenance(d))}. <a href="/${escape(compatibility.path)}">Read the comparison report</a>.`,
    );
  } else {
    $(".differential-number").text("—");
    $(".differential-label").text("No comparison report available");
    $(".differential-detail, .differential-heading .text-link").remove();
    $(".coverage-evidence").text(
      "No differential comparison report available.",
    );
  }

  const c = catalog.value;
  const medians = c.medians;
  $("[data-build]").each((_, row) => {
    const key = $(row).attr("data-build");
    const q = medians?.[`quill_${key}`]?.wall_seconds;
    const r = medians?.[`rustc_${key}`]?.wall_seconds;
    if (!Number.isFinite(q) || !Number.isFinite(r) || q <= 0)
      throw new Error(`Missing Catalog Flow timings: ${key}`);
    $(row).find(".quill-value").text(ms(q));
    $(row).find(".rustc-value").text(ms(r));
    $(row)
      .find(".ratio-value")
      .text(`${(r / q).toFixed(2)}×`);
  });
  const runs = c.results?.filter((r) => r.label === "quill_cold").length || 0;
  $(".performance-intro > div > p")
    .last()
    .text(
      `Our Catalog Flow benchmark compiles ${number(c.rust_lines)} lines of Rust. Measurements cover cold, warm, and incremental builds of the same workload.`,
    );
  $(".benchmark-context > p").text(
    `${runs} runs per build state. ${c.cpu_model}, ${c.logical_cpu_count} logical CPUs. ${c.quill_profile} Quill against ${c.toolchain}. Cold builds clear compiler caches; the OS file cache stays warm.`,
  );
  $(".benchmark-table caption").text(
    `Catalog Flow comparison. Medians of ${runs} runs.`,
  );
  $(".benchmark-revision-note").text(" · " + provenance(c));
  $(".benchmark-context .raw-results").attr("href", "/" + catalog.path);

  const runtime = c.runtime_medians;
  if (runtime) {
    const quill = runtime.quill_binary_runtime;
    const rustc = runtime.rustc_binary_runtime;
    $("[data-runtime]").each((_, row) => {
      const metric = $(row).attr("data-runtime");
      for (const [column, values] of [quill, rustc].entries()) {
        const value = values?.[metric];
        if (!Number.isFinite(value) || value < 0)
          throw new Error(`Invalid binary runtime: ${metric}`);
        $(row)
          .find("td")
          .eq(column)
          .text(`${(value * 1000).toFixed(3)} ms`);
      }
    });
    const ratio = quill?.wall_seconds / rustc?.wall_seconds;
    $(".binary-comparison").text(
      Number.isFinite(ratio) && ratio > 0
        ? `Quill takes ${ratio.toFixed(1)}× as long as rustc to run this fixture.`
        : "See the individual elapsed times above.",
    );
    const method = c.runtime_measurement;
    const sampling = method
      ? `${method.samples_per_binary} launches per binary after ${method.warmups_per_binary} warmups. ${method.method}. ${method.output_checked_each_run ? "Output checked on every run." : ""}`
      : "See the raw report for sampling details.";
    $(".binary-method").text(
      `${sampling} One synthetic fixture; see the method for executable profiles and scope. ${provenance(c)}.`,
    );
  }

  if (lsp) {
    const d = lsp.value;
    const completion = d.measurements.completion;
    const edit = d.measurements.incremental_edit_to_diagnostics;
    if (!Number.isFinite(completion?.p50_ms) || !Number.isFinite(edit?.p50_ms))
      throw new Error(`Missing LSP timings: ${lsp.path}`);
    $(".lsp-time").html(`${decimal(completion.p50_ms)}<span>ms</span>`);
    $(".lsp-secondary").html(
      `${decimal(edit.p50_ms)} ms <span>from edit to syntax diagnostics</span>`,
    );
    $(".lsp-source > p").text(
      `Generated ${number(d.source_lines)}-line Rust file · ${d.cpu}. Medians of ${d.rounds} requests. Completion p95: ${decimal(completion.p95_ms)} ms; edit diagnostics p95: ${decimal(edit.p95_ms)} ms. ${provenance(d)}. Synthetic workload, not an editor comparison.`,
    );
    $(".lsp-source a").attr("href", "/" + lsp.path);
  } else {
    $(".lsp-time").text("—");
    $(".lsp-secondary").text("Measurements not available");
    $(".lsp-source > p").text(
      "Run the LSP latency benchmark to publish measurements.",
    );
    $(".lsp-source a").remove();
  }

  // Real-crate results are compatibility probes, not paired performance runs.
  if (data.crates) {
    const report = data.crates.value;
    if (!Array.isArray(report.packages)) throw new Error(`Invalid crate report: ${data.crates.path}`);
    const outcome = (result, failureLabel = "Rejected") => Number.isInteger(result?.exit_code)
      ? result.exit_code === 0 ? "Pass" : failureLabel
      : "Not measured";
    const packages = report.packages;
    const parsed = packages.filter(p => p.quill_cargo_parse?.exit_code === 0).length;
    const checked = packages.filter(p => p.quill_cargo_check_subset?.exit_code === 0).length;
    $("[data-crate-summary]").text(`${parsed} of ${packages.length} crates parse · ${checked} of ${packages.length} pass Quill’s subset checker`);
    $("[data-crate-rows]").html(packages.map(p => `<tr>
      <th scope="row">${escape(p.name)} <span>${escape(p.version)}</span></th>
      <td>${Number.isFinite(p.rust_source_lines) ? number(p.rust_source_lines) : "—"}</td>
      <td>${outcome(p.quill_cargo_parse)}</td>
      <td>${outcome(p.quill_cargo_check_subset, "Failed")}</td>
      <td>Not measured</td>
      <td>${outcome(p.rustc_cargo_check)}</td>
    </tr>`).join(""));
    const binary = report.quill_binary || {};
    const source = binary.source_revision ? `Compiler revision ${binary.source_revision}` : "Compiler source revision unknown";
    $("[data-crate-provenance]").text(
      `Recorded ${report.recorded_at || "date unknown"}. ${source}.${binary.last_modified_utc ? ` Binary last built ${binary.last_modified_utc}.` : ""} These results describe that binary, not necessarily the current compiler.`,
    );
    $("[data-crate-report]").attr("href", "/" + data.crates.path);
  } else {
    $("[data-crate-summary]").text("Crate results have not been published yet.");
    $("[data-crate-table], [data-crate-provenance], [data-crate-report]").remove();
  }

  // Project notes use the same report, rather than keeping another copied summary.
  $(".notes h2")
    .filter((_, el) => $(el).text() === "Current measurements")
    .next("p")
    .text(
      `On the ${number(c.rust_lines)}-line Catalog Flow fixture, median cold-build time is ${ms(medians.quill_cold.wall_seconds)} for Quill and ${ms(medians.rustc_cold.wall_seconds)} for rustc through Cargo. A code change takes ${ms(medians.quill_semantic_incremental.wall_seconds)} versus ${ms(medians.rustc_semantic_incremental.wall_seconds)}. These are medians of ${runs} runs on ${c.cpu_model}.`,
    );
  if ($(".notes").length) {
    const items = data.docs
      .map(
        (doc) =>
          `<li>${link(doc.path, doc.title)} <small>— ${escape(doc.path)}</small></li>`,
      )
      .join("");
    $(".notes").append(
      `<section class="references" aria-labelledby="all-docs-title"><h2 id="all-docs-title">All project documents</h2><ul>${items}</ul></section>`,
    );
    if (data.syntax) {
      const s = data.syntax.value;
      $(".notes").append(
        `<section class="references"><h2>Editor syntax coverage</h2><p>${number(s.cases_checked)} cases checked; ${number(s.mismatch_count)} mismatches. ${number(s.incremental_roundtrips_checked)} incremental round trips; ${number(s.incremental_mismatch_count)} mismatches. Syntax evidence is separate from compiler semantic compatibility.</p><p>${link(data.syntax.path, "Read the syntax report")}</p></section>`,
      );
    }
  }
  return $.html();
}
