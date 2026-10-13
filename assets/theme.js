(() => {
  const key = 'quill-theme';
  const modes = ['system', 'dark', 'light'];
  const system = matchMedia('(prefers-color-scheme: dark)');
  const buttons = [...document.querySelectorAll('.theme-toggle')];
  let preference = readPreference();

  function readPreference() {
    try {
      const stored = localStorage.getItem(key);
      return modes.includes(stored) ? stored : 'system';
    } catch {
      return 'system';
    }
  }

  function apply() {
    const theme = preference === 'system' ? (system.matches ? 'dark' : 'light') : preference;
    document.documentElement.dataset.theme = theme;
    document.querySelector('meta[name="theme-color"]')?.setAttribute(
      'content', theme === 'dark' ? '#1b201d' : '#f6f3ec',
    );
    const next = modes[(modes.indexOf(preference) + 1) % modes.length];
    for (const button of buttons) {
      button.hidden = false;
      button.querySelector('[data-theme-label]').textContent = preference[0].toUpperCase() + preference.slice(1);
      const label = `Color theme: ${preference}. Change to ${next} theme`;
      button.setAttribute('aria-label', label);
      button.title = label;
    }
    document.dispatchEvent(new Event('quill:themechange'));
  }

  for (const button of buttons) button.addEventListener('click', () => {
    preference = modes[(modes.indexOf(preference) + 1) % modes.length];
    try { localStorage.setItem(key, preference); } catch {}
    apply();
  });
  system.addEventListener('change', () => {
    if (preference === 'system') apply();
  });
  window.addEventListener('storage', event => {
    if (event.key === key || event.key === null) {
      preference = readPreference();
      apply();
    }
  });
  apply();
})();
