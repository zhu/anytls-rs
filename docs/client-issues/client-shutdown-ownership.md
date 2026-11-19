# [client] 缺少完整关闭 API，Drop Client/SessionPool 后后台会话继续持有连接

<!-- anytls-client-audit: client-shutdown-ownership -->

## 范围与优先级

建议优先级：**P2**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。已在本地有界复现，未修改生产代码。

## 问题与影响

`Client` 没有关闭全部会话的 API。`stop_session_pool_cleanup()` 只停止清理定时器；`SessionPool::drop` 也只 abort 清理任务，没有关闭池内 Session。`Session::start_client` 另外启动持有 `Arc<Session>` 的接收、发送、心跳任务，因此调用方丢弃全部 Client 和 Session 引用后仍然无法释放连接。

定位：[src/client/client.rs:312-327](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/client.rs#L312-L327)；[src/client/session_pool.rs:324-343](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/session_pool.rs#L324-L343)；[src/session/session.rs:1242-1293](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/session/session.rs#L1242-L1293)；[src/session/session.rs:1295-1362](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/session/session.rs#L1295-L1362)。

嵌入式客户端或服务切换配置、反复创建/销毁 Client 时会积累后台任务和 socket。健康对端回复心跳会让这些“没有用户所有者”的 Session 持续存活。

## 已执行复现

1. 运行本地 Rust 服务端，以 `check_interval=50ms`、`idle_timeout=200ms`、`min_idle_sessions=1` 创建 Client。
2. 完整建立一个 TCP proxy stream，确认目标接受连接，然后正常 `Stream::close()` 并等待 active map 为空。
3. 保存 Session 的 `Weak`，drop Stream、Session、Client 的全部调用方强引用。
4. 等待 650ms（超过 3 个配置的 idle/heartbeat timeout）后，Weak 仍可 upgrade，Session `is_closed()==false`，strong_count 为 4。服务端回复心跳维持了它。
5. 仅通过 Weak 重新取得 Session 并显式 `close()` 后，Weak 才最终无法 upgrade。

Go Client 独立跟踪所有会话，`Close()` 会取消客户端并关闭它们：[proxy/session/client.go:175-198](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/proxy/session/client.go#L175-L198)。

## 预期修复与验收

- 提供明确的 async `Client::shutdown/close`，关闭并等待该 Client 拥有的所有活动和空闲会话、池清理任务以及客户端 worker。
- 定义 Drop 的取消语义；不能只停止清理任务却留下自持有的 Session。若允许外部 Session 在 Client 被 drop 后继续使用，也应提供不依赖调用方手动枚举 Session 的完整关闭途径。
- 测试重复创建/关闭、健康心跳、空闲及活动流、并发关闭；确认 task/FD 数回落且无持有环。

## 依赖更新后的复核

在服务端发布候选提交 [`c250994`](https://github.com/zhu/anytls-rs/tree/c250994c468a7e68d3aba8b2d45816e0a9d55f74) 更新依赖后，本项已再次运行并复现。客户端相关生产源码未改变；[补充输出与固定依赖](https://github.com/zhu/anytls-rs/blob/custom/client-issue-drafts/docs/client-issues/repro/VALIDATION_c250994.md)保留了本轮结果。
