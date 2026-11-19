# [client] 未协商 v2 也强制等待 SYNACK，连接 v1 服务端必然超时

<!-- anytls-client-audit: client-version1-fallback -->

## 范围与优先级

建议优先级：**P2**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。已在本地有界复现，未修改生产代码。

## 问题与影响

`create_proxy_stream()` 在每次发送地址后都等待 SYNACK，超时固定30秒，没有检查服务器是否通过 ServerSettings 确认 v2。符合 v1 协议的服务端不会发送 SYNACK，所以有效的代理数据即使已收到，创建流仍不能成功。

定位：[src/client/client.rs:145-193](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/client.rs#L145-L193)；客户端 peer version 初始值：[src/session/session.rs:163-166](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/session/session.rs#L163-L166)。后台主动心跳也没有按对端版本启用：[src/session/session.rs:1295-1340](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/session/session.rs#L1295-L1340)。

协议明确规定：v1 服务端不发 ServerSettings；v2 客户端未收到 v2 协商时应按 v1 工作，只在双方都支持 v2 时启用 SYNACK/主动心跳：[docs/protocol.md:230-245](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/docs/protocol.md#L230-L245)。Go 在已知 peerVersion>=2 的复用流上才启动相应 watcher：[proxy/session/session.go:135-161](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/proxy/session/session.go#L135-L161)。

## 已执行复现

用本地 TLS 协议对端读取认证、Settings、SYN 和目标 PSH，保持连接开放，随后按 v1 发送同 Stream 的 `PSH("v1-ready")`，不发送 v2 的 ServerSettings/SYNACK。

Rust 实际在30秒后返回 `Protocol error: SYNACK timeout after 30s`。外层32秒截止时间保证测试有界。此项是受控 v1 线协议对端测试，未声称运行了历史 v1 二进制。

## 预期修复与验收

- 根据协商结果决定 v2 行为；首个流在尚未知对端版本时须可与 v1 协同。
- v2 连接仍保留握手错误传播及有界卡死检测，不能用取消所有 ACK 检查来规避问题。
- 测试无 ServerSettings 的 v1 对端、明确 v2 对端、v2 错误 ACK、复用流 ACK 超时和心跳版本门控。
- 与 UDP 的“等待 SYNACK 才发送 UoT request”属于不同依赖问题，应分别验证。
