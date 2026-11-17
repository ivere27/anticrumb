(args => {
  const visible = el => !!el && el.getClientRects().length > 0 && !el.closest('[hidden],[aria-hidden="true"]') && getComputedStyle(el).visibility !== 'hidden';
  const one = selector => [...document.querySelectorAll(selector)].find(visible);
  const enabled = el => !!el && !el.disabled && el.getAttribute('aria-disabled') !== 'true';
  const editor = () => one(
    '#prompt-textarea, [data-composer-markdown][contenteditable="true"], div.ProseMirror[contenteditable="true"], textarea[data-id="root"]'
  );
  const editorText = () => {
    const el = editor();
    if (!el) return '';
    if (el.tagName === 'TEXTAREA') return el.value || '';
    const paragraphs = [...el.querySelectorAll('p')];
    if (paragraphs.length && [...el.children].every(c => c.tagName === 'P')) {
      return paragraphs.map(p => p.textContent || '').join('\n');
    }
    return (el.innerText || el.textContent || '');
  };
  // The current composer uses an inline square for stop and an SVG use for voice.
  const stopBtn = () => one('button[data-testid="stop-button"]') || one(
    'form[data-chatgpt-composer] button[type="button"]:has(svg.icon-primary-action > path)'
  );
  const sendBtn = () => one(
    'button[data-testid="send-button"]'
  ) || one('form[data-chatgpt-composer] button[type="submit"], form:has(#prompt-textarea) button[type="submit"]');
  const newChatBtn = () => one(
    'a[href="/"], button[data-testid="create-new-chat-button"], button[aria-label*="New chat" i], a[aria-label*="New chat" i]'
  );
  const tempBtn = () => one(
    'button[aria-label*="Temporary chat" i], button[data-testid="temporary-chat-button"], a[href*="temporary-chat=true"]'
  );
  const temporary = () => {
    if (new URLSearchParams(location.search).get('temporary-chat') === 'true') return true;
    const b = tempBtn();
    return Boolean(b && (b.getAttribute('aria-pressed') === 'true' || b.getAttribute('data-state') === 'on')) ||
      !!one('[data-testid="temporary-chat-banner"]');
  };
  const modelsBtn = () => one(
    'button[data-testid="model-switcher-dropdown-button"], button[aria-label*="Model selector" i], button[aria-haspopup="menu"][data-testid*="model" i]'
  );
  const thinkBtn = () => [...document.querySelectorAll('button')]
    .filter(visible)
    .find(b => (b.innerText || '').trim() === 'Think' && b.hasAttribute('aria-pressed'));
  const thinkEnabled = () => thinkBtn()?.getAttribute('aria-pressed') === 'true';
  const uploadBtn = () => one(
    'button[aria-label*="Add files" i], button[aria-label*="Attach" i], button[data-testid="composer-plus-btn"], input[type="file"]'
  );
  const verification = () => {
    const title = (document.title || '').toLowerCase();
    return title.includes('just a moment') ||
      !!one('iframe[src*="challenges.cloudflare.com"], #cf-turnstile');
  };
  const isAuthPage = () => location.hostname.includes('auth.openai.com') ||
    /\/auth\/login/i.test(location.pathname);
  const signedIn = () => !verification() && !isAuthPage() && !!editor() &&
    !!one('[data-testid="profile-button"], button[aria-label*="Open profile" i], [data-testid="accounts-profile-button"]');
  const responses = () => document.querySelectorAll('[data-response-body], [data-message-author-role="assistant"]');
  const menuItems = () => {
    const menu = one('[role="menu"], [role="listbox"], [data-radix-popper-content-wrapper]');
    if (!menu) return [];
    return [...menu.querySelectorAll('[role="menuitem"], [role="menuitemradio"], [role="option"], button[data-testid*="model"]')]
      .filter(visible);
  };
  const rectOf = (el, extra = {}) => {
    if (!el) return { ok: false };
    el.scrollIntoView({ block: 'nearest', inline: 'nearest' });
    const r = el.getBoundingClientRect();
    if (!r || r.width < 2 || r.height < 2) return { ok: false, no_box: true, ...extra };
    return { ok: true, x: r.left, y: r.top, width: r.width, height: r.height, ...extra };
  };

  switch (args.op) {
    case 'probe': {
      const mBtn = modelsBtn();
      const tBtn = thinkBtn();
      return {
        editor: !!editor(),
        has_draft: !!editorText().trim(),
        signed_in: signedIn(),
        verification: verification(),
        temporary: temporary(),
        generating: !!stopBtn(),
        url: location.origin + location.pathname,
        model: mBtn?.textContent?.trim() || (tBtn ? (thinkEnabled() ? 'Auto (Think)' : 'Auto') : null),
        model_button: mBtn?.textContent?.trim() || (tBtn ? 'Auto' : null),
        uploads: !!document.querySelector('input[type="file"]') || !!uploadBtn(),
        models: !!mBtn || !!tBtn,
        response_count: responses().length
      };
    }
    case 'target_rect': {
      switch (args.target) {
        case 'editor': return rectOf(editor());
        case 'send': { const el = sendBtn(); return enabled(el) ? rectOf(el) : { ok: false }; }
        case 'stop': { const el = stopBtn(); return enabled(el) ? rectOf(el) : { ok: false }; }
        case 'new_chat': { const el = newChatBtn(); return enabled(el) ? rectOf(el) : { ok: false, fallback: true }; }
        case 'temporary': {
          if (temporary()) return { ok: true, already_active: true };
          const b = tempBtn();
          return enabled(b) ? rectOf(b) : { ok: false };
        }
        case 'models_open': {
          const b = modelsBtn();
          if (b) {
            if (!enabled(b)) return { ok: false };
            if (menuItems().length) return { ok: true, already_open: true };
            return rectOf(b);
          }
          if (thinkBtn()) return { ok: true, already_open: true };
          return { ok: false };
        }
        case 'thinking_open': {
          const t = thinkBtn();
          if (!t || !enabled(t)) return { ok: true, available: false };
          return { ok: true, already_open: true, available: true };
        }
        case 'select': {
          if (args.thinking) {
            const t = thinkBtn();
            if (!t || !enabled(t)) return { ok: false };
            const wantThink = args.id === 'think' || args.id === 'Think';
            const wantStd = args.id === 'standard' || args.id === 'Standard';
            if (!wantThink && !wantStd) return { ok: false };
            if (wantThink === thinkEnabled()) return { ok: true, already_active: true };
            return rectOf(t);
          }
          if (!modelsBtn() && thinkBtn() && (args.id === 'chatgpt-web-default' || args.id === 'Auto (Free)')) {
            return { ok: true, already_active: true };
          }
          const item = menuItems().find((el, i) => (el.getAttribute('data-testid') || `model:${i + 1}`) === args.id || el.textContent.trim() === args.id);
          return (!item || !enabled(item)) ? { ok: false } : rectOf(item);
        }
        case 'uploads_open': {
          if (document.querySelector('input[type="file"]')) return { ok: true, already_open: true };
          const b = uploadBtn();
          return enabled(b) ? rectOf(b) : { ok: false };
        }
        default: return { ok: false };
      }
    }
    case 'select_draft': {
      const el = editor();
      if (!el || stopBtn()) return { ok: false };
      el.focus();
      if (el.tagName === 'TEXTAREA') {
        el.select();
      } else {
        const range = document.createRange();
        range.selectNodeContents(el);
        const selection = window.getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
      }
      return { ok: true };
    }
    case 'prepare': {
      const el = editor();
      if (!el || stopBtn()) return { ok: false, reason: 'editor_unavailable' };
      if (editorText().trim()) return { ok: false, reason: 'composer_not_empty' };
      el.focus();
      return { ok: true };
    }
    case 'verify_input': {
      const actual = editorText();
      return {
        ok: actual.replace(/\r\n/g, '\n').trim() === String(args.text || '').trim(),
        actual_length: actual.length
      };
    }
    case 'send': {
      const btn = sendBtn();
      if (!enabled(btn)) return { ok: false };
      btn.click();
      return { ok: true };
    }
    case 'stop': {
      const btn = stopBtn();
      if (!enabled(btn)) return { ok: false };
      btn.click();
      return { ok: true };
    }
    case 'new_chat': {
      const btn = newChatBtn();
      if (!enabled(btn)) return { ok: false };
      btn.click();
      return { ok: true };
    }
    case 'temporary': {
      if (temporary()) return { ok: true };
      const btn = tempBtn();
      if (!enabled(btn)) return { ok: false };
      btn.click();
      return { ok: true };
    }
    case 'models_open': {
      const b = modelsBtn();
      if (b) {
        if (!enabled(b)) return { ok: false };
        if (!menuItems().length) b.click();
        return { ok: true };
      }
      if (thinkBtn()) return { ok: true };
      return { ok: false };
    }
    case 'thinking_open': {
      const t = thinkBtn();
      if (!t || !enabled(t)) return { ok: true, available: false };
      return { ok: true, available: true };
    }
    case 'models': {
      if (args.thinking) {
        const t = thinkBtn();
        if (!t) return [];
        const on = thinkEnabled();
        return [
          {
            index: 1,
            mode_id: 'standard',
            label: 'Standard',
            sublabel: 'Fast default response',
            selected: !on,
            locked: !enabled(t),
            rate_limit_reset: null
          },
          {
            index: 2,
            mode_id: 'think',
            label: 'Think',
            sublabel: 'Extended reasoning toggle',
            selected: on,
            locked: !enabled(t),
            rate_limit_reset: null
          }
        ];
      }
      if (!modelsBtn() && thinkBtn()) {
        return [
          {
            index: 1,
            mode_id: 'chatgpt-web-default',
            label: 'Auto (Free)',
            sublabel: 'Free plan (Standard / Think toggle)',
            selected: true,
            locked: false,
            rate_limit_reset: null
          }
        ];
      }
      const items = menuItems();
      return items.map((el, index) => {
        const lines = (el.innerText || el.textContent || '').split('\n').map(s => s.trim()).filter(Boolean);
        return {
          index: index + 1,
          mode_id: el.getAttribute('data-testid') || `model:${index + 1}`,
          label: lines[0] || `Model ${index + 1}`,
          sublabel: lines.slice(1).join(' '),
          selected: el.getAttribute('aria-checked') === 'true' || el.getAttribute('aria-selected') === 'true' || !!el.querySelector('svg[data-icon="check"]'),
          locked: !enabled(el),
          rate_limit_reset: null
        };
      });
    }
    case 'select': {
      if (args.thinking) {
        const t = thinkBtn();
        if (!t || !enabled(t)) return { ok: false };
        const wantThink = args.id === 'think' || args.id === 'Think';
        const wantStd = args.id === 'standard' || args.id === 'Standard';
        if (!wantThink && !wantStd) return { ok: false };
        if (wantThink !== thinkEnabled()) t.click();
        return { ok: true };
      }
      if (!modelsBtn() && thinkBtn() && (args.id === 'chatgpt-web-default' || args.id === 'Auto (Free)')) {
        return { ok: true };
      }
      const item = menuItems().find((el, i) => (el.getAttribute('data-testid') || `model:${i + 1}`) === args.id || el.textContent.trim() === args.id);
      if (!item || !enabled(item)) return { ok: false };
      item.click();
      return { ok: true };
    }
    case 'chips': {
      const composer = [...document.querySelectorAll('[data-composer-attachments] [class~="group/composer-attachment"]')];
      const chips = composer.length ? composer : [...document.querySelectorAll('[data-testid*="file-tile"], [data-testid*="attachment"]')];
      return chips
        .filter(visible)
        .map(el => ({
          name: el.querySelector('button[aria-label^="Remove "]')?.getAttribute('aria-label').slice(7)
            || (el.getAttribute('aria-label') || el.innerText || '').trim().split('\n')[0],
          pending: !!el.querySelector('[role="progressbar"], .animate-spin'),
          failed: !!el.querySelector('.text-red-500, [data-testid*="error"]')
        }));
    }
    case 'overlay_open': return menuItems().length > 0;
    case 'uploads_open': {
      if (document.querySelector('input[type="file"]:not([accept]), input[type="file"]')) return { ok: true };
      const b = uploadBtn();
      if (!enabled(b)) return { ok: false };
      b.click();
      return { ok: true };
    }
    case 'file_input': {
      return document.querySelector('input[type="file"]:not([accept])') || document.querySelector('input[type="file"]') || null;
    }
    case 'media': return [];
    default: throw new Error('unknown adapter operation: ' + args.op);
  }
})
