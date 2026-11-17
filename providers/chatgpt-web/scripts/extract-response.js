(() => {
  const visible = el => !!el && el.getClientRects().length > 0 && !el.closest('[hidden],[aria-hidden="true"]');
  const resps = [...document.querySelectorAll('[data-response-body], [data-message-author-role="assistant"]')];
  const last = resps.length ? resps[resps.length - 1] : null;
  const stopBtn = [...document.querySelectorAll('button[data-testid="stop-button"]')].find(visible) ||
    [...document.querySelectorAll('form[data-chatgpt-composer] button[type="button"]:has(svg.icon-primary-action > path)')].find(visible);
  const turn = last?.closest('[data-talvt-turn-state], article') || last?.parentElement || last;
  const turnState = turn?.getAttribute?.('data-talvt-turn-state') || '';
  const streaming = Boolean(stopBtn) ||
    Boolean(turnState && turnState !== 'complete') ||
    Boolean(last && last.querySelector('.result-streaming'));
  const hasActionBar = Boolean(!streaming && turn && (
    turnState === 'complete' ||
    turn.querySelector(
      'button[data-testid="copy-turn-action-button"], button[data-testid="good-response-turn-action-button"]'
    )
  ));
  const verification = (document.title || '').toLowerCase().includes('just a moment') ||
    [...document.querySelectorAll('iframe[src*="challenges.cloudflare.com"], #cf-turnstile')].some(visible);

  // Defer URL-shaped link rendering until the surrounding tool envelope is known.
  // Provider autolinks can point at a search/redirect URL instead of their label.
  const responseLinks = [];
  let linkPrefix = '\uE000anticrumb-link:';
  while (last && (last.textContent || '').includes(linkPrefix)) linkPrefix += '_';

  function responseLink(node, label, markdown) {
    if (!/^https?:\/\/\S+$/i.test(label) || label === markdown) return markdown;
    const inTable = Boolean(node.closest('table'));
    responseLinks.push({
      label,
      markdown: inTable ? markdown.replace(/\|/g, '\\|') : markdown
    });
    return linkPrefix + (responseLinks.length - 1) + '\uE001';
  }

  function restoreResponseLinks(text) {
    if (!responseLinks.length) return text;
    const pattern = new RegExp(linkPrefix + '(\\d+)\\uE001', 'g');
    let inTool = false;
    let fence = null;
    return text.split('\n').map(line => {
      const trimmed = line.trim();
      if (!inTool) {
        if (trimmed.startsWith('```') || trimmed.startsWith('~~~')) {
          const marker = trimmed[0];
          if (fence === marker) fence = null;
          else if (fence === null) fence = marker;
        } else if (fence === null && trimmed === '[TOOL_CALL]') {
          inTool = true;
        }
      }
      const rendered = line.replace(pattern, (_, index) => {
        const link = responseLinks[Number(index)];
        return inTool ? link.label : link.markdown;
      });
      if (inTool && trimmed === '[/TOOL_CALL]') inTool = false;
      return rendered;
    }).join('\n');
  }

  function serializeNode(node, depth = 0) {
    if (!node) return '';
    if (node.nodeType === Node.TEXT_NODE) return node.nodeValue || '';
    if (node.nodeType !== Node.ELEMENT_NODE) return '';
    const tag = node.tagName.toLowerCase();
    if (tag === 'button' || tag === 'style' || tag === 'script') return '';
    if (tag === 'pre') {
      const codeEl = node.querySelector('code');
      const cls = (codeEl?.className || '') + ' ' + (node.className || '');
      const m = cls.match(/language-([a-z0-9_+-]+)/i);
      const lang = m ? m[1].toLowerCase() : '';
      const code = (codeEl ? codeEl.textContent : node.textContent) || '';
      return '\n```' + lang + '\n' + code.replace(/\n+$/, '') + '\n```\n\n';
    }
    if (tag === 'code') return '`' + (node.textContent || '') + '`';
    if (tag === 'br') return '\n';
    if (tag === 'hr') return '\n---\n\n';
    if (tag === 'table') {
      const trs = [...node.querySelectorAll('tr')];
      if (!trs.length) return '';
      const out = [];
      trs.forEach((tr, rowIdx) => {
        const cellEls = [...tr.children].filter(c => c.tagName === 'TH' || c.tagName === 'TD');
        if (!cellEls.length) return;
        const cells = cellEls.map(c => serializeNode(c, depth).replace(/\s*\n+\s*/g, ' ').trim().replace(/\|/g, '\\|'));
        out.push('| ' + cells.join(' | ') + ' |');
        if (rowIdx === 0) out.push('| ' + cellEls.map(() => '---').join(' | ') + ' |');
      });
      return out.length ? '\n' + out.join('\n') + '\n\n' : '';
    }
    let inner = '';
    if (tag === 'ul' || tag === 'ol') {
      let idx = 1;
      for (const child of node.children) {
        if (child.tagName.toLowerCase() === 'li') {
          const indent = '  '.repeat(depth);
          const prefix = tag === 'ol' ? `${idx++}. ` : '- ';
          let liText = '';
          for (const c of child.childNodes) liText += serializeNode(c, depth + 1);
          inner += indent + prefix + liText.trim().replace(/\n{2,}/g, '\n' + indent + '  ') + '\n';
        }
      }
      return inner + (depth === 0 ? '\n' : '');
    }
    for (const c of node.childNodes) inner += serializeNode(c, depth);
    if (/^h[1-6]$/.test(tag)) return '\n' + '#'.repeat(Number(tag[1])) + ' ' + inner.trim() + '\n\n';
    if (tag === 'p') {
      const t = inner.trim();
      return t ? t + '\n\n' : '';
    }
    if (tag === 'strong' || tag === 'b') return inner ? `**${inner}**` : '';
    if (tag === 'em' || tag === 'i') return inner ? `*${inner}*` : '';
    if (tag === 'a') {
      const href = node.getAttribute('href') || '';
      const label = inner.trim();
      return href && label ? responseLink(node, label, `[${label}](${href})`) : inner;
    }
    if (tag === 'blockquote') {
      const lines = inner.trim().split('\n').map(l => '> ' + l).join('\n');
      return lines ? '\n' + lines + '\n\n' : '';
    }
    return inner;
  }

  let text = '';
  if (last) {
    const contentEl = last.querySelector('[data-markdown-text-style="assistant-message"], [data-dil-response], .markdown, .prose') || last;
    text = restoreResponseLinks(serializeNode(contentEl)).replace(/\n{3,}/g, '\n\n').trim();
  }
  const thinkBtn = [...document.querySelectorAll('button')].filter(visible)
    .find(b => (b.innerText || '').trim() === 'Think' && b.hasAttribute('aria-pressed'));
  const modePill = [...document.querySelectorAll('button[data-testid="model-switcher-dropdown-button"], button[aria-label*="Model selector" i]')].find(visible)?.textContent?.trim() ||
    (thinkBtn ? (thinkBtn.getAttribute('aria-pressed') === 'true' ? 'Auto (Think)' : 'Auto') : null);

  return {
    respCount: resps.length,
    text,
    hasStop: streaming,
    ariaBusy: streaming ? 'true' : '',
    thinking: false,
    thinkingText: '',
    thinkingLen: 0,
    hasActionBar,
    verification,
    mediaReady: false,
    modePill,
    url: location.origin + location.pathname
  };
})()
