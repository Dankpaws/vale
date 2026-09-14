import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
const source = readFileSync(new URL('../../static/vale-interactions.js', import.meta.url), 'utf8');
const offlineSource = readFileSync(new URL('../../static/offline.js', import.meta.url), 'utf8');
const slice = (start,end) => {
 const first=source.indexOf(start),last=source.indexOf(end);
 assert.ok(first>=0&&last>first,`production test boundary missing: ${start}`);
 return source.slice(first,last);
};
function threadHarness(groupCount = 100, nodesPerGroup = 20) {
 const counts = {groupScans:0, hiddenWrites:0, projectionScans:0};
 const ids = new Map();
 const groups = [];
 const projection = {querySelectorAll: selector => {counts.projectionScans++;return selector === '[data-thread-group]' ? groups : groups.flatMap(g=>g.nodes)}};
 const element = () => ({dataset:{}, attributes:new Map(), classList:{values:new Set(),contains(name){return this.values.has(name)},toggle(name,on){if(on)this.values.add(name);else this.values.delete(name)}},getAttribute(name){return this.attributes.get(name)||null},setAttribute(name,value){this.attributes.set(name,value)}, set hidden(value){this.isHidden=value;counts.hiddenWrites++},get hidden(){return this.isHidden}});
 for(let g=0;g<groupCount;g++){
  const group = {dataset:{threadGroupId:`g${g}-0`}, nodes:[],closest:selector=>projection};
  const descendants = element();group.descendants=descendants;
  descendants.querySelectorAll=()=>group.nodes.slice(1);
  ids.set(`replies-${g}`,descendants);
  const replies = element();
  replies.getAttribute=name=>name==='aria-controls'?`replies-${g}`:replies.attributes.get(name)||null;
  replies.querySelector=()=>null;
  replies.closest=selector=>selector==='[data-thread-group]'?group:projection;
  replies.attributes.set('aria-expanded','true');
  group.querySelector=selector=>selector===':scope > [data-thread-descendants]'?descendants:replies;
  group.querySelectorAll=()=>{counts.groupScans++;return group.nodes};
  for(let n=0;n<nodesPerGroup;n++){
   const node=element();
   node.classList.values.add('comment');
   node.dataset={threadNodeId:`g${g}-${n}`,threadAncestorPath:n===0?'':`g${g}-0`,threadRootId:`g${g}-0`};
   const content=element();ids.set(`content-${g}-${n}`,content);
   const collapse=element();collapse.getAttribute=name=>name==='aria-controls'?`content-${g}-${n}`:collapse.attributes.get(name)||null;
   collapse.closest=selector=>selector==='.comment'?node:selector==='[data-thread-group]'?group:projection;
   node.querySelector=selector=>selector==='[data-replies-toggle]'?(n===0?replies:null):selector==='[data-comment-collapse]'?collapse:null;
   node.closest=selector=>selector==='[data-thread-group]'?group:projection;
   group.nodes.push(node);
  }
  groups.push(group);
 }
 const context={document:{querySelector:()=>projection,getElementById:id=>ids.get(id)},scheduleNavigationStateWrite:()=>{}};
 vm.runInNewContext(slice('\tconst ancestorIds =', '\tconst keywordFilteredComments =')+slice('\tconst expandCommentSearchPath =', '\tconst activateCommentSearchMatch =')+'\nthis.api={setCommentState,setRepliesState,syncThreadProjection,expandCommentSearchPath};',context);
 return {api:context.api,counts,groups,projection};
}
function anchorHarness(count=2000, firstAt=0) {
 const counts={rectReads:0};
 const nodes=Array.from({length:count},(_,i)=>({dataset:{threadNodeId:`n${i}`},getBoundingClientRect(){counts.rectReads++;return {top:(i-firstAt)*100,bottom:(i-firstAt)*100+100,width:600,height:100}},matches:()=>true}));
 const context={refreshMobileFeedContext:null,effectiveTopInset:()=>50,threadNodeElements:()=>nodes,threadProjection:()=>({}),document:{querySelectorAll:()=>[]},window:{scrollY:0}};
 vm.runInNewContext(slice('\tconst capturePageAnchor =','\tconst captureFocus =')+'\nthis.capturePageAnchor=capturePageAnchor;',context);
 return {run:context.capturePageAnchor,counts,nodes,context};
}
function jumpHarness() {
 const counts={writes:0};const events=new Map();const frames=[];let top=1000;
 const text=()=>({set textContent(value){this.value=value;counts.writes++},get textContent(){return this.value}});
 const label=text(), icon=text();
 const control={attributes:new Map(),querySelector:s=>s==='[data-reading-jump-label]'?label:icon,setAttribute(name,value){this.attributes.set(name,value);counts.writes++},getAttribute(name){return this.attributes.get(name)},addEventListener(){}};
 const comments={getBoundingClientRect:()=>({top})},post={};
 const context={window:{innerHeight:844,matchMedia:()=>({matches:true}),addEventListener:(name,fn)=>events.set(name,fn)},document:{querySelector:s=>s==='[data-reading-jump]'?control:{getBoundingClientRect:()=>({height:50})},getElementById:id=>id==='comments'?comments:post},requestAnimationFrame:fn=>frames.push(fn)};
 vm.runInNewContext(slice('\tconst setupReadingJump =','\tconst applyConfirmedHiddenState =')+'\nsetupReadingJump();',context);
 return {counts,control,label,icon,scroll(nextTop=top){top=nextTop;events.get('scroll')();frames.splice(0).forEach(fn=>fn())}};
}
function settingsHarness() {
 const counts={serializations:0};const events=new Map();const formEvents=new Map();const frames=[];let formValue='default';
 const decor=()=>({hidden:false,style:{setProperty(){},removeProperty(){}},classList:{add(){},remove(){}}});
 const form={getBoundingClientRect:()=>({top:0,bottom:3000}),addEventListener:(name,fn)=>formEvents.set(name,fn),contains:()=>false};
 const bar=decor(), root=decor();
 const context={refreshSettingsSaveBar:null,window:{innerHeight:844,location:{href:'https://vale.example/settings'},matchMedia:()=>({matches:true,addEventListener(){}}),addEventListener:(name,fn)=>events.set(name,fn)},document:{readyState:'loading',documentElement:root,querySelector:()=>bar,getElementById:id=>id==='preferences-form'?form:id==='save'?{getBoundingClientRect:()=>({top:2900})}:{}},measureOffFlow:()=>50,visualViewportTop:()=>0,visualViewportBottom:()=>844,serializedFormIsDirty:(a,b)=>a!==b,settingsSaveBarShouldActivate:state=>state.dirty,URLSearchParams,FormData:class{constructor(){counts.serializations++}*[Symbol.iterator](){yield ['preference',formValue]}},requestAnimationFrame:fn=>frames.push(fn)};
 vm.runInNewContext(slice('\tconst setupSettingsSaveBar =','\tconst focusSavedReturnTarget =')+'\nsetupSettingsSaveBar();',context);
 const flush=()=>frames.splice(0).forEach(fn=>fn());
 return {counts,bar,scroll(){events.get('scroll')();flush()},change(value,type='input'){formValue=value;formEvents.get(type)();flush()},refresh(value=formValue){formValue=value;context.refreshSettingsSaveBar();flush()},restore(value){formValue=value;events.get('pageshow')();flush()}};
}

