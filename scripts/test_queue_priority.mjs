import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
const source = readFileSync(new URL('../crates/rustykrab-gateway/static/monitor.js', import.meta.url), 'utf8');
const fragment = source.slice(source.indexOf('  function renderWork()'), source.indexOf('  const views ='));
class Element {
  constructor(text = '') { this.textContent = text; this.children = []; this.value = ''; }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
}
function render(withRanks) {
  const nodes = new Map();
  const $ = id => {if (!nodes.has(id)) nodes.set(id, new Element()); return nodes.get(id);};
  $('status').value = 'progress';
  const row = (id, state, priority) => ({item:{id,title:id,status:{status:state},priority,created_at:'2026-10-12T00:00:00Z'},spend:{runs:0},evidence_count:0,verified_evidence_count:0});
  const context = {snapshot:{work:{items:[row('independent','ready',999),row('shared repair','ready',0),row('active','running',0),row('finished','done',0)]},priority_list:withRanks ? {entries:[{item:'shared repair',position:1},{item:'independent',position:2},{item:'active',position:3}]} : undefined,queue_priorities:withRanks ? {'shared repair':{waiters:3,priority:0},independent:{waiters:0,priority:999}} : undefined},$,num:String,status:i=>i.status.status,reason:()=>null,human:String,node:(_tag,text)=>new Element(text),badge:label=>new Element(label),itemButton:(_id,title)=>new Element(title)};
  vm.createContext(context); vm.runInContext(fragment, context); context.renderWork();
  return nodes;
}
test('monitor uses the exact server order rather than recomputing priority or pinning active work', () => {
  const nodes=render(true), rows=nodes.get('workRows').children;
  assert.deepEqual(rows.map(r=>r.children[0].children[0].textContent), ['shared repair','independent','active']);
  assert(rows[0].children[0].children.some(c=>c.textContent==='Needed by 3 open tasks'));
  assert(rows[0].children[1].children.some(c=>c.textContent==='Queued'));
});
test('monitor tolerates an older daemon without derived queue ranks', () => {
  const rows=render(false).get('workRows').children;
  assert.deepEqual(rows.map(r=>r.children[0].children[0].textContent), ['active','independent','shared repair']);
});

const priorityFragment = source.slice(source.indexOf('  function priorityTitle('),source.indexOf('  function renderWork()'));
class PriorityElement extends Element {
  setAttribute(name,value){this[name]=value;}
  addEventListener(name,callback){this[name]=callback;}
}
function priorityContext(api){
  const nodes=new Map(), $=id=>{if(!nodes.has(id))nodes.set(id,new PriorityElement());return nodes.get(id);};
  const entry=(item,position)=>({item,title:item,position,status:{status:'blocked',reason:'needs_consent'},wait_reason:'Needs your permission',waiting_on:[],waiters:0});
  const list={revision:'a'.repeat(64),entries:[entry('held first',1),entry('ready second',2)]};
  const context={snapshot:{priority_list:list},priorityMoving:false,priorityError:'',priorityVersion:0,generation:0,api,$,renderWork(){},num:String,human:String,node:(_tag,text)=>new PriorityElement(text),itemButton:(_id,title)=>new PriorityElement(title)};
  vm.createContext(context);vm.runInContext(priorityFragment,context);return{context,nodes};
}
test('priority list retains a blocked first entry and sends a reviewed revision when moving it',async()=>{
  const {context,nodes}=priorityContext(async(path,body)=>{
    assert.equal(path,'/api/work/priority');assert.equal(body.revision,'a'.repeat(64));
    assert.deepEqual(Array.from(body.ordered),['ready second','held first']);
    return{revision:'b'.repeat(64),entries:context.snapshot.priority_list.entries.toReversed().map((e,i)=>({...e,position:i+1}))};
  });
  context.renderPriorityList();
  assert.equal(nodes.get('priorityRows').children[0].children[1].children[0].textContent,'held first');
  await context.movePriority(0,1);
  assert.equal(nodes.get('priorityRows').children[0].children[1].children[0].textContent,'ready second');
  assert.equal(context.snapshot.priority_list.entries[1].status.reason,'needs_consent');
});
test('a refused move preserves order and exposes its explanation',async()=>{
  const {context,nodes}=priorityContext(async()=>{throw new Error('The priority list changed. Refresh it before moving work.');});
  await context.movePriority(0,1);
  assert.equal(context.snapshot.priority_list.entries[0].item,'held first');
  assert.match(nodes.get('priorityError').textContent,/Refresh/);assert.equal(nodes.get('priorityError').hidden,false);
});

test('a monitor read started before a move cannot restore an older order',async()=>{
  let complete;
  const response=new Promise(resolve=>{complete=resolve;});
  const {context}=priorityContext(()=>response);
  context.pending=false;context.access={connected:true};context.currentView='priority';
  context.render=()=>{};context.refreshQuestions=async()=>{};
  const refreshFragment=source.slice(source.indexOf('  async function refresh() {'),source.indexOf('  async function showDetail('));
  vm.runInContext(refreshFragment,context);
  const reading=context.refresh();
  context.priorityVersion++;
  const moved={revision:'moved',entries:context.snapshot.priority_list.entries.toReversed()};
  context.snapshot.priority_list=moved;
  complete({priority_list:{revision:'old',entries:[]}});
  await reading;
  assert.equal(context.snapshot.priority_list,moved);
});
