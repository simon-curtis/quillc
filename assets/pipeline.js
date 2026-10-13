(() => {
  const track = document.querySelector('.compiler-pipeline');
  if (!track) return;
  const tabs = [...track.querySelectorAll('.pipeline-step')];
  const panels = tabs.map(tab => document.querySelector(tab.getAttribute('href')));
  const reduced = matchMedia('(prefers-reduced-motion: reduce)');
  let active = -1;
  function select(index, focus = false) {
    const changed = active !== index;
    active = index;
    tabs.forEach((tab, position) => {
      const selected = position === index;
      tab.setAttribute('aria-selected', String(selected));
      tab.tabIndex = selected ? 0 : -1;
      panels[position].hidden = !selected;
    });
    if (focus) tabs[index].focus();
    if (changed && index !== -1 && !reduced.matches && panels[index].animate) {
      panels[index].getAnimations().forEach(animation => animation.cancel());
      panels[index].animate([{ opacity: .3, transform: 'translateY(4px)' }, { opacity: 1, transform: 'translateY(0)' }], {
        duration: 250, easing: 'ease-out'
      });
    }
  }
  document.querySelector('.pipeline-panels').classList.add('is-interactive');
  track.setAttribute('role', 'tablist');
  tabs.forEach((tab, index) => {
    tab.setAttribute('role', 'tab');
    tab.setAttribute('aria-controls', panels[index].id);
    panels[index].setAttribute('role', 'tabpanel');
    panels[index].tabIndex = 0;
    tab.addEventListener('click', event => {
      if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
      event.preventDefault();
      select(index);
    });
    tab.addEventListener('keydown', event => {
      let next;
      if (event.key === 'ArrowRight') next = (index + 1) % tabs.length;
      if (event.key === 'ArrowLeft') next = (index + tabs.length - 1) % tabs.length;
      if (event.key === 'Home') next = 0;
      if (event.key === 'End') next = tabs.length - 1;
      if (event.key === ' ') next = index;
      if (next === undefined) return;
      event.preventDefault();
      select(next, true);
    });
  });
  const initial = panels.findIndex(panel => `#${panel.id}` === location.hash);
  select(initial === -1 ? 0 : initial);
  // A stage permalink also works when followed after the page has loaded.
  addEventListener('hashchange', () => {
    const index = panels.findIndex(panel => `#${panel.id}` === location.hash);
    if (index !== -1) select(index);
  });
})();
