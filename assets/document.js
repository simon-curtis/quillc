import DOMPurify from 'dompurify';
import { marked } from './vendor/marked.js';

const article = document.querySelector('#document');
const contents = document.querySelector('#contents');
const source = new URL(location.href);
source.hash = '';
source.searchParams.set('raw', '1');
document.querySelector('#raw-link').href = source.href;
const isJson = location.pathname.toLowerCase().endsWith('.json');
document.querySelector('#raw-link').firstChild.textContent = isJson ? 'View raw data ' : 'View raw Markdown ';

try {
  const response = await fetch(source, { headers: { Accept: 'text/plain' } });
  if (!response.ok) throw new Error(`Document unavailable (${response.status})`);
  const content = await response.text();
  if (response.headers.get('content-type')?.includes('text/html')) throw new Error('This page needs the Quill Vite+ server: npm run dev');
  if (isJson) {
    const value = JSON.parse(content);
    const heading = document.createElement('h1');
    heading.textContent = decodeURIComponent(location.pathname.split('/').pop());
    article.classList.add('json-document');
    const tree = document.createElement('ul');
    tree.className = 'json-tree';

    function appendValue(parent, key, item, root = false) {
      const row = document.createElement('li');
      const container = item !== null && typeof item === 'object';
      if (container) {
        const array = Array.isArray(item);
        const entries = array ? item : Object.entries(item);
        const details = document.createElement('details');
        details.className = 'json-node';
        details.open = root;
        const summary = document.createElement('summary');
        summary.className = 'json-summary';
        if (!root) {
          const property = document.createElement('span');
          property.className = 'json-key';
          property.textContent = key;
          summary.append(property);
        }
        const type = document.createElement('span');
        type.className = 'json-type';
        const count = array ? item.length : entries.length;
        type.textContent = `${array ? 'Array' : 'Object'} · ${count} ${array ? 'items' : 'keys'}`;
        summary.append(type);
        details.append(summary);
        const children = document.createElement('ul');
        children.className = 'json-children';
        details.append(children);
        const populate = () => {
          if (details.dataset.populated) return;
          details.dataset.populated = 'true';
          if (array) item.forEach((child, index) => appendValue(children, `[${index}]`, child));
          else for (const [name, child] of entries) appendValue(children, name, child);
        };
        details.addEventListener('toggle', () => {
          if (details.open) populate();
        });
        if (root) populate();
        row.append(details);
      } else {
        const line = document.createElement('div');
        line.className = 'json-value-row';
        const property = document.createElement('span');
        property.className = 'json-key';
        property.textContent = key;
        const value = document.createElement('span');
        value.className = `json-value json-${item === null ? 'null' : typeof item}`;
        value.textContent = typeof item === 'string' ? JSON.stringify(item) : String(item);
        if (!root) line.append(property);
        line.append(value);
        row.append(line);
      }
      parent.append(row);
    }

    appendValue(tree, '', value, true);
    article.replaceChildren(heading, tree);
  } else {
    article.innerHTML = DOMPurify.sanitize(marked.parse(content, { gfm: true }), { USE_PROFILES: { html: true } });
  }
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
