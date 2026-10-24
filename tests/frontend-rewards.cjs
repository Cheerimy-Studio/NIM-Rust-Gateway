const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const {webcrypto} = require('node:crypto');

class Control {
  constructor(value = '') { this.value=value; this.disabled=false; this.style={}; this.handlers={}; this.textContent=''; }
  addEventListener(name,fn) { this.handlers[name]=fn; }
}
const decode = s => s.replace(/&quot;/g,'"').replace(/&#39;/g,"'").replace(/&amp;/g,'&');
class Row {
  constructor() { this.controls = new Map(); }
  set innerHTML(html) {
    for (const match of html.matchAll(/<(input|select|button)\b([^>]*)(?:>([\s\S]*?)<\/\1>|>)/g)) {
      const attrs=match[2]||match[4]||'';
      const cls=(attrs.match(/class="([^"]*)"/)||[])[1]||'';
      const name=cls.split(' ').find(c=>c.startsWith('wz-'));
      if (!name) continue;
      let value=(attrs.match(/value="([^"]*)"/)||[])[1]||'';
      if (match[1]==='select') {
        const options=[...(match[3]||'').matchAll(/<option value="([^"]*)"([^>]*)>/g)];
        value=(options.find(o=>o[2].includes('selected'))||options[0]||[])[1]||'';
      }
      this.controls.set('.'+name,new Control(decode(value)));
    }
  }
  querySelector(s) { if(!this.controls.has(s)) this.controls.set(s,new Control());return this.controls.get(s); }
}
const admin=fs.readFileSync('web/admin.js','utf8');
const rowContext=vm.createContext({document:{createElement:()=>new Row()},esc:s=>String(s??'').replace(/"/g,'&quot;'),wheelSum:()=>{}});
vm.runInContext(admin.slice(admin.indexOf('const PRIZE_TYPES'),admin.indexOf('\nfunction wheelSum()')),rowContext);
const row=vm.runInContext('prizeRowHtml({type:"grant",amount:1,model:"original",weight:100})',rowContext);
const input=row.querySelector('.wz-amount-model'),type=row.querySelector('.wz-type');
input.value='2.5';type.value='model_quota';type.handlers.change();
assert.equal(input.value,'original');assert.equal(row.querySelector('.wz-quota').disabled,false);
input.value='new-model';type.value='grant';type.handlers.change();assert.equal(input.value,'2.5');
type.value='model_unlimited';type.handlers.change();assert.equal(input.value,'new-model');assert.equal(row.querySelector('.wz-quota').disabled,true);
type.value='none';type.handlers.change();assert.equal(input.disabled,true);
const empty=vm.runInContext('prizeRowHtml()',rowContext);assert.equal(empty.querySelector('.wz-amount-model').placeholder,'金额');
console.log('Admin prize editing preserves typed amount/model across type changes: PASS');

const source=fs.readFileSync('web/user.js','utf8');
const controls=new Map();
const control=s=>{if(!controls.has(s))controls.set(s,new Control());return controls.get(s);};
const storage=new Map();let attempts=[],scenario='network',shown=null,actualWheel=null,signCalls=0,releaseSign;
const state={enabled:true,signed_today:false,today_amount:0,reset_in:3600};
let resolveDraw;
const api=async (path,opts={})=>{
  if(path==='promo')return {events:[]};
  if(path==='sign') {
    if(opts.method==='POST') {signCalls++;await new Promise(resolve=>{releaseSign=resolve});state.signed_today=true;state.today_amount=.000001;return {amount:.000001};}
    return {...state};
  }
  if(path==='me')return {id:'user',balance:1,grant:.000001,grant_total:.000001,recharge:1,recharge_total:1};
  if(path==='wheels/draw') {
    attempts.push({...opts.json});
    if(scenario==='network')throw new Error('connection lost');
    if(scenario==='business')throw Object.assign(new Error('price changed'),{status:400});
    if(scenario==='auth')throw Object.assign(new Error('login expired'),{status:401});
    await new Promise(resolve=>{resolveDraw=resolve});
    return {prize:{id:'new-prize',amount:.4},wheel:{id:'wheel',cost:1,prizes:[{id:'new-prize',percent:100}]}};
  }
  return {};
};
const context=vm.createContext({$:control,api,crypto:webcrypto,Uint8Array,Math,Number,Array,JSON,Promise,
  toast:()=>{},esc:String,sessionStorage:{getItem:k=>storage.get(k)||null,setItem:(k,v)=>storage.set(k,v),removeItem:k=>storage.delete(k)},
  setTimeout:(fn,ms)=>{if(ms===2850)queueMicrotask(fn);return 1},clearTimeout:()=>{},
  wheelModal:()=>{},backToWheelList:()=>{},openWheelDetail:w=>{actualWheel=w},showPrizeModal:d=>{shown=d},loadPrizes:async()=>{},loadWheels:async()=>{},
  run:async fn=>{try{return await fn()}catch{}},ME:{id:'user'},PROMO:null});
vm.runInContext(source.slice(source.indexOf('function rewardMoney'),source.indexOf('\nasync function loadKeys()')),context);
vm.runInContext('let drawing=false;'+source.slice(source.indexOf('// 未收到确定结果时'),source.indexOf('// 抽奖结果弹窗')),context);
(async()=>{
  const btn=control('#btn-sign');
  const first=btn.onclick();await Promise.resolve();await btn.onclick();assert.equal(signCalls,1);releaseSign();await first;
  assert.equal(control('#sign-hint').textContent,'今日已领 ¥0.000001');assert.equal(btn.disabled,true);
  state.enabled=false;await context.loadSign();assert.equal(control('#sign-box').style.display,'none');
  console.log('Concurrent sign clicks send once; micro rewards and disabled state render correctly: PASS');
  const wheel={id:'wheel',cost:1,server_time:1700000000,prizes:[{id:'old-prize',percent:100}]},drawBtn=control('#wd-draw');
  await context.doDraw(wheel,drawBtn);assert.equal(attempts.length,1);assert.equal(drawBtn.disabled,false);
  const firstId=attempts[0].request_id;assert.equal(attempts[0].requested_at,1700000000);assert.ok(storage.has('pendingDraw'));
  scenario='success';
  const draw=context.doDraw(wheel,drawBtn);await Promise.resolve();await context.doDraw(wheel,drawBtn);
  assert.equal(attempts.length,2);assert.equal(attempts[1].request_id,firstId);resolveDraw();await draw;
  assert.equal(actualWheel.prizes[0].id,'new-prize');assert.equal(shown.prize.amount,.4);assert.equal(storage.has('pendingDraw'),false);
  assert.match(control('#wd-disc').style.transform,/rotate/);assert.equal(drawBtn.disabled,false);assert.equal(control('#wheel-back').disabled,false);
  scenario='auth';await context.doDraw(wheel,drawBtn);assert.equal(storage.has('pendingDraw'),true);
  scenario='business';await context.doDraw(wheel,drawBtn);assert.equal(storage.has('pendingDraw'),false);
  console.log('Lost draw reply reuses ID; duplicate clicks guarded; server snapshot controls landing; controls recover: PASS');
})().catch(error=>{console.error(error);process.exitCode=1});
