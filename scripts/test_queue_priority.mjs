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
  const context = {snapshot:{work:{items:[row('independent','ready',999),row('shared repair','ready',0),row('active','running',0),row('finished','done',0)]},queue_priorities:withRanks ? {'shared repair':{waiters:3,priority:0},independent:{waiters:0,priority:999}} : undefined},$,num:String,status:i=>i.status.status,reason:()=>null,human:String,node:(_tag,text)=>new Element(text),badge:label=>new Element(label),itemButton:(_id,title)=>new Element(title)};
  vm.createContext(context); vm.runInContext(fragment, context); context.renderWork();
  return nodes;
}
test('monitor puts shared blockers ahead of explicit urgency while preserving active work and showing impact', () => {
  const nodes=render(true), rows=nodes.get('workRows').children;
  assert.deepEqual(rows.map(r=>r.children[0].children[0].textContent), ['active','shared repair','independent']);
  assert(rows[1].children[0].children.some(c=>c.textContent==='Needed by 3 open tasks'));
  assert(rows[1].children[1].children.some(c=>c.textContent==='Queued'));
});
test('monitor tolerates an older daemon without derived queue ranks', () => {
  const rows=render(false).get('workRows').children;
  assert.deepEqual(rows.map(r=>r.children[0].children[0].textContent), ['active','independent','shared repair']);
});
