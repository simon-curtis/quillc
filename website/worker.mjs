// Keep document reader URLs equivalent to the local Vite+ behavior.
// Missing assets continue to the existing blog through a service binding.
export default {
  async fetch(request, env) {
    if (!['GET', 'HEAD'].includes(request.method)) {
      return env.ORIGINAL_SITE.fetch(request);
    }
    const url = new URL(request.url);
    if (url.pathname === '/') {
      return env.ASSETS.fetch(new Request(new URL('/index.html', url), {
        method: request.method,
        headers: request.headers,
      }));
    }
    if (/\.(md|json)$/i.test(url.pathname) && !['/site-data.json', '/project-status.json'].includes(url.pathname)) {
      const markdown = await env.ASSETS.fetch(request);
      if (!markdown.ok) return markdown;
      if (url.searchParams.get('raw') === '1') {
        const headers = new Headers(markdown.headers);
        headers.set('Content-Type', 'text/plain; charset=utf-8');
        return new Response(markdown.body, { status: markdown.status, headers });
      }
      const document = new Request(new URL('/document.html', url), {
        method: request.method,
        headers: request.headers,
      });
      return env.ASSETS.fetch(document);
    }
    const asset = await env.ASSETS.fetch(request);
    return asset.status === 404 ? env.ORIGINAL_SITE.fetch(request) : asset;
  },
};
