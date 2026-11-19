# [client/udp] 把合法零长度 UDP 响应当作 EOF，提前终止整个代理

<!-- anytls-client-audit: client-udp-empty-response -->

## 范围与优先级

建议优先级：**P2**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。已通过只使用回环 UDP 和内存 Stream 的有界临时探针运行复现；未修改生产代码。

## 问题与影响

连接模式的 UoT 以两字节长度加 payload 表示每个 UDP 数据报；长度0表示合法空数据报。`read_udp_packet()` 对它返回空 Vec，但 `stream_to_udp()` 随即根据 `payload.is_empty()` 退出循环。结果是空数据报没有交付，本地 UDP socket 被释放，整个代理不再转发后续流量。

定位：长度0被解码为空 Vec，[src/client/udp_client.rs:272-283](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/udp_client.rs#L272-L283)；空 payload 被当作关闭，[src/client/udp_client.rs:238-240](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/udp_client.rs#L238-L240)；任一方向完成就退出代理，[src/client/udp_client.rs:142-157](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/udp_client.rs#L142-L157)。

## 已执行复现

1. 创建真实回环 UDP socket，启动原样 `udp_proxy_loop()`；Stream 的输入/输出用内存 channel 接到探针，输入 channel 始终保持开放。
2. 从本地应用 socket 发出一个非空数据报，等待其被编码并送入 Stream，保证代理已记录本地 peer。
3. 向 Stream 输入完整两字节 `[0x00, 0x00]`，表示一个空 UoT 数据报。
4. 1秒有界等待内，代理 future 成功返回，本地应用未收到任何 UDP 数据报；重新 bind 原地址成功，确认代理 socket 已释放。
5. 同时观察到 `Stream::is_closed()==false`，输出 channel 没有 FIN sentinel。此清理问题另由 UDP 生命周期 issue 跟踪。

实际探针输出：

```text
CONFIRMED: valid empty response stopped proxy; no datagram, no FIN, Stream still open
```

临时探针测试名为 `client::udp_client::udp_audit_probes::audit_empty_response_exits_without_fin`，本次执行通过后已移除，未提交或保留为当前复现项目的一部分。因此 `evidence_paths` 为空；现有客户端审计 README/可执行探针不应被描述成已经包含这项 UDP 复现。

## 预期修复与验收

- 将合法空数据报作为零长度 UDP 数据报交付，并继续读取下一包。
- 用独立状态表示包边界 EOF；空 Vec 不得兼作 EOF 标记。
- 用结构化 I/O 错误区分完整数据报之后的关闭与长度字段/payload 被截断，替换当前对错误字符串中 `UnexpectedEof`/`EOF` 的猜测。
- 回归按顺序传送非空、空、非空三包，断言三包均交付、连接仍可继续使用；另测头/体截断和真正边界 EOF。
