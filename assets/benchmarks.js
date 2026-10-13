// Published medians from benchmarks/catalog-flow/results.json.
// Keep the HTML time table usable if scripts or the measurement file are unavailable.
(async () => {
  const buttons = [...document.querySelectorAll('[data-metric]')];
  const metrics = {
    wall: { key: 'wall_seconds', label: 'Median build time · lower is better', heading: 'Speedup', format: v => `${Math.round(v * 1000).toLocaleString('en-US')} ms` },
    memory: { key: 'peak_process_tree_rss_mib', label: 'Peak process-tree RSS · median of three runs', heading: 'Less memory', format: v => `${v.toFixed(1)} MiB` },
    cpu: { key: 'cpu_seconds', label: 'Aggregate CPU time · median of three runs', heading: 'Less CPU', format: v => `${Math.round(v * 1000).toLocaleString('en-US')} ms` }
  };
  try {
    const response = await fetch('benchmarks/catalog-flow/results.json');
    if (!response.ok) throw new Error('Measurements unavailable');
    const { medians, results } = await response.json();
    const runs = results.filter(result => result.label === 'quill_cold').length;
    metrics.memory.label = `Peak process-tree RSS · median of ${runs} runs`;
    metrics.cpu.label = `Aggregate CPU time · median of ${runs} runs`;
    const rows = [...document.querySelectorAll('[data-build]')];
    for (const row of rows) {
      for (const compiler of ['quill', 'rustc']) {
        for (const metric of Object.values(metrics)) {
          if (!Number.isFinite(medians[`${compiler}_${row.dataset.build}`]?.[metric.key])) throw new Error('Incomplete measurements');
        }
      }
    }
    function show(name) {
      const metric = metrics[name];
      buttons.forEach(button => button.setAttribute('aria-pressed', String(button.dataset.metric === name)));
      document.querySelector('#measurement-label').textContent = metric.label;
      document.querySelector('.ratio-heading').textContent = metric.heading;
      for (const row of rows) {
        const q = medians[`quill_${row.dataset.build}`][metric.key];
        const r = medians[`rustc_${row.dataset.build}`][metric.key];
        row.querySelector('.quill-value').textContent = metric.format(q);
        row.querySelector('.rustc-value').textContent = metric.format(r);
        row.querySelector('.ratio-value').textContent = name === 'wall' ? `${(r / q).toFixed(2)}×` : `${((1 - q / r) * 100).toFixed(1)}%`;
      }
    }
    buttons.forEach(button => button.addEventListener('click', () => show(button.dataset.metric)));
    document.querySelector('.metric-switch').classList.add('ready');
    show('wall');
  } catch {
    // The source-linked, static time comparison remains readable.
  }
})();
