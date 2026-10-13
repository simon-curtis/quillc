(() => {
  const repo = 'https://github.com/simon-curtis/quillc';
  async function updateCoverage() {
    try {
      const response = await fetch('/compatibility.html');
      if (!response.ok) return;
      const index = new DOMParser().parseFromString(await response.text(), 'text/html');
      const entries = [...index.querySelectorAll('.compatibility-row[data-status]')];
      if (!entries.length) return;
      const counts = { available: 0, partial: 0, planned: 0, unassessed: 0 };
      for (const entry of entries) {
        if (!(entry.dataset.status in counts)) return;
        counts[entry.dataset.status]++;
      }
      document.querySelector('#compatibility-score').textContent = (counts.available / entries.length * 100).toFixed(1);
      const bar = document.querySelector('#coverage-bar');
      for (const [status, count] of Object.entries(counts)) {
        const segment = bar.querySelector(`.coverage-${status}`);
        segment.style.flex = String(count);
        segment.hidden = count === 0;
      }
      const summary = `${counts.available} available · ${counts.partial} partial · ${counts.planned} not yet · ${counts.unassessed} unassessed`;
      document.querySelector('#coverage-counts').textContent = summary;
      bar.setAttribute('aria-label', `${entries.length} tracked entries: ${summary}`);
    } catch {
      // The documented snapshot in the HTML remains readable offline.
    }
  }
  async function updateRevision() {
    try {
      const response = await fetch('/project-status.json', { cache: 'no-store' });
      if (!response.ok) return;
      const { revision } = await response.json();
      if (revision === null) {
        document.querySelector('#checkout-revision').textContent = 'Uncommitted';
        return;
      }
      if (!/^[a-f0-9]{40}$/.test(revision || '')) return;
      const link = document.createElement('a');
      link.href = `${repo}/commit/${revision}`;
      link.textContent = revision.slice(0, 8);
      link.title = revision;
      link.setAttribute('aria-label', `Current commit ${revision}`);
      document.querySelector('#checkout-revision').replaceChildren(link);
    } catch {
      // An uncommitted checkout has no revision to link.
    }
  }
  updateCoverage();
  updateRevision();
})();
