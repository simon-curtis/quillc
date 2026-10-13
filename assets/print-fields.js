(() => {
  'use strict';
  // One impression per resize: varying dot area creates the tonal surface.
  // Coordinates stay in CSS pixels so the screen retains its printed scale.
  const clamp = value => Math.max(0, Math.min(1, value));
  function print(canvas) {
    const box = canvas.getBoundingClientRect();
    if (!box.width || !box.height) return;
    const scale = Math.min(devicePixelRatio || 1, 2);
    canvas.width = Math.round(box.width * scale);
    canvas.height = Math.round(box.height * scale);
    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    ctx.scale(scale, scale);
    const w = box.width, h = box.height;
    const kind = canvas.dataset.print;
    const pitch = 4;
    const angle = Math.PI / 12;
    const cos = Math.cos(angle), sin = Math.sin(angle);
    const diagonal = Math.hypot(w, h);
    ctx.fillStyle = kind === 'landscape' ? '#c4d2b3' : '#ba482f';
    ctx.beginPath();
    for (let gy = -diagonal; gy < diagonal; gy += pitch) {
      for (let gx = -diagonal; gx < diagonal; gx += pitch) {
        const x = gx * cos - gy * sin + w / 2;
        const y = gx * sin + gy * cos + h / 2;
        if (x < -pitch || x > w + pitch || y < -pitch || y > h + pitch) continue;
        const u = x / w, v = y / h;
        let tone;
        if (kind === 'divider') {
          const phase = Number(canvas.dataset.phase || 0);
          const density = .08 + .26 * Math.pow(Math.sin(u * 3.2 + phase), 2);
          const center = .5 + .045 * Math.sin(u * 11 + phase);
          tone = density * Math.exp(-Math.pow((v - center) / .23, 2) * 2);
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
        const grain = .96 + .08 * (Math.sin(gx * 12.9898 + gy * 78.233) * 43758.5453 % 1);
        const radius = pitch * .57 * Math.sqrt(clamp(tone)) * grain;
        if (radius < .16) continue;
        ctx.moveTo(x + radius, y);
        ctx.arc(x, y, radius, 0, Math.PI * 2);
      }
    }
    ctx.fill();
  }
  for (const canvas of document.querySelectorAll('[data-print]')) {
    let frame;
    const observer = new ResizeObserver(() => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => print(canvas));
    });
    observer.observe(canvas);
  }
})();
