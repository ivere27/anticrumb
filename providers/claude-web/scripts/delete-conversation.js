// Exact-link deletion in the persistent maintenance tab. Never match chat titles.
(args) => {
  const spec = {
    origin: 'https://claude.ai',
    path: '/chat/',
    trigger: 'button[aria-haspopup="menu"][data-row-action]',
    header: 'button[data-testid="chat-menu-trigger"],[data-testid="session-title-split"] button[aria-haspopup="menu"]',
    remove: '[data-testid="delete-chat-trigger"],[data-testid="delete-session-trigger"]',
    confirm: 'button[data-testid="delete-chat-confirm"],button[data-cds="Button"]:has([data-cds-part="paint"].bg-fill-danger)'
  };
  const visible = e => !!e && e.isConnected && e.getClientRects().length > 0
    && getComputedStyle(e).visibility !== 'hidden' && getComputedStyle(e).display !== 'none';
  const all = (root, selector) => [...root.querySelectorAll(selector)].filter(visible);
  if (/just a moment|security verification/i.test(document.title) || all(document, '#challenge-running,#cf-turnstile,iframe[src*="challenges.cloudflare.com"]').length)
    return {state:'verification_required'};
  if (location.origin !== spec.origin || /\/login|\/auth\//.test(location.pathname))
    return {state:'auth_required'};
  const expected = new URL(args.url);
  const valid = u => u.origin === spec.origin && new RegExp('^' + spec.path + '[a-fA-F0-9-]{36}$').test(u.pathname)
    && !u.username && !u.password && !u.search && !u.hash;
  if (!valid(expected)) return {state:'error',error:'exact provider conversation URL required'};
  const matches = a => { try { return new URL(a.href).href === expected.href; } catch { return false; } };
  const links = () => all(document, 'a[href]').filter(matches);
  let state = window.__anticrumbChatDeletion;
  if (state?.url !== args.url && state?.phase === 'done') state = window.__anticrumbChatDeletion = null;
  if (state && state.url !== args.url) return {state:'error',error:'cleanup identity changed'};
  if (!state) {
    let trigger = null;
    for (const link of links()) {
      for (let row=link.parentElement, depth=0; row && depth<8; row=row.parentElement,depth++) {
        const ids = new Set([...row.querySelectorAll('a[href]')].filter(a=>valid(new URL(a.href))).map(a=>new URL(a.href).href));
        if (ids.size > 1) break;
        const controls = all(row, spec.trigger);
        if (controls.length === 1) { trigger=controls[0]; break; }
      }
      if (trigger) break;
    }
    if (!trigger && location.pathname === expected.pathname) {
      const controls = all(document, spec.header);
      if (controls.length === 1) trigger = controls[0];
    }
    if (!trigger) {
      // Sidebar pagination can be virtualized. Give the UI time to load rows;
      // Rust may open the exact recorded chat once if its row is unavailable.
      let lookup=window.__anticrumbDeleteLookup;
      if (!lookup || lookup.url !== args.url) lookup=window.__anticrumbDeleteLookup={url:args.url,since:Date.now()};
      const nav=document.querySelector('nav, [data-app-shell-sidebar], aside');
      const scroller=nav && [...nav.querySelectorAll('*')].find(e=>e.scrollHeight>e.clientHeight+10 && /auto|scroll/.test(getComputedStyle(e).overflowY));
      if(scroller) scroller.scrollTop=scroller.scrollHeight;
      return Date.now()-lookup.since>3000 ? {state:'needs_navigation'} : {state:'waiting',step:'exact conversation row'};
    }
    state=window.__anticrumbChatDeletion={url:args.url,page:location.href,phase:'menu',trigger,oldMenus:all(document,'[role=menu]')};
    trigger.click();
    return {state:'waiting',step:'conversation actions menu'};
  }
  if (state.phase === 'menu') {
    if (location.href !== state.page) return {state:'error',error:'page changed before deletion'};
    const controlled=document.getElementById(state.trigger.getAttribute('aria-controls') || '');
    const menus=visible(controlled) ? [controlled] : all(document,'[role=menu]').filter(e=>!state.oldMenus.includes(e));
    if(!menus.length && !state.trigger.isConnected){
      // Hydration can replace the button before its first click is handled.
      // No delete action has run; reacquire only this same conversation's row.
      window.__anticrumbChatDeletion=null;
      return {state:'waiting',step:'conversation actions button replaced'};
    }
    if(menus.length!==1)return {state:'waiting',step:'scoped conversation menu'};
    const buttons=all(menus[0],spec.remove);
    if(buttons.length!==1)return {state:'error',error:'unambiguous conversation delete action unavailable'};
    state.oldDialogs=all(document,'[role=dialog],[role=alertdialog],dialog');
    state.phase='confirm';
    buttons[0].click();
    return {state:'waiting',step:'delete confirmation'};
  }
  const dialogs=state.phase==='confirm'
    ? all(document,'[role=dialog],[role=alertdialog],dialog').filter(e=>!state.oldDialogs.includes(e))
    : (visible(state.confirmation) ? [state.confirmation] : []);
  if(state.phase==='confirm' && dialogs.length){
    if(dialogs.length!==1 || location.href!==state.page)return {state:'error',error:'ambiguous delete confirmation'};
    const buttons=all(dialogs[0],spec.confirm);
    if(buttons.length!==1)return {state:'error',error:'unambiguous delete confirmation unavailable'};
    state.confirmation=dialogs[0];
    state.phase='verify';
    buttons[0].click();
    return {state:'waiting',step:'confirmed deletion'};
  }
  if(state.phase==='confirm')return {state:'waiting',step:'delete confirmation'};
  if(!dialogs.length && !links().length){
    state.absentSince??=Date.now();
    if(Date.now()-state.absentSince>1500){state.phase='done';return {state:'deleted'};}
  }else state.absentSince=null;
  return {state:'waiting',step:'conversation removal acknowledgement'};
}
