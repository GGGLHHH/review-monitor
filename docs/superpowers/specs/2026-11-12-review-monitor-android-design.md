# 差评监控 安卓版（apk）设计

日期：2026-11-12　状态：**草稿，等用户审**　前置：一期 Windows exe 已完成（提交 8e52ffa）

## 1. 目标与边界

和电脑版一样：手机上登录一次经营宝子账号，之后每 3 分钟查一次美团和点评的评价，有新差评就弹手机系统通知（点通知打开应用）。重点是后台保活和不重复通知。

- **纯本地，没有后端**：不建服务器、不用厂商推送或 FCM，通知全部由手机本机发出。
- 两端各自独立：各自登录、各自轮询、各自去重，互不通信。用户确认同一个子账号两处同时登录不会互踢。
- 只读：和电脑版相同，只调用查询接口。
- 只给用户自己用：不上架应用商店，自己签名，侧载安装。
- 目标手机：魅族（Flyme）。先在模拟器上开发，真机后面再测。

不做：iOS；多账号；云同步；回复评价。

## 2. 技术路线（用户已选）

**原生 Kotlin（Jetpack Compose 界面）+ 复用 Rust 写的 `rm-core`。**

| 层 | 用什么 | 说明 |
|---|---|---|
| 界面 | Kotlin + Compose | 只有一页：状态、保持后台运行（含保活记录）、参数。用户定：只要「登录、配置参数、定时通知」，不做差评列表/已读；后台 WebView 平时不显示，只在需要登录时整页弹出，登录成功自动收起 |
| 常驻 | 前台服务（Foreground Service） | 通知栏常驻一条「差评监控运行中」，每 3 分钟醒一次 |
| 后台页面 | Android 系统 WebView | 打开经营宝评价页，注入**同一份** `web/poller.js` |
| 业务逻辑 | `rm-core`（Rust，编译成 `.so`） | 通过 UniFFI 生成 Kotlin 绑定，Poller / Store / 规则 / 调度全复用 |
| 通知 | Android NotificationManager | 在 Kotlin 里实现 `Notifier`，通过 UniFFI 回调接口交给 rm-core |

为什么不用 Tauri / Slint：前台服务和服务里常驻的 WebView 无论如何都得用 Kotlin 写，界面又很小，跨平台框架省不了多少事，反而要多接一层。

### 2.1 复用 rm-core 的方式

- `rm-core` 现在通过 `Page` trait（`call / goto_reviews / user_visible`）和页面打交道，通过 `Notifier` trait 发通知。安卓端用 Kotlin 实现这两个接口，经 UniFFI 的回调接口（callback interface）传进 Rust。
- 新增一个很薄的 crate `crates/ffi`：对外只暴露一个 `Engine`（包着 Poller + Store），方法有 `tick()`、`on_message(json)`、`check_now()`、`list(only_bad)`、`set_read(key, read)`、`config()/save_config()`。电脑版不受影响。
- 编译：`cargo-ndk` 出 `arm64-v8a`（真机）和 `x86_64`（模拟器）两份 `.so`。rusqlite 用 bundled，交叉编译时由 NDK 的 clang 编 SQLite。
- `web/poller.js` 原样复用：安卓 WebView 用 `addJavascriptInterface(obj, "ipc")` 暴露 `window.ipc.postMessage(String)`，和 wry 的接口名一模一样，所以脚本一行都不用改。

## 3. 最大的风险：后台能不能一直跑（先做原型验证）

手机和电脑最大的不同，是系统会冻结后台应用。下面这几件事现在谁也不知道，**必须先做原型验证**，不通的话整个方案要改：

| # | 要验证的事 | 怎么算通过 |
|---|---|---|
| ① | 安卓 WebView 能打开经营宝网页版评价页（设置电脑版 UA），手动拖滑块能登录 | 能看到评价列表 |
| ② | 在 iframe 里同源发 XHR，H5guard 照样自动签名 | 两个列表接口返回 200、有数据 |
| ③ | 杀掉应用重开，登录状态还在 | 不用重新登录 |
| ④ | 应用退到后台、熄屏，前台服务里的 WebView 还能按时取数 | 模拟器上用 `adb shell dumpsys deviceidle force-idle` 强制进入 Doze，30 分钟内每轮都成功 |
| ⑤ | 美团风控会不会因为是模拟器、新设备而拦截 | 不弹额外验证；如果弹了，记录是什么验证 |

原型只写最少的 Kotlin：一个 Activity（显示 WebView，用来登录）+ 一个前台服务（持有同一个 WebView，每 1 分钟调用一次 `__rm.whoami / fetchList`，结果打到 logcat）。先不接 rm-core。**原型代码是一次性的**，验证完就扔，结论写进 `docs/spike-android-webview.md`。

