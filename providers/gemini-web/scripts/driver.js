(args => {
  const visible = el => !!el && el.getClientRects().length > 0;
  const one = selector => [...document.querySelectorAll(selector)].find(visible);
  const enabled = el => el && !el.disabled && el.getAttribute('aria-disabled') !== 'true';
  const icon = name => `mat-icon[fonticon="${name}"], mat-icon[data-mat-icon-name="${name}"]`;
  const buttonIcon = name => `button:has(${icon(name)})`;
  const deepAll = (selector, root = document) => {
    const out = [...root.querySelectorAll(selector)];
    for (const el of root.querySelectorAll('*')) {
      if (el.shadowRoot) out.push(...deepAll(selector, el.shadowRoot));
    }
    return out;
  };
  const editor = () => one('rich-textarea [contenteditable="true"], .ql-editor[contenteditable="true"]');
  // Quill renders each inserted line as a paragraph; innerText adds extra layout newlines.
  const editorText = () => { const el=editor(); if(!el)return ''; const children=[...el.children]; return children.length && children.every(e=>e.tagName==='P') ? children.map(e=>e.textContent||'').join('\n') : el.innerText||''; };
  const stop = () => one('[data-test-id="stop-button"], ' + buttonIcon('stop'));
  const temp = () => one('[data-test-id="temp-chat-button"], [data-test-id="temporary-chat-button"], [data-test-id="temp-chat-button-container"] button, temp-chat-button button');
  const temporary = () => {
    const b=temp();
    return !!one('chat-window.is-temporary-chat, .is-temporary-chat, .temporary-chat-card, .temporary-chat-card-container, [data-test-id="temp-chat-badge"], [data-test-id="temporary-chat-header"], .temp-chat-banner, .temporary-chat-indicator, temp-chat-button .temp-chat-on') || !!b && (b.classList.contains('selected') || b.classList.contains('active') || b.getAttribute('aria-pressed')==='true' || b.getAttribute('aria-selected')==='true' || !!b.querySelector(icon('close')));
  };
  const verification = () => location.pathname.startsWith('/sorry') || !!one('iframe[src*="recaptcha"][src*="bframe"], iframe[src*="challenge"], #captcha, form[action*="/sorry"]');
  const auth = () => !!document.querySelector('[data-test-id="profile-avatar"], a[href*="SignOutOptions"]');
  const emailRe = /[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}/i;
  const extractEmail = text => {
    if (typeof text !== 'string') return null;
    const match = text.match(emailRe);
    return match ? match[0].toLowerCase() : null;
  };
  const accountIdentity = () => {
    try {
      const wiz = window.WIZ_global_data && window.WIZ_global_data.oPEP7c;
      const fromWiz = extractEmail(wiz);
      if (fromWiz) return fromWiz;
    } catch (_) {}
    for (const selector of [
      'a[href*="SignOutOptions"]',
      '[data-test-id="profile-avatar"]',
      '[data-test-id="profile-avatar"] button',
      'a[aria-label*="@"]',
      'button[aria-label*="@"]',
      '[aria-label*="Google Account"]'
    ]) {
      for (const el of document.querySelectorAll(selector)) {
        for (const attr of ['aria-label', 'title', 'data-email']) {
          const email = extractEmail(el.getAttribute(attr));
          if (email) return email;
        }
      }
    }
    return null;
  };
  const accountSlot = () => {
    const m = location.pathname.match(/^\/u\/(\d+)(?:\/|$)/);
    if (m) return m[1];
    try {
      const q = new URLSearchParams(location.search).get('authuser');
      if (q && /^\d+$/.test(q)) return q;
    } catch (_) {}
    return '0';
  };
  const menuFor = thinking => {
    const root=one('gem-menu[data-test-id="gem-mode-menu"]');
    if(!thinking)return root;
    const trigger=menuItems(root).find(el=>el.getAttribute('value')==='thinking_level');
    if(!trigger)return null;
    const controls=trigger.getAttribute('aria-controls');
    const controlled=controls?document.getElementById(controls):null;
    if(controlled)return controlled.matches('gem-menu')?controlled:[...controlled.querySelectorAll('gem-menu')].find(visible)||controlled;
    const inline=trigger.querySelector('.expand-content');
    if(inline)return inline;
    const overlay=[...document.querySelectorAll('.cdk-overlay-container gem-menu, .cdk-overlay-container [role="menu"]')].filter(visible).find(el=>el!==root&&!root?.contains(el));
    return overlay||trigger;
  };
  const menuItems = menu => menu?[...menu.querySelectorAll('gem-menu-item')].filter(el=>{
    let parent=el.parentElement;
    while(parent&&parent!==menu){if(parent.matches('gem-menu,gem-menu-item'))return false;parent=parent.parentElement;}
    return parent===menu;
  }):[];
  const menuId = (el,index) => el.getAttribute('data-mode-id')||el.getAttribute('value')||`option:${index}`;
  const chipSelector = 'uploader-file-preview, uploader-image-preview, [data-test-id="file-preview"], [data-test-id="image-preview"], .file-preview, .image-preview, .gem-attachment';
  const chips = () => [...document.querySelectorAll(chipSelector)].filter(visible).filter(el => !el.parentElement?.closest(chipSelector)).map(el => ({
    name:el.querySelector('[data-test-id="file-name"], .file-name, .filename')?.textContent?.trim() || el.getAttribute('title') || el.querySelector('[title]')?.getAttribute('title') || [...el.querySelectorAll('[aria-describedby]')].flatMap(e=>(e.getAttribute('aria-describedby')||'').split(/\s+/).map(id=>document.getElementById(id)?.textContent?.trim())).find(Boolean) || el.querySelector('img')?.getAttribute('alt') || '',
    pending:!!el.querySelector('mat-progress-spinner,mat-spinner,[role="progressbar"],[aria-busy="true"],.loading'),
    failed:!!el.querySelector('.gem-attachment-loading-error,'+icon('error')+','+icon('warning'))
  }));
  const sendBtn = () => one('[data-test-id="send-button"],button.send-button,'+buttonIcon('arrow_upward'));
  const newChatBtn = () => one('[data-test-id="new-chat-button"] a, [data-test-id="new-chat-button"] button') || one('[data-test-id="new-chat-button"], a[href="/app"][mat-list-item], a[href^="/u/"][href$="/app"][mat-list-item]');
  const modelsBtn = () => one('[data-test-id="bard-mode-menu-button"],button.input-area-switch');
  const thinkingTrigger = () => menuItems(menuFor(false)).find(el=>el.getAttribute('value')==='thinking_level');
  const uploadBtn = () => one('[data-test-id="upload-button"],button:has(mat-icon[fonticon="add_2"]),button:has(mat-icon[fonticon="add"]),button:has(mat-icon[fonticon="plus"])');
  const rectOf = (el, extra = {}) => {
    if (!el) return {ok:false};
    el.scrollIntoView({block:'nearest',inline:'nearest'});
    const r = el.getBoundingClientRect();
    if (!r || r.width < 2 || r.height < 2) return {ok:false, no_box:true, ...extra};
    return {ok:true, x:r.left, y:r.top, width:r.width, height:r.height, ...extra};
  };
  switch(args.op) {
    case 'probe': {
      const identity = accountIdentity();
      return {editor:!!editor(),has_draft:!!editorText().trim(),signed_in:auth(),identity,label:identity,slot:accountSlot(),verification:verification(),temporary:temporary(),generating:!!stop(),url:location.origin+location.pathname,model:one('[data-test-id="logo-pill-label-container"]')?.textContent?.trim()||null,model_button:modelsBtn()?.textContent?.trim()||null,uploads:!!document.querySelector('input[type="file"]'),models:!!modelsBtn(),response_count:document.querySelectorAll('model-response').length};
    }
    case 'target_rect': {
      switch(args.target) {
        case 'editor': return rectOf(editor());
        case 'send': { const el=sendBtn(); return enabled(el)?rectOf(el):{ok:false}; }
        case 'stop': { const el=stop(); return enabled(el)?rectOf(el):{ok:false}; }
        case 'new_chat': { const el=newChatBtn(); return enabled(el)?rectOf(el):{ok:false,fallback:true}; }
        case 'temporary': {
          if(temporary())return {ok:true,already_active:true};
          const b=temp();
          return enabled(b)?rectOf(b.querySelector('button,a')||b):{ok:false};
        }
        case 'models_open': {
          const b=modelsBtn();
          if(!enabled(b))return {ok:false};
          if(one('gem-menu[data-test-id="gem-mode-menu"]'))return {ok:true,already_open:true};
          return rectOf(b);
        }
        case 'thinking_open': {
          const trigger=thinkingTrigger();
          if(!trigger||!enabled(trigger))return {ok:true,available:false};
          if(trigger.getAttribute('aria-expanded')==='true')return {ok:true,already_open:true,available:true};
          return rectOf(trigger,{available:true});
        }
        case 'select': {
          const menu=menuFor(args.thinking);
          const el=menuItems(menu).find((el,index)=>menuId(el,index)===args.id);
          return (!enabled(el)||el.querySelector(icon('lock')))?{ok:false}:rectOf(el);
        }
        case 'uploads_open': {
          if(deepAll('input[type="file"]').length)return {ok:true,already_open:true};
          const b=uploadBtn();
          if(!enabled(b))return {ok:false};
          if(b.getAttribute('aria-expanded')==='true'&&!args.reset)return {ok:true,already_open:true};
          return rectOf(b);
        }
        default: return {ok:false};
      }
    }
    case 'select_draft': { const el=editor();if(!el||stop())return {ok:false};el.focus();const range=document.createRange();range.selectNodeContents(el);const selection=window.getSelection();selection.removeAllRanges();selection.addRange(range);return {ok:true}; }
    case 'prepare': {
      const el=editor();if(!el||stop())return {ok:false,reason:'editor_unavailable'};
      if(el.innerText.trim())return {ok:false,reason:'composer_not_empty'};
      el.focus();return {ok:true};
    }
    case 'verify_input': return {ok:editorText().replace(/\r\n/g,'\n').trim()===args.text.trim(),actual_length:editorText().length};
    case 'send': {
      const btn=sendBtn();
      if(!enabled(btn))return {ok:false};btn.click();return {ok:true};
    }
    case 'stop': {const b=stop();if(enabled(b)){b.click();return {ok:true};}return {ok:false};}
    case 'new_chat': {
      const b=newChatBtn();
      if(enabled(b)){b.click();return {ok:true};}
      const expand=one('[data-test-id="side-nav-sparkle-button"]');
      if(enabled(expand)&&expand.querySelector('[fonticon="side_nav_expand"]')){expand.click();return {ok:false,expanding:true};}
      return {ok:false};
    }
    case 'temporary': {if(temporary())return {ok:true};const b=temp();if(enabled(b)){(b.querySelector('button,a')||b).click();return {ok:true};}return {ok:false};}
    case 'models_open': {const b=modelsBtn();if(!enabled(b))return {ok:false};if(!one('gem-menu[data-test-id="gem-mode-menu"]'))b.click();return {ok:true};}
    case 'thinking_open': {
      const trigger=thinkingTrigger();
      if(!trigger)return {ok:true,available:false};
      if(!enabled(trigger))return {ok:true,available:false};
      if(trigger.getAttribute('aria-expanded')!=='true')trigger.click();return {ok:true,available:true};
    }
    case 'models': {
      const menu=menuFor(args.thinking);if(!menu)return [];
      return menuItems(menu).map((el,index)=>({index:index+1,mode_id:menuId(el,index),label:(el.querySelector('.label,.mode-title')?.textContent||el.innerText.split('\n')[0]||'').trim(),sublabel:(el.querySelector('.sublabel,.mode-desc')?.textContent||'').trim(),selected:el.getAttribute('aria-checked')==='true'||el.classList.contains('selected')||!!el.querySelector(icon('check')),locked:!enabled(el)||!!el.querySelector(icon('lock')+','+icon('hourglass_empty')),rate_limit_reset:el.querySelector('.rate-limit-info,.limit-reset')?.textContent?.trim()||null})).filter(item=>item.mode_id!=='thinking_level');
    }
    case 'select': {
      const menu=menuFor(args.thinking);
      const el=menuItems(menu).find((el,index)=>menuId(el,index)===args.id);
      if(!enabled(el)||el.querySelector(icon('lock')))return {ok:false};el.click();return {ok:true};
    }
    case 'chips': return chips();
    case 'overlay_open': return !!one('.cdk-overlay-backdrop, gem-menu[data-test-id="gem-mode-menu"]');
    case 'uploads_open': {
      if(deepAll('input[type="file"]').length)return {ok:true};
      const b=uploadBtn();
      if(!enabled(b))return {ok:false};
      if(b.getAttribute('aria-expanded')==='true'){
        if(args.reset){b.click();return {ok:false,resetting:true};}
        return {ok:true};
      }
      b.click();
      return {ok:true};
    }
    case 'file_input': {
      const inputs=deepAll('input[type="file"]');
      const isImageAccept = a => /image|\.(?:png|jpe?g|webp|gif)\b/i.test(a || '');
      const input=inputs.find(el=>args.image ? isImageAccept(el.accept) : !isImageAccept(el.accept)) || inputs.find(el=>!el.accept);
      if(!input)return null;return input;
    }
    case 'media': {
      const last=[...document.querySelectorAll('model-response')].at(-1);
      return last?[...last.querySelectorAll('generated-image img, generated-images img, .generated-image img, img.image')].filter(img=>img.complete&&img.naturalWidth>128).map(img=>({url:img.currentSrc||img.src})):[];
    }
    default: throw Error('unknown adapter operation');
  }
})
