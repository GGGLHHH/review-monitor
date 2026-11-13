// spike: 验证「页面外直接请求」经营宝评价接口是否可行（mtgsig 问题）。
// 前置：调试版程序在跑（RM_CDP_PORT=9333），页面已登录。
// 步骤：CDP 取 cookie → 触发一次页面内 whoami 并抓取带签名的真实 URL →
//       页面外用 Node fetch 分别发：无签名 / 复用的签名 / 列表 POST 无签名。
const CDP = process.env.CDP || 'http://127.0.0.1:9333';
const targets = await (await fetch(`${CDP}/json/list`)).json();
const t = targets.find(x => x.type === 'page' && x.url.includes('ecom.meituan.com'));
if (!t) { console.error('找不到页面'); process.exit(2); }
const ws = new WebSocket(t.webSocketDebuggerUrl);
await new Promise((ok, bad) => { ws.onopen = ok; ws.onerror = bad; });
let id = 0; const pending = new Map(); const net = [];
ws.onmessage = e => {
  const m = JSON.parse(e.data);
  if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
  else if (m.method === 'Network.requestWillBeSent' && m.params.request.url.includes('emis/gw')) {
    net.push(m.params.request.url);
  }
};
const send = (method, params = {}) => new Promise(ok => { const i = ++id; pending.set(i, ok); ws.send(JSON.stringify({ id: i, method, params })); });
const evalJs = async (expr) => {
  const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
  if (r.result?.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails.exception?.description || r.result.exceptionDetails));
  return r.result?.result?.value;
};

await send('Network.enable');
const cookies = (await send('Network.getCookies', { urls: ['https://ecom.meituan.com/'] })).result.cookies;
console.log(`cookie 共 ${cookies.length} 个:`, cookies.map(c => c.name).join(', '));
const cookieHeader = cookies.map(c => `${c.name}=${c.value}`).join('; ');
const hasMtg = cookies.some(c => /mtg/i.test(c.name));
console.log('有没有 mtg* cookie:', hasMtg ? cookies.filter(c => /mtg/i.test(c.name)).map(c => c.name + '=' + c.value.slice(0, 20)) : '无');

// 1) 触发页面内 whoami，抓带签名的真实 URL
net.length = 0;
await evalJs('window.__rm.whoami("spike1")');
await new Promise(r => setTimeout(r, 3000));
const signedWhoami = net.find(u => u.includes('getUserInfo')) || null;
console.log('\n[页面内 whoami 实际 URL]\n', signedWhoami);

// 2) 页面外直接请求
const base = 'https://ecom.meituan.com';
const H = { 'Cookie': cookieHeader, 'User-Agent': 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36', 'Referer': 'https://ecom.meituan.com/emis/evaluation/poi' };
async function probe(label, url, init = {}) {
  try {
    const r = await fetch(url, { headers: H, ...init });
    const text = await r.text();
    console.log(`\n=== ${label} ===\nHTTP ${r.status}\n${text.slice(0, 400)}`);
  } catch (e) { console.log(`\n=== ${label} ===\n网络错误: ${e.message}`); }
}

const whoamiPlain = base + '/emis/gw/account/biz/getUserInfo?_tm=' + Date.now();
await probe('whoami 无签名', whoamiPlain);
if (signedWhoami) await probe('whoami 复用页面签名(同URL)', signedWhoami);

const listBody = JSON.stringify({ pageNo: 1, pageSize: 20, poiId: process.env.POI_ID || '10000001', startTime: Date.now() - 86400000, endTime: Date.now(), replyType: '全部', starType: '全部', sourceType: '全部', userType: '全部', timeType: '全部' });
await probe('MT 列表 POST 无签名', base + '/emis/gw/rpc/TFeedbackListNewService/queryMTFeedbackPCNew?_tm=' + Date.now(),
  { method: 'POST', headers: { ...H, 'Content-Type': 'application/json' }, body: listBody });
await probe('DP 列表 POST 无签名', base + '/emis/gw/rpc/TFeedbackListNewService/queryDPFeedbackPCNew?_tm=' + Date.now(),
  { method: 'POST', headers: { ...H, 'Content-Type': 'application/json' }, body: listBody });
// 复用列表接口的页面签名：先触发一次页面内列表请求抓 URL
net.length = 0;
await evalJs(`window.__rm.fetchList("spike2","meituan",${JSON.stringify(JSON.parse(listBody))})`);
await new Promise(r => setTimeout(r, 3000));
const signedList = net.find(u => u.includes('queryMTFeedbackPCNew')) || null;
console.log('\n[页面内 MT 列表实际 URL]\n', signedList);
if (signedList) await probe('MT 列表 复用页面签名(同URL)', signedList);
ws.close();