// Exercise the production functions with counted DOM operations. The counts
// detect work proportional to unrelated comments, independently of CPU speed.
test('a comment or reply disclosure only synchronizes its owning thread group', () => {
 const h=threadHarness();
 const root=h.groups[0].nodes[0], parent=h.groups[0].nodes[1], child=h.groups[0].nodes[2];
 child.dataset.threadAncestorPath=`${root.dataset.threadNodeId} ${parent.dataset.threadNodeId}`;
 const button=parent.querySelector('[data-comment-collapse]');
 h.api.setCommentState(button,false,false);
 assert.equal(h.counts.groupScans,1);
 assert.equal(h.counts.projectionScans,0);
 assert.equal(h.counts.hiddenWrites,21);
 assert.equal(child.hidden,true,'collapsed ancestor hides its descendant');
 assert.equal(h.groups[1].descendants.hidden,undefined,'an unrelated group is not rewritten');
 h.api.setCommentState(button,true,false);
 assert.equal(child.hidden,false);
 h.api.setRepliesState(root.querySelector('[data-replies-toggle]'),false,false);
 assert.equal(h.groups[0].descendants.hidden,true);
 assert.equal(h.counts.groupScans,3);
 h.api.setCommentState(root.querySelector('[data-comment-collapse]'),false,false);
 h.api.setRepliesState(root.querySelector('[data-replies-toggle]'),true,false);
 assert.equal(h.groups[0].descendants.hidden,true,'root collapse still takes precedence over expanded replies');
 h.api.syncThreadProjection(h.projection);
 assert.equal(h.counts.groupScans,105,'initialization and restoration can still synchronize every group');
});

