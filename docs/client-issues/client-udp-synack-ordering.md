# [client/udp] 等到 SYNACK 才发送 UoT 初始请求，与 Go 参考服务端形成循环等待

<!-- anytls-client-audit: client-udp-synack-ordering -->

## 范围与优先级

建议优先级：**P1**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。下述结论由双方源码执行顺序证明，未声称完成 stock Rust 客户端到 Go 服务端的 UDP 运行复现。

## 问题与影响

`create_udp_proxy()` 先等待 `create_proxy_stream()` 返回，随后才发送 UoT v2 的 `isConnect + 目标地址`。但 `create_proxy_stream()` 发送 magic SOCKS 地址后，需要等服务器 SYNACK 才会返回，超时固定为 **30 秒**。

Go 参考服务端则先 `uot.ReadRequest(conn)` 读完 UoT 初始请求，再报告握手成功并发送 SYNACK。认证已通过且进入这一 v2 路径时，客户端等 ACK，服务端等客户端尚未发送的请求，两端不能继续。

定位：[src/client/udp_client.rs:41-63](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/udp_client.rs#L41-L63)；[src/client/client.rs:145-167](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/client.rs#L145-L167)。固定30秒超时及其错误路径：[src/client/client.rs:158-193](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/client.rs#L158-L193)。

Go 在读取 UoT 请求之后才报告成功：[cmd/server/outbound_tcp.go:32-46](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/cmd/server/outbound_tcp.go#L32-L46)；报告成功对 v2 对端发送 SYNACK：[proxy/session/stream.go:153-159](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/proxy/session/stream.go#L153-L159)。

## 证据边界与 Rust 服务端差异

这是独立的静态执行顺序证明。另一个认证分包问题已经单独用真实 Go 服务端复现：当前 stock Rust 客户端可能在认证阶段就被断开，尚未进入这里的 UDP 建立流程。因此不能把本项表述成已运行 stock Rust-to-Go UDP 后观察到30秒超时；认证修复后，这里的循环等待仍需单独解决。

当前 Rust 服务端在调用 UoT handler 前就发 SYNACK：[src/server/handler.rs:95-105](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/server/handler.rs#L95-L105)。因此 Rust-to-Rust 能建立 UDP 并不能排除这个客户端互联问题，也不需要为此改变当前服务端的提前 ACK 行为。

## 复现步骤（待固化为运行回归）

1. 使用能通过认证并协商 v2 的受控对端，或先单独修复认证后连接上述提交的 Go 参考服务端。
2. 对端收到 SYN 及 `sp.v2.udp-over-tcp.arpa` 的 SOCKS 地址后，按 Go 顺序等待完整 UoT 初始请求，再发送成功 SYNACK。
3. 调用 `create_udp_proxy(local_addr, target_addr)`。按现有源码，Rust 停在 `create_proxy_stream()` 的 SYNACK 等待，UoT 请求发送语句尚未执行；Go/受控对端停在请求读取。
4. 固定30秒 ACK 定时器到期后，Rust 会走 `SYNACK timeout after 30s` 错误分支。此处是根据固定代码路径推导的预期复现结果，不是本次已经采集的运行输出。

## 预期修复与验收

- 将“创建流并发送 magic 地址”和“等待 SYNACK”分开，使 UDP 能在等待 ACK 前发送完整 UoT 初始请求。
- 用按 Go 顺序延后 ACK 的对端验证建立成功，不依赖服务器提前 ACK，也不通过延长30秒超时规避问题。
- 保留 v2 失败 ACK 的传播与建流清理，覆盖初始请求跨多次 Stream/PSH 到达的情况。
- 认证问题解决后，再以真实 Go 参考服务端验证 UDP 往返；区分认证修复和 UoT 建流顺序修复的证据。
