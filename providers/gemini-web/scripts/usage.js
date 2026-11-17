(() => {
  // Read only the account's usage island. Conversation history and profile
  // menus are deliberately outside this contract.
  const visible = el => !!el && el.getClientRects().length > 0 && !el.closest('[hidden],[aria-hidden="true"]') && getComputedStyle(el).visibility === 'visible';
  const text = el => (el?.textContent || '').replace(/[\u0000-\u001f\u007f-\u009f]/g, ' ').replace(/\s+/g, ' ').trim().slice(0, 512);
  const root = [...document.querySelectorAll('[data-usage-island-root]')].find(visible);
  if (location.pathname.startsWith('/sorry') || [...document.querySelectorAll('iframe[src*="recaptcha"][src*="bframe"], iframe[src*="challenge"], #captcha, form[action*="/sorry"]')].some(visible)) {
    return {state: 'verification'};
  }
  if (!root) {
    const signIn = [...document.querySelectorAll('a[href*="accounts.google.com/ServiceLogin"], a[href*="accounts.google.com/signin"]')].some(visible);
    return {state: signIn ? 'auth_required' : 'loading'};
  }
  if ([...root.querySelectorAll('[aria-busy="true"], [role="progressbar"]:not([aria-valuenow])')].some(visible)) return {state: 'loading'};
  const percent = value => {
    // Accept a displayed percentage, never an inferred count or CSS width.
    // Bounds such as "<1%" stay as text rather than becoming an exact number.
    const normalized = value.normalize('NFKC').replace(/[٠-٩۰-۹]/g, d => String(d.charCodeAt(0) - (d <= '٩' ? 0x660 : 0x6f0))).replace(/٫/g, '.');
    const match = normalized.match(/^(\d+(?:[.,]\d+)?)\s*[%٪]/);
    const number = match ? Number(match[1].replace(',', '.')) : NaN;
    return Number.isFinite(number) && number >= 0 && number <= 100 ? number : null;
  };
  const rows = [...root.querySelectorAll('li[data-testid$="-usage"]')].filter(visible);
  const limits = rows.map(row => {
    const strong = [...row.querySelectorAll('strong')].filter(visible);
    const label = text(strong[0]);
    const used = text(strong[1]);
    const reset = [...row.querySelectorAll('span,time,p')].filter(visible).filter(el =>
      !el.children.length && !el.closest('button,strong,[aria-hidden="true"]') && text(el)
    ).map(text).join(' ').slice(0, 512);
    const used_percent = percent(used);
    return {label, used: used || null, remaining: null, limit: null, reset: reset || null,
      used_percent, status: used_percent === 100 ? 'exhausted' : 'reported', detail: ''};
  }).filter(row => row.label && row.used);
  if (!limits.length) return {state: 'loading', page_present: true};
  const heading = root.querySelector('header h1');
  const plan = text(heading?.parentElement?.querySelector('span')) || null;
  return {state: 'ready', usage: {
    plan, limits, updated_label: text(root.querySelector('[data-testid="usage-updated"]')),
    timezone: Intl.DateTimeFormat().resolvedOptions().timeZone || '',
    notice: 'The website reports usage percentages, not exact remaining messages or tokens.'
  }};
})()
