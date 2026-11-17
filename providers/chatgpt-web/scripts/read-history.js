// Read complete history for the selected workspace. Tokens stay in this page.
async (args) => {
  const access = () => {
    if (/just a moment|security verification/i.test(document.title)) return 'verification_required';
    if(location.origin!=='https://chatgpt.com' || /\/auth\//.test(location.pathname))return 'auth_required';
    return null;
  };
  const blocked=access();if(blocked)return {state:blocked};
  const deadline=Date.now()+(args.timeoutMs || 60000);
  const read=async(url,headers={})=>{
    if(Date.now()>=deadline)throw Error('deadline');
    return fetch(url,{headers,credentials:'same-origin',cache:'no-store',redirect:'error',signal:AbortSignal.timeout(Math.min(15000,deadline-Date.now()))});
  };
  try{
    const auth=await read('/api/auth/session');
    if(!auth.ok)return {state:'auth_required'};
    const session=await auth.json();
    const scope=session.account?.id;
    if(!session.accessToken || typeof scope!=='string' || !scope)return {state:'auth_required'};
    if(args.scope && args.scope!==scope)return {state:'error',error:'ChatGPT workspace changed'};
    const headers={Authorization:'Bearer '+session.accessToken,'chatgpt-account-id':scope};
    const urls=new Set();
    for(const archived of [false,true]){
      let offset=0,expectedTotal=null,complete=false;
      for(let page=0;page<1000;page++){
        if(access())return {state:access()};
        const r=await read('/backend-api/conversations?offset='+offset+'&limit=28&order=updated&is_archived='+archived,headers);
        if(r.status===401 || r.status===403)return {state:'auth_required'};
        if(!r.ok)return {state:'error',error:'ChatGPT history request failed ('+r.status+')'};
        const v=await r.json();
        if(!Array.isArray(v.items) || !Number.isInteger(v.total) || v.total<0 || v.offset!==offset)
          return {state:'error',error:'ChatGPT history response schema changed'};
        if(expectedTotal!==null && expectedTotal!==v.total)return {state:'error',error:'ChatGPT history changed during pagination; retry reading history'};
        expectedTotal=v.total;
        for(const chat of v.items){
          if(!/^[a-f0-9-]{36}$/i.test(chat.id))return {state:'error',error:'invalid ChatGPT conversation identity'};
          urls.add('https://chatgpt.com/c/'+chat.id);
        }
        offset+=v.items.length;
        if(offset===expectedTotal){complete=true;break;}
        if(!v.items.length || offset>expectedTotal)return {state:'error',error:'incomplete ChatGPT history pagination'};
        await new Promise(r=>setTimeout(r,150));
      }
      if(!complete)return {state:'error',error:'ChatGPT history exceeds pagination limit'};
    }
    // The user can switch workspaces while a read is in progress.
    const current=await read('/api/auth/session').then(r=>r.json());
    if(access() || current.account?.id!==scope)return {state:access() || 'error',error:'ChatGPT workspace changed'};
    return {state:'ready',scope,complete:true,urls:[...urls]};
  }catch{return {state:access() || 'error',error:'ChatGPT history could not be read completely'};}
}
