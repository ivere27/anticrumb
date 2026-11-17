(() => {
  const resps = document.querySelectorAll('model-response');
  const last = resps.length ? resps[resps.length - 1] : null;
  const stopBtn = document.querySelector(
    '[data-test-id="stop-button"], button:has(mat-icon[fonticon="stop"]), button:has(mat-icon[data-mat-icon-name="stop"])'
  );
  const thoughtsSelector = 'thinking-overlay, model-thoughts, [data-test-id="model-thoughts"], [data-test-id*="thought"], .thoughts-container, .thoughts-wrapper, .thinking-process, processing-state, .response-container-header-processing-state';
  const thoughtTextOf = el => (el ? ((el.innerText || el.textContent || '').trim()) : '');
  const thoughtCandidates = [];
  if (last) {
    for (const el of last.querySelectorAll(thoughtsSelector)) {
      thoughtCandidates.push(el);
    }
  }
  for (const el of document.querySelectorAll(thoughtsSelector)) {
    const resp = el.closest('model-response');
    const relevant = resp
      ? resp === last
      : (!last || Boolean(last.compareDocumentPosition(el) & Node.DOCUMENT_POSITION_FOLLOWING));
    if (relevant && !thoughtCandidates.includes(el)) {
      thoughtCandidates.push(el);
    }
  }
  const thoughtsEl = thoughtCandidates.find(el => thoughtTextOf(el).length > 0) || thoughtCandidates[0] || null;
  const mc = last
    ? ([...last.querySelectorAll('message-content, .markdown')]
        .find(el => !el.closest(thoughtsSelector)) || null)
    : null;
  const ariaBusy = mc ? (mc.getAttribute('aria-busy') || '') : '';
  const hasActionBar = Boolean(last && last.querySelector(
    'button:has(mat-icon[fonticon="content_copy"]), button:has(mat-icon[data-mat-icon-name="content_copy"]), button:has(mat-icon[fonticon="copy"]), button:has(mat-icon[data-mat-icon-name="copy"]), button:has(mat-icon[fonticon="refresh"]), button:has(mat-icon[fonticon="thumb_up"])'
  ));

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

  function serializeCodeBlock(el) {
    const langEl = el.querySelector('.code-block-decoration span');
    const lang = langEl ? (langEl.textContent || '').trim().toLowerCase() : '';
    const codeEl = el.querySelector('pre code[data-test-id="code-content"], pre code, code');
    const code = codeEl ? (codeEl.textContent || '').replace(/\n+$/, '') : '';
    return '\n```' + lang + '\n' + code + '\n```\n\n';
  }

  function serializeNode(node, listDepth = 0) {
    if (!node) return '';
    if (node.nodeType === Node.TEXT_NODE) {
      return node.nodeValue || '';
    }
    if (node.nodeType !== Node.ELEMENT_NODE) return '';
    const tag = node.tagName.toLowerCase();

    if (tag === 'code-block') {
      return serializeCodeBlock(node);
    }
    if (
      tag === 'sources-carousel-inline' ||
      tag === 'source-inline-chip' ||
      tag === 'source-footnote' ||
      tag === 'button' ||
      (node.classList && (
        node.classList.contains('code-block-decoration') ||
        node.classList.contains('table-footer') ||
        node.classList.contains('buttons') ||
        node.classList.contains('hide-from-message-actions') ||
        node.classList.contains('source-inline-chip-container') ||
        node.classList.contains('cdk-visually-hidden')
      ))
    ) {
      return '';
    }
    if (tag === 'pre') {
      const codeEl = node.querySelector('code');
      const code = (codeEl ? codeEl.textContent : node.textContent) || '';
      return '\n```\n' + code.replace(/\n+$/, '') + '\n```\n\n';
    }
    if (tag === 'code') {
      return '`' + (node.textContent || '') + '`';
    }
    if (tag === 'br') {
      return '\n';
    }
    if (tag === 'hr') {
      return '\n---\n\n';
    }
    if (tag === 'table') {
      const trs = [...node.querySelectorAll('tr')];
      if (!trs.length) return '';
      const out = [];
      trs.forEach((tr, rowIdx) => {
        const cellEls = [...tr.children].filter(c => {
          const ct = c.tagName ? c.tagName.toLowerCase() : '';
          return ct === 'th' || ct === 'td';
        });
        if (!cellEls.length) return;
        const cells = cellEls.map(c =>
          serializeNode(c, listDepth)
            .replace(/\s*\n+\s*/g, ' ')
            .trim()
            .replace(/\|/g, '\\|')
        );
        out.push('| ' + cells.join(' | ') + ' |');
        if (rowIdx === 0) {
          const delims = cellEls.map(c => {
            const align = (
              c.getAttribute('align') ||
              (c.style && c.style.textAlign) ||
              ''
            ).toLowerCase();
            if (align === 'center') return ':---:';
            if (align === 'right') return '---:';
            return '---';
          });
          out.push('| ' + delims.join(' | ') + ' |');
        }
      });
      return out.length ? '\n' + out.join('\n') + '\n\n' : '';
    }

    let inner = '';
    if (tag === 'ul' || tag === 'ol') {
      let idx = 1;
      for (const child of node.children) {
        if (child.tagName && child.tagName.toLowerCase() === 'li') {
          const prefix = tag === 'ol' ? `${idx++}. ` : '- ';
          const indent = '  '.repeat(listDepth);
          let liText = '';
          for (const gc of child.childNodes) {
            liText += serializeNode(gc, listDepth + 1);
          }
          inner += indent + prefix + liText.trim() + '\n';
        }
      }
      return inner + (listDepth === 0 ? '\n' : '');
    }

    for (const child of node.childNodes) {
      inner += serializeNode(child, listDepth);
    }

    if (/^h[1-6]$/.test(tag)) {
      const level = Number(tag[1]);
      return '#'.repeat(level) + ' ' + inner.trim() + '\n\n';
    }
    if (tag === 'p') {
      let trimmed = inner.trim();
      const isStreaming = Boolean(stopBtn) || ariaBusy === 'true';
      if (isStreaming && trimmed && !node.querySelector('table')) {
        // Before <table-block> mounts, Gemini briefly renders the raw Markdown
        // header/delimiter lines ("| col1 | col2 |\n| :---") inside a <p>.
        // Hold back trailing raw pipe lines while streaming so they don't emit
        // a ":---" prefix that changes to "---" when <table-block> mounts.
        const lines = trimmed.split('\n');
        while (lines.length && /^\s*\|/.test(lines[lines.length - 1])) {
          lines.pop();
        }
        trimmed = lines.join('\n').trim();
      }
      return trimmed ? trimmed + '\n\n' : '';
    }
    if (tag === 'blockquote') {
      return inner.trim().split('\n').map(l => '> ' + l).join('\n') + '\n\n';
    }
    if (tag === 'strong' || tag === 'b') {
      return inner ? '**' + inner + '**' : '';
    }
    if (tag === 'em' || tag === 'i') {
      return inner ? '*' + inner + '*' : '';
    }
    if (tag === 'a') {
      const href = node.getAttribute('href') || '';
      const txt = inner.trim();
      if (href && /^https?:\/\//i.test(href) && txt && txt !== href) {
        return responseLink(node, txt, `[${txt}](${href})`);
      }
      return txt || href;
    }
    return inner;
  }

  let text = '';
  if (mc) {
    text = restoreResponseLinks(serializeNode(mc)).replace(/\n{3,}/g, '\n\n').trim();
    if (!text) {
      text = (mc.innerText || '').trim();
    }
    if (last && hasActionBar) {
      const seen = new Set();
      const sources = [];
      for (const a of last.querySelectorAll('sources-carousel-inline a[href], source-inline-chip a[href], source-footnote a[href], .sources-list a[href]')) {
        if (a.closest(thoughtsSelector)) continue;
        const href = (a.getAttribute('href') || '').trim();
        if (!/^https?:\/\//i.test(href) || seen.has(href)) continue;
        seen.add(href);
        const label = (a.innerText || a.textContent || a.getAttribute('aria-label') || '')
          .replace(/\s+/g, ' ')
          .trim();
        if (label && label !== href) {
          sources.push(`- [${label}](${href})`);
        } else {
          sources.push(`- ${href}`);
        }
      }
      if (sources.length && text) {
        text += '\n\nSources:\n' + sources.join('\n');
      }
    }
  }
  const thoughtsTextRaw = thoughtsEl
    ? ((thoughtsEl.innerText || thoughtsEl.textContent || '').trim())
    : '';
  const thoughtsScope = thoughtsEl || last;
  const thoughtsHeaderEl = thoughtsScope
    ? thoughtsScope.querySelector('[data-test-id="thoughts-header-button"], button.thoughts-header-button, .thoughts-header, .thought-header, .thoughts-header-text, .thinking-status, .thinking-indicator, [role="status"]')
    : null;
  const thoughtsHeader = thoughtsHeaderEl
    ? (thoughtsHeaderEl.innerText || thoughtsHeaderEl.textContent || thoughtsHeaderEl.getAttribute('aria-label') || '').trim().replace(/\s+/g, ' ')
    : '';
  const busyScope = last || thoughtsEl;
  const busyIndicator = busyScope
    ? busyScope.querySelector('[aria-busy="true"], mat-progress-spinner, [role="progressbar"], .loading, .thinking-indicator, .spinner')
    : null;
  const verification = location.pathname.startsWith('/sorry') || Boolean(
    document.querySelector('iframe[src*="recaptcha"][src*="bframe"], iframe[src*="challenge"], #captcha, form[action*="/sorry"]')
  );
  const thinking = !hasActionBar && (
    Boolean(thoughtsEl && !text) ||
    Boolean(thoughtsEl && thoughtsEl.getAttribute('aria-busy') === 'true') ||
    Boolean(stopBtn && !text) ||
    Boolean(busyIndicator && !text)
  );
  const thoughtLines = thoughtsTextRaw
    .split('\n')
    .map(s => s.replace(/\s+/g, ' ').trim())
    .filter(Boolean);
  const latestThoughtLine = thoughtLines.length ? thoughtLines[thoughtLines.length - 1] : '';
  let thinkingText = '';
  if (latestThoughtLine && latestThoughtLine !== thoughtsHeader) {
    thinkingText = latestThoughtLine;
  } else if (thoughtsHeader) {
    thinkingText = thoughtsHeader;
  } else if (latestThoughtLine) {
    thinkingText = latestThoughtLine;
  } else if (thinking) {
    thinkingText = 'Thinking...';
  }
  if (thinkingText.length > 120) {
    thinkingText = thinkingText.slice(thinkingText.length - 120);
  }
  const thinkingLen = thoughtsTextRaw.length;

  const edEl = document.querySelector(
    'rich-textarea div.ql-editor[contenteditable="true"], rich-textarea div[role="textbox"][contenteditable="true"], div.ql-editor[contenteditable="true"]'
  );
  const editorTextLen = edEl ? (edEl.innerText || '').trim().length : 0;
  const userCount = document.querySelectorAll('user-query').length;
  const pendingChipCount = document.querySelectorAll('uploader-file-preview').length;
  const modeBtn = document.querySelector('[data-test-id="bard-mode-menu-button"]');
  const modePill = modeBtn
    ? (modeBtn.querySelector('[data-test-id="logo-pill-label-container"]')?.innerText || '').trim().replace(/\s+/g, ' ')
    : '';
  const mediaEls = last
    ? [...last.querySelectorAll('generated-image img, .image-container img, img[src*="googleusercontent"], img[src*="blob:"], response-element img, img')]
        .filter(img => {
          const cls = (img.className || '').toLowerCase();
          if (cls.includes('avatar')) return false;
          const w = img.naturalWidth || img.width || 0;
          const h = img.naturalHeight || img.height || 0;
          if (w > 0 && w < 48 && h > 0 && h < 48) return false;
          return Boolean(img.currentSrc || img.src);
        })
    : [];
  const hasMedia = mediaEls.length > 0;
  const mediaReady = hasMedia && mediaEls.every(img => img.complete && img.naturalWidth > 0);

  return {
    url: location.href,
    verification,
    respCount: resps.length,
    userCount,
    pendingChipCount,
    hasStop: Boolean(stopBtn),
    ariaBusy,
    hasActionBar,
    hasMedia,
    mediaReady,
    editorTextLen,
    modePill,
    thinking,
    thinkingText,
    thinkingLen,
    text
  };
})()
