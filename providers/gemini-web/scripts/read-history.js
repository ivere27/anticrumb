// Refresh the observed read-only history RPC in the existing maintenance page.
// Rust validates and strips other operations from the captured UI batch.
async (args) => {
  const scope = u => {
    if (u.origin !== 'https://gemini.google.com') return null;
    const path = u.pathname.match(/^\/u\/(\d+)\//)?.[1];
    const query = u.searchParams.get('authuser');
    if (path && query !== null && path !== query) return null;
    return query ?? path ?? '0';
  };
  const access = () => {
    const here = new URL(location.href);
    const visible = e => e.getClientRects().length > 0
      && getComputedStyle(e).visibility !== 'hidden' && getComputedStyle(e).display !== 'none';
    if (here.pathname.startsWith('/sorry') || [...document.querySelectorAll(
        'iframe[src*="recaptcha"][src*="bframe"], iframe[src*="challenge"], #captcha, form[action*="/sorry"]')].some(visible))
      return 'verification_required';
    if (here.hostname === 'accounts.google.com') return 'auth_required';
    if (scope(here) !== args.scope || !/^(?:\/u\/\d+)?\/app(?:\/[A-Za-z0-9_-]+)?\/?$/.test(here.pathname))
      return 'error';
    return null;
  };
  const blocked = access();
  if (blocked) return {state: blocked};
  const endpoint = new URL(args.url);
  if (scope(endpoint) !== args.scope || !endpoint.pathname.endsWith('/batchexecute')
      || endpoint.searchParams.get('rpcids') !== 'MaZiqc') return {state: 'error'};
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), args.timeoutMs);
  try {
    const response = await fetch(endpoint.href, {
      method: 'POST', credentials: 'same-origin', cache: 'no-store', redirect: 'error',
      headers: {'Content-Type': 'application/x-www-form-urlencoded;charset=UTF-8'},
      body: args.post, signal: controller.signal,
    });
    const blocked = access();
    if (blocked) return {state: blocked};
    if (response.status === 401 || response.status === 403) return {state: 'auth_required'};
    if ([408, 500, 502, 503, 504].includes(response.status)) return {state: 'transport_error'};
    if (response.status !== 200 || scope(new URL(response.url)) !== args.scope) return {state: 'error'};
    return {state: 'ready', url: response.url, body: await response.text()};
  } catch {
    return {state: access() || 'transport_error'};
  } finally {
    clearTimeout(timer);
  }
}
