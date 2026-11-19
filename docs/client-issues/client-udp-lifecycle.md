# [client/udp] 建立失败和中继退出不关闭 Stream，调用方也无法管理 UDP 代理生命周期

<!-- anytls-client-audit: client-udp-lifecycle -->

## 范围与优先级

建议优先级：**P2**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。本项为混合证据：local bind 失败及 task 所有权问题来自静态审查；空包导致 relay 退出却未关闭 Stream 已有界运行复现。

## 问题与影响

`create_udp_proxy()` 先创建远端 Stream 并发送 UoT 初始请求，随后才 bind 本地 UDP socket。若本地 bind 失败，函数经 `?` 返回，却没有关闭已经创建的 Stream。

成功建立后，函数丢弃 `tokio::spawn` 返回的 JoinHandle，只返回本地 `SocketAddr`；调用方没有对应的显式关闭/取消入口。`udp_proxy_loop()` 在任一方向正常或错误退出时，也没有关闭 Stream 的 guard。

定位：远端初始化先于 local bind，[src/client/udp_client.rs:41-79](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/udp_client.rs#L41-L79)；任务被 detach 且只返回地址，[src/client/udp_client.rs:84-99](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/udp_client.rs#L84-L99)；中继退出不关闭 Stream，[src/client/udp_client.rs:142-157](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/udp_client.rs#L142-L157)。

Session 的注册表持有 `Arc<Stream>`：[src/session/session.rs:919-934](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/session/session.rs#L919-L934)，所以错误路径只 drop 局部引用不会自动关闭远端关联。`Stream::close()` 才会标记关闭并入队 FIN：[src/session/stream.rs:120-149](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/session/stream.rs#L120-L149)。

## 已执行部分：relay 退出后没有 FIN

本次空包临时探针先从本地 UDP 发送一包以确定 peer，再向仍保持开放的 Stream 输入 `[0, 0]`。代理 loop 在1秒内退出、socket 可被重新 bind，但 `stream.is_closed()` 仍为 false，输出 channel 也没有 FIN sentinel。这直接验证了一个 relay 退出路径的清理缺失。

该运行结果与“零长度 UDP 响应被当作 EOF”来自同一探针；不是另一次独立的 bind 失败或取消试验。临时探针已经移除，`evidence_paths` 为空；现有 README 不包含其可执行入口。

## 静态部分：local bind 失败的复现步骤

以下步骤尚未在本次审计中运行，依据上述必经错误路径确认：

1. 先用本地 UDP socket 占用待绑定地址。
2. 连接可正常完成认证/提前 ACK 的服务器，调用 `create_udp_proxy()` 并传入已占用地址。
3. 等远端 Stream 已建立并收到 UoT 请求后，local bind 会报地址被占用。
4. 检查 Session 的活动 Stream 与服务端 FIN：当前源码从 bind 错误直接返回，没有执行 Stream close；应将这一判断固化为回归，而不能称作本次已采集的 bind 失败运行日志。

## 预期修复与验收

- 可行时先 bind 本地 socket，再分配远端 Stream；仍为后续每个失败/退出/取消路径设置无条件 Stream close guard。
- 提供拥有地址、取消信号和任务完成状态的代理 handle，或等价的显式 shutdown API，使调用方可以终止代理并等待清理。
- 测试 local bind 失败后没有活动 Stream/远端关联遗留；显式 shutdown 释放本地 socket、发送 FIN、结束任务。
- 测试中继错误和外层取消清理，并保留已成功入队数据的顺序。
- 本项只描述 UDP helper 自己的建立和任务所有权；可与更广泛的客户端中继/Client shutdown issue 合并，但应保留 UDP bind 错误及 relay 退出两个单独验收点。
