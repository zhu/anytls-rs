# Custom 版本依赖审计

## 结果

2026-11-19 使用 `cargo-audit 0.23.0` 和 RustSec 数据库提交
[`472a4c9`](https://github.com/rustsec/advisory-db/tree/472a4c9a785030434f15d7da84964d60fbe789ab)
检查锁定依赖。更新前为 **18 条漏洞记录、9 条允许的提示**；更新后为
**0 条漏洞记录、3 条停止维护提示**，没有设置忽略的 advisory。
更新后的锁文件包含 265 个依赖包；数量包含开发依赖和平台专用依赖。

原始发现来自 [CI job 135347469212](https://github.com/zhu/anytls-rs/actions/runs/44460999178/job/135347469212)。
该 job 的 `cargo audit` 实际退出码是 1，但旧工作流使用了 `continue-on-error: true`，
所以 GitHub 显示成功。配套工作流修复移除此例外，并在 Release 验证阶段执行相同审计；
此后发现漏洞或审计执行失败都会阻止相应验证通过。

完整版本、advisory 标识和前后结果见 [DEPENDENCY_AUDIT.json](DEPENDENCY_AUDIT.json)。

## 依赖更新

| 依赖 | 原锁定版本 | 新锁定版本 | 对应的原漏洞记录数 |
| --- | --- | --- | --- |
| `aws-lc-sys`，经 `aws-lc-rs` | 0.32.3 | 0.47.0 | 5 |
| `bytes` | 1.10.1 | 1.12.2 | 1 |
| `crossbeam-epoch` | 0.9.18 | 0.9.21 | 1 |
| `idna` | 0.4.0 与 1.1.0 并存 | 仅保留 1.1.0 | 1 |
| `rustls` | 0.23.35 | 0.23.46 | 4 |
| `rustls-webpki` | 0.103.8 | 0.103.17 | 5 |
| `time` | 0.3.44 | 0.3.55 | 1 |

同步更新有兼容修复版本的 Tokio、rand、anyhow 和 rustix。构建继续使用提交的
`Cargo.lock` 与 `--locked`；没有为了获得绿色结果而过滤目标平台、降低严重程度门槛或忽略漏洞。

`trust-dns-resolver 0.23` 引入的 `idna 0.4` 没有同系列修补版本。
自定义 DNS 因此迁移到同一项目维护的 `hickory-resolver 0.26.3`，而不是停留在仍有
已知问题的旧 Hickory 系列。系统 DNS 路径仍使用 Tokio 的系统解析。
自定义解析保留服务器 IP/端口、UDP 与 TCP 回退；新的 loopback 回归覆盖非53端口、
A/AAAA 答案、UDP 截断后 TCP 长度前缀交换及缓存的目标端口隔离。

版本选择依据包括 [idna advisory](https://rustsec.org/advisories/RUSTSEC-2024-0421.html)、
[Rustls advisory 列表](https://rustsec.org/packages/rustls.html) 和
[Hickory 修复公告](https://rustsec.org/advisories/RUSTSEC-2026-0513.html)。

## 服务端适用性与剩余维护事项

18 条记录是锁文件匹配结果，不等于18个已经证明可从当前服务端远程触发的问题。
例如 Rustls 的 [Brotli 证书解压问题](https://rustsec.org/advisories/RUSTSEC-2026-0348.html)
要求启用非默认的 `brotli` 特性，服务端还需要请求客户端证书；当前服务端没有启用该特性，
也没有配置 mTLS。其他条目涉及客户端握手、特定密码算法、开发依赖或特定调用方式。
本次统一采用修补后的依赖，以免后续配置变化再次落入受影响版本。

以下停止维护提示仍保留在审计输出中，未隐藏或标成“已修复”：

| 依赖 | 提示 | 后续维护方向 |
| --- | --- | --- |
| `chrono 0.4.42` | [RUSTSEC-2026-0543](https://rustsec.org/advisories/RUSTSEC-2026-0543.html) | 计划替换时间格式化/证书日期相关调用，并保留日期边界回归 |
| `pin-utils 0.1.0` | [RUSTSEC-2026-0441](https://rustsec.org/advisories/RUSTSEC-2026-0441.html) | 随上游异步依赖迁移移除这一传递依赖 |
| `rustls-pemfile 2.2.0` | [RUSTSEC-2025-0134](https://rustsec.org/advisories/RUSTSEC-2025-0134.html) | 迁移到 `rustls-pki-types` 的 PEM 读取接口，保留证书/私钥格式测试 |

这些条目被数据库归类为维护提示，不是本次审计中的未修补漏洞记录。
“0条漏洞记录”只描述这一数据库快照对锁文件的检查，仍需与协议、资源生命周期和真实客户端测试一起判断发布质量。

## 重现

```sh
cargo install cargo-audit --version 0.23.0 --locked
cargo audit
cargo test --locked --test custom_dns
```

日常 CI 与 Release 获取当前 advisory 数据库；如果以后出现新的公告，结果可以随之改变。
需要严格重现上述数字时，将 `rustsec/advisory-db` 检出到文首固定提交，使用
`cargo audit --no-fetch --db /path/to/advisory-db`。
