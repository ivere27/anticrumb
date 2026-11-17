// Destructive controls are scoped to an exact recorded conversation link.
// The cleanup worker reuses one maintenance page for successive deletions.
(args) => {
  const visible = e => !!e && e.isConnected && e.getClientRects().length > 0
    && getComputedStyle(e).visibility !== 'hidden' && getComputedStyle(e).display !== 'none';
  const findVisible = (root, selector) => [...root.querySelectorAll(selector)].filter(visible);
  const expected = new URL(args.url);
  const here = new URL(location.href);
  if (here.pathname.startsWith('/sorry') || findVisible(document,
      'iframe[src*="recaptcha"][src*="bframe"], iframe[src*="challenge"], #captcha, form[action*="/sorry"]').length)
    return {state: 'verification_required'};
  // Target.create can return before the first navigation commits.
  if (here.href === 'about:blank' && !window.__anticrumbChatDeletion)
    return {state: 'waiting', step: 'initial conversation navigation'};
  if (here.hostname === 'accounts.google.com') return {state: 'auth_required'};
  if (here.origin !== 'https://gemini.google.com' || expected.origin !== here.origin)
    return {state: 'error', error: 'cleanup tab is not on the expected Gemini origin'};
  const account = u => {
    const match = u.pathname.match(/^(?:\/u\/(\d+))?\/app(?:\/[A-Za-z0-9_-]+)?\/?$/);
    if (!match) return null;
    const query = u.searchParams.get('authuser');
    if (match[1] && query !== null && match[1] !== query) return null;
    return query ?? match[1] ?? '0';
  };
  if (account(here) === null || account(here) !== account(expected))
    return {state: 'error', error: 'cleanup tab switched Google accounts'};
  const chatId = path => {
    const segs = path.split('/').filter(Boolean);
    if (segs.length === 2 && segs[0] === 'app') return segs[1];
    if (segs.length === 4 && segs[0] === 'u' && /^\d+$/.test(segs[1]) && segs[2] === 'app') return segs[3];
    return null;
  };
  const expectedId = chatId(expected.pathname);
  if (!expectedId) return {state: 'error', error: 'cleanup requires an exact conversation URL'};
  const linkMatches = e => {
    try {
      const u = new URL(e.getAttribute('href'), here);
      if (!u.searchParams.has('authuser') && here.searchParams.has('authuser'))
        u.searchParams.set('authuser', here.searchParams.get('authuser'));
      return u.origin === expected.origin && account(u) === account(expected) && chatId(u.pathname) === expectedId;
    }
    catch { return false; }
  };
  const rows = () => [...document.querySelectorAll('conversations-list [data-test-id="conversation"], [data-test-id="all-conversations"] [data-test-id="conversation"]')]
    .filter(e => [...e.querySelectorAll('a[href]')].some(linkMatches));
  let state = window.__anticrumbChatDeletion;
  if (state && state.url !== args.url && state.phase === 'done') {
    delete window.__anticrumbChatDeletion;
    state = null;
  }
  if (state && state.url !== args.url) return {state: 'error', error: 'cleanup identity changed'};
  if (!state) {
    // The new home screen can omit the conversation sidebar entirely. Open
    // the exact search result through the site's router, preserving this page.
    const searchDialog = findVisible(document, '[role="dialog"]').find(e => e.querySelector('input[type="search"]'));
    if (searchDialog) {
      const link = findVisible(searchDialog, 'a[href]').find(linkMatches);
      if (link) {
        link.click();
        return {state: 'waiting', step: 'opening exact conversation from search'};
      }
      const scroller = [...searchDialog.querySelectorAll('*')].find(e => e.scrollHeight > e.clientHeight + 10 && /auto|scroll/.test(getComputedStyle(e).overflowY));
      if (scroller) scroller.scrollTop = scroller.scrollHeight;
      return {state: 'waiting', step: 'exact conversation in search results'};
    }
    const matches = rows().filter(visible);
    if (!matches.length) {
      if (!document.querySelector('conversations-list')) {
        const search = document.querySelector('[data-test-id="search-chats-button"] button, button[aria-label="Search chats"]');
        if (visible(search)) {
          search.click();
          return {state: 'waiting', step: 'opening conversation search'};
        }
      }
      const expand = document.querySelector('[data-test-id="side-nav-sparkle-button"]');
      if (visible(expand) && expand.querySelector('[fonticon="side_nav_expand"]')) {
        expand.click();
        return {state: 'waiting', step: 'opening conversation sidebar'};
      }
      // Older conversations are lazy-loaded. Search only this sidebar's
      // scroller, retaining exact URL matching before any destructive action.
      const list = document.querySelector('conversations-list[data-test-id="all-conversations"]');
      const scroller = list?.closest('infinite-scroller') || list?.querySelector('infinite-scroller');
      let lookup = window.__anticrumbChatLookup;
      if (!lookup || lookup.url !== args.url) {
        lookup = window.__anticrumbChatLookup = {url: args.url, scrollAt: Date.now(), bottomSince: null};
        if (scroller) {
          // A previous complete-history scan may have left a virtualized list
          // at its bottom. Search from the top, not just the last loaded rows.
          scroller.scrollTop = 0;
          scroller.dispatchEvent(new Event('scroll'));
        }
      }
      const atBottom = !scroller || scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - 2;
      if (atBottom) lookup.bottomSince ??= Date.now();
      else lookup.bottomSince = null;
      if (scroller && Date.now() - lookup.scrollAt > 700) {
        lookup.scrollAt = Date.now();
        scroller.scrollTop += Math.max(100, scroller.clientHeight * 0.8);
        scroller.dispatchEvent(new Event('scroll'));
      }
      // Rust verifies this against a fresh complete server history snapshot.
      if (document.readyState === 'complete' && lookup.bottomSince !== null && Date.now() - lookup.bottomSince >= 3000)
        return {state: 'missing_on_page'};
      return {state: 'waiting', step: 'exact conversation link in sidebar'};
    }
    // A pinned entry can also be present in Recent. Either exact URL is safe.
    const row = matches[0];
    const trigger = row.querySelector('[data-test-id="actions-menu-button"] button, [data-test-id="actions-menu-button"], [data-test-id="conversation-menu-button"], .conversation-menu-button');
    if (!trigger) return {state: 'error', error: 'conversation action control is unavailable'};
    state = window.__anticrumbChatDeletion = {
      url: args.url, pageUrl: here.href, phase: 'menu', trigger,
      oldMenus: findVisible(document, '[role="menu"]'),
    };
    trigger.click();
    return {state: 'waiting', step: 'conversation actions menu'};
  }
  if (state.phase === 'menu') {
    if (here.href !== state.pageUrl) return {state: 'error', error: 'page changed before deletion'};
    const trigger = state.trigger;
    const control = [trigger, trigger.parentElement].map(e => e?.getAttribute('aria-controls')).find(Boolean);
    const controlled = control ? document.getElementById(control) : null;
    const menus = visible(controlled) ? [controlled] : findVisible(document, '[role="menu"]').filter(e => !state.oldMenus.includes(e));
    if (menus.length !== 1) return {state: 'waiting', step: 'scoped conversation actions menu'};
    const buttons = findVisible(menus[0], '[data-test-id="delete-button"], [data-test-id="delete-conversation-button"]');
    if (buttons.length !== 1) return {state: 'error', error: 'delete action is unavailable for this conversation'};
    state.oldDialogs = findVisible(document, '[role="dialog"], [role="alertdialog"]');
    state.phase = 'confirm';
    buttons[0].click();
    return {state: 'waiting', step: 'delete confirmation or removal'};
  }
  const dialogs = findVisible(document, '[role="dialog"], [role="alertdialog"]').filter(e => !state.oldDialogs.includes(e));
  if (state.phase === 'confirm' && dialogs.length) {
    if (dialogs.length !== 1 || here.href !== state.pageUrl)
      return {state: 'error', error: 'ambiguous conversation deletion dialog'};
    const buttons = findVisible(dialogs[0], '[data-test-id="confirm-button"], [data-test-id="confirm-delete-button"], [data-test-id="delete-button"], .confirm-button, .delete-button, message-dialog gem-button[cdkfocusinitial] button[jslog^="186009;"]');
    if (buttons.length !== 1) return {state: 'error', error: 'delete confirmation control is unavailable'};
    state.phase = 'verify';
    buttons[0].click();
    return {state: 'waiting', step: 'waiting for confirmed removal'};
  }
  // Require disappearance after our scoped action, never infer deletion merely
  // because an old chat was not loaded in the sidebar on initial navigation.
  if (!rows().length && !dialogs.length) {
    state.absentSince ??= Date.now();
    if (Date.now() - state.absentSince >= 1500) {
      state.phase = 'done';
      return {state: 'deleted'};
    }
  } else {
    state.absentSince = null;
  }
  return {state: 'waiting', step: 'conversation removal acknowledgement'};
}
