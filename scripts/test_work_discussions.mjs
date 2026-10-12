import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
const source = readFileSync(new URL('../crates/rustykrab-gateway/static/monitor.js', import.meta.url), 'utf8');
new vm.Script(source);
const fragment = source.slice(source.indexOf('  const discussionDrafts ='), source.indexOf('  async function refresh() {'));
class Element {
  constructor(tag, text) { this.tag = tag; this.textContent = text ?? ''; this.children = []; this.handlers = {}; this.dataset = {}; this.value = ''; }
  append(...children) { this.children.push(...children); }
  prepend(child) { this.children.unshift(child); }
  setAttribute() {}
  addEventListener(name, handler) { this.handlers[name] = handler; }
  set innerHTML(_) { throw new Error('Discussion must render text as data'); }
}
const all = element => [element, ...element.children.flatMap(all)];
const question = (id='q1', item='i1') => ({ id, item, text:'Which direction?', status:'open', kind:'decision', options:[], asked_by:'worker:example', created_at:'2026-10-12T01:00:00Z' });
function client(data) {
  const calls = [], opened = [], body = new Element('body');
  let fail = false;
  const c = { generation:1, detailRequest:1, lastQuestions:'old', access:{connected:true}, snapshot:null,
    $: () => ({open:true}), node:(tag,text) => new Element(tag,text), num:String, human:String, at:String,
    refresh:async()=>{}, showDetail:async id=>opened.push(id),
    api:async (path, payload) => { calls.push({path,payload}); if (payload && fail) throw new Error('Connection interrupted'); return data; }
  };
  vm.createContext(c); vm.runInContext(fragment, c);
  return {c,body,calls,opened,setFail:value=>{fail=value;}};
}
test('attention groups decisions by owning work, retaining distinct decisions within an item', () => {
  const {c}=client({}); const grouped=c.questionsByWork([question('a'),question('b'),question('c','i2')]);
  assert.equal(grouped.size,2); assert.equal(grouped.get('i1').length,2);
});
test('consolidated requests retain a history notice and expose only the canonical decision editor', async () => {
  const c=client({comments:[],activity:[],questions:[question(),{...question('old'),status:'obsolete',superseded_by:'q1'}],total_comments:0});
  await c.c.renderDiscussion('i1',c.body,1,1); const nodes=all(c.body);
  assert.equal(nodes.filter(n=>n.tag==='textarea').length,2);
  assert(nodes.some(n=>String(n.textContent).includes('repeated requests consolidated')));
});
test('posting a comment carries only text and the owning item, never a decision or spoofed actor', async () => {
  const c=client({comments:[],activity:[],questions:[],total_comments:0}); await c.c.renderDiscussion('i1',c.body,1,1);
  const form=all(c.body).find(n=>n.tag==='form'); const input=form.children.find(n=>n.tag==='textarea'); input.value='<img src=x> user direction'; await form.handlers.submit({preventDefault(){}});
  const post=c.calls.find(x=>x.payload); assert.equal(post.path,'/api/work/i1/discussion'); assert.equal(post.payload.text,input.value); assert.deepEqual(Object.keys(post.payload),['text']); assert.deepEqual(c.opened,['i1']);
});
test('failed comment submission preserves the draft and enables retry after refresh', async () => {
  const c=client({comments:[],activity:[],questions:[],total_comments:0}); await c.c.renderDiscussion('i1',c.body,1,1);
  const form=all(c.body).find(n=>n.tag==='form'), input=form.children.find(n=>n.tag==='textarea'), button=form.children.find(n=>n.tag==='button'); input.value='Keep my draft'; input.handlers.input(); c.setFail(true); await form.handlers.submit({preventDefault(){}});
  assert.equal(input.value,'Keep my draft'); assert.equal(button.disabled,false); assert(all(form).some(n=>n.textContent==='Connection interrupted'));
  const refreshed=new Element('body'); await c.c.renderDiscussion('i1',refreshed,1,1); assert.equal(all(refreshed).find(n=>n.tag==='textarea').value,'Keep my draft');
});
test('recording a decision uses its canonical question endpoint and preserves a separate comment draft', async () => {
  const c=client({comments:[],activity:[],questions:[question()],total_comments:0}); await c.c.renderDiscussion('i1',c.body,1,1);
  const forms=all(c.body).filter(n=>n.tag==='form'); const comment=forms[1].children.find(n=>n.tag==='textarea'); comment.value='Direction still being edited'; comment.handlers.input(); const answer=forms[0].children.find(n=>n.tag==='textarea'); answer.value='Use A'; await forms[0].handlers.submit({preventDefault(){}});
  assert.equal(c.calls.find(x=>x.payload).path,'/api/questions/q1/answer'); const refreshed=new Element('body'); await c.c.renderDiscussion('i1',refreshed,1,1); assert.equal(all(refreshed).filter(n=>n.tag==='textarea')[1].value,'Direction still being edited');
});
test('a late discussion response cannot append to a different item dialog', async () => {
  const c=client({comments:[],activity:[],questions:[],total_comments:0}); c.c.detailRequest=2; await c.c.renderDiscussion('i1',c.body,1,1); assert.equal(c.body.children.length,0);
});
