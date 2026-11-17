(() => {
  const visible = el => !!el && el.getClientRects().length > 0 && !el.closest('[hidden],[aria-hidden="true"]');
  const text = el => (el?.textContent || '').replace(/\s+/g, ' ').trim().slice(0, 512);
  if ((document.title || '').toLowerCase().includes('just a moment')) return { state: 'verification' };
  if (/\/login/i.test(location.pathname)) return { state: 'auth_required' };
  const main = document.querySelector('main') || document.body;
  const raw = text(main);
  if (!raw || /loading/i.test(raw) && raw.length < 40) return { state: 'loading' };
  if (/\/upgrade/i.test(location.pathname)) {
    return {
      state: 'ready',
      usage: {
        plan: 'Free',
        limits: [],
        updated_label: '',
        timezone: Intl.DateTimeFormat().resolvedOptions().timeZone || '',
        notice: 'Claude Free plan redirected to /upgrade; no usage meter available.'
      }
    };
  }
  const matches = [...document.querySelectorAll('section, div, p, span')].filter(visible)
    .filter(el => el.children.length <= 4 && /\d+%/.test(el.textContent || '') && !/save\s+\d+%/i.test(el.textContent || ''))
    .filter(el => /used|limit|reset|session|weekly|remaining/i.test(el.textContent || ''))
    .slice(0, 6)
    .map(el => {
      const t = text(el);
      const pct = t.match(/(\d+(?:\.\d+)?)\s*%/);
      const used_percent = pct ? Number(pct[1]) : null;
      return {
        label: t.slice(0, 80),
        used: pct ? pct[0] : null,
        remaining: null,
        limit: null,
        reset: null,
        used_percent,
        status: used_percent === 100 ? 'exhausted' : 'reported',
        detail: t
      };
    });
  if (!matches.length) return { state: 'loading', page_present: true };
  return {
    state: 'ready',
    usage: {
      plan: null,
      limits: matches,
      updated_label: '',
      timezone: Intl.DateTimeFormat().resolvedOptions().timeZone || '',
      notice: 'Observed directly from https://claude.ai/settings/usage.'
    }
  };
})()
