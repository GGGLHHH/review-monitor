// 极简 CDP 客户端（探索用）：连到调试 Chrome 的某个页面，执行 JS / 截图。
// 用法：
//   node tools/cdp.mjs eval  "<js 表达式，可 await>"   [--match ecom.meituan.com]
//   node tools/cdp.mjs shot  <out.png>                  [--match ...]
//   node tools/cdp.mjs net   <毫秒> [过滤子串]           [--match ...]   监听网络请求
// 环境变量 CDP=http://127.0.0.1:9333
const CDP = process.env.CDP || 'http://127.0.0.1:9333';
const args = process.argv.slice(2);
const mi = args.indexOf('--match');
const match = mi >= 0 ? args.splice(mi, 2)[1] : 'ecom.meituan.com';
const [cmd, ...rest] = args;

const targets = await (await fetch(`${CDP}/json/list`)).json();
const t = targets.find(x => x.type === 'page' && x.url.includes(match));
if (!t) { console.error('找不到页面:', match, targets.filter(x => x.type === 'page').map(x => x.url)); process.exit(2); }

const ws = new WebSocket(t.webSocketDebuggerUrl);
await new Promise((ok, bad) => { ws.onopen = ok; ws.onerror = bad; });
let id = 0; const pending = new Map(); const listeners = [];
ws.onmessage = e => {
  const m = JSON.parse(e.data);
  if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
  else if (m.method) listeners.forEach(f => f(m));
};
const send = (method, params = {}) => new Promise(ok => { const i = ++id; pending.set(i, ok); ws.send(JSON.stringify({ id: i, method, params })); });

if (cmd === 'eval') {
  const r = await send('Runtime.evaluate', { expression: rest.join(' '), awaitPromise: true, returnByValue: true, userGesture: true });
  if (r.result?.exceptionDetails) console.error('异常:', JSON.stringify(r.result.exceptionDetails.exception?.description || r.result.exceptionDetails));
  const v = r.result?.result?.value;
  console.log(typeof v === 'string' ? v : JSON.stringify(v, null, 2));
} else if (cmd === 'shot') {
  const r = await send('Page.captureScreenshot', { format: 'png' });
  const fs = await import('node:fs');
  fs.writeFileSync(rest[0], Buffer.from(r.result.data, 'base64'));
  console.log('saved', rest[0]);
} else if (cmd === 'click') {          // click <x> <y>（CSS 像素）
  const [x, y] = rest.map(Number);
  for (const type of ['mouseMoved', 'mousePressed', 'mouseReleased'])
    await send('Input.dispatchMouseEvent', { type, x, y, button: 'left', clickCount: 1 });
  console.log('clicked', x, y);
} else if (cmd === 'type') {           // type <文字>：往当前焦点输入
  for (const ch of rest.join(' ')) { await send('Input.insertText', { text: ch }); await new Promise(r => setTimeout(r, 60 + Math.random() * 80)); }
  console.log('typed', rest.join(' ').length, 'chars');
} else if (cmd === 'net') {
  const ms = Number(rest[0] || 10000), filt = rest[1] || '';
  const reqs = new Map();
  listeners.push(async m => {
    if (m.method === 'Network.requestWillBeSent' && m.params.request.url.includes(filt)) {
      const q = m.params.request;
      reqs.set(m.params.requestId, q);
      console.log('\n>>', q.method, q.url.slice(0, 300));
      if (q.postData) console.log('   body:', q.postData.slice(0, 1500));
    }
    if (m.method === 'Network.loadingFinished' && reqs.has(m.params.requestId)) {
      const b = await send('Network.getResponseBody', { requestId: m.params.requestId });
      console.log('<<', reqs.get(m.params.requestId).url.split('?')[0], '\n   resp:', (b.result?.body || '').slice(0, 3000));
    }
  });
  await send('Network.enable');
  await new Promise(r => setTimeout(r, ms));
}
ws.close();
