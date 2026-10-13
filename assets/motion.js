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
  const selectors = '.hero-copy > *, .quickstart-copy, .quickstart-commands, .project-overview > h2, .overview-columns > div, .approach-title, .approach-copy, .performance-intro > *, .measurement-controls, .benchmark-table, .benchmark-context, .stack > div, .stack-list > div, .inspiration-intro > *, .inspiration-list > div, .inspiration-footnote, .development-heading, .development-copy, .footer';
  document.querySelectorAll(selectors).forEach(element => entrances.observe(element));
  document.querySelectorAll('.hero-copy > *, .stack-list > div').forEach((element, index) => {
    element.dataset.entranceDelay = String((index % 3) * 70);
  });

  // Artwork follows scrolling slightly. No continuous redraw or idle animation.
  const artwork = [...document.querySelectorAll('.print-transition, .development-print')];
  const visible = new Set();
  let frame = 0;
  function draw() {
    frame = 0;
    if (reduced.matches || document.hidden) return;
    const height = innerHeight;
    for (const holder of visible) {
      const box = holder.getBoundingClientRect();
      const progress = Math.max(-1, Math.min(1, (height / 2 - box.top - box.height / 2) / (height / 2 + box.height / 2)));
      const art = holder.querySelector('canvas');
      if (!art) continue;
      art.style.transform = `translateY(${progress * -18}px) scale(1.035)`;
    }
  }
  function schedule() {
    if (!frame && !reduced.matches && !document.hidden && visible.size) frame = requestAnimationFrame(draw);
  }
  const visibility = new IntersectionObserver(entries => {
    entries.forEach(entry => entry.isIntersecting ? visible.add(entry.target) : visible.delete(entry.target));
    schedule();
  });
  artwork.forEach(element => visibility.observe(element));
  addEventListener('scroll', schedule, { passive: true });
  addEventListener('resize', schedule, { passive: true });
  document.addEventListener('visibilitychange', schedule);

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
      cancelAnimationFrame(frame);
      frame = 0;
      artwork.forEach(element => element.querySelector('canvas')?.style.removeProperty('transform'));
    } else schedule();
  });
})();
