# cc-same 安全审计与加固说明

审计对象：上游 `songkeys/cc-same` commit `8e12cbe`（0.1.10）。
范围：CLI（`crates/cli`）、核心库（`crates/core`）、GUI app（`crates/app`）、打包脚本、GitHub Actions、两份 `Cargo.lock`。

## 结论

- **CLI + 核心库没有网络代码**。依赖只有 anyhow、chrono、clap、dirs、reflink-copy、serde、serde_json、tempfile、libc/windows-sys。`cargo audit` 扫描 97 个 crate，没有已知漏洞。
- 文件操作整体很谨慎：不跟随 symlink、原子写入、`0600`/`0700` 权限、写入前做 CoW 快照、删除的文件进回收站而不是真删。
- 主要风险集中在三处：
  1. **配置会"静默失效"**：短 ID、默认自动加入新账号。这是用户已经实测踩到的问题。
  2. **账号切换会搬动登录凭证**（`switch`/`add`/`remove`）。
  3. **GUI 自动更新的信任链太弱**。

本 fork 已修复下表中所有标为"已修复"的问题。

## 发现与处理

| # | 严重度 | 问题 | 处理 |
|---|---|---|---|
| 1 | 高 | `exclude` 只做精确字符串匹配。`doctor` 显示的 8 位短 ID 写进去照样保存，但什么都匹配不上，那个组织会被照常同步，而且没有任何提示 | **已修复**：`config` 会把唯一前缀展开成完整 UUID，有歧义或匹配不到就直接报错；配置里只要有一条不是完整 ID，sync 和后台 agent 就**拒绝同步**（fail closed），`doctor` 也会给出警告 |
| 2 | 高 | 只有黑名单，没有白名单。`autoJoinNew` 默认开启：以后登录的任何账号或组织（比如个人账号）都会自动收到全部公司会话，也会把自己的会话带进来 | **已修复**：新增 `config --only <account/org>` 白名单，名单之外的一律不参与，包括以后才出现的账号和组织，首次同步也一样；`autoJoinNew` 默认改为关闭 |
| 3 | 中 | `switch`/`add`/`remove` 会搬动 Claude Desktop 的 Cookies、Local Storage、IndexedDB 和 `oauth:tokenCache*`；保存下来的登录态从不自动清理 | **已修复**：这些命令默认**不再编译进 CLI**，需要 `--features account-switching` 才会出现；`forget` 增加了 UUID 校验，防止 `accounts.json` 被改后 `remove_dir_all` 越出目录 |
| 4 | 高（GUI） | 自动更新只信任 GitHub API 返回的 digest。没有 digest 时直接跳过校验；macOS 只比对 `TeamIdentifier` 字符串，自签证书就能伪造；Windows/Linux 完全没有签名校验；而且在校验签名**之前**就运行了下载的程序（`--version`） | **已修复**：默认关闭更新检查和自动更新；没有 digest 就拒绝；强制 HTTPS，重定向只允许跳到 GitHub 的几个域名；下载地址必须以本项目 releases 开头；改为先验签名再运行；macOS 改用锚定 Apple Developer ID 证书链的 `codesign -R` 要求，并加 `spctl --assess`；"查看发布页"只会打开 `https://github.com/` 开头的链接（发布说明里的 Markdown 链接未改） |
| 5 | 中（GUI） | GUI 依赖的 `rustls 0.23.45` 有 4 个公告（RUSTSEC-2026-0346/0347/0348/0349） | **已修复**：升级到 0.23.46 |
| 6 | 中 | 发布流水线：Actions 用标签而不是 commit 固定；发布构建会复用缓存（可能被投毒）；签名证书和公证密钥在 `cargo build` 之前就导入了，构建脚本和 proc-macro 运行期间都能拿到；构建没有加 `--locked` | **已修复**：所有 Actions 固定到 commit SHA；发布任务去掉缓存；改为先构建、再导入签名身份；所有构建都加 `--locked`；发布时附带 `SHA256SUMS`；CI 新增 `cargo audit` 任务 |
| 7 | 低 | 日志文件用默认权限 `0644` 创建（macOS 上在 `~/Library/Logs`，不在私有目录里），内容包含账号/组织 ID 和文件名；launchd 的 agent 日志也是这样 | **已修复**：日志以 `0600` + `O_NOFOLLOW` 打开，旧日志也会被收紧权限；agent 日志预先以 `0600` 创建 |
| 8 | 低 | LaunchAgent plist、systemd unit、autostart 文件用 `fs::write` 写入，会跟随 symlink | **已修复**：改为原子写入，并拒绝写穿 symlink |
| 9 | 低 | `clone_file` 在目标已存在（包括 symlink）时会写穿 | **已修复**：目标已存在就拒绝 |
| 10 | 低 | `restore <id>` 的核心函数不校验 id，可以带路径分隔符；快照 manifest 用默认权限写入；heartbeat 文件读取没有大小上限；`notify-send` 的参数没有用 `--` 隔开 | **已修复** |
| 10a | 中 | 后台进程启动时如果 `config.json` 读不出来，会退回默认配置，`only` 白名单随之失效；只靠 `state.json` 兜底，状态文件再丢失的话，本机所有组织都会被同步 | **已修复**：读不出配置时一律不同步，弹一次通知；配置修好后立即恢复同步 |
| 11 | 信息 | 快照（含永久保留的基线快照）和回收站里有会话元数据（标题、路径等）的副本，位于 `0700` 的私有目录 | 设计如此；卸载后如需清除，删除状态目录即可 |
| 12 | 信息 | `CC_SAME_FAKE_RUNNING` 等测试用环境变量在 release 构建中仍然生效 | 只有同一用户能设置，可以接受 |
| 13 | 信息 | GUI 依赖链里有几个"不再维护"的 crate（`paste`、`instant`、`rustybuzz` 等，都来自 GPUI） | 不影响 CLI；GUI 不建议在公司设备上使用 |
| 14 | 信息 | `logins::tests::a_failed_switch_puts_everything_back` 在 root 下运行会失败（`chmod 0555` 对 root 无效）。以普通用户运行，44 个测试全部通过 | 测试环境问题，不是代码缺陷 |

## 仍需知道的限制（不是缺陷）

- 侧边栏的分组、排序和置顶保存在各账号的服务器端设置里，无法同步。
- 被同步的是会话索引，也就是元数据：标题、cwd、模型、权限模式、关联 PR 等。转录本身本来就在 `~/.claude/projects` 中共享。
  - 一个组织能看到另一个组织会话的**元数据**。两边都是公司组织时，这正是想要的效果；如果涉及个人账号，务必用 `--only` 把它排除在外。
- 账号绑定的字段不会跨组织复制：连接器、Remote Control、已发布的 artifact、星标。

## 验证

- `cargo test --workspace --locked`：CLI 和核心库的所有测试都通过（包括新增的 3 个集成测试和 3 个单元测试），唯一的例外是第 14 条所说、只在 root 下失败的那个测试。
- `cargo clippy --all-targets -D warnings`：开启和关闭 `account-switching` 都没有警告。
- 端到端：用 3 个假组织跑 `scripts/secure-setup.sh`。被选中的 2 个组织互相同步，第 3 个完全没被碰到；后台 agent 会实时同步新会话；在 `exclude` 里写入 8 位短 ID 后，agent 拒绝同步并写日志说明原因。
