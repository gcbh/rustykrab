import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
const source=readFileSync(new URL('../crates/rustykrab-gateway/static/monitor.js',import.meta.url),'utf8');
new vm.Script(source);
const fragment=source.slice(source.indexOf('  const progressStates ='),source.indexOf('  const discussionDrafts ='));
class Element {
  constructor(tag,text=''){this.tag=tag;this.textContent=text;this.children=[];this.dataset={};this.handlers={};this.hidden=false;this.value='';}
  append(...children){this.children.push(...children);}
  replaceChildren(...children){this.children=children;}
  addEventListener(name,handler){this.handlers[name]=handler;}
  setAttribute(name,value){this[name]=value;}
  set innerHTML(_){throw Error('Untrusted work must render as text');}
}
const all=e=>[e,...e.children.flatMap(all)];
const item=(id,kind='code',state='running')=>({id,title:'Task '+id,kind,status:{status:state},objective:'Read my mail',done_when:'Summary saved',constraints:[],writable_resources:['mailbox'],required_tools:['email'],required_mcp_servers:[],budget:{tokens:200,wall_seconds:60},updated_at:'2026-10-12T01:00:00Z'});
function client(data={preferences:[]}) {
  const ids=new Map(),panes=['priority','attention','progress','results','automatic','systems'].map(p=>Object.assign(new Element('section'),{dataset:{pane:p}})),buttons=panes.map(p=>Object.assign(new Element('button'),{dataset:{view:p.dataset.pane}}));
  const calls=[],opened=[];let fail=false;
  const c={generation:1,detailRequest:1,access:{connected:true},
    window:{location:{hash:'',pathname:'/monitor.html',search:'?work=abc'},history:{replaceState:(_a,_b,url)=>{calls.push({url});}}},
    document:{querySelectorAll:q=>q==='[data-pane]'?panes:buttons},
    $:id=>{if(!ids.has(id))ids.set(id,new Element('div'));return ids.get(id);},
    node:(tag,text)=>new Element(tag,text),status:i=>i.status.status,reason:i=>i.status.reason,
    closed:new Set(['done','failed','cancelled','expired']),num:String,age:String,at:String,
    itemButton:(id,title)=>Object.assign(new Element('button',title),{item:id}),badge:s=>new Element('span',s),
    refresh:async()=>{},showDetail:async id=>opened.push(id),
    api:async(path,payload)=>{calls.push({path,payload});if(fail)throw Error('Connection interrupted');return data;}
  };
  vm.createContext(c);vm.runInContext(fragment,c);
  return {c,ids,panes,buttons,calls,opened,setFail:v=>{fail=v;}};
}
test('separate views default to the priority list and preserve the work link on navigation',()=>{
  const t=client();t.c.selectView(undefined,false);assert.equal(t.panes.find(p=>!p.hidden).dataset.pane,'priority');
  t.c.selectView('progress');assert.deepEqual(t.panes.filter(p=>!p.hidden).map(p=>p.dataset.pane),['progress']);assert.equal(t.calls.at(-1).url,'/monitor.html?work=abc#progress');
  t.c.selectView('unknown',false);assert.equal(t.panes.find(p=>!p.hidden).dataset.pane,'priority');
});
test('results exclude accepted proposals and do not imply those were executed',()=>{
  const t=client();t.c.renderPriorityViews({work:{items:[{item:item('proposal','proposal','done')},{item:item('execution','code','done')},{item:item('active')}],items_truncated:false}});
  const nodes=all(t.ids.get('resultRows'));assert(nodes.some(n=>n.item==='execution'));assert(!nodes.some(n=>n.item==='proposal'||n.item==='active'));
});
test('start defaults to once and posts no author or model-controlled scope',async()=>{
  const t=client(),body=new Element('body');t.c.renderStartOptions({item:item('a')},body,1,1);
  const form=all(body).find(n=>n.tag==='form');await form.handlers.submit({preventDefault(){}});
  const post=t.calls.find(c=>c.payload);assert.equal(post.path,'/api/work/a/start');assert.equal(post.payload.mode,'once');assert.deepEqual(Object.keys(post.payload),['mode','daily_limit','note']);
});
test('automatic start requires selecting the option and records the chosen note and limit',async()=>{
  const t=client(),body=new Element('body');t.c.renderStartOptions({item:item('a')},body,1,1);
  const checkbox=all(body).find(n=>n.type==='checkbox');checkbox.checked=true;checkbox.handlers.change();
  const note=all(body).find(n=>n.tag==='textarea');note.value='Daily summary';note.handlers.input();
  const limit=all(body).find(n=>n.type==='number');limit.value='2';limit.handlers.input();
  await all(body).find(n=>n.tag==='form').handlers.submit({preventDefault(){}});
  const p=t.calls.find(c=>c.payload).payload;assert.equal(p.mode,'automatic');assert.equal(p.daily_limit,2);assert.equal(p.note,'Daily summary');
});
test('failed start preserves the choice and draft for retry',async()=>{
  const t=client(),body=new Element('body');t.c.renderStartOptions({item:item('a')},body,1,1);
  const note=all(body).find(n=>n.tag==='textarea');note.value='Keep this';note.handlers.input();t.setFail(true);
  await all(body).find(n=>n.tag==='form').handlers.submit({preventDefault(){}});
  assert(all(body).some(n=>n.textContent==='Connection interrupted'));
  const retry=new Element('body');t.c.renderStartOptions({item:item('a')},retry,1,1);assert.equal(all(retry).find(n=>n.tag==='textarea').value,'Keep this');assert.equal(all(body).find(n=>n.tag==='button').disabled,false);
});
test('revocation uses the explicit policy id and preserves the dataset history',async()=>{
  const t=client({preferences:[{id:'pref',scope:'work_template',checks:[{check:'work_template',mode:'automatic',daily_limit:3,template:{example_item:'a',title:'Mail'}}],granted_by:'user:master',granted_at:'now'}]});
  await t.c.refreshPreferences(1);const revoke=all(t.ids.get('preferenceRows')).find(n=>n.textContent==='Revoke automatic permission');
  await revoke.handlers.click();assert(t.calls.some(c=>c.path==='/api/judgment/pref/revoke'));
});
test('stale preference replies cannot change a new session',async()=>{
  const t=client({preferences:[]});t.c.generation=2;await t.c.refreshPreferences(1);assert.equal(t.ids.size,0);
});

test('In progress displays execution states while excluding blocks, failures and completed work',()=>{
  const t=client();t.c.human=String;
  const rows=['queued','ready','leased','running','verifying','blocked','failed','done'].map(state=>({item:item(state,'code',state),spend:{runs:0}}));
  t.c.snapshot={work:{items:rows,items_truncated:false},priority_list:{entries:['ready','leased','queued','running','verifying'].map((item,i)=>({item,position:i+1}))}};
  t.c.$('search').value='';t.c.$('status').value='progress';
  const renderer=source.slice(source.indexOf('  function renderWork() {'),source.indexOf('  const progressStates ='));
  vm.runInContext(renderer,t.c);t.c.renderWork();
  assert.deepEqual(all(t.c.$('workRows')).filter(n=>n.item).map(n=>n.item),['ready','leased','queued','running','verifying']);
  t.c.$('status').value='running';t.c.renderWork();assert.deepEqual(all(t.c.$('workRows')).filter(n=>n.item).map(n=>n.item),['running']);
});
