# 差评监控（review-monitor）

监控自家门店在美团、大众点评上的新差评，有新差评就弹 Windows 通知。只读，不会替你回复。

数据来源是美团经营宝网页版（`ecom.meituan.com`）的评价页：程序里嵌了一个浏览器，你在里面登录一次，之后程序直接在后台按间隔调用评价列表接口（复用内嵌浏览器的登录 cookie；实测这几个接口不需要页面里的签名参数）。

只监控你自己的门店、用你自己的账号；纯本地运行，没有服务器，数据不上传。Windows 版和安卓版各自独立（各自登录、各自检查、各自提醒）。

## 安卓版

源码在 `android/`（Kotlin + Compose 界面，检查逻辑复用 `crates/core`，通过 `crates/ffi` 的 UniFFI 绑定调用）。使用说明见 `dist/安卓说明.md`，设计见 `docs/superpowers/specs/2026-11-12-review-monitor-android-design.md`。

```bash
bash android/build-rust.sh              # 编译 Rust 库（需要 Android NDK 和 cargo-ndk）并生成 Kotlin 绑定
cd android && ./gradlew assembleRelease # 产物：android/app/build/outputs/apk/release/app-release.apk
```

登录在应用自己的登录页完成；美团的滑块验证需要你自己拖，程序不会代做。

## 运行（Windows）

```powershell
cargo run -p review-monitor            # 调试版，带控制台日志
cargo build --release -p review-monitor # 发布版：target\release\review-monitor.exe
```

需要 Windows 10/11 和 WebView2 运行时（Win11 自带；Win10 一般随 Edge 装好了）。同一时间只能开一个实例，再开会提示已在运行。

## 第一次使用

1. 打开程序，点左侧「后台页面」。
2. 在里面登录经营宝账号。**滑块请手动拖**，程序不会自动填表或拖滑块。
3. 登录后左下角显示「后台已登录」，几秒后开始第一轮检查。
4. 第一轮只记录已有的评价、不提醒（想让已有差评也提醒，在设置里打开「首次运行也提醒已有差评」）。之后出现的新差评才会弹通知。

登录状态保存在本机，重启程序不用重新登录。如果后台掉线，程序会暂停检查、弹一次通知并切到「后台页面」，重新登录后自动补拉掉线期间的评价。

关掉窗口不会退出，程序缩到托盘继续监控。真正退出：托盘图标右键 →「退出」。

## 点通知跳转

- 点「新差评」通知：打开主窗口 → 后台页面 → 自动切到美团/点评标签，滚到那条评价并黄框高亮，同时标为已读。差评列表里点卡片效果一样。
- 点「需要登录」通知：打开后台页面去登录。
- 程序退出后，从操作中心点旧通知也行：会先启动程序再跳过去。
- 原理：通知里带一个 `review-monitor://open?review=dp:<id>` 链接。程序每次启动时在 `HKCU\Software\Classes\review-monitor` 注册这个协议（指向当前 exe），在 `HKCU\Software\Classes\AppUserModelId\ReviewMonitor.App` 注册通知显示名和图标。只写当前用户，不需要管理员权限。点通知时系统再启动一个 exe，它发现已有实例在跑，就把链接写进 `%APPDATA%\review-monitor\activate.txt` 交给那个实例，然后自己退出。
- 这个链接任何网页都能触发，所以解析很严格：只认「打开」「后台页」「定位某条评价（平台 + 纯数字 id）」三种，别的一律当作「打开窗口」。
- 再双击一次 exe 也一样：会把已经在运行的窗口提到最前面，不会开第二份。
- 不用了想清干净：删掉上面两个注册表项即可。

## 数据目录

全部在 `%APPDATA%\review-monitor\`：

| 路径 | 内容 |
|---|---|
| `config.toml` | 配置（设置页改的就是它） |
| `config.toml.bad` | 配置文件损坏时的备份，程序会改用默认值并在顶部提示 |
| `data.db` | SQLite：评价、推送发件箱、运行状态 |
| `webview\` | 内嵌浏览器的数据（登录状态） |
| `logs\` | 按天滚动的日志，保留 14 天 |

删掉 `webview\` 等于退出登录；删掉 `data.db` 会重新做一次「首次运行」。

## 配置

设置页里都能改，保存时会校验。对应 `config.toml`：

```toml
[poll]
interval_minutes = 3           # 检查间隔，1–120
jitter_percent = 20            # 间隔随机浮动，0–50
overlap_minutes = 120          # 每次往前多查一段，防止漏掉延迟出现的评价，0–1440
first_run_lookback_hours = 48  # 首次运行回看多久，1–720
max_pages = 5                  # 每个平台每轮最多翻几页（每页 20 条），1–50

[rules]
meituan_max_star = 3.5         # 美团 ≤ 这个星级算差评，0.5–5
dianping_max_star = 3.5        # 点评同上
platforms = ["meituan", "dianping"]
only_unreplied = false         # 只提醒还没回复的
alert_existing_on_first_run = false

[notify]
enabled = ["toast"]
```

推送失败会按 1、5、15、15 分钟重试，第 5 次仍失败就放弃并在界面顶部提示。接口连续失败 3 次会提醒一次，同类提醒 6 小时内不重复。

## 代码结构

```
crates/core/   业务逻辑（纯 Rust，有单元测试）
  parse.rs     解析接口返回
  rules.rs     差评判定
  store.rs     SQLite：评价去重、发件箱
  schedule.rs  间隔、抖动、退避、查询时间窗
  config.rs    配置读写和校验
  poller.rs    轮询状态机（通过 Page trait 和页面交互，可脱离界面测试）
  notify/      推送抽象层和 Windows 通知
crates/app/    Slint 界面 + wry 内嵌 WebView + 托盘
  direct.rs    直连评价接口（cookie 从 WebView2 的 cookie 库取，免页面签名）
  webview.rs   WebView 只管登录界面和定位评价
web/poller.js  注入页面的脚本：安卓用；Windows 上只负责上报页面导航和定位评价
```

测试：`cargo test --workspace`。

## 新增推送渠道（手机通知、短信等）

1. 在 `crates/core/src/notify/` 里新建文件，实现 `Notifier`：

   ```rust
   pub struct SmsNotifier { /* 配置 */ }
   impl Notifier for SmsNotifier {
       fn id(&self) -> &'static str { "sms" }
       fn send(&self, alert: &Alert) -> anyhow::Result<()> {
           // alert.title / alert.body / alert.kind / alert.platform / alert.star
           Ok(())
       }
   }
   ```

   `send` 返回 `Err` 时发件箱会自动重试，不用自己写重试。

2. 在 `notify/mod.rs` 的 `Dispatcher::build` 里加一个分支：`"sms" => v.push(Box::new(SmsNotifier { .. }))`。
3. 在 `config.rs` 的 `KNOWN_NOTIFIERS` 里加上 `"sms"`；如果需要账号、手机号之类的配置，在 `NotifyCfg` 里加字段（带 `#[serde(default)]`，老配置文件才读得进来）。
4. 设置页（`crates/app/ui/settings.slint` 的「推送渠道」一节，和 `main.rs` 里的 `settings_to_ui` / `settings_from_ui`）加一个开关。

新差评会给每个启用的渠道各建一条发件箱记录，一个渠道失败不影响其他渠道。

## 不做的事

- 不调用回复、申诉等任何写接口。
- 不自动登录、不拖滑块、不读取你浏览器里的 Cookie。
- 只看登录账号名下的门店。

## 许可

MIT，见 `LICENSE`。本项目与美团、大众点评没有关联；请只用于你自己有权访问的门店后台，并遵守平台的服务协议。
