const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync('web/user.js','utf8');
async function scenario(status) {
  const nodes = new Map();
  const make = () => ({style:{},value:'',textContent:'',innerHTML:'',disabled:false,children:[],
    classList:{add(){},toggle(){},contains:()=>true},addEventListener(){},setAttribute(){},appendChild(c){this.children.push(c)},append(...c){this.children.push(...c)},remove(){}});
  const get = id => {if(!nodes.has(id)) nodes.set(id,make()); return nodes.get(id)};
  const document = {querySelector:get,getElementById:id=>get('#'+id),querySelectorAll:()=>[],createElement:make,body:make()};
  const calls = [];
  const ctx = vm.createContext({document,window:{addEventListener(){},NGW_UI:{}},location:{hash:'',origin:'http://localhost',search:''},history:{replaceState(){}},
    URLSearchParams,localStorage:{getItem:()=>null,setItem(){}},setInterval:()=>0,setTimeout:()=>0,clearTimeout(){},navigator:{},console,
    fetch:async url=> {
      calls.push(url);
      const fail = url.endsWith('/me') && status !== 200;
      const data = url.endsWith('/me') ? {id:'u1',username:'test',balance:1.000001} : url.endsWith('/stats') ? {total_calls:0,total_cost:.000001,recent:[]} : {rows:[],enabled:false};
      return {ok:!fail,status:fail?status:200,json:async()=>fail?{error:{message:'session unavailable'}}:data};
    }});
  vm.runInContext(source,ctx);
  for (let i=0;i<20;i++) await new Promise(r=>setImmediate(r));
  assert.equal(typeof get('#reg-go').onclick,'function','registration must be bound during bootstrap');
  assert.ok(calls.some(url=>url.endsWith('/me')),'bootstrap must check session');
  if(status===200) {
    assert.equal(get('#panel-view').style.display,'block');
    assert.equal(get('#ov-balance').textContent,'1.000001');
    assert.equal(get('#ov-cost').textContent,'¥0.000001');
    assert.equal(get('#api-base').value,'http://localhost/v1');
    assert.ok(calls.some(url=>url.endsWith('/keys')));
  } else {
    assert.equal(get('#login-view').style.display,'flex');
    assert.equal(get('#panel-view').style.display,'none');
    if(status!==401) assert.equal(get('#login-err').textContent,'session unavailable');
  }
  console.log('PASS 用户中心启动，session HTTP ' + status);
}
(async()=>{for(const status of [200,401,503]) await scenario(status)})().catch(e=>{console.error(e);process.exitCode=1});
