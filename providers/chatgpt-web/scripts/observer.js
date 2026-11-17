(() => {
  if (globalThis.__anticrumbObserver) return true;
  let scheduled = false;
  globalThis.__anticrumbObserver = new MutationObserver(records => {
    if (scheduled || !records.some(r => {
      const e = r.target.nodeType === Node.ELEMENT_NODE ? r.target : r.target.parentElement;
      return e?.closest('[data-message-author-role="assistant"], article, #prompt-textarea, button, [role="alert"], [role="status"]');
    })) return;
    scheduled = true;
    setTimeout(() => {
      scheduled = false;
      if (typeof globalThis.anticrumbCdpChanged === 'function') globalThis.anticrumbCdpChanged('{}');
    }, 50);
  });
  globalThis.__anticrumbObserver.observe(document.documentElement, {
    subtree: true,
    childList: true,
    characterData: true,
    attributes: true,
    attributeFilter: ['data-message-author-role', 'aria-busy', 'disabled', 'aria-label', 'hidden']
  });
  return true;
})()
