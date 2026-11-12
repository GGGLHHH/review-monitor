// review-monitor 注入脚本：只做「按参数代发请求、把原始结果送回宿主」，不含任何业务判断。
// 宿主通过 evaluate_script 调用 window.__rm.*，结果用 window.ipc.postMessage(JSON) 回传。
// 只调用只读查询接口。
(function () {
  if (window.top !== window) return;          // 只在顶层文档里安装
  if (window.__rm) return;

  const post = (msg) => {
    try { window.ipc.postMessage(JSON.stringify(msg)); } catch (e) { /* 宿主不在时忽略 */ }
  };

  const LIST_URL = {
    meituan: '/emis/gw/rpc/TFeedbackListNewService/queryMTFeedbackPCNew',
    dianping: '/emis/gw/rpc/TFeedbackListNewService/queryDPFeedbackPCNew',
  };
  const REVEAL_TAB = { meituan: '美团评价', dianping: '点评评价' };
  const WHOAMI_URL ='/emis/gw/account/biz/getUserInfo';
  const TIMEOUT_MS = 15000;

  // 评价模块所在的同源 iframe：H5guard（自动加 mtgsig 签名）挂在它的 XHR 上
  function evalFrame() {
    const f = document.querySelector('iframe[src*="/emis/evaluation/poi"]');
    try { return f && f.contentWindow && f.contentWindow.XMLHttpRequest ? f.contentWindow : null; }
    catch (e) { return null; }
  }

  function xhr(reqId, kind, method, url, body) {
    const w = evalFrame();
    if (!w) { post({ reqId, kind, error: 'no_iframe', href: location.href }); return; }
    const x = new w.XMLHttpRequest();
    x.open(method, url + (url.includes('?') ? '&' : '?') + '_tm=' + Date.now());
    x.timeout = TIMEOUT_MS;
    if (body != null) x.setRequestHeader('Content-Type', 'application/json');
    x.onload = () => post({ reqId, kind, status: x.status, body: x.responseText });
    x.onerror = () => post({ reqId, kind, error: 'network' });
    x.ontimeout = () => post({ reqId, kind, error: 'timeout' });
    x.send(body == null ? null : JSON.stringify(body));
  }

  window.__rm = {
    version: 1,
    // platform: "meituan" | "dianping"；body 见 docs/接口调研.md
    fetchList(reqId, platform, body) {
      const url = LIST_URL[platform];
      if (!url) { post({ reqId, kind: 'list', error: 'bad_platform' }); return; }
      xhr(reqId, 'list', 'POST', url, body);
    },
    whoami(reqId) { xhr(reqId, 'whoami', 'GET', WHOAMI_URL, null); },
    // 子账号名下门店列表（只读）
    poiList(reqId) { xhr(reqId, 'poi', 'GET', '/emis/gw/rpc/TFeedbackListNewService/getCityPoiListSimple', null); },
    // 页面状态：是否在登录页、评价 iframe 是否已加载
    // 点通知 / 卡片后：切到对应平台的标签，滚到那条评价并高亮。只读页面、不改任何数据。
    reveal(platform, id) {
      const label = REVEAL_TAB[platform];
      if (!label) { post({ kind: 'reveal', error: 'bad_platform' }); return; }
      const deadline = Date.now() + 20000;
      let tabDone = false;
      const step = () => {
        const f = document.querySelector('iframe[src*="/emis/evaluation/poi"]');
        let d = null; try { d = f && f.contentDocument; } catch (e) {}
        const later = () => Date.now() < deadline && (setTimeout(step, 400), true);
        if (!d || !d.body) { if (!later()) post({ kind: 'reveal', error: 'no_iframe', href: location.href }); return; }
        if (!tabDone) {
          const tab = [...d.querySelectorAll('button')].find(b => b.textContent.trim().startsWith(label));
          if (!tab) { if (!later()) post({ kind: 'reveal', error: 'no_tab' }); return; }
          if (!/active/i.test(tab.className)) tab.click();
          tabDone = true;
          setTimeout(step, 300);
          return;
        }
        const el = d.getElementById('feedback-' + id);
        if (!el) { if (!later()) post({ kind: 'reveal', error: 'not_found' }); return; }
        if (!d.getElementById('__rm_style')) {
          const s = d.createElement('style'); s.id = '__rm_style';
          s.textContent = '.__rm_hl{outline:3px solid #ffc300!important;outline-offset:6px;border-radius:8px;animation:__rm_pulse 1.2s ease-in-out 3}'
            + '@keyframes __rm_pulse{50%{outline-color:rgba(255,195,0,.25)}}';
          d.head.appendChild(s);
        }
        d.querySelectorAll('.__rm_hl').forEach(x => x.classList.remove('__rm_hl'));
        el.classList.add('__rm_hl');
        el.scrollIntoView({ block: 'center' });
        setTimeout(() => el.classList.remove('__rm_hl'), 12000);
        post({ kind: 'reveal' });
      };
      step();
    },
    probe(reqId) {
      post({ reqId, kind: 'probe', href: location.href,
             login: /\/login\.html|epassport\.meituan\.com/.test(location.href),
             frame: !!evalFrame() });
    },
  };

  // 每次导航都报告一次所在页面，便于宿主发现「被踢回登录页」
  const report = () => post({ kind: 'nav', href: location.href,
                              login: /\/login\.html/.test(location.href) });
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', report);
  else report();
})();
