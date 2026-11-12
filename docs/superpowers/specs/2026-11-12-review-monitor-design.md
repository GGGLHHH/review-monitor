# 差评监控（review-monitor）设计文档

- 日期：2026-11-12
- 状态：待用户评审
- 相关文档：`docs/接口调研.md`（接口实测）、`docs/子账号.md`（登录凭证，本地文件，不进版本库）

## 1. 目标

自家门店（示例餐厅（示例店））在**美团**或**大众点评**上出现新差评时，尽快提醒到人。

**成功标准**
- 新差评出现后，在「一个轮询间隔 + 1 分钟」内收到提醒（默认间隔是 3 分钟）。
- 连续运行 7 天：不漏报，不重复提醒，账号不触发风控。
- 掉线后 1 个轮询间隔内收到「请重新登录」提醒。

**不做（YAGNI）**
- 不回复评价，不改后台任何设置，全程只读。
- 不做后端：exe 和 apk 都纯本地运行，不建服务器、不用云推送，通知只走本机系统通知（2026-11-12 用户确认）。
- 不做多账号，也不做门店切换界面。子账号名下的门店由程序自动获取，有几个就监控几个（见第 5 节）。
- 不做 AI 识别「隐性差评」，只按星级阈值判断。以后有需要再加。
- 一期不做安卓。

## 2. 约束（来自用户和实测）

| 约束 | 来源 |
|---|---|
| 只监控自己的门店，用子账号 `demo_sub` 登录 | 用户 |
| 一个账号同一时间只在一处登录，exe 和 apk 互相独立 | 用户 |
| 登录由人手动完成：账号密码 + **滑块验证**。程序不自动填表单，也不拖滑块 | 实测 + 风控考虑 |
| 网页登录态看起来不会过期（还在观察） | 用户 + 待验证 |
| 取数必须在已登录的后台页面里同源发请求，签名交给页面里的 H5guard 自动处理，不在程序外伪造请求 | 实测 + 风控考虑 |
| GUI 用 Rust + **Slint**；嵌入 WebView 的方案走不通，就整体改用 **Tauri 2** | 用户 |
| 间隔、阈值等全部可以自定义；推送做成抽象层，以后要接手机通知、短信 | 用户 |

## 3. 分期

| 期 | 内容 |
|---|---|
| **第 0 步：技术原型** | Slint 窗口里嵌 wry（WebView2），验证第 7 节列出的 5 件事。原型代码用完就扔 |
| **一期：Windows exe** | 本文档的全部内容 |
| 二期：安卓 apk | 另写设计。复用 `core` 库和巡检脚本，外壳方案到时候再定 |

## 4. 架构

```
review-monitor/
├─ crates/core/            纯 Rust 库，不依赖任何 GUI 或 WebView，可单独测试，二期复用
│   ├─ model.rs            Platform{Meituan,Dianping}、Review、Alert
│   ├─ parse.rs            解析两个平台的接口 JSON → Vec<Review>（顺带去掉点评正文里的 HTML）
│   ├─ rules.rs            按平台阈值判断是否算差评
│   ├─ store.rs            SQLite：评价表 + 推送发件箱（去重、重试）
│   ├─ schedule.rs         下一次轮询时间 = 间隔 ± 抖动；失败退避
│   ├─ config.rs           TOML 配置读写，带默认值和校验
│   └─ notify/             Notifier trait + 各个渠道实现
│        ├─ mod.rs         trait、分发器
│        └─ toast.rs       Windows 系统通知（一期唯一的实现）
├─ crates/app/             Windows 程序（Slint + wry）
│   ├─ ui/*.slint          主窗口、设置页、后台窗口外框
│   ├─ webview.rs          创建 wry WebView、注入脚本、接收 IPC、检测跳转到登录页
│   ├─ poller.rs           轮询主循环：到点 → 让页面取数 → 交给 core 处理
│   └─ tray.rs             托盘图标：关闭窗口时缩到托盘，监控继续运行
└─ web/poller.js           注入到后台页面的脚本，只负责「按参数发请求、把原始 JSON 送回来」
```

**原则**：业务逻辑全部放在 Rust 的 `core` 里。JS 只是一个「代发请求」的执行器，不做判断，这样换外壳（Tauri、安卓）时逻辑不用重写。

## 5. 数据流

