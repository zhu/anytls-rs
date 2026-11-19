# [client/pool] 正在承载流的会话仍被当作 idle 重用，偏离协议的空闲复用策略

<!-- anytls-client-audit: client-idle-pool-policy -->

## 范围与优先级

建议优先级：**P2**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。已在本地有界复现，未修改生产代码。

## 问题与影响

`get_idle_session()` 只检查 Session 未关闭，既不检查活动 Stream，也不把选中的会话移出 idle 集合。`create_new_session()` 又会在打开第一个代理流前立即把会话加入该集合。后续并发请求通常持续复用同一条仍在使用的 TLS/TCP 连接。

定位：[src/client/session_pool.rs:89-124](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/session_pool.rs#L89-L124)；[src/client/client.rs:197-207](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/client.rs#L197-L207)；[src/client/client.rs:312-321](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/client.rs#L312-L321)。

这和协议要求的“只有空闲 Session 才被取出，Stream 完成后再归还，归还时设置 idle 起点”不同：[docs/protocol.md:157-173](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/docs/protocol.md#L157-L173)。参考代码取出时移除，并通过 stream dieHook 归还：[proxy/session/client.go:109-148](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/proxy/session/client.go#L109-L148)。

影响是并发流相互共享单个 TCP 的排队/丢包延迟和连接失败范围；某流接收侧阻塞还可能阻挡同 Session 后续帧处理。这是可观察的调度/隔离策略偏差，不应误称为服务端不接受多 Stream 或所有场景必然互联失败。

## 已执行复现

把一个 Session 放入 pool，`open_stream()` 后保持该 Stream 活动；连续两次 `get_idle_session()` 都返回 Some，`Arc::ptr_eq` 为 true，返回的 Session `has_active_streams()` 也为 true。

## 预期修复与验收

- 将“拥有所有 Session 的注册表”和“可被复用的 idle 集合”分开；原有 FD 泄漏修复的所有权不能因为简单移除条目而丢失。
- 按 Stream 生命周期取出/归还 idle Session，归还时记录真实 idle 起点；缺少 idle 时建立新 Session。
- 测试并发活动流不会被当作 idle、顺序请求复用最近空闲会话、关闭/错误时正确归还或丢弃、清理保留较新的空闲会话，以及不存在原有 orphan Session/FD 回归。
