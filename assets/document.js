import DOMPurify from 'dompurify';
import { marked } from './vendor/marked.js';

const article = document.querySelector('#document');
const contents = document.querySelector('#contents');
const source = new URL(location.href);
source.hash = '';
source.searchParams.set('raw', '1');
document.querySelector('#raw-link').href = source.href;

try {
  const response = await fetch(source, { headers: { Accept: 'text/plain' } });
  if (!response.ok) throw new Error(`Document unavailable (${response.status})`);
  const markdown = await response.text();
  if (response.headers.get('content-type')?.includes('text/html')) throw new Error('This page needs the Quill Vite+ server: npm run dev');
  article.innerHTML = DOMPurify.sanitize(marked.parse(markdown, { gfm: true }), { USE_PROFILES: { html: true } });
  const ids = new Set();
  for (const heading of article.querySelectorAll('h1,h2,h3,h4,h5,h6')) {
    const title = heading.textContent;
    const base = title.toLowerCase().trim().replace(/[^\p{L}\p{N}\p{M}\s_-]/gu, '').replace(/\s/g, '-') || 'section';
    let id = base, count = 0;
    while (ids.has(id)) id = `${base}-${++count}`;
    ids.add(id);
    heading.id = id;
    const anchor = document.createElement('a');
    anchor.href = `#${encodeURIComponent(id)}`;
    anchor.className = 'heading-anchor';
    anchor.setAttribute('aria-label', `Link to ${title}`);
    anchor.textContent = '#';
    heading.append(anchor);
    if (heading.matches('h2,h3')) {
      const link = document.createElement('a');
      link.href = anchor.href;
      link.textContent = title;
      if (heading.matches('h3')) link.className = 'subheading';
      contents.append(link);
    }
  }
  for (const table of article.querySelectorAll('table')) {
    const wrapper = document.createElement('div');
    wrapper.className = 'markdown-table';
    wrapper.tabIndex = 0;
    wrapper.setAttribute('role', 'region');
    wrapper.setAttribute('aria-label', 'Scrollable table');
    table.before(wrapper);
    wrapper.append(table);
  }
  for (const pre of article.querySelectorAll('pre')) pre.tabIndex = 0;
  // Relative links and image paths naturally resolve against the original .md URL.
  for (const link of article.querySelectorAll('a[href]')) {
    if (link.origin !== location.origin) link.rel = 'noopener noreferrer';
  }
  const firstHeading = article.querySelector('h1');
  document.title = `${firstHeading?.childNodes[0]?.textContent || decodeURIComponent(location.pathname.split('/').pop())} — Quill`;
  if (!contents.children.length) document.querySelector('.document-contents').hidden = true;
  const reducedMotion = matchMedia('(prefers-reduced-motion: reduce)');
  function followHash(animate = false) {
    if (!location.hash) return;
    try {
      document.getElementById(decodeURIComponent(location.hash.slice(1)))?.scrollIntoView({
        behavior: animate && !reducedMotion.matches ? 'smooth' : 'instant',
        block: 'start'
      });
    } catch {}
  }
  contents.addEventListener('click', event => {
    const link = event.target.closest('a[href]');
    if (!link || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    const target = document.getElementById(decodeURIComponent(link.hash.slice(1)));
    if (!target) return;
    event.preventDefault();
    if (location.hash !== link.hash) history.pushState(null, '', link.hash);
    // Move keyboard focus without interrupting the scroll animation.
    target.setAttribute('tabindex', '-1');
    target.focus({ preventScroll: true });
    target.addEventListener('blur', () => target.removeAttribute('tabindex'), { once: true });
    followHash(true);
  });
  await document.fonts.ready;
  followHash();
  window.addEventListener('hashchange', () => followHash(true));
} catch (error) {
  article.replaceChildren();
  const heading = document.createElement('h1');
  heading.textContent = 'Unable to open this document.';
  const message = document.createElement('p');
  message.textContent = error.message;
  article.append(heading, message);
  document.querySelector('.document-contents').hidden = true;
} finally {
  article.setAttribute('aria-busy', 'false');
}
