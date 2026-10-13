(() => {
  const form = document.querySelector('.compatibility-filters');
  const search = document.querySelector('#feature-search');
  const status = document.querySelector('#feature-status');
  const rows = [...document.querySelectorAll('.compatibility-row')];
  const count = document.querySelector('#feature-count');
  form.hidden = false;
  form.addEventListener('submit', event => event.preventDefault());
  function filter() {
    const terms = search.value.trim().toLowerCase().split(/\s+/).filter(Boolean);
    let shown = 0;
    for (const row of rows) {
      row.hidden = !(status.value === 'all' || row.dataset.status === status.value) || !terms.every(term => row.textContent.toLowerCase().includes(term));
      if (!row.hidden) shown++;
    }
    count.textContent = `${shown} of ${rows.length} entries`;
    document.querySelector('#feature-empty').hidden = shown !== 0;
  }
  search.addEventListener('input', filter);
  status.addEventListener('change', filter);
})();
