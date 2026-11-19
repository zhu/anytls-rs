# Rust 客户端待提交 Issues

这些问题属于仓库自带的 Rust 客户端；用户的部署组合是 Rust **服务端**配合 Shadowrocket / dae。
这里保存逐条 Issue 正文和复现依据，供后续客户端维护使用。

**状态：16 份草稿，尚未创建 GitHub Issues。** 创建时 GitHub 返回 `410: Issues has been disabled in this repository`。
本分支不修改生产代码，也不作为服务端版本的发布 tag 目标。

审计源码固定为 [`9424f3d`](https://github.com/zhu/anytls-rs/tree/9424f3d83836649590a6ae452b2528e00d8b0822)，
Go 参考实现固定为 [`fd6167a`](https://github.com/anytls/anytls-go/tree/fd6167acd6d73b9fa3e607659951847fbc9e6c50)。
每份正文标明严重程度建议、影响范围、不可变源码链接、证据边界、修复建议与验收条件。
13 项有完整的有界运行复现，1 项包含运行与静态证据，2 项仅有静态证据；这些数量不代表16个独立的端到端互通测试。

## 清单

| 优先级 | 问题 | 证据 |
| --- | --- | --- |
| P1 | [[client] 认证分成多次 TLS 写入，无法连接 anytls-go 参考服务端](client-auth-fragmentation.md) | 真实 Go 服务端，对照试验 |
| P1 | [[client] CLI 默认接受任意证书，并跳过 TLS 握手签名校验](client-tls-verification.md) | 有界运行复现；已保存探针 |
| P1 | [[client] HTTP/SOCKS TCP 中继在单向 EOF 后不关闭 Stream，连接和任务持续悬挂](client-frontends-eof.md) | 有界运行复现；已保存探针 |
| P2 | [[client] 缺少完整关闭 API，Drop Client/SessionPool 后后台会话继续持有连接](client-shutdown-ownership.md) | 有界运行复现；已保存探针 |
| P2 | [[client/http] CONNECT 头后预读到的隧道字节被静默丢弃](http-connect-buffered-data.md) | 有界运行复现；已保存探针 |
| P2 | [[client] UpdatePaddingScheme 在默认初始化后永远失败，后续会话也不继承配置](client-padding-update.md) | 有界运行复现；已保存探针 |
| P2 | [[client] 会话 padding 计数从 0 开始，重复使用认证策略并整体错位](client-padding-counter.md) | 有界运行复现；已保存探针 |
| P2 | [[client/pool] 正在承载流的会话仍被当作 idle 重用，偏离协议的空闲复用策略](client-idle-pool-policy.md) | 有界运行复现；已保存探针 |
| P2 | [[client] 未协商 v2 也强制等待 SYNACK，连接 v1 服务端必然超时](client-version1-fallback.md) | 有界运行复现；已保存探针 |
| P2 | [[client/socks5] 忽略 CMD，把 BIND、UDP_ASSOCIATE 和未知命令都当作 TCP CONNECT](client-socks-command-dispatch.md) | 有界运行复现；已保存探针 |
| P2 | [[client] TCP/TLS/认证阶段没有整体截止时间，SYNACK timeout 无法限制前置握手](client-connect-deadline.md) | 有界运行复现；已保存探针 |
| P3 | [[client] Settings 的 client 版本固定为 anytls-rs/0.1.0，与发布版本不符](client-version-advertisement.md) | 静态确认，未声明运行复现 |
| P1 | [[client/udp] 等到 SYNACK 才发送 UoT 初始请求，与 Go 参考服务端形成循环等待](client-udp-synack-ordering.md) | 静态确认，未声明运行复现 |
| P2 | [[client/udp] 把合法零长度 UDP 响应当作 EOF，提前终止整个代理](client-udp-empty-response.md) | 已运行临时探针；探针未保留 |
| P2 | [[client/udp] 全局 last_peer 将延迟响应交给最后发包者，多个本地 peer 串包](client-udp-last-peer-routing.md) | 已运行临时探针；探针未保留 |
| P2 | [[client/udp] 建立失败和中继退出不关闭 Stream，调用方也无法管理 UDP 代理生命周期](client-udp-lifecycle.md) | 混合：退出清理已复现；bind/所有权为静态 |

## 复现与参考实现的限定

保存的 [复现程序与操作说明](repro/README.md) 覆盖前12份草稿中的11项运行证据；版本字段是静态确认。
UDP 的两个运行探针及生命周期的一个退出路径曾执行，但临时探针没有保留，正文明确列出步骤和结果。
UoT 请求与 SYNACK 次序是双方源码证明的循环等待；认证问题会更早阻塞 stock Rust → Go，不把这两项证据混为一谈。

Go 示例 CLI 自身也默认跳过证书验证，Go padding factory 也有全局持有设计；相关正文没有把参考实现表述为这些方面的完整生产方案。
固定远端目标是 UDP helper 的接口设计，串 peer 的问题是接受多个本地发送者却错误地把响应交给最后一个发送者。

## 开启 Issues 后提交

在已授权的 GitHub 环境中开启该仓库的 Issues，然后运行本分支提供的导入程序：

```sh
gh repo edit zhu/anytls-rs --enable-issues
python3 docs/client-issues/publish.py
python3 docs/client-issues/publish.py --apply
```

第一条 Python 命令只预览标题；`--apply` 才逐条调用 GitHub 创建。程序固定写入 `zhu/anytls-rs`，
检测现有标题或审计标识避免重复，某条失败会立即停止；已创建的 Issue 不会被覆盖。
完整结构化内容保存在 [issues.json](issues.json)。
