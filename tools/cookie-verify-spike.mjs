// spike 配套: 解密 cookie-extract-spike.py 的输出, 并实测「仅 bsid cookie 直接请求」
import { createDecipheriv } from 'node:crypto';
import { readFileSync } from 'node:fs';
const data = JSON.parse(readFileSync(process.argv[2], 'utf-8'));
const key = Buffer.from(data.aes_key_hex, 'hex');
const jar = {};
for (const [name, v] of Object.entries(data.cookies)) {
  if (typeof v === 'string') { jar[name] = v; continue; }
  const d = createDecipheriv('aes-256-gcm', key, Buffer.from(v.nonce, 'hex'));
  d.setAAD(Buffer.from('chrome_cookie'));
  jar[name] = Buffer.concat([d.update(Buffer.from(v.ct, 'hex')), d.final()]).toString('utf-8');
}
console.log('解出 cookie:', Object.keys(jar).join(', '));
console.log('bsid =', (jar['com.sankuai.meishi.fe.kdb-bsid'] || '缺失').slice(0, 20) + '…');

const base = 'https://ecom.meituan.com';
const UA = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36';
const H = { 'User-Agent': UA, 'Referer': 'https://ecom.meituan.com/emis/evaluation/poi', 'Cookie': 'com.sankuai.meishi.fe.kdb-bsid=' + jar['com.sankuai.meishi.fe.kdb-bsid'] };
const who = await fetch(base + '/emis/gw/account/biz/getUserInfo?_tm=' + Date.now(), { headers: H });
console.log('\nwhoami(仅bsid): HTTP ' + who.status, (await who.text()).slice(0, 150));
const body = JSON.stringify({ pageNo: 1, pageSize: 20, poiId: process.env.POI_ID || '10000001', startTime: Date.now() - 86400000, endTime: Date.now(), replyType: '全部', starType: '全部', sourceType: '全部', userType: '全部', timeType: '全部' });
const list = await fetch(base + '/emis/gw/rpc/TFeedbackListNewService/queryMTFeedbackPCNew?_tm=' + Date.now(), { method: 'POST', headers: { ...H, 'Content-Type': 'application/json' }, body });
const j = await list.json();
console.log('MT列表(仅bsid): HTTP ' + list.status, 'total=' + j?.data?.data?.total, '首条分数=' + j?.data?.data?.feedbacks?.[0]?.score);
