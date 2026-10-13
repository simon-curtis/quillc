import assert from 'node:assert/strict';
import { test } from 'node:test';
import worker from './worker.mjs';

function environment() {
  const forwarded = [];
  return {
    forwarded,
    ASSETS: {
      async fetch(request) {
        const path = new URL(request.url).pathname;
        const files = { '/README.md': '# Quill', '/document.html': '<main id="document"></main>', '/index.html': '<h1>Quill</h1>' };
        return new Response(request.method === 'HEAD' ? null : files[path] ?? 'Not found', {
          status: path in files ? 200 : 404,
          headers: { 'Content-Type': path.endsWith('.html') ? 'text/html' : 'text/markdown' },
        });
      },
    },
    ORIGINAL_SITE: {
      async fetch(request) {
        forwarded.push(request);
        return new Response('Original blog');
      },
    },
  };
}

test('Markdown URLs serve the reader and raw URLs serve the source', async () => {
  const env = environment();
  const reader = await worker.fetch(new Request('https://quill.example/README.md'), env);
  assert.match(reader.headers.get('Content-Type'), /text\/html/);
  assert.match(await reader.text(), /id="document"/);
  const raw = await worker.fetch(new Request('https://quill.example/README.md?raw=1'), env);
  assert.match(raw.headers.get('Content-Type'), /text\/plain/);
  assert.equal(await raw.text(), '# Quill');
  assert.equal(env.forwarded.length, 0);
});

test('a missing Markdown file remains a 404', async () => {
  const env = environment();
  assert.equal((await worker.fetch(new Request('https://quill.example/missing.md'), env)).status, 404);
  assert.equal(env.forwarded.length, 0);
});

test('existing blog requests retain the URL, method, body, and cookies', async () => {
  const env = environment();
  const get = new Request('https://quill.example/blog?search=rust', { headers: { Cookie: 'session=example' } });
  assert.equal(await (await worker.fetch(get, env)).text(), 'Original blog');
  assert.equal(env.forwarded[0], get);
  const post = new Request('https://quill.example/admin/login', { method: 'POST', body: 'login=example' });
  await worker.fetch(post, env);
  assert.equal(env.forwarded[1], post);
  assert.equal(await env.forwarded[1].text(), 'login=example');
});

test('static assets and Markdown HEAD requests do not go to the blog', async () => {
  const env = environment();
  assert.equal(await (await worker.fetch(new Request('https://quill.example/'), env)).text(), '<h1>Quill</h1>');
  assert.equal(await (await worker.fetch(new Request('https://quill.example/index.html'), env)).text(), '<h1>Quill</h1>');
  const head = await worker.fetch(new Request('https://quill.example/README.md', { method: 'HEAD' }), env);
  assert.equal(head.status, 200);
  assert.equal(await head.text(), '');
  assert.equal(env.forwarded.length, 0);
});