```
            ┌───────── 定时器（Rust，间隔±抖动）─────────┐
            ▼                                            │
 app::poller ──evaluate_script──▶ 页面里的 poller.js      │
                                   │ 在评价 iframe 里同源 XHR（H5guard 自动签名）
                                   │  美团 queryMTFeedbackPCNew  starType=全部
                                   │  点评 queryDPFeedbackPCNew  starType=全部
                                   ▼
             ◀──── ipc.postMessage({reqId, platform, status, body}) ────
 core::parse → core::rules（按阈值筛选）→ core::store（以「平台 + feedbackId」去重）
                                   │ 新出现的差评
                                   ▼
                     发件箱 outbox（每条差评 × 每个启用的渠道）
                                   ▼
                     core::notify 分发 → Toast / （以后）Webhook、短信…
                                   ▼
                     Slint 主界面刷新列表
```

**取数细节**
- 用 `starType=全部` 加时间窗口来拉数，阈值在本地判断。这样阈值可以设成任意值，比如 ≤4 星，不受平台固定档位的限制。每次轮询 2 个请求，两个请求之间间隔 1–3 秒。
- 时间窗口：`[上次成功时间 − 重叠时长, 现在]`，重叠时长默认 2 小时，防止评价晚入库导致漏报。首次运行回看 2 天。
- 分页：`pageSize=20`，按 `total` 翻页，最多翻 5 页（默认值，可配置）。不依赖 `hasMore`，因为实测中它和 `total` 对不上。
- 门店 ID：启动时从 `getCityPoiListSimple` 取一次，存进配置；后续轮询直接用配置里的值。

**首次运行**：默认把回看窗口里已有的评价全部记为「已存在」，不发提醒（配置项可以改）。界面上照常列出来，可以查看。

## 6. 配置（`%APPDATA%\review-monitor\config.toml`，设置页可以改）

```toml
[poll]
interval_minutes = 3         # 1–120（2026-11-12 用户改为默认 3）
jitter_percent   = 20        # 0–50
overlap_minutes  = 120
first_run_lookback_hours = 48
max_pages = 5

[rules]
meituan_max_star  = 3.5      # ≤ 这个值算差评；0.5–5
dianping_max_star = 3.5
platforms = ["meituan", "dianping"]
only_unreplied = false
alert_existing_on_first_run = false

[notify]
enabled = ["toast"]          # 以后：["toast", "wecom", "sms"]
[notify.toast]
# 暂时没有参数
```

- 保存设置时先校验，超出范围就在界面上提示，不写盘。
- 改动立即生效，不用重启；间隔改了之后从下一轮开始算。

## 7. 第 0 步：技术原型（Slint + wry）

**只验证一个问题**：Slint 窗口里嵌入的 WebView2，能不能完整跑通「登录 → 取数 → 回传」这条链路。

| # | 验证项 | 通过标准 |
|---|---|---|
| 1 | wry 用 `build_as_child` 嵌进 Slint 窗口，跟随窗口缩放 | 后台页面显示正常，可以点击、输入 |
| 2 | 在嵌入的 WebView 里手动登录（包括滑块） | 能进入评价管理页 |
| 3 | 登录态持久化：WebView2 数据目录固定在 `%APPDATA%\review-monitor\webview` | 重启程序后不用重新登录 |
| 4 | Rust 调用 `evaluate_script`，在评价 iframe 里发 XHR，结果通过 IPC 回到 Rust | 拿到 `queryMTFeedbackPCNew` 和 `queryDPFeedbackPCNew` 的 JSON，返回 200 |
| 5 | 后台窗口隐藏或最小化后，第 4 项依然有效 | 隐藏 30 分钟以后还能取到数据 |

- **全部通过**：进入一期开发。
- **任何一项走不通，并且查过资料也绕不过去**：整体改用 Tauri 2。`core` 和 `poller.js` 原样复用，只把 `crates/app` 换成 Tauri 的外壳，UI 改成网页技术。
- 原型代码放在 `spike/`，结论写进 `docs/spike-slint-wry.md`，原型代码本身不并入正式代码。

## 8. 推送抽象层

```rust
pub struct Alert {
    pub kind: AlertKind,                 // NewBadReview | LoginLost | LoginRestored | ApiError
    pub platform: Option<Platform>,
    pub star: Option<f32>,
    pub time: Option<DateTime<Local>>,
    pub title: String,                   // 例："点评 2.5★ 新差评"
    pub body: String,                    // 正文前 80 字
    pub review_key: Option<String>,      // "dp:3550718756"
}

#[async_trait]
pub trait Notifier: Send + Sync {
    fn id(&self) -> &'static str;        // "toast" / "wecom" / "sms"
    async fn send(&self, alert: &Alert) -> anyhow::Result<()>;
}
```