test('search reveal expands the matching path with one group scan and no full projection scans', () => {
 const h=threadHarness();
 const root=h.groups[0].nodes[0], parent=h.groups[0].nodes[1], child=h.groups[0].nodes[2], sibling=h.groups[0].nodes[3];
 child.dataset.threadAncestorPath=`${root.dataset.threadNodeId} ${parent.dataset.threadNodeId}`;
 for(const node of [root,parent,child,sibling])node.classList.values.add('is-comment-collapsed');
 h.api.expandCommentSearchPath(child);
 for(const node of [root,parent,child])assert.equal(node.classList.contains('is-comment-collapsed'),false);
 assert.equal(sibling.classList.contains('is-comment-collapsed'),true);
 assert.equal(h.counts.groupScans,2,'one scan selects the path, one synchronizes the group');
 assert.equal(h.counts.projectionScans,0);
 assert.equal(h.groups[1].descendants.hidden,undefined);
});

test('reading anchors measure each preceding node once and stop at the chosen node', () => {
 for(const firstAt of [0,1000,1999]){
  const h=anchorHarness(2000,firstAt);
  const anchor=h.run();
  assert.equal(anchor.id,`n${firstAt}`);
  assert.equal(anchor.offset,-50);
  assert.equal(h.counts.rectReads,firstAt+1);
 }
 const hidden=anchorHarness(3,0);
 hidden.nodes[0].getBoundingClientRect=()=>{hidden.counts.rectReads++;return {width:0,height:0,top:0,bottom:100}};
 assert.equal(hidden.run().id,'n1','a hidden node cannot become the anchor');
 assert.equal(hidden.counts.rectReads,2);
 const pastEnd=anchorHarness(3,5);
 assert.equal(pastEnd.run().id,'n2','past the end retains the final visible-node fallback');
 assert.equal(pastEnd.counts.rectReads,3);
 const empty=anchorHarness(0);
 assert.equal(empty.run().kind,'scroll');
});

test('reading jump writes only when crossing the post/comment boundary', () => {
 const h=jumpHarness();
 assert.equal(h.control.getAttribute('href'),'#comments');
 for(let frame=0;frame<120;frame++)h.scroll();
 assert.equal(h.counts.writes,4);
 h.scroll(132);
 assert.equal(h.control.getAttribute('href'),'#post-top');
 assert.equal(h.control.getAttribute('aria-label'),'Jump to post');
 assert.equal(h.label.textContent,'Jump to post');
 assert.equal(h.icon.textContent,'↑');
 for(let frame=0;frame<120;frame++)h.scroll(0);
 assert.equal(h.counts.writes,8);
 h.scroll(133);
 assert.equal(h.control.getAttribute('href'),'#comments');
 assert.equal(h.counts.writes,12);
});

test('Settings scrolling reuses dirty state and all form-change paths refresh it', () => {
 const h=settingsHarness();
 for(let frame=0;frame<120;frame++)h.scroll();
 assert.equal(h.counts.serializations,1);
 assert.equal(h.bar.hidden,true);
 h.change('changed');
 assert.equal(h.counts.serializations,2);
 assert.equal(h.bar.hidden,false);
 for(let frame=0;frame<120;frame++)h.scroll();
 assert.equal(h.counts.serializations,2);
 h.change('default','change');
 assert.equal(h.bar.hidden,true,'reverting inputs clears dirty state');
 h.refresh('shortcut');
 assert.equal(h.bar.hidden,false,'programmatic shortcut capture is detected');
 h.refresh('default');
 assert.equal(h.bar.hidden,true,'programmatic shortcut reset is detected');
 h.restore('restored');
 assert.equal(h.bar.hidden,false,'BFCache-restored form values are detected');
 h.change('default','reset');
 assert.equal(h.bar.hidden,true,'native reset is checked after the frame boundary');
});

function offlineHarness(mediaTypes) {
 const counts={decodedBytes:0,blobBytes:0,created:0,revoked:0};
 const roots=new Map();const objectUrls=[];
 const node=(tag,text)=>({tag,text,children:[],append(...children){this.children.push(...children)},replaceChildren(){this.children=[]}});
 const context={current:{pack:{queue:[],items:[],archives:[{title:'Synthetic archive',body:'',media:mediaTypes.map(type=>({type,data:Buffer.alloc(1024).toString('base64')})),comments:[]}]}},objectUrls,node,$:id=>{if(!roots.has(id))roots.set(id,node(id));return roots.get(id)},unb64:data=>{const bytes=Buffer.from(data,'base64');counts.decodedBytes+=bytes.length;return bytes},URL:{createObjectURL(){counts.created++;return `blob:synthetic-${counts.created}`},revokeObjectURL(){counts.revoked++}},Blob:class{constructor(parts){counts.blobBytes+=parts.reduce((sum,part)=>sum+part.length,0)}}};
 vm.runInNewContext(offlineSource.slice(offlineSource.indexOf(' function render(){'),offlineSource.indexOf(' window.addEventListener("pagehide",lock);'))+'\nthis.render=render;render();',context);
 return {counts,roots,render:context.render,objectUrls};
}

