// Read-only sidebar pagination. Completion is established from the observed
// response cursor in the host, never from a stable number of DOM rows.
(args) => {
  const visible = e => !!e && e.isConnected && e.getClientRects().length > 0
    && getComputedStyle(e).visibility !== 'hidden' && getComputedStyle(e).display !== 'none';
  const here = new URL(location.href);
  if ((/(^|\.)google\.com$/.test(here.hostname) && /^\/sorry(?:\/|$)/.test(here.pathname))
      || [...document.querySelectorAll(
        'iframe[src*="recaptcha"][src*="bframe"], iframe[src*="challenge"], #captcha, form[action*="/sorry"]')].some(visible))
    return {state: 'verification_required'};
  if (here.hostname === 'accounts.google.com') return {state: 'auth_required'};
  if (here.origin !== 'https://gemini.google.com') return {state: 'waiting'};
  const list = document.querySelector('conversations-list[data-test-id="all-conversations"]');
  const expand = document.querySelector('[data-test-id="side-nav-sparkle-button"]');
  if ((!list || !visible(list)) && visible(expand) && expand.querySelector('[fonticon="side_nav_expand"]')) expand.click();
  const urls = [];
  for (const a of document.querySelectorAll('conversations-list [data-test-id="conversation"] a[href]')) {
    const u = new URL(a.getAttribute('href'), location.href);
    if (u.origin === here.origin && /^\/(?:u\/\d+\/)?app\/[A-Za-z0-9_-]+$/.test(u.pathname)) {
      if (!u.searchParams.has('authuser') && new URL(location.href).searchParams.has('authuser'))
        u.searchParams.set('authuser', new URL(location.href).searchParams.get('authuser'));
      u.hash = '';
      urls.push(u.href);
    }
  }
  if (args.scroll && list) {
    const scroller = list.closest('infinite-scroller') || list.querySelector('infinite-scroller');
    if (scroller) {
      scroller.scrollTop = scroller.scrollHeight;
      scroller.dispatchEvent(new Event('scroll'));
    }
  }
  return {state: list ? 'ready' : 'waiting', url: location.href, urls};
}
