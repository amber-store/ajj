const content = document.querySelector('#content');
const escape = value => String(value).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
let files = {};
let query = '';
const changes = [
  {id:'kqvuxnso',title:'Show transfer progress during fetch and push',author:'zimbatm',time:'2 hours ago',status:'Ready for review',bookmark:'transfer-progress',diff:'+ progress.set_phase("Uploading objects");\n+ progress.set_total(closure.len());\n+ progress.finish();'},
  {id:'rwmplkts',title:'Preserve conflicted bookmarks on fetch',author:'alice',time:'5 hours ago',status:'Conflict',bookmark:'bookmark-merge',diff:'<<<<<<< local bookmark\n  main → 5101a8e9\n=======\n  main → 5101b7d2\n>>>>>>> remote bookmark\n\nFetch, resolve the bookmark, then push again.'},
  {id:'tyzmnxqp',title:'Prune complete subtrees when fetching history',author:'zimbatm',time:'Yesterday',status:'Ready for review',bookmark:'main',diff:'+ if store.has_complete_subtree(&key).await? {\n+     continue;\n+ }'}
];
const readme = `<article class="panel readme"><div class="panel-title"><span>▤ &nbsp; README.md</span><span>Overview</span></div><div class="readme-body"><h2>ajj</h2><p><strong>Jujutsu, with an amber-store backend.</strong></p><p>ajj is jj with a commit backend whose objects are <a href="https://github.com/amber-store/core">amber-store</a> objects. Bookmarks are shared through a dstore cluster. Everything else is jj, unchanged.</p><h3>Your workflow. Shared by default.</h3><p>Work with changes locally. Share bookmarks with your cluster. The same objects, wherever you build.</p><pre><span style="color:#8d927f"># Start a working copy</span>\nexport DSTORE_TICKET=dstore1…\najj dstore clone myrepo --prefix myrepo\ncd myrepo\n\n<span style="color:#8d927f"># Make a change and share it</span>\najj commit -m "Build something good"\najj bookmark move main --to @-\najj dstore push</pre><p>Commits are content-addressed amber objects. A remote bookmark is a dstore reference holding the commit’s key.</p><a href="#code/README.md">Read the full README →</a></div></article>`;
function fileView(path='') {
  if (Object.hasOwn(files,path)) {
    content.innerHTML = `<div class="toolbar"><a class="subtle" href="#code/${encodeURIComponent(path.split('/').slice(0,-1).join('/'))}">← Back to files</a><span class="subtle">Repository snapshot</span></div><div class="panel"><div class="panel-title">${escape(path)}</div><pre class="file-source">${escape(files[path])}</pre></div>`;
    return;
  }
  const prefix = path ? `${path}/` : '';
  const entries = [...new Set(Object.keys(files).filter(n=>n.startsWith(prefix)).map(n=>n.slice(prefix.length).split('/')[0]))].map(name=>({name,directory:!Object.hasOwn(files,prefix+name)})).sort((a,b)=>Number(b.directory)-Number(a.directory)||a.name.localeCompare(b.name));
  content.innerHTML = `<div class="toolbar"><div><select aria-label="Bookmark" id="bookmark-select"><option>main</option><option>transfer-progress</option><option>bookmark-merge</option></select> <span class="subtle">&nbsp; / ${escape(path)}</span></div><input class="search" id="file-search" type="search" placeholder="Find a file…" aria-label="Find a file" value="${escape(query)}"></div><div class="panel"><div class="latest"><span class="avatar">Z</span><strong>zimbatm</strong><span>Prune complete subtrees on fetch</span><span class="time"><span class="change-id">tyzmnxqp</span> · yesterday</span></div><div id="file-list"></div></div>${path?'':readme}`;
  const list = () => { document.querySelector('#file-list').innerHTML = (path?`<a class="file-row" href="#code/${encodeURIComponent(path.split('/').slice(0,-1).join('/'))}"><span>↰</span><span>..</span></a>`:'') + entries.filter(e=>e.name.toLowerCase().includes(query.toLowerCase())).map(e=>`<a class="file-row" href="#code/${encodeURIComponent(prefix+e.name)}"><span class="file-icon">${e.directory?'▱':'▤'}</span><span>${escape(e.name)}</span><span class="file-message">${e.directory?'Repository sources':e.name==='README.md'?'Document the amber backend':'Initial repository snapshot'}</span><span class="file-time">snapshot</span></a>`).join('') || '<div class="empty">No files match your search.</div>'; };
  list();
  document.querySelector('#file-search').addEventListener('input',e=>{query=e.target.value;list();});
  document.querySelector('#bookmark-select').addEventListener('change',e=>{if(e.target.value!=='main')location.hash=`changes/${changes.find(c=>c.bookmark===e.target.value).id}`;});
}
function changesView(id) {
  content.innerHTML = `<h2 class="section-heading">A home for every change.</h2><p class="intro">Review the intent, even as the commit evolves. Sample change stack.</p><div class="toolbar"><select id="status-filter" aria-label="Filter changes"><option value="all">All changes · 3</option><option value="Ready for review">Ready for review</option><option value="Conflict">Conflicted</option></select><input id="change-search" class="search" placeholder="Search changes…" aria-label="Search changes" type="search"></div><div class="panel" id="change-list"></div>`;
  const render = () => {
    const status = document.querySelector('#status-filter').value;
    const search = document.querySelector('#change-search').value.toLowerCase();
    document.querySelector('#change-list').innerHTML = changes.filter(c=>(status==='all'||c.status===status)&&`${c.title} ${c.id} ${c.bookmark}`.toLowerCase().includes(search)).map(c=>`<article class="change-row"><span class="node"></span><div class="change-body"><a class="change-title" href="#changes/${c.id}">${escape(c.title)}</a><div class="change-meta"><span class="change-id">${c.id}</span> &nbsp; ${c.author} · ${c.time}<br>⚑ ${c.bookmark} &nbsp; <span class="pill ${c.status==='Conflict'?'conflict':''}">${c.status}</span></div>${id===c.id?`<pre class="diff">${escape(c.diff)}</pre><p class="subtle">Illustrative ${c.status==='Conflict'?'bookmark conflict':'diff'} · stable change ID ${c.id}</p><a class="subtle" href="#changes">Collapse ↑</a>`:''}</div></article>`).join('') || '<div class="empty">No changes match your filters.</div>';
  };
  render();
  document.querySelector('#status-filter').addEventListener('change',render);
  document.querySelector('#change-search').addEventListener('input',render);
}
function render() {
  const [tab='code',...parts] = (location.hash.slice(1)||"code").split('/');
  let path; try { path=decodeURIComponent(parts.join('/')); } catch { path=''; }
  document.querySelectorAll('[data-tab]').forEach(a=>{a.classList.toggle('active',a.dataset.tab===tab); if(a.dataset.tab===tab)a.setAttribute('aria-current','page');else a.removeAttribute('aria-current');});
  document.querySelectorAll('.side-link').forEach(a=>a.classList.toggle('selected',a.getAttribute('href')===(tab==='changes'?'#changes':'#code')));
  if(tab==='code'||!tab)fileView(path);
  else if(tab==='changes')changesView(path);
  else if(tab==='bookmarks')content.innerHTML=`<h2 class="section-heading">Bookmarks</h2><p class="intro">Named pointers to changes, shared as dstore references. Sample positions.</p><div class="panel">${changes.slice().reverse().map(c=>`<div class="change-row"><span class="file-icon">⚑</span><div class="change-body"><a class="change-title" href="#changes/${c.id}">${c.bookmark}</a> ${c.bookmark==='main'?'<span class="pill">Default</span>':''}<div class="change-meta">ajj/${c.bookmark} <br><span class="change-id">${c.id}</span> · ${c.title}</div></div><span class="pill ${c.status==='Conflict'?'conflict':''}">${c.status==='Conflict'?'Diverged':'Tracked'}</span></div>`).join('')}</div>`;
  else if(tab==='activity')content.innerHTML=`<h2 class="section-heading">Repository activity</h2><p class="intro">A shared history of changes. Sample activity.</p><div class="panel">${changes.map(c=>`<article class="change-row"><span class="avatar">${c.author[0].toUpperCase()}</span><div><strong>${c.author}</strong><p class="intro">${c.status==='Conflict'?'Fetched a divergent bookmark':'Updated a change'} · ${c.time}</p><a class="change-title" href="#changes/${c.id}">${c.title}</a><div class="change-meta"><span class="change-id">${c.id}</span> → ${c.bookmark}</div></div></article>`).join('')}</div>`;
  else content.innerHTML='<div class="empty">Page not found. <a href="#code">Return to repository</a></div>';
}
document.querySelector('#clone-button').addEventListener('click',()=>document.querySelector('#clone-dialog').showModal());
document.querySelector('#copy').addEventListener('click',async()=>{
  try { await navigator.clipboard.writeText(document.querySelector('#clone-command').textContent);document.querySelector('#copy').textContent='Copied!'; }
  catch { document.querySelector('#copy').textContent='Select and copy the command above'; }
});
window.addEventListener('hashchange',()=>{query='';render();});
try { const response=await fetch('repository.json');if(!response.ok)throw new Error('load');files=await response.json();render(); }
catch {content.innerHTML='<div class="empty">Unable to load repository files. Refresh to try again.</div>';}