test('offline images avoid unused blobs while audio/video URLs retain cleanup', () => {
 const images=offlineHarness(['image/png','image/jpeg','image/webp']);
 assert.equal(images.counts.decodedBytes,0);
 assert.equal(images.counts.blobBytes,0);
 assert.equal(images.counts.created,0);
 const media=images.roots.get('content').children[0].children.filter(node=>node.tag==='img');
 assert.equal(media.length,3);
 assert.ok(media.every(node=>node.src.startsWith('data:image/')&&node.loading==='lazy'));
 const mixed=offlineHarness(['image/png','video/mp4','audio/mpeg']);
 assert.equal(mixed.counts.created,2);
 assert.equal(mixed.counts.blobBytes,2048);
 mixed.render();
 assert.equal(mixed.counts.revoked,2,'rerender releases only the URLs actually created');
 assert.equal(mixed.objectUrls.length,2);
});

function checkpointHarness({explicit='',hidden=false}={}) {
 let handler;const counts={rectReads:0};const nodes=Array.from({length:2000},(_,index)=>({id:`n${index}`,getBoundingClientRect(){counts.rectReads++;return {top:(index-1000)*100,bottom:(index-1000)*100+100,width:hidden?0:600,height:hidden?0:100}}}));
 const fields={anchor:{value:''},resume_state:{value:''},revision:{value:1}};
 const resume={href:''};const form={dataset:{explicitAnchor:explicit},elements:fields,matches:selector=>selector==='[data-reading-command]',getAttribute:()=>'/reading/place',closest:()=>({querySelector:()=>resume})};
 const context={document:{addEventListener:(_type,callback)=>{handler=callback},querySelectorAll:()=>nodes,getElementById:id=>nodes.find(node=>node.id===id),querySelector:()=>null},effectiveTopInset:()=>50,window:{innerHeight:844,matchMedia:()=>({matches:false})},captureThreadPresentation:()=>({groupStates:[],commentStates:[]}),URLSearchParams,FormData:class{*[Symbol.iterator](){yield ['action','checkpoint']}},fetch:async()=>({ok:true,json:async()=>({revision:2,url:'/comments/synthetic'})}),showToast(){}};
 vm.runInNewContext(slice('    const readingAnchorChoice =','\tclass OrderedKeyQueue')+slice('    document.addEventListener("submit", (event) => {','\tconst expandCommentSearchPath ='),context);
 handler({target:form,submitter:{value:'checkpoint'},preventDefault(){}});
 return {counts,fields};
}

test('checkpoint capture measures comments once while preserving explicit and viewport anchors', () => {
 const viewport=checkpointHarness();
 assert.equal(viewport.fields.anchor.value,'n1000');
 assert.equal(JSON.parse(viewport.fields.resume_state.value).offset,0,'the existing zero-top offset behavior is preserved');
 assert.equal(viewport.counts.rectReads,2000);
 const explicit=checkpointHarness({explicit:'n1500'});
 assert.equal(explicit.fields.anchor.value,'n1500');
 assert.equal(JSON.parse(explicit.fields.resume_state.value).offset,0);
 assert.equal(explicit.counts.rectReads,2000);
 const hidden=checkpointHarness({explicit:'n1500',hidden:true});
 assert.equal(hidden.fields.anchor.value,'post-top','hidden comments are excluded even for explicit anchors');
 assert.equal(hidden.counts.rectReads,2000);
});


test('checkpoint presentation excludes continuation roots and preserves comment disclosure state', () => {
 const group=(id,expanded)=>({dataset:{threadGroupId:id},querySelector:()=>({getAttribute:()=>String(expanded)})});
 const comment={dataset:{threadNodeId:'t1_child'},querySelector:()=>({getAttribute:()=> 'false'})};
 const projection={querySelectorAll:s=>s==='[data-thread-group]'?[group('t1_root',false),group('more_placeholder',true)]:s==='.comment[data-thread-node-id]'?[comment]:[]};
 const context={threadProjection:()=>projection,appliedThreadPatches:new Map(),document:{body:{classList:{contains:()=>false}}},commentSearchState:{query:'',currentId:''}};
 vm.runInNewContext(slice('\tconst captureThreadPresentation =','\tconst captureFeedPresentation =')+'\nthis.capture=captureThreadPresentation;',context);
 const state=JSON.parse(JSON.stringify(context.capture()));
 assert.deepEqual(state.groupStates,[{id:'t1_root',expanded:false}]);
 assert.deepEqual(state.commentStates,[{id:'t1_child',expanded:false}]);
});
