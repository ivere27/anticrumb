// Shared by snapshot and actions, so names and roles resolve identically.
const normalize = s => String(s || '').replace(/\s+/g, ' ').trim();
const visible = el => {
  if (!el || !el.isConnected) return false;
  const style = getComputedStyle(el), r = el.getBoundingClientRect();
  return style.display !== 'none' && style.visibility !== 'hidden' && style.opacity !== '0' && r.width > 0 && r.height > 0;
};
const role = el => el.getAttribute('role') || ({
  BUTTON:'button', A:el.hasAttribute('href') ? 'link' : '', TEXTAREA:'textbox',
  SELECT:el.multiple ? 'listbox' : 'combobox', SUMMARY:'button',
  INPUT:({checkbox:'checkbox',radio:'radio',range:'slider',button:'button',submit:'button',reset:'button',number:'spinbutton'})[el.type] || 'textbox'
})[el.tagName] || (el.isContentEditable ? 'textbox' : '');
const name = el => {
  const ids = (el.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean);
  const labelled = ids.map(id => el.getRootNode().getElementById?.(id)?.textContent || '').join(' ');
  return normalize(labelled || el.getAttribute('aria-label') ||
    (el.labels && [...el.labels].map(label => label.textContent).join(' ')) ||
    el.getAttribute('alt') || el.getAttribute('placeholder') || el.getAttribute('title') ||
    (el.tagName === 'INPUT' && ['button','submit','reset'].includes(el.type) ? el.value : '') ||
    el.innerText || el.textContent);
};
const disabled = el => el.matches(':disabled') || el.getAttribute('aria-disabled') === 'true' || !!el.closest('[inert]');
const signature = el => JSON.stringify([el.tagName, role(el), name(el), el.getAttribute('type'), el.getAttribute('href')]);
const roots = [document];
for (let i=0; i<roots.length; i++) {
  for (const el of roots[i].querySelectorAll('*')) if (el.shadowRoot) roots.push(el.shadowRoot);
}
const all = selector => roots.flatMap(root => [...root.querySelectorAll(selector)]);
const interactive = 'a[href],button,input:not([type="hidden"]),textarea,select,summary,[role],[contenteditable="true"],[tabindex]:not([tabindex="-1"])';
const resolve = (target, allowMissing=false) => {
  if (target.ref !== undefined) {
    const record = window.__anticrumbElements?.get(target.ref);
    if (!record || !record.el.isConnected || record.signature !== signature(record.el)) {
      throw new Error('stale element reference; request a new snapshot before acting');
    }
    return record.el;
  }
  let matches;
  if (target.css !== undefined) {
    try { matches = all(target.css); } catch (_) { throw new Error('invalid CSS selector'); }
  } else {
    matches = all(interactive).filter(el => name(el) === target.name && (!target.role || role(el) === target.role));
  }
  // For actions use visible matches, for hidden waits include hidden ones too.
  if (!allowMissing) matches = matches.filter(visible);
  if (matches.length > 1) throw new Error('ambiguous target: ' + matches.length + ' matches; use a snapshot reference or a more precise locator');
  if (!matches.length && !allowMissing) throw new Error('target not found or not visible; request snapshot or wait for it');
  return matches[0] || null;
};
const actionable = el => {
  if (!visible(el)) throw new Error('target is not visible');
  if (disabled(el)) throw new Error('target is disabled');
  el.scrollIntoView({block:'center',inline:'center',behavior:'instant'});
};
