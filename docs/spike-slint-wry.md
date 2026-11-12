# 第 0 步原型结论：Slint + wry 能用

日期：2026-11-12　原型代码：`spike/slint-wry/`（一次性代码，不进正式程序）

## 结论

**走 Slint + wry，不需要退回 Tauri 2。** spec 第 7 节的五项验收全部通过。

| # | 验收项 | 结果 |
|---|---|---|
| ① | WebView 嵌进 Slint 窗口，显示和交互正常 | ✅ `build_as_child` 嵌成子窗口，缩放时 `set_bounds` 跟着调整 |
| ② | 人工登录（含滑块） | ✅ 在原型窗口里手动拖滑块登录成功 |
| ③ | 重启进程后仍是登录状态 | ✅ 登录态存在 `%APPDATA%\review-monitor\webview`，原型和正式程序共用这个目录，正式程序启动直接是已登录 |
| ④ | 页面内同源调用两个列表接口返回 200 | ✅ 用评价页 iframe 自己的 `XMLHttpRequest` 发请求，H5guard 会自动带上 `mtgsig` |
| ⑤ | WebView 隐藏后还能取数 | ✅ 切到别的标签页（WebView 隐藏）能取数；关掉主窗口缩到托盘后也能，见正式程序的日志（每分钟一轮） |

## 踩到的点

- **请求要从 iframe 里发。** 外层页面的 `XMLHttpRequest` 不会被签名，所以 `poller.js` 每次都拿 `iframe[src*="/emis/evaluation/poi"]` 的 `contentWindow.XMLHttpRequest`。
- **页面刚加载时 iframe 还没出来。** 第一轮检查延后 6 秒；拿不到 iframe 时等 10 秒重试，如果当前已经在评价页，就每连续 3 次才重新载入一次，免得一直刷新。
- **`getUserInfo` 的返回是 `{"user":{...}}`**，不是常见的 `{"data":...}`，判断登录要兼容两种写法。
- **新窗口请求**（后台里点了会弹新窗口的链接）用 `with_new_window_req_handler` 拦下来，在同一个 WebView 里打开。
- WebView2 的 `WebContext` 需要活得和 WebView 一样久，正式程序里直接 `Box::leak`。

## 和 spec 的差异

spec 第 10 节写的是「后台窗口」单独一个窗口。实际做成了**主窗口里的「后台页面」标签**：WebView 叠在内容区上，只在这个标签可见时显示。好处是只有一个窗口要管，点差评卡片直接切过去。
