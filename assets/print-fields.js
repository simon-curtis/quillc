(() => {
  'use strict';
  // Varying dot area creates the tonal surface; divider waves drift slowly.
  // Coordinates stay in CSS pixels so the screen retains its printed scale.
  const clamp = value => Math.max(0, Math.min(1, value));
  const grids = new WeakMap();
  const reduced = matchMedia('(prefers-reduced-motion: reduce)');
  let waveTime = 0;
  let ink, landscape;
  function palette() {
    const style = getComputedStyle(document.documentElement);
    ink = style.getPropertyValue('--print-ink').trim();
    landscape = style.getPropertyValue('--print-landscape').trim();
  }
  palette();
  document.addEventListener('quill:themechange', () => {
    palette();
    document.querySelectorAll('[data-print]').forEach(print);
  });
  function print(canvas) {
    const box = canvas.getBoundingClientRect();
    if (!box.width || !box.height) return;
    const scale = Math.min(devicePixelRatio || 1, 2);
    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    const w = box.width, h = box.height;
    const kind = canvas.dataset.print;
    const pitch = 4;
    let grid = grids.get(canvas);
    if (!grid || grid.w !== w || grid.h !== h || grid.scale !== scale) {
      canvas.width = Math.round(w * scale);
      canvas.height = Math.round(h * scale);
      const angle = Math.PI / 12;
      const cos = Math.cos(angle), sin = Math.sin(angle);
      const diagonal = Math.hypot(w, h);
      const points = [];
      for (let gy = -diagonal; gy < diagonal; gy += pitch) {
        for (let gx = -diagonal; gx < diagonal; gx += pitch) {
          const x = gx * cos - gy * sin + w / 2;
          const y = gx * sin + gy * cos + h / 2;
          if (x < -pitch || x > w + pitch || y < -pitch || y > h + pitch) continue;
          const grain = .96 + .08 * (Math.sin(gx * 12.9898 + gy * 78.233) * 43758.5453 % 1);
          points.push({ x, y, grain });
        }
      }
      grid = { w, h, scale, points };
      grids.set(canvas, grid);
    }
    ctx.setTransform(scale, 0, 0, scale, 0, 0);
    ctx.clearRect(0, 0, w, h);
    ctx.fillStyle = kind === 'landscape' ? landscape : ink;
    ctx.beginPath();
    for (const { x, y, grain } of grid.points) {
      const u = x / w, v = y / h;
      let tone;
      if (kind === 'divider') {
        const phase = Number(canvas.dataset.phase || 0);
        const density = .08 + .16 * Math.pow(Math.sin(u * 3.2 + phase), 2);
        // A single shallow wave within the existing compact impression.
        const center = .5 + .12 * Math.sin(u * Math.PI * 2 + phase * .25 - (reduced.matches ? 0 : waveTime));
        tone = density * Math.exp(-Math.pow((v - center) / .18, 2) * 2);
        tone *= Math.pow(Math.sin(Math.PI * clamp(u)), .35);
      } else if (kind === 'ribbon') {
        const center = .5 + .22 * Math.sin(u * 6.4 - .8);
        const width = .13 + .085 * Math.sin(u * 4 + .5);
        const distance = (v - center) / width;
        tone = Math.exp(-distance * distance * 1.6) * (.35 + .5 * Math.pow(Math.sin(u * 5.8 + .2), 2));
        tone *= Math.pow(Math.sin(Math.PI * clamp(u)), .45);
      } else {
        const ridge = .51 + .16 * Math.sin(u * 7 + .4) + .08 * Math.sin(u * 15);
        const distance = v - ridge;
        tone = clamp(distance * 1.7 + .25) * (.35 + .4 * Math.pow(Math.sin(u * 6 - v * 3), 2));
        tone += .13 * Math.exp(-Math.pow((v - ridge + .22) / .16, 2));
        tone *= clamp(v * 5);
      }
      const radius = pitch * .57 * Math.sqrt(clamp(tone)) * grain;
      if (radius < .16) continue;
      ctx.moveTo(x + radius, y);
      ctx.arc(x, y, radius, 0, Math.PI * 2);
    }
    ctx.fill();
  }
  const visible = new Set();
  let animationFrame = 0;
  let lastTick = 0;
  function tick(now) {
    animationFrame = 0;
    if (reduced.matches || document.hidden || !visible.size) return;
    if (!lastTick) lastTick = now;
    if (now - lastTick >= 1000 / 24) {
      // One gentle travelling wave every twelve seconds.
      waveTime += (now - lastTick) * Math.PI * 2 / 12000;
      lastTick = now;
      visible.forEach(print);
    }
    animationFrame = requestAnimationFrame(tick);
  }
  function updateMotion() {
    cancelAnimationFrame(animationFrame);
    animationFrame = 0;
    lastTick = 0;
    if (!reduced.matches && !document.hidden && visible.size) {
      animationFrame = requestAnimationFrame(tick);
    }
  }
  const visibility = new IntersectionObserver(entries => {
    for (const entry of entries) {
      if (entry.isIntersecting) visible.add(entry.target);
      else visible.delete(entry.target);
    }
    updateMotion();
  });
  reduced.addEventListener('change', () => {
    document.querySelectorAll('[data-print="divider"]').forEach(print);
    updateMotion();
  });
  document.addEventListener('visibilitychange', updateMotion);
  for (const canvas of document.querySelectorAll('[data-print]')) {
    if (canvas.dataset.print === 'divider') visibility.observe(canvas);
    let frame;
    const observer = new ResizeObserver(() => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => print(canvas));
    });
    observer.observe(canvas);
  }
})();
