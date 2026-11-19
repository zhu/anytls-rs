# [client/udp] 全局 last_peer 将延迟响应交给最后发包者，多个本地 peer 串包

<!-- anytls-client-audit: client-udp-last-peer-routing -->

## 范围与优先级

建议优先级：**P2**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。已通过两个真实回环 UDP 应用 socket 和内存 Stream 的有界临时探针运行复现；未修改生产代码。

## 问题与影响

每个 UDP proxy 只有一个共享 `last_peer`。每次本地收到数据报都会覆盖它，所有远端返回数据又都发送到此刻的 `last_peer`。当两个本地 socket 交错发包，较早请求的延迟响应就可能交给另一个 socket，无法保证关联的正确性。

定位：共享状态，[src/client/udp_client.rs:139](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/udp_client.rs#L139)；每个输入包更新 peer，[src/client/udp_client.rs:175-187](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/udp_client.rs#L175-L187)；所有响应取最新 peer，[src/client/udp_client.rs:245-249](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/udp_client.rs#L245-L249)。

## 已执行复现

1. 启动原样 `udp_proxy_loop()`，使用真实回环 UDP socket 和可控内存 Stream。
2. 应用 socket A 发 payload `A`；等待探针在 Stream 输出端收到编码 `[0, 1, A]`，确保第一包已被处理。
3. 应用 socket B 发 payload `B`；同样等待输出 `[0, 1, B]`。
4. 向 Stream 注入延迟到达的 A 响应 `[0, 1, A]`。
5. B 实际收到 payload `A`；A 在有界观察窗口内没有收到数据。每一步均同步等待，不依赖请求发送的调度概率。

实际探针输出：

```text
CONFIRMED: A request, then B request, delayed A response delivered to B
```

临时探针测试名为 `client::udp_client::udp_audit_probes::audit_delayed_reply_goes_to_last_local_sender`，本次执行通过后已移除，未提交或保留为当前复现项目的一部分。因此 `evidence_paths` 为空；现有客户端审计 README/可执行探针不应被描述成已经包含这项 UDP 复现。

## 范围澄清

`create_udp_proxy(local_addr, target_addr)` 的公开接口本来就是一个固定远端 `SocketAddr`，仅支持该固定目标不是本项缺陷。这里的问题是多个本地发送者被接受后，没有隔离它们的关联。改用 UoT mode0 本身也不能从任意响应 payload 推断本地接收者；不能把它作为单独的充分修复。

## 预期修复与验收

- 若支持多个本地 peer，为每个本地 peer 建立和管理独立远端 UoT 关联。
- 若接口只支持一个本地 peer，明确固定/验证该 peer，拒绝其他来源，不能默默覆盖响应目标。
- 回归两个本地 socket 交错请求，主动延迟或调整远端响应顺序，确认响应不会被交给另一 peer。
- 若新增关联表，同时验证关闭/空闲清理和数量上界，避免把串包修成无界资源增长。
