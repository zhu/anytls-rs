# [client] HTTP/SOCKS TCP 中继在单向 EOF 后不关闭 Stream，连接和任务持续悬挂

<!-- anytls-client-audit: client-frontends-eof -->

## 范围与优先级

建议优先级：**P1**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。已在本地有界复现，未修改生产代码。

## 问题与影响

HTTP 和 SOCKS5 前端分别 spawn 两个转发任务，然后 `join!` 等待两个任务自然结束。任一方向 EOF / 读写错误仅退出自己的循环，没有调用 `Stream::close()`，没有协调关闭另一方向，也没有为外层取消管理子任务。

定位：[src/client/http_proxy.rs:104-154](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/http_proxy.rs#L104-L154)；[src/client/socks5.rs:163-174](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/socks5.rs#L163-L174)；[src/client/socks5.rs:237-340](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/socks5.rs#L237-L340)。

本地应用关闭、远端目标保持空闲时，服务器收不到 FIN，目标 TCP 连接和 AnyTLS Stream 持续保留，池认为会话仍 active。反过来，服务器 FIN 已被共享 Session 正确处理后，前端的另一个 `tokio::io::split` half 仍持有 socket，本地应用可能一直等不到 EOF。

## 已执行复现

使用真实回环 Rust 服务端、Rust `Client` 和本地 TCP 目标。

- HTTP CONNECT 与 SOCKS5 CONNECT 两条路径都先发送 `alive`，确认目标实际收到，然后完全 drop 本地连接。500ms 后目标 read 仍未返回 EOF；`session.has_active_streams()` 为 true；前端 handler 尚未结束。
- HTTP 反向场景：目标先关闭；等待 `session.has_active_streams()` 变为 false，证明服务端 FIN 和共享 Session 清理已完成。本地应用 read 仍在 500ms 内不返回 EOF，handler 仍等待另一方向。
- 最后显式 `session.close()` 才完成资源清理。500ms 是探针的有界观察窗口；代码上没有在此后自动结束这些中继的单向 EOF 处理路径。

复现调用的是快照中原样复制的私有 handler，唯一追加为测试入口 wrapper；TLS/Client/Session/服务端全使用仓库库代码。这些是 Rust **客户端前端**的问题，不是本次已修好的服务端 handler 或共享 FIN 清理回归。

Go 示例在中继退出时 defer 关闭代理流：[cmd/client/inbound.go:49-57](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/cmd/client/inbound.go#L49-L57)。

## 预期修复与验收

- 按现有 AnyTLS FIN 的 full-close 语义协调两个转发方向，确保退出、错误及外层取消都会关闭 Stream 并终止所拥有的任务。
- 保留已经入队的有效尾部数据，并给无法完成的排空设置明确上界。
- 不要在取消其中一个流时截断正在写入共享 TLS 会话的帧；已有 transport write guard 会关闭整个 Session，修复应避免误伤其他流。
- 对 HTTP/SOCKS 分别测试本地 EOF、远端 FIN、目标读写失败、外层取消；断言对端 EOF、Stream map 清空、handler 结束及任务数不增长。
