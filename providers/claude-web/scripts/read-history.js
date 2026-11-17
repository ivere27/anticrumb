// Read every page in the currently selected organization, including archives.
async (args) => {
  const access = () => {
    if (/just a moment|security verification/i.test(document.title)) return 'verification_required';
    if (location.origin !== 'https://claude.ai' || /\/login|\/oauth/.test(location.pathname)) return 'auth_required';
    return null;
  };
  const org = () => decodeURIComponent(document.cookie.split(';').map(c=>c.trim()).find(c=>c.startsWith('lastActiveOrg='))?.slice(14) || '');
  const blocked=access(); if(blocked)return {state:blocked};
  const scope=org();
  if(!/^[a-f0-9-]{36}$/i.test(scope) || (args.scope && args.scope!==scope))return {state:'error',error:'Claude organization changed or is unavailable'};
  const urls=new Set();
  const deadline=Date.now()+(args.timeoutMs || 60000);
  try {
    for(const archived of [false,true]){
      let offset=0,complete=false;
      for(let page=0;page<1000;page++){
        if(Date.now()>=deadline)return {state:'error',error:'history pagination timed out'};
        if(access() || org()!==scope)return {state:access() || 'error',error:'Claude organization changed'};
        const endpoint='/api/organizations/'+scope+'/chat_conversations_v2?limit=30&offset='+offset+'&archived='+archived+'&consistency=strong';
        const r=await fetch(endpoint,{credentials:'same-origin',cache:'no-store',redirect:'error',signal:AbortSignal.timeout(Math.min(15000,deadline-Date.now()))});
        if(r.status===401 || r.status===403)return {state:'auth_required'};
        if(!r.ok)return {state:'error',error:'Claude history request failed ('+r.status+')'};
        const v=await r.json();
        if(!Array.isArray(v.data) || typeof v.has_more!=='boolean')return {state:'error',error:'Claude history response schema changed'};
        for(const chat of v.data){
          if(!/^[a-f0-9-]{36}$/i.test(chat.uuid))return {state:'error',error:'invalid Claude conversation identity'};
          urls.add('https://claude.ai/chat/'+chat.uuid);
        }
        if(!v.has_more){complete=true;break;}
        if(!v.data.length)return {state:'error',error:'incomplete Claude history cursor chain'};
        offset+=v.data.length;
        await new Promise(r=>setTimeout(r,150));
      }
      if(!complete)return {state:'error',error:'Claude history exceeds pagination limit'};
    }
    if(access() || org()!==scope)return {state:access() || 'error',error:'Claude organization changed'};
    return {state:'ready',scope,complete:true,urls:[...urls]};
  }catch{return {state:access() || 'error',error:'Claude history could not be read completely'};}
}
