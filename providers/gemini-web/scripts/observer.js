(() => {
  if (globalThis.__geminiCdpObserver) return true;
  let scheduled = false;
  globalThis.__geminiCdpObserver = new MutationObserver(records => {
    if (scheduled || !records.some(r => {
      const e = r.target.nodeType === Node.ELEMENT_NODE ? r.target : r.target.parentElement;
      return e?.closest('model-response, thinking-overlay, model-thoughts, processing-state, .response-container-header-processing-state, [data-test-id*="thought"], user-query, rich-textarea, button, [role="alert"], [role="status"]');
    })) return;
    scheduled = true;
    setTimeout(() => {
      scheduled = false;
      if (typeof globalThis.geminiCdpChanged === 'function') globalThis.geminiCdpChanged('{}');
    }, 50);
  });
  globalThis.__geminiCdpObserver.observe(document.documentElement, {
    subtree: true, childList: true, characterData: true, attributes: true,
    attributeFilter: ['aria-busy', 'disabled', 'src', 'aria-expanded', 'aria-label', 'hidden']
  });
  return true;
})()