### 3.1 已知的系统限制和对策（待原型确认）

- **前台服务类型**：Android 15 起，`dataSync` 类型的前台服务每天最多只能跑 6 小时，不能用。改用 `specialUse` 类型（侧载安装不经过应用商店审核，可以用）。
- **Doze 省电模式**：熄屏一段时间后，系统会暂停应用的网络访问。对策：引导用户把应用加入「忽略电池优化」白名单；每轮检查时短暂持有 partial wake lock，查完就释放。
- **WebView 不在屏幕上**：服务持有 WebView，用 application context 创建；打开界面时把同一个 WebView 挂到 Activity 上，关界面时再摘下来。摘下来以后 JS 还跑不跑，正是验证项 ④。
- **魅族 Flyme**：通常还要在「手机管家 → 后台管理」里允许后台运行，并关闭「智能省电」对本应用的限制。应用里做一个引导页，一步步带用户去设置。
- **通知权限**：Android 13 起要在运行时申请通知权限。
- **熄屏计时**：Rust 的 `Instant` 在安卓上走 CLOCK_MONOTONIC，深睡时不走。`rm-ffi` 改用 CLOCK_BOOTTIME 推算时间；每轮检查前用 `setExactAndAllowWhileIdle` 闹钟叫醒（没有精确闹钟权限就退回不精确的），检查期间持有 partial wake lock，查完就放。

## 4. 功能对照（和电脑版一致的部分不重复写）

| 电脑版 | 安卓版 |
|---|---|
| 托盘常驻 | 前台服务 + 常驻通知（点它打开应用） |
| Windows 通知，点了用 `review-monitor://` 跳转 | 系统通知，点了打开应用（安卓版没有列表，通知正文里就是评价内容） |
| `%APPDATA%\review-monitor\` | 应用私有目录：`files/data.db`、`files/config.toml`；WebView 登录状态由系统 CookieManager 持久化 |
| 单实例互斥量 | 系统本来就只有一个实例 |
| 设置页 | 同样的配置项和校验（直接用 rm-core 的 `Config::validate`）；多一个「后台保活引导」 |

## 5. 工程结构

```
review-monitor/
├─ crates/core/        已有，不动（只在需要时加几个 pub 方法）
├─ crates/ffi/         新增：UniFFI 门面，编译成 Android 的 .so
├─ crates/app/         已有：Windows 版
├─ android/            新增：Gradle 工程（app 模块，Kotlin + Compose）
│  └─ app/src/main/assets/poller.js   构建时从 web/poller.js 复制过去
└─ spike/android-webview/   原型（一次性）
```

环境（2026-11-12 已装在本机）：Android SDK `%LOCALAPPDATA%\Android\Sdk`（platform 36、build-tools 36.1.0、NDK 30.0.16248370、platform-tools、模拟器 + Android 36 x86_64 镜像），JDK 21，Rust 的 `aarch64-linux-android` / `x86_64-linux-android` 编译目标。Gradle 由工程自带的 wrapper 下载。

## 6. 测试

- `rm-core` 已有 48 个单元测试，安卓端直接受益；`crates/ffi` 加少量测试，验证 JSON 进出。
- Kotlin 侧：Notifier、通知点击跳转、配置读写做 instrumented test，在模拟器上跑。
- 后台保活：在模拟器上用 `dumpsys deviceidle` 模拟 Doze，连续跑 2 小时，看日志里有没有漏掉的轮次。真机（魅族）到手后再跑一遍 24 小时。

## 7. 分步

0. **原型**（第 3 节五项）→ 结论文档。不通就停下来，和用户重新商量方案。
1. `crates/ffi` + cargo-ndk 构建脚本，在模拟器里能调用 `Engine.tick()`。
2. Gradle 工程：前台服务 + WebView + 注入 poller.js + IPC，接上 Engine，跑通「取数 → 入库」。
3. 通知 + 点通知跳转定位。
4. Compose 界面：单页（状态、保活引导与保活记录、参数）；登录页按需弹出。
5. 签名出 apk（`dist/review-monitor.apk`）+ 安装说明，模拟器上验收；真机验收放到拿到魅族手机以后。

## 8. 待确认

| 事项 | 什么时候 |
|---|---|
| 第 3 节五项能不能通过 | 原型 |
| 魅族 Flyme 的版本、对应的安卓版本 | 真机测试前问用户 |
| apk 的签名密钥放哪（只存本机，不进库） | 第 5 步 |
