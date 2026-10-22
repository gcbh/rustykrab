import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
import { webcrypto } from 'node:crypto';
const source = readFileSync(new URL('../crates/rustykrab-gateway/static/monitor.js', import.meta.url), 'utf8');
const taskFragment = source.slice(source.indexOf('  function taskOverview('), source.indexOf('  const badge ='));
const stateFragment = source.slice(source.indexOf('  const status ='), source.indexOf('  const at ='));
new vm.Script(source);
const fragment = source.slice(source.indexOf('  const discussionDrafts ='), source.indexOf('  async function refresh() {'));
class Element {
  constructor(tag, text) { this.tag = tag; this.textContent = text ?? ''; this.children = []; this.handlers = {}; this.dataset = {}; this.value = ''; this.attributes = {}; this.classList = {add(){}}; }
  append(...children) { this.children.push(...children); }
  prepend(child) { this.children.unshift(child); }
  setAttribute(name,value) { this.attributes[name] = value; }
  querySelectorAll() { return []; }
  addEventListener(name, handler) { this.handlers[name] = handler; }
  set innerHTML(_) { throw new Error('Discussion must render text as data'); }
}
const all = element => [element, ...element.children.flatMap(all)];
const question = (id='q1', item='i1') => ({ id, item, text:'Which direction?', status:'open', kind:'decision', options:[], asked_by:'worker:example', created_at:'2026-10-12T01:00:00Z' });
function client(data) {
  const calls = [], opened = [], body = new Element('body');
  let fail = false, failure = null;
  const questions = new Map((data.questions || []).map(q => [q.id,q]));
  const c = { generation:1, detailRequest:1, lastQuestions:'old', access:{connected:true}, snapshot:null,
    $: () => ({open:true}), node:(tag,text) => new Element(tag,text), num:String, human:String, at:String, crypto:webcrypto,
    refresh:async()=>{}, showDetail:async id=>opened.push(id),
    api:async (path, payload) => {
      calls.push({path,payload}); if (payload && fail) throw new Error('Connection interrupted');
      const match = /^\/api\/questions\/([^/]+)(\/answer)?$/.exec(path);
      if (match) {
        const id = decodeURIComponent(match[1]), q = questions.get(id);
        if (!payload) return structuredClone(q);
        if (failure === 'before') { failure = null; throw new Error('Connection interrupted'); }
        const receipt = {id:payload.submission_id,answer:payload.answer,actor:'user:owner',submitted_at:new Date().toISOString(),status:'pending'};
        const saved = {...q,answer_submission:receipt}; questions.set(id,saved);
        if (failure === 'after') { failure = null; throw new Error('Connection interrupted'); }
        return {question:structuredClone(saved),resumed:[],released:[],cancelled:[]};
      }
      return data;
    }
  };
  vm.createContext(c); vm.runInContext(stateFragment + taskFragment + fragment, c);
  return {c,body,calls,opened,questions,setFail:value=>{fail=value;},setFailure:value=>{failure=value;}};
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

test('a work approval stays in the central start control while its question remains in history',async()=>{
  const c=client({comments:[],activity:[],questions:[{...question(),kind:'approval'}],total_comments:0});c.body.dataset.startOptions='true';
  await c.c.renderDiscussion('i1',c.body,1,1);
  assert.equal(all(c.body).filter(n=>n.tag==='textarea').length,1);
  assert(all(c.body).some(n=>String(n.textContent).includes('Choose Start once')));
});

const receipt = (status,id='submission-1') => ({id,answer:'Use A',actor:'user:owner',submitted_at:'2026-10-21T20:00:00Z',status,error:status==='rejected'?'Work closed before the decision was applied':null});
const answerRows = body => all(body).filter(n=>n.dataset.state);

test('a sent decision remains visible as saved while the server applies it',async()=>{
  const c=client({comments:[],activity:[],questions:[question()],total_comments:0});
  await c.c.renderDiscussion('i1',c.body,1,1);
  const form=all(c.body).find(n=>n.tag==='form'), input=form.children.find(n=>n.tag==='textarea');
  input.value='Use A'; await form.handlers.submit({preventDefault(){}});
  const row=answerRows(c.body)[0];
  assert.equal(row.dataset.state,'pending'); assert.equal(form.hidden,true);
  assert(all(row).some(n=>n.textContent==='Use A'));
  assert(all(row).some(n=>String(n.textContent).includes('Saved. Waiting')));
  assert(all(row).some(n=>n.attributes.role==='status' && n.attributes['aria-live']==='polite'));
});

test('a lost acknowledgement reconciles with the saved receipt without resending',async()=>{
  const c=client({comments:[],activity:[],questions:[question()],total_comments:0});c.setFailure('after');
  await c.c.renderDiscussion('i1',c.body,1,1);
  await c.c.recordAnswer(question(),'Use A');
  assert.equal(answerRows(c.body)[0].dataset.state,'pending');
  assert.equal(c.calls.filter(x=>x.payload).length,1);
  assert(c.calls.some(x=>x.path==='/api/questions/q1' && !x.payload));
});

test('unconfirmed delivery retries the same words and identity',async()=>{
  const c=client({comments:[],activity:[],questions:[question()],total_comments:0});c.setFailure('before');
  await c.c.renderDiscussion('i1',c.body,1,1);
  await c.c.recordAnswer(question(),'Use A');
  assert.equal(answerRows(c.body)[0].dataset.state,'unconfirmed');
  await c.c.recordAnswer(question(),'Different words must not replace an uncertain send');
  const posts=c.calls.filter(x=>x.payload);
  assert.equal(posts.length,2);assert.equal(posts[0].payload.submission_id,posts[1].payload.submission_id);
  assert.equal(posts[1].payload.answer,'Use A');
  assert.equal(answerRows(c.body)[0].dataset.state,'pending');
});

test('the existing message updates to applied and follows the task into running',async()=>{
  const q={...question(),answer_submission:receipt('pending')};
  const c=client({comments:[],activity:[],questions:[q],total_comments:0});
  c.c.snapshot={work:{items:[{item:{id:'i1',status:'queued'}}]},priority_list:{entries:[]}};
  await c.c.renderDiscussion('i1',c.body,1,1);
  const row=answerRows(c.body)[0];
  c.questions.set('q1',{...q,status:'answered',answer:'Use A',answer_submission:receipt('applied')});
  await c.c.refreshAnswerMessages(1);
  assert.equal(answerRows(c.body)[0],row); assert.equal(row.dataset.state,'applied');
  c.c.snapshot.work.items[0].item.status='running';c.c.paintAnswers();
  assert(all(row).some(n=>String(n.textContent).includes('Work is running')));
});

test('reload restores a pending or rejected submission from the question record',async()=>{
  for(const state of ['pending','rejected']){
    const c=client({comments:[],activity:[],questions:[{...question(),answer_submission:receipt(state)}],total_comments:0});
    await c.c.renderDiscussion('i1',c.body,1,1);
    const row=answerRows(c.body)[0];assert.equal(row.dataset.state,state);
    if(state==='rejected') assert(all(row).some(n=>String(n.textContent).includes('Work closed')));
  }
});

test('an older open-question response cannot undo an applied receipt',()=>{
  const c=client({});
  c.c.observeAnswerQuestion({...question(),status:'answered',answer_submission:receipt('applied')});
  const observed=c.c.observeAnswerQuestion({...question(),answer_submission:receipt('pending')});
  assert.equal(observed.status,'answered'); assert.equal(observed.answer_submission.status,'applied');
});

test('late answer acknowledgements after disconnect cannot restore another session state',async()=>{
  const c=client({comments:[],activity:[],questions:[question()],total_comments:0});
  let finish;c.c.api=async()=>await new Promise(resolve=>{finish=resolve;});
  const sent=c.c.recordAnswer(question(),'Use A');c.c.generation++;
  finish({question:{...question(),answer_submission:receipt('pending')}});await sent;
  assert.equal(c.c.answerMessageView(question(),null,null).state,'none');
});

test('an acknowledged duplicate adopts the server receipt identity for subsequent updates',async()=>{
  const c=client({comments:[],activity:[],questions:[question()],total_comments:0});
  await c.c.renderDiscussion('i1',c.body,1,1);
  const saved={...question(),answer_submission:receipt('pending','original-server-receipt')};
  c.c.api=async(path,payload)=>payload?{question:structuredClone(saved)}:{...saved,status:'answered',answer:'Use A',answer_submission:receipt('applied','original-server-receipt')};
  await c.c.recordAnswer(question(),'Use A');await c.c.refreshAnswerMessages(1);
  assert.equal(answerRows(c.body)[0].dataset.state,'applied');
});

test('reload retains a rejected answer on a consolidated question without a second decision editor',async()=>{
  const old={...question('old'),status:'obsolete',superseded_by:'q1',answer_submission:receipt('rejected')};
  const c=client({comments:[],activity:[],questions:[old,question()],total_comments:0});
  await c.c.renderDiscussion('i1',c.body,1,1);
  assert.equal(answerRows(c.body).length,1);
  assert.equal(answerRows(c.body)[0].dataset.state,'rejected');
  assert(all(answerRows(c.body)[0]).some(n=>String(n.textContent).includes('Work closed')));
  assert.equal(all(c.body).filter(n=>n.tag==='textarea').length,2);
});

test('an acknowledged canonical answer keeps updating on the existing message',async()=>{
  const data={comments:[],activity:[],questions:[question('old')],total_comments:0};
  const c=client(data);await c.c.renderDiscussion('i1',c.body,1,1);
  let canonical;
  c.c.api=async(path,payload)=>{
    if(payload){canonical={...question(),answer_submission:receipt('pending',payload.submission_id)};return{question:canonical};}
    assert.equal(path,'/api/questions/q1');
    return{...canonical,status:'answered',answer:'Use A',answer_submission:{...canonical.answer_submission,status:'applied'}};
  };
  await c.c.recordAnswer(question('old'),'Use A');const row=answerRows(c.body)[0];
  await c.c.refreshAnswerMessages(1);assert.equal(row.dataset.state,'applied');assert.equal(answerRows(c.body)[0],row);
  data.questions=[{...question('old'),status:'obsolete',superseded_by:'q1'},{...canonical,status:'answered',answer:'Use A',answer_submission:{...canonical.answer_submission,status:'applied'}}];
  c.c.api=async()=>data;
  const refreshed=new Element('body');await c.c.renderDiscussion('i1',refreshed,1,1);
  assert.equal(answerRows(refreshed).length,1,'a refreshed canonical answer must not duplicate the old message');
});

test('a lost canonical acknowledgement follows consolidation while original accepted receipts stay put',async()=>{
  for(const originalAccepted of [false,true]){
    const c=client({comments:[],activity:[],questions:[question('old')],total_comments:0});
    await c.c.renderDiscussion('i1',c.body,1,1);let id;const reads=[];
    c.c.api=async(path,payload)=>{
      if(payload){id=payload.submission_id;throw new Error('Acknowledgement lost');}
      reads.push(path);
      if(path==='/api/questions/old')return{...question('old'),status:'obsolete',superseded_by:'q1',...(originalAccepted?{answer_submission:receipt('rejected',id)}:{})};
      return{...question(),answer_submission:receipt('pending',id)};
    };
    await c.c.recordAnswer(question('old'),'Use A');
    assert.equal(answerRows(c.body)[0].dataset.state,originalAccepted?'rejected':'pending');
    assert.deepEqual(reads,originalAccepted?['/api/questions/old']:['/api/questions/old','/api/questions/q1']);
  }
});