- **发件箱**：每条「差评 × 渠道」写一行发件记录，发送成功就标记完成。失败按 1 → 5 → 15 分钟重试，最多 5 次，仍失败就标记为放弃，并在界面上显示。程序重启后会接着发没发完的。
- 各渠道互不影响，一个渠道挂了不会拖累别的渠道。
- 以后加新渠道（企业微信或飞书 Webhook、短信、手机推送）：新写一个 `impl Notifier`，在配置里加一节，再到 `enabled` 里开启就行，不用改动 core 的其他代码。

## 9. 登录态与错误处理

| 情况 | 怎么发现 | 怎么处理 |
|---|---|---|
| 掉线 | WebView 跳转到 `bizaccount/login.html`，或接口返回未登录 | 暂停轮询，发一次 `LoginLost` 提醒，把后台窗口弹到前台等待手动登录；检测到恢复后发 `LoginRestored`，并补拉这段时间的数据 |
| 网络错误或超时 | XHR 的 `onerror` 或超时（15 秒） | 记日志；本轮跳过，退避后重试；连续失败 3 次发 `ApiError` 提醒 |
| 接口返回非 200，或 JSON 结构变了 | 状态码或解析失败 | 记日志并保存原始响应（截断），发 `ApiError` 提醒（提示「接口可能有变化」），同一个错误 6 小时内只提醒一次 |
| 弹出滑块或风控页 | 页面出现验证弹窗，或接口返回风控相关的码 | 当作掉线处理：暂停轮询，提醒人工处理，**不自动重试** |

日志放在 `%APPDATA%\review-monitor\logs\`，按天滚动，只保留 14 天。

## 10. 界面（Slint，自定义主题，不用默认样式）

- **主窗口**
  - 顶部状态栏：登录状态（绿/红）、上次检查时间、下次检查倒计时，以及「立即检查」「打开后台」两个按钮。
  - 中间：差评列表，新的排在最前。每条显示平台、星级、时间、正文、是否已回复，带「已读」标记。
  - 点击一条差评：打开后台窗口并跳到评价页，回复由人自己操作。
  - 点系统通知（2026-11-12 用户追加）：和点卡片一样，切到后台页、选好美团/点评标签、滚到这条评价并高亮（页面里评价容器的 id 是 `feedback-<feedbackId>`）。通知走 `review-monitor://` 协议激活，程序退出后从操作中心点也有效。
- **设置页**：第 6 节的全部配置项，保存时校验。
- **后台窗口**：外框是 Slint 窗口，里面嵌 WebView，用来登录，也可以直接看后台。
  - 实现时改成了主窗口里的「后台页面」标签（WebView 叠在内容区上），不单开窗口。见 `docs/spike-slint-wry.md`。
- **托盘**：关闭主窗口时缩到托盘，监控继续运行；托盘菜单有「打开」「立即检查」「退出」。

## 11. 测试

- **core 单元测试**（TDD）
  - `parse`：用 2026-11-12 实测抓到的真实响应做测试样本，去掉用户名和头像，存到 `crates/core/tests/fixtures/`。覆盖两个平台、正文为空、正文带 HTML、半星。
  - `rules`：阈值边界，比如 3.5 和 3.6。
  - `store`：去重；首次运行只做基线、不提醒；发件箱的重试和放弃。
  - `schedule`：抖动范围；失败退避。
  - `config`：默认值；非法值被拒绝。
  - `notify`：用模拟渠道验证分发和失败隔离。
- **集成测试**：把 `poller.js` 的回传换成测试样本，跑通「回传 → parse → rules → store → notify」整条链路。
- **真机验收**：用子账号连续跑 7 天，对照后台页面人工核对有没有漏报或重复（对应第 1 节的成功标准）。

## 12. 待验证事项

| 事项 | 什么时候验证 |
|---|---|
| Slint + wry 能不能跑通（第 7 节） | ✅ 已通过，见 `docs/spike-slint-wry.md` |
| 网页登录态实际能保持多久 | 调试 Chrome 持续观察中，一期上线后继续看 |
| 子账号登录会不会挤掉主账号或手机 App | 等用户反馈 |
| 接口是否强制校验签名 | 二期（安卓）之前再验证，做之前先问用户 |
