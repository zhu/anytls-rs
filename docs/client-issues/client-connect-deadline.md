# [client] TCP/TLS/认证阶段没有整体截止时间，SYNACK timeout 无法限制前置握手

<!-- anytls-client-audit: client-connect-deadline -->

## 范围与优先级

建议优先级：**P2**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。已在本地有界复现，未修改生产代码。

## 问题与影响

客户端只在发送目标地址后设置30秒 SYNACK timeout；此前的 DNS/TCP connect、TLS connect、认证发送都直接 await。一个接受 TCP 后不完成 TLS 握手的端点可使 CLI 的代理请求持续等待，30秒 SYNACK 定时器根本尚未创建。

定位：[src/client/client.rs:214-223](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/client.rs#L214-L223)；[src/client/client.rs:271-291](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/client.rs#L271-L291)；[src/client/client.rs:158-161](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/client.rs#L158-L161)。库调用方可自行用外层 timeout 包裹调用，但捆绑的 HTTP/SOCKS 前端没有这样做。

## 已执行复现

本地 TCP listener 接受连接并读到 TLS ClientHello 的首字节 `0x16`，随后保持 socket 打开而不发送任何 TLS 数据。31秒后 Rust `create_proxy_stream()` task 仍 pending；测试主动 abort 后结束。

这证明现有30秒 SYNACK timeout不覆盖 TLS 阶段；“可无限等待”由 await 路径没有截止时间的静态代码支持，不能仅把31秒观察窗口说成无穷时长试验。此处也不声称 Go 示例默认提供完整总 deadline；它的会话创建 API 允许调用方传入 context，而 CLI 示例本身也有可改进之处。

## 预期修复与验收

- 给创建代理流提供可配置的总连接/握手 deadline 或调用方取消机制，覆盖 DNS、TCP、TLS、认证及目标建立；明确阶段性和总体预算的关系。
- CLI 前端实际使用该预算，并返回有用错误且清理未完成连接。
- 覆盖 TCP 接受后 TLS 沉默、认证写阻塞、DNS/多地址连接失败、调用方取消；测试超时后没有任务、Stream 或 socket 遗留。
