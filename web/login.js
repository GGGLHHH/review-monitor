// review-monitor 登录辅助脚本：只注入 epassport.meituan.com 的登录表单所在的帧。
// 做两件事：① 把宿主（应用自己的登录页）传来的账号、密码填进美团的表单并点「登录」；② 把表单状态报给宿主。
// 短信验证同理：手机号、按钮状态报给宿主；用户在应用里点「获取验证码」、输入验证码，这里照着点、照着填。
// 不碰滑块：滑块出现时只上报，由用户自己在页面上拖。不保存、不外传账号密码和验证码。
(function () {
  if (window.__rmLogin || !window.rmLogin) return;
  window.__rmLogin = 1;

  const port = window.rmLogin;
  const post = (o) => { try { port.postMessage(JSON.stringify(o)); } catch (e) { /* 宿主不在时忽略 */ } };
  const $ = (s) => document.querySelector(s);
  const vis = (e) => !!e && e.offsetWidth > 0 && e.offsetHeight > 0;

  // 页面框架接管了输入框：用原生 setter 赋值再派发事件，框架才认
  function setValue(el, v) {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(el, v);
    for (const t of ['input', 'change', 'blur']) el.dispatchEvent(new Event(t, { bubbles: true }));
  }

  function state() {
    const form = !!($('#login') && $('#password') && $('button.ep-login_btn'));
    // 短信验证（新设备登录时美团要求）：可以在应用自己的页面上完成
    const sms = vis($('#yodaSms')) && !!$('#yodaSmsCodeInput');
    // 其余验证（滑块，或不认识的类型）：必须把页面露出来让用户自己操作
    const captcha = !sms && (vis($('#yodaPopupMask')) || vis($('.yoda-slider-wrapper')));
    const error = [...document.querySelectorAll('.ep-input_error, #yodaErrorTip, [class*="toast"], [class*="Toast"]')]
      .filter(vis).map((e) => e.textContent.trim()).filter(Boolean).join('；').slice(0, 200);
    const s = { t: 'state', form, captcha, sms, error };
    if (sms) {
      const b = $('#yodaSmsCodeBtn'), m = $('#yodaSms [class*="mobileRow"]');
      s.phone = m ? m.textContent.trim() : '';
      s.sendText = b ? b.textContent.trim() : '';
      s.sendEnabled = !!b && !b.disabled;
    }
    return s;
  }

  // 验证弹窗出现期间（以及消失后的 2 秒内）把页面其余内容藏起来：
  // 否则滑块拖完、弹窗先消失的那一瞬间，会露出后面美团原本的登录表单。只改外观。
  let capUntil = 0;
  function hideRest(st) {
    if (st.captcha || st.sms) capUntil = Date.now() + 2000;
    document.documentElement.classList.toggle('rm-cap', Date.now() < capUntil);
  }

  let last = '';
  function report() {
    const st = state();
    hideRest(st);
    const s = JSON.stringify(st);
    if (s !== last) { last = s; post(JSON.parse(s)); }
  }

  port.onmessage = (e) => {
    let cmd; try { cmd = JSON.parse(e.data); } catch (_) { return; }
    if (cmd.t === 'smsSend') { const b = $('#yodaSmsCodeBtn'); if (b && !b.disabled) b.click(); return; }
    if (cmd.t === 'smsVerify') {
      const i = $('#yodaSmsCodeInput');
      if (!i) return;
      setValue(i, cmd.code);
      setTimeout(() => { const b = $('#yodaSubmit'); if (b && !b.disabled) b.click(); }, 300);
      return;
    }
    if (cmd.t !== 'submit') return;
    const u = $('#login'), p = $('#password'), btn = $('button.ep-login_btn'), agree = $('#checkbox');
    if (!u || !p || !btn) { post({ t: 'submitted', ok: false }); return; }
    setValue(u, cmd.user);
    setValue(p, cmd.pass);
    // 协议的勾：用户已在应用的登录页上勾过，这里照着勾上
    if (agree && !agree.checked) agree.click();
    setTimeout(() => { btn.click(); post({ t: 'submitted', ok: true }); }, 300);
  };

  // 只改外观：验证弹窗的遮罩改成不透明的深色，露出页面时只看得到验证本身，看不到后面的网页表单
  const css = document.createElement('style');
  css.textContent = '#yodaPopupMask{background:#14161B!important;position:fixed!important;left:0!important;top:0!important;width:100%!important;height:100%!important;z-index:2147483000!important}#yodaPopupMask .popup-container{width:92%!important;border-radius:12px!important;overflow:hidden!important}' +
    'html.rm-cap,html.rm-cap body{background:#14161B!important}html.rm-cap body *{visibility:hidden!important}html.rm-cap #yodaPopupMask,html.rm-cap #yodaPopupMask *{visibility:visible!important}';
  (document.head || document.documentElement).appendChild(css);

  setInterval(report, 400);
  report();
})();
