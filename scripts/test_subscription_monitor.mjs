import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
const source=readFileSync(new URL('../crates/rustykrab-gateway/static/monitor.js',import.meta.url),'utf8');
new vm.Script(source);
const fragment=source.slice(source.indexOf('  function renderSubscription('),source.indexOf('  function render(reply)'));
class Element {
  constructor(tag,text){this.tag=tag;this.textContent=text;this.children=[];}
  append(...children){this.children.push(...children);}
  set innerHTML(_){throw Error('Subscription observations must render as text');}
}
function render(kind,runtime) {
  const card=new Element('article');
  const c={node:(tag,text)=>new Element(tag,text),num:String,age:String};
  vm.createContext(c);vm.runInContext(fragment,c);c.renderSubscription(card,kind,runtime);
  return card.children.map(n=>n.textContent).join('\n');
}
test('Claude exhaustion shows measured session and weekly usage with reset and freshness',()=>{
  const text=render('claude_code',{subscription:'max',quota_checked_at:'2026-10-13T18:00:00Z',rate_limited_until:'2026-10-14T23:00:00Z',quota_windows:{five_hour:{label:'Current session',used_percent:15},seven_day:{label:'Current week (all models)',used_percent:100,exhausted:true,resets_at:1792018800}}});
  assert(text.includes('Current session: 15% used'));
  assert(text.includes('Current week (all models): 100% used · Exhausted · Resets'));
  assert(text.includes('Quota checked'));
  assert(text.includes('Next retry'));
});
test('unknown usage and failed reads never imply zero used',()=>{
  const text=render('claude_code',{subscription:'max',quota_error:'Usage observation unavailable',quota_windows:{seven_day:{used_percent:null,exhausted:true}}});
  assert(text.includes('usage unknown'));
  assert(text.includes('Subscription usage has not been observed.'));
  assert(text.includes('Usage observation unavailable'));
  assert(!text.includes('0% used'));
});
test('Codex windows retain their measured usage and missing windows stay absent',()=>{
  const text=render('codex',{subscription:'pro',rate_limits:{codex:{primary:{used_percent:7,window_minutes:10080,resets_at:1792018800},secondary:null}}});
  assert(text.includes('10080 min: 7% used'));
  assert(!text.includes('secondary')&&!text.includes('0% used'));
});
test('native labels remain literal text',()=>{
  const text=render('claude_code',{quota_windows:{a:{label:'<img src=x onerror=run()>',used_percent:5}}});
  assert(text.includes('<img src=x onerror=run()>'));
});
