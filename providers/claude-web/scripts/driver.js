(args => {
  const visible = el => !!el && el.getClientRects().length > 0 && !el.closest('[hidden],[aria-hidden="true"]') && getComputedStyle(el).visibility !== 'hidden';
  const one = selector => [...document.querySelectorAll(selector)].find(visible);
  const enabled = el => !!el && !el.disabled && el.getAttribute('aria-disabled') !== 'true';
  const dismissUpsell = () => {
    for (const d of document.querySelectorAll('[role="dialog"]')) {
      const txt = d.innerText || '';
      if (/Upgrade to Pro|Claude Pro|Higher usage limits/i.test(txt)) {
        const closeBtn = d.querySelector('button[aria-label="Close"], button[aria-label="Dismiss"]');
        if (closeBtn) closeBtn.click();
      }
    }
  };
  const editor = () => one(
    'div[contenteditable="true"].ProseMirror, div[data-testid="chat-input"][contenteditable="true"], [role="textbox"][contenteditable="true"], fieldset div[contenteditable="true"]'
  );
  const editorText = () => {
    const el = editor();
    if (!el) return '';
    const paragraphs = [...el.querySelectorAll('p')];
    if (paragraphs.length && [...el.children].every(c => c.tagName === 'P')) {
      return paragraphs.map(p => p.textContent || '').join('\n');
    }
    return (el.innerText || el.textContent || '');
  };
  const stopBtn = () => one(
    'button[data-testid="chat-input-stop"], button[data-testid="stop-button"]'
  );
  const sendBtn = () => one(
    'button[data-testid="chat-input-send"], button[data-testid="send-button"]'
  ) || one('fieldset button[type="submit"]');
  const newChatBtn = () => one(
    'a[href="/new"], button[aria-label*="New chat" i], a[aria-label*="New chat" i], button[data-testid="new-chat"]'
  );
  const modelsBtn = () => one(
    'button[data-testid="model-selector-dropdown"], button[aria-haspopup="menu"][class*="model" i], button[aria-label*="Select model" i]'
  );
  const uploadBtn = () => one(
    'button[data-testid="chat-input-attach"], button[aria-label*="Add files" i], button[aria-label*="Upload" i], button[aria-label*="Attach" i], button[data-testid*="attach" i], input[type="file"]'
  );
  const verification = () => {
    const title = (document.title || '').toLowerCase();
    return title.includes('just a moment') ||
      !!one('iframe[src*="challenges.cloudflare.com"], #cf-turnstile, [data-testid="cf-turnstile"]');
  };
  const isAuthPage = () => /\/login|\/oauth/i.test(location.pathname) ||
    !!one('input[type="email"][name="email"], button[data-testid="login-with-google"]');
  const signedIn = () => !verification() && !isAuthPage() && !!editor();
  const responses = () => document.querySelectorAll(
    '[data-testid="assistant-message"], div.font-claude-response, div.font-claude-message, [data-is-streaming]'
  );
  const allMenus = () => [...document.querySelectorAll('[role="menu"], [role="listbox"], [data-radix-popper-content-wrapper]')].filter(visible);
  const cleanLines = el => (el.innerText || el.textContent || '')
    .split('\n')
    .map(s => s.trim())
    .filter(s => s && !/^[\uE000-\uF8FF]+$/.test(s));
  const isEffortMenu = m => [...m.querySelectorAll('[role="menuitemradio"], [role="menuitem"]')].some(el =>
    /^(Low|Medium|High|Extra|Max)$/i.test(cleanLines(el)[0] || '')
  );
  const mainMenu = () => allMenus().find(m => !isEffortMenu(m)) || null;
  const effortSubmenu = () => allMenus().find(isEffortMenu) || null;
  const effortTrigger = () => {
    const m = mainMenu();
    if (!m) return null;
    return [...m.querySelectorAll('[role="menuitem"], button')]
      .filter(visible)
      .find(el => el.getAttribute('aria-haspopup') === 'menu' && (cleanLines(el)[0] || '') === 'Effort') || null;
  };
  const menuItems = () => {
    const menu = mainMenu();
    if (!menu) return [];
    return [...menu.querySelectorAll('[role="menuitem"], [role="menuitemradio"], [role="option"], button')]
      .filter(visible)
      .filter(el => el.tagName !== 'A' && el.getAttribute('aria-haspopup') !== 'menu');
  };
  const thinkingItems = () => {
    const sub = effortSubmenu();
    if (!sub) return [];
    return [...sub.querySelectorAll('[role="menuitemradio"], [role="menuitem"], [role="option"]')]
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
      dismissUpsell();
      const mBtn = modelsBtn();
      return {
        editor: !!editor(),
        has_draft: !!editorText().trim(),
        signed_in: signedIn(),
        verification: verification(),
        temporary: false,
        generating: !!stopBtn(),
        url: location.origin + location.pathname,
        model: mBtn?.textContent?.trim() || null,
        model_button: mBtn?.textContent?.trim() || null,
        uploads: !!document.querySelector('input[type="file"]') || !!uploadBtn(),
        models: !!mBtn,
        response_count: responses().length
      };
    }
    case 'target_rect': {
      switch (args.target) {
        case 'editor': return rectOf(editor());
        case 'send': { const el = sendBtn(); return enabled(el) ? rectOf(el) : { ok: false }; }
        case 'stop': { const el = stopBtn(); return enabled(el) ? rectOf(el) : { ok: false }; }
        case 'new_chat': { const el = newChatBtn(); return enabled(el) ? rectOf(el) : { ok: false, fallback: true }; }
        case 'temporary': return { ok: false };
        case 'models_open': {
          const b = modelsBtn();
          if (!enabled(b)) return { ok: false };
          if (menuItems().length) return { ok: true, already_open: true };
          return rectOf(b);
        }
        case 'thinking_open': {
          if (effortSubmenu()) return { ok: true, already_open: true, available: true };
          const t = effortTrigger();
          if (!enabled(t)) return { ok: true, available: false };
          return { ok: true, available: true };
        }
        case 'select': {
          if (args.thinking) {
            const item = thinkingItems().find((el, i) => {
              const label = cleanLines(el)[0] || '';
              const id = label ? label.toLowerCase() : `effort:${i + 1}`;
              return id === String(args.id || '').toLowerCase() || label === args.id;
            });
            return (!item || !enabled(item)) ? { ok: false } : { ok: true };
          }
          const item = menuItems().find((el, i) => (el.getAttribute('data-value') || `model:${i + 1}`) === args.id || cleanLines(el)[0] === args.id || el.textContent.trim() === args.id);
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
      const range = document.createRange();
      range.selectNodeContents(el);
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      return { ok: true };
    }
    case 'prepare': {
      dismissUpsell();
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
    case 'temporary': return { ok: false };
    case 'models_open': {
      const b = modelsBtn();
      if (!enabled(b)) return { ok: false };
      if (!menuItems().length) b.click();
      return { ok: true };
    }
    case 'thinking_open': {
      if (effortSubmenu()) return { ok: true, available: true };
      const t = effortTrigger();
      if (!enabled(t)) return { ok: true, available: false };
      const r = t.getBoundingClientRect();
      const opts = { bubbles: true, cancelable: true, clientX: r.left + r.width / 2, clientY: r.top + r.height / 2, pointerType: 'mouse' };
      t.dispatchEvent(new PointerEvent('pointerover', opts));
      t.dispatchEvent(new PointerEvent('pointerenter', { ...opts, bubbles: false }));
      t.dispatchEvent(new MouseEvent('mouseover', opts));
      t.dispatchEvent(new PointerEvent('pointermove', opts));
      t.dispatchEvent(new MouseEvent('mousemove', opts));
      t.click();
      return { ok: true, available: true };
    }
    case 'models': {
      if (args.thinking) {
        return thinkingItems().map((el, index) => {
          const lines = cleanLines(el);
          const label = lines[0] || `Effort ${index + 1}`;
          return {
            index: index + 1,
            mode_id: label.toLowerCase(),
            label,
            sublabel: lines.slice(1).join(' '),
            selected: el.getAttribute('aria-checked') === 'true' || el.getAttribute('aria-selected') === 'true',
            locked: !enabled(el),
            rate_limit_reset: null
          };
        });
      }
      const items = menuItems();
      return items.map((el, index) => {
        const lines = cleanLines(el);
        const hasUpgrade = lines.some(s => s === 'Upgrade');
        const sub = lines.slice(1).filter(s => s !== 'Upgrade').join(' ');
        const selected = el.getAttribute('aria-checked') === 'true' ||
          el.getAttribute('aria-selected') === 'true' ||
          !!el.querySelector('svg[data-icon="check"]');
        return {
          index: index + 1,
          mode_id: el.getAttribute('data-value') || `model:${index + 1}`,
          label: lines[0] || `Model ${index + 1}`,
          sublabel: sub,
          selected,
          locked: !enabled(el) || hasUpgrade,
          rate_limit_reset: null
        };
      });
    }
    case 'select': {
      if (args.thinking) {
        const item = thinkingItems().find((el, i) => {
          const label = cleanLines(el)[0] || '';
          const id = label ? label.toLowerCase() : `effort:${i + 1}`;
          return id === String(args.id || '').toLowerCase() || label === args.id;
        });
        if (!item || !enabled(item)) return { ok: false };
        item.click();
        return { ok: true };
      }
      const item = menuItems().find((el, i) => (el.getAttribute('data-value') || `model:${i + 1}`) === args.id || cleanLines(el)[0] === args.id || el.textContent.trim() === args.id);
      if (!item || !enabled(item)) return { ok: false };
      item.click();
      return { ok: true };
    }
    case 'chips': {
      const thumbnails = [...document.querySelectorAll('[data-testid="file-thumbnail"]')];
      const chips = thumbnails.length ? thumbnails : [...document.querySelectorAll('[data-testid*="attachment"]')];
      return chips
        .filter(visible)
        .map(el => ({
          name: el.querySelector('button[aria-label^="Remove "]')?.getAttribute('aria-label').slice(7)
            || el.querySelector('[data-testid="file-name"]')?.textContent.trim()
            || (el.getAttribute('aria-label') || el.innerText || '').trim().split('\n')[0],
          pending: !!el.querySelector('[role="progressbar"], .animate-spin'),
          failed: !!el.querySelector('[data-testid*="error"], .text-danger')
        }));
    }
    case 'overlay_open': return allMenus().length > 0;
    case 'uploads_open': {
      if (document.querySelector('input[type="file"]')) return { ok: true };
      const b = uploadBtn();
      if (!enabled(b)) return { ok: false };
      b.click();
      return { ok: true };
    }
    case 'file_input': {
      return document.querySelector('input[type="file"]') || null;
    }
    case 'media': return [];
    default: throw new Error('unknown adapter operation: ' + args.op);
  }
})
