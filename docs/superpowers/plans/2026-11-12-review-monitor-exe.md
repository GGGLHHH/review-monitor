# 差评监控 Windows exe 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 做出 Windows exe：嵌入的 WebView 里登录经营宝 → 定时在页面内同源取美团和点评的评价 → 按可配置阈值判断差评 → 通过可扩展的推送层发 Windows 通知。

**Architecture:** 业务逻辑全部放在纯 Rust 库 `crates/core`（解析、规则、SQLite 存储与发件箱、调度、配置、Notifier）。`crates/app` 用 Slint 做界面，用 wry 0.57 把 WebView2 嵌进 Slint 窗口。`web/poller.js` 只负责代发请求，结果通过 IPC 回传。

**Tech Stack:** Rust 1.99、slint 1.18、wry 0.57、rusqlite 0.42（bundled）、serde + toml 1.x、chrono 0.4、tauri-winrt-notification 0.8、tray-icon 0.26、tracing + tracing-appender。

**Spec:** `docs/superpowers/specs/2026-11-12-review-monitor-design.md`

## Global Constraints

- 只读：只调用第 5 节列出的查询接口，不调用任何回复或修改接口。
- 不自动填写登录表单，不拖滑块。出现滑块或风控时暂停轮询，提醒人工处理。
- 数据目录 `%APPDATA%\review-monitor\`：`config.toml`、`data.db`、`webview\`、`logs\`。
- 默认配置：interval 3 分钟（原定 10，2026-11-12 用户改为 3）（范围 1–120）、jitter 20%（0–50）、overlap 120 分钟、首次回看 48 小时、max_pages 5、两个平台的阈值都是 3.5（0.5–5）、only_unreplied=false、alert_existing_on_first_run=false、enabled=["toast"]。
- 去重键：`mt:<feedbackId>` / `dp:<feedbackId>`。
- 推送重试 1 → 5 → 15 分钟，最多 5 次，超过就标记 gave_up。同一种 ApiError 6 小时内只提醒一次。
- 每次轮询的两个请求之间间隔 1–3 秒；XHR 超时 15 秒。
- 用户规定：**不准私自 commit / push**。计划里不写提交步骤，用户要求时再提交。
- 界面用自定义主题，不用 Slint 的默认样式。

## Review Focus

1. 点评正文带 `<img ...>` 等 HTML，或者正文为空 → 列表和通知里只显示纯文字，为空时显示「（无文字）」。（Task 1）
2. 后台把同一条评价重复返回，或者时间窗口重叠 → 只提醒一次。（Task 3）
3. 程序在推送发出前崩溃或被关掉 → 重启后接着发没发完的。（Task 3）
4. 用户把配置改成非法值，或者手动把 config.toml 改坏 → 保存时拒绝；启动时读到坏文件就回退到默认值，并备份原文件。（Task 5）
5. 掉线期间的评价 → 重新登录后补拉，不漏报。（Task 8：恢复后从上次成功的时间点开始拉）

## 文件结构

```
review-monitor/
├─ Cargo.toml                     workspace
├─ spike/slint-wry/               Task 0 原型（验证完就扔）
├─ crates/core/src/{lib,model,parse,rules,store,schedule,config}.rs
├─ crates/core/src/notify/{mod,toast}.rs
├─ crates/core/tests/fixtures/{mt_list.json,dp_list.json}
├─ crates/app/{build.rs,Cargo.toml}
├─ crates/app/src/{main,webview,poller,tray,paths}.rs
├─ crates/app/ui/{app.slint,theme.slint,settings.slint}
└─ web/poller.js
```

---

### Task 0: 技术原型 Slint + wry（决定走 Slint 还是 Tauri）

**Files:** Create `spike/slint-wry/{Cargo.toml,src/main.rs}`、`docs/spike-slint-wry.md`

- [x] 一个 Slint 窗口，拿到 HWND 后用 `wry::WebViewBuilder::new_as_child(&window_handle)` 嵌入 WebView，`WebContext::new(Some(%APPDATA%\review-monitor\webview))` 用来持久化登录态，打开评价页。
- [x] 窗口 resize 时调用 `webview.set_bounds()`。
- [x] 用一个按钮触发 `evaluate_script(poller.js 的调用)`，`with_ipc_handler` 收到 JSON 后打印到控制台。
- [x] 验收（spec 第 7 节）：①显示和交互正常 ②人工登录 ③重启后仍是登录状态 ④两个接口返回 200 ⑤窗口隐藏后仍能取数。结论写进 `docs/spike-slint-wry.md`。
- [~] （没走到，原型通过）失败处理：停下来，把 `crates/app` 改成 Tauri 2 的方案，并更新本计划的 Task 6–9。

### Task 1: core 模型与解析

**Files:** `crates/core/src/{model,parse}.rs`、fixtures 两份（来自实测响应，用户名和头像替换成假的）

**Produces:**
- `enum Platform { Meituan, Dianping }`，提供 `key_prefix()`（返回 `"mt"` / `"dp"`）和 `label()`（返回 `"美团"` / `"点评"`）
- `struct Review { key: String, platform: Platform, feedback_id: i64, add_time: DateTime<Local>, star: f32, content: String, replied: bool, source: String, poi_name: String, user_name: String }`
- `fn parse_list(platform: Platform, body: &str) -> Result<ListPage, ParseError>`，其中 `ListPage { total: u32, reviews: Vec<Review> }`
- `fn clean_text(s: &str) -> String`：去掉 HTML 标签、合并空白

- [x] 测试 `parse_mt_fixture`：total=7，第一条 key="mt:10566010284"，star=1.0，replied=false。
- [x] 测试 `parse_dp_fixture`：读出 star=2.5，content 里不包含 `<img`。
- [x] 测试 `clean_text_strips_html_and_empty`：`clean_text("<img src=\"x\">好吃\n\n ") == "好吃"`，`clean_text("")==""`。
- [x] 测试 `parse_rejects_non_json`：返回 `Err(ParseError::Json)`；结构缺 `data.data` 时返回 `Err(ParseError::Shape)`。
- [x] 实现：star 先取 `accurateScore`，没有再取 `score`；replied 判断 `replyCount>0` 或 `merchantReplyList` 非空。
- [x] `cargo test -p rm-core` 全部通过。

### Task 2: core 规则

**Files:** `crates/core/src/rules.rs`

**Produces:** `struct Rules { meituan_max_star: f32, dianping_max_star: f32, platforms: Vec<Platform>, only_unreplied: bool }`，`fn is_bad(&self, r: &Review) -> bool`

- [x] 测试 `threshold_boundary`：阈值 3.5 时，3.5 → true，3.6 → false。
- [x] 测试 `platform_disabled`：platforms 只有美团时，点评的 1 星 → false。
- [x] 测试 `only_unreplied`：开启后，已回复的 1 星 → false。
- [x] 实现并通过测试。

### Task 3: core 存储与发件箱

**Files:** `crates/core/src/store.rs`（rusqlite，bundled）

**Produces:**
- `Store::open(path) / open_in_memory()`
- `fn ingest(&mut self, reviews: &[Review], rules: &Rules, notifiers: &[&str], baseline: bool) -> Result<Vec<Review>>`：返回本次**新出现的差评**。baseline=true 时只入库、不进发件箱。
- `fn due_outbox(&self, now) -> Vec<OutboxItem>`、`fn mark_sent(id)`、`fn mark_failed(id, now)`（失败后按 1/5/15/15/15 分钟排下一次，第 5 次失败标记 gave_up）
- `fn recent(&self, limit) -> Vec<StoredReview>`（附带 is_bad、read 字段）、`fn mark_read(key)`
- `fn get_state(k)/set_state(k,v)`：保存 last_success_ms、initialized 等
- 表：`reviews(key PK, platform, feedback_id, add_time, star, content, replied, source, poi_name, user_name, is_bad, read, first_seen)`、`outbox(id PK, review_key, notifier, status, attempts, next_at, last_error)`、`state(k PK, v)`

- [x] 测试 `dedupe_same_key_twice`：同一条差评 ingest 两次，第二次返回空，outbox 只有 1 行。
- [x] 测试 `baseline_no_outbox`：baseline=true 时 outbox 为空，但评价已入库。
- [x] 测试 `outbox_retry_schedule`：mark_failed 5 次后状态为 gave_up，期间 next_at 依次往后推 1/5/15 分钟。
- [x] 测试 `outbox_survives_reopen`：用临时文件建库，关闭后重新打开，due_outbox 仍然能取到之前的项。
- [x] 测试 `multi_notifier_rows`：notifiers=["toast","x"] 时，一条差评对应 2 行 outbox。
- [x] 实现并通过测试。

### Task 4: core 调度

**Files:** `crates/core/src/schedule.rs`

**Produces:** `fn next_delay(interval_min: u32, jitter_pct: u32, rng: &mut impl Rng) -> Duration`、`fn backoff(consecutive_failures: u32, interval_min: u32) -> Duration`（取 min(interval×2^n, 60 分钟)）、`fn query_window(last_success: Option<DateTime>, now, overlap_min, first_lookback_h) -> (start_ms, end_ms)`

- [x] 测试 `jitter_within_bounds`：10 分钟、20% 抖动，抽样 1000 次全部落在 [8, 12] 分钟。
- [x] 测试 `window_first_run`：没有 last_success 时，start = now − 48h。
- [x] 测试 `window_overlap`：有 last_success 时，start = last_success − 120 分钟。
- [x] 测试 `backoff_caps_at_60`。
- [x] 实现并通过测试。

### Task 5: core 配置

**Files:** `crates/core/src/config.rs`

**Produces:** `struct Config { poll: PollCfg, rules: RulesCfg, notify: NotifyCfg }`（字段与 spec 第 6 节一致，serde 默认值取自 Global Constraints）、`fn validate(&self) -> Result<(), Vec<String>>`（错误信息用中文）、`fn load_or_default(path) -> (Config, Option<String /*警告*/>)`、`fn save(&self, path) -> Result<()>`（先校验，再原子写入），以及 `fn rules(&self) -> Rules`

- [x] 测试 `defaults_match_spec`：默认值逐个断言。
- [x] 测试 `validate_rejects`：interval=0、jitter=60、star=6 都报错，并带中文提示。
- [x] 测试 `corrupt_file_falls_back`：写入垃圾内容后加载，得到默认值和警告信息，并且生成了 `config.toml.bad` 备份。
- [x] 测试 `save_roundtrip`。
- [x] 实现并通过测试。

### Task 6: core 推送层

**Files:** `crates/core/src/notify/{mod,toast}.rs`

**Produces:**
- `struct Alert { kind: AlertKind, platform: Option<Platform>, star: Option<f32>, time: Option<DateTime<Local>>, title: String, body: String, review_key: Option<String> }`
- `enum AlertKind { NewBadReview, LoginLost, LoginRestored, ApiError }`
- `trait Notifier: Send + Sync { fn id(&self) -> &'static str; fn send(&self, a: &Alert) -> anyhow::Result<()>; }`（一期用同步接口，Webhook 和短信这类阻塞 IO 放到工作线程里跑）
- `fn alert_for(r: &Review) -> Alert`：标题格式「点评 2.5★ 新差评」，正文取前 80 个字，正文为空时显示「（无文字）」
- `struct Dispatcher { notifiers: Vec<Box<dyn Notifier>> }`
  - `fn flush(&self, store: &mut Store, now)`：取出到期的发件项逐个发送，记录成功或失败
  - `fn system(&self, a: &Alert)`：系统消息直接发给所有渠道，不进发件箱
- `ToastNotifier`（基于 tauri-winrt-notification）

- [x] 测试 `alert_title_format`、`alert_empty_body`。
- [x] 测试 `flush_isolates_failures`：注册一个总是失败的 notifier 和一个正常的，正常的那一行被标记 sent，失败的那一行 attempts=1。
- [x] 实现并通过测试（Toast 在 Task 9 手动验证）。

### Task 7: 注入脚本 poller.js

**Files:** `web/poller.js`

**Produces:** 在顶层页面上定义 `window.__rm = { fetchList(reqId, platform, body), whoami(reqId) }`
- 先找 `iframe[src*="/emis/evaluation/poi"]`，用它的 `contentWindow.XMLHttpRequest` 发请求；找不到 iframe 就回传 `{reqId, error:"no_iframe"}`。
- 请求地址：美团 `/emis/gw/rpc/TFeedbackListNewService/queryMTFeedbackPCNew?_tm=`，点评 `/emis/gw/rpc/TFeedbackListNewService/queryDPFeedbackPCNew?_tm=`。
- 每个请求超时 15 秒。结果通过 `window.ipc.postMessage(JSON.stringify({reqId, kind, status, body|error}))` 回传。
- `whoami` 调用 `/emis/gw/account/biz/getUserInfo`，用来探测登录状态。
- 另外：`location.href` 包含 `login.html` 时，回传 `{kind:"login_page"}`。这一条放在 wry 的 initialization script 里，每次导航都会执行。
- [x] 在 Task 0 原型里真机验证 fetchList 能拿到两个平台的数据。

### Task 8: app 轮询主循环与 WebView 宿主

**Files:** `crates/app/src/{webview,poller,paths}.rs`

**Consumes:** Task 1–7 的全部接口

- `paths.rs`：`data_dir()` 返回 `%APPDATA%\review-monitor`，不存在就自动创建。
- `webview.rs`：照搬 Task 0 原型的做法创建 WebView，提供 `fn request(&self, js_call: String)`，IPC 消息通过 channel 送回 poller。
- `poller.rs`：在 Slint 事件循环里用 `slint::Timer` 驱动的状态机，状态有 `Idle → WaitingWhoami → WaitingMt → WaitingDp → Idle`。
  - 每一步请求都有 20 秒的看门狗，超时就算失败。
  - 两个请求之间随机等待 1–3 秒。
  - 分页：按 total 翻页，最多 max_pages 页。
  - 首次运行（state 里 initialized 不存在）：baseline = !alert_existing_on_first_run。
  - 成功：保存 last_success，然后调用 `Dispatcher::flush`。
  - 失败：按 backoff 推迟下一轮；连续失败 3 次发 ApiError（同类错误 6 小时内只发一次，用 state 键 `apierr:<kind>` 记录）。
  - 掉线（收到 login_page，或 whoami 失败）：暂停轮询，发一次 LoginLost，把后台窗口弹到前台；恢复后发 LoginRestored，立刻拉一次，窗口从 last_success 开始，确保补上掉线期间的评价。
- [x] 手动验证：开启 Debug 日志，把间隔设成 1 分钟，日志里能看到完整一轮：whoami → mt → dp → ingest → flush。

### Task 9: Slint 界面、托盘、通知、打包

**Files:** `crates/app/ui/{theme,app,settings}.slint`、`crates/app/src/{main,tray}.rs`、`crates/app/build.rs`

- 主题：自定义深色加暖黄强调色（呼应经营宝的品牌色），圆角卡片，系统中文字体（Microsoft YaHei UI）。
- 主窗口：
  - 状态栏：登录状态圆点、上次检查时间、下次检查倒计时，「立即检查」「打开后台」两个按钮。
  - 差评列表：平台标签、星级、时间、正文、已回复或未回复，可以切换「只看差评 / 全部」；点击一条标记为已读，同时打开后台窗口。
- 设置页：spec 第 6 节的全部字段；保存时调用 `Config::validate`，有错误就在页面上显示。
- 后台窗口：一个 Slint 窗口，内嵌 WebView。
- 托盘：关闭主窗口时只隐藏不退出；托盘菜单有「打开」「立即检查」「退出」。
- 打包：`cargo build --release -p review-monitor`；在 `build.rs` 里设置 `windows_subsystem = "windows"` 和程序图标，产物是 `target/release/review-monitor.exe`。
- [ ] 手动验证：Toast 能弹出；托盘三个菜单项都能用；设置页填非法值会提示，填合法值保存后下一轮生效。（已验证：Toast 弹出、关窗缩托盘后继续轮询；托盘菜单和设置页校验待手点）
- [ ] 真机验证：用子账号跑 1 小时，日志无报错，界面上的差评数量和后台页面对得上。

### Task 10: 收尾

- [x] `README.md`：怎么运行、数据目录、怎么登录、配置说明、怎么新增推送渠道（实现 `Notifier`）。
- [x] 跑一遍 `cargo test --workspace`、`cargo clippy --workspace`，两项都要通过。
- [x] 把 exe 复制到 `dist/review-monitor.exe`，附 `dist/说明.md`。
