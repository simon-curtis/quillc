(() => {
  'use strict';
  const reduced = matchMedia('(prefers-reduced-motion: reduce)');
  const animations = new Set();
  const seen = new WeakSet();
  const ease = 'cubic-bezier(.22,1,.36,1)';
  function animate(element, frames, options) {
    if (reduced.matches || !element.animate) return;
    const animation = element.animate(frames, options);
    animations.add(animation);
    const release = () => animations.delete(animation);
    animation.addEventListener('finish', release, { once: true });
    animation.addEventListener('cancel', release, { once: true });
  }

  // Content remains visible before JS runs and if animation is unavailable.
  const entrances = new IntersectionObserver(entries => {
    for (const entry of entries) {
      if (!entry.isIntersecting || seen.has(entry.target)) continue;
      seen.add(entry.target);
      entrances.unobserve(entry.target);
      animate(entry.target, [
        { opacity: 0, transform: 'translateY(22px)' },
        { opacity: 1, transform: 'translateY(0)' }
      ], { duration: 850, easing: ease, delay: Number(entry.target.dataset.entranceDelay || 0) });
    }
  }, { threshold: .08 });
  const selectors = '.hero-copy > *, .quickstart-copy, .quickstart-commands, .project-overview > h2, .overview-columns > div, .approach > *, .performance-intro > *, .measurement-controls, .benchmark-table, .benchmark-context, .lsp-heading > *, .lsp-measurement, .lsp-detail, .stack-heading, .compiler-pipeline, .pipeline-panels, .stack-footer, .inspiration-intro > *, .inspiration-print, .inspiration-list > div, .inspiration-reading > .text-link, .footer';
  document.querySelectorAll(selectors).forEach(element => entrances.observe(element));
  document.querySelectorAll('.hero-copy > *').forEach((element, index) => {
    element.dataset.entranceDelay = String((index % 3) * 70);
  });

  document.querySelector('.metric-switch')?.addEventListener('click', event => {
    if (!event.target.closest('button')) return;
    document.querySelectorAll('.benchmark-table tbody tr').forEach((row, index) => {
      for (const cell of row.querySelectorAll('td')) {
        cell.getAnimations().forEach(animation => animation.cancel());
        animate(cell, [{ opacity: .25, transform: 'translateY(5px)' }, { opacity: 1, transform: 'translateY(0)' }], {
          duration: 380, delay: index * 35, easing: ease
        });
      }
    });
  });
  reduced.addEventListener('change', () => {
    if (reduced.matches) {
      animations.forEach(animation => animation.cancel());
    }
  });
})();
