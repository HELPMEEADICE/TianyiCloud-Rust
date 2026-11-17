# Headless CLI 使用说明

天翼云盘客户端除了 Slint 桌面界面，还提供一套**完全不依赖图形环境**的命令行接口，面向：

* Linux 服务器 / SSH 终端（无 DISPLAY、无 Wayland、无桌面会话）；
* Docker 容器与最小化镜像；
* 自动化脚本（shell / Python / 任意语言）；
* AI Agent（Codex、Claude Code、OpenCode 等）：用稳定的 JSON 协议驱动，不依赖交互。

设计上有两条硬约束，贯穿全文：

1. **GUI 与 CLI 共用同一套业务实现**——会话、路径解析、文件操作、上传下载全部在
   `tianyi-core` 里，两边只是不同的「壳」。因此同一个账号、同一份配置、同一个
   `tasks.json` 在两个前端之间完全互通。
2. **不伪造能力**——没有常驻守护进程，就没有「后台传输」。CLI 的传输是前台传输，
   进程活着才在传；这一点在 [传输与任务](#传输与任务) 里逐条说明。

---

## 1. 构建

项目拆成四个 crate：

| crate | 作用 | 依赖图形库 |
| --- | --- | --- |
| `tianyi-core` | 协议、会话、路径、传输、任务表（共享） | 否 |
| `tianyi-cli` | 命令行实现（clap / rustyline / rpassword） | 否 |
| `tianyi-gui` | Slint 桌面界面 + 控制器 | 是 |
| `tianyi-app` | 启动器：先决定模式，再转交 | 否 |

```bash
# 默认构建：GUI + CLI（桌面用户拿到的是同一个可执行文件）
cargo build --release -p tianyi-app

# 纯 CLI / Headless 构建：依赖树里没有 slint、winit、rfd、arboard
cargo build --release -p tianyi-app --no-default-features --features cli

# 纯 GUI 构建（不需要 CLI）
cargo build --release -p tianyi-app --no-default-features --features gui
```

想自己确认「纯 CLI 构建真的没有图形依赖」：

```bash
cargo tree -p tianyi-app --no-default-features --features cli | grep -E 'slint|winit|arboard|rfd' || echo "没有图形依赖"
```

ARM64 Linux（在 x86 主机上交叉编译，或直接在 arm64 机器上构建）：

```bash
rustup target add aarch64-unknown-linux-gnu
cargo build --release -p tianyi-app --no-default-features --features cli \
    --target aarch64-unknown-linux-gnu
```

纯 CLI 产物只依赖系统 libc 与 OpenSSL 无关的 rustls，可直接放进 `scratch` / `alpine`
这类最小镜像（glibc 版放进 `debian:stable-slim` 即可）。

---

## 2. 运行模式

启动器只做一件事：**在加载任何图形后端之前**决定这次是 GUI 还是 CLI。

| 调用方式 | 行为 |
| --- | --- |
| `tianyi-app` | 打开图形界面（与历史版本完全一致） |
| `tianyi-app -c` + stdin 是终端 | 进入交互式 Shell（REPL） |
| `tianyi-app -c` + stdin 不是终端 | 直接报用法错误并退出（**不会**挂住等输入） |
| `tianyi-app -c <子命令>` | 执行一次，输出结果，退出 |
| `tianyi-app -c --json <子命令>` | 同上，但 stdout 只有一条 JSON |
| `tianyi-app -c --jsonl` | NDJSON 会话：一行请求一行响应 |
| `tianyi-app <子命令>` | 省略 `-c` 也按 CLI 处理（已知子命令名） |
| `tianyi-app --help` / `--version` | 命令行帮助 / 版本 |

判定依据是**明确的 CLI 信号**：`-c` / `--cli`、CLI 专有全局参数、或已知子命令名。
其它未知参数一律维持 GUI（macOS 从 Finder 启动残留的 `-psn_...` 不会被误当成 CLI）。

纯 CLI 构建（`--no-default-features --features cli`）不带 `-c` 启动时：

```text
本构建没有编译图形界面（缺少 gui feature）。
这个可执行文件只提供命令行，请加 -c 使用，例如：
  tianyi-app -c status
```
退出码 2（参数错误）。它**不会**崩溃，也不会假装成功。

---

## 3. 全局参数

| 参数 | 说明 |
| --- | --- |
| `-c, --cli` | 进入命令行模式（其余参数都在这个模式下解释） |
| `--json` | 输出 JSON 信封（stdout 只有结果） |
| `--jsonl, --ndjson` | NDJSON 事件流模式；与 `--json` 互斥 |
| `--non-interactive` | 禁止一切人工输入；需要确认的操作必须显式 `--yes` |
| `-y, --yes` | 对破坏性操作直接确认 |
| `--timeout <SECS>` | 单条命令的总超时（秒） |
| `--config-dir <DIR>` | 使用指定配置目录（默认与 GUI 同一个） |
| `--account <USER>` | 本次调用使用哪个已保存账号 |
| `--space <personal\|family>` | 本次调用使用哪个云空间 |
| `-q, --quiet` | 不打印进度与诊断（stderr 静默） |

---

## 4. 命令参考

### 4.1 账号

```bash
tianyi-app -c login                        # 交互式密码登录（密码不回显）
tianyi-app -c login password --username USER --password-stdin
tianyi-app -c login qr                     # 终端里画二维码，用天翼云盘 App 扫码
tianyi-app -c login qr --qr-file q.svg     # 同时把二维码写成矢量图
tianyi-app -c logout                       # 只是退出当前会话，已保存账号仍可切回
tianyi-app -c account list
tianyi-app -c account use 13800138000
tianyi-app -c account current
tianyi-app -c status
```

密码来源（四选一，**都不经过 argv**）：

| 参数 | 说明 |
| --- | --- |
| （不给） | stdin 是终端时隐藏回显输入；非终端环境直接报错 |
| `--password-stdin` | 从 stdin 读第一行 |
| `--password-file <PATH>` | 从文件读（去掉行尾换行，其余原样） |
| `--password-fd <N>` | 从已打开的文件描述符读（仅 Unix） |

验证码：第一次登录若命中风控，会返回 `CAPTCHA_REQUIRED`，同时把图片**原样落盘**：

```bash
tianyi-app -c --json login password --username USER --password-stdin
# stdout:
# {"success":false,"data":null,"error":{"code":"CAPTCHA_REQUIRED", "message":"...",
#  "retryable":true,"details":{"captcha_path":"/home/u/.config/tianyi-cloud-rust/captcha-1234.png",
#  "hint":"查看图片后，用 --captcha <code> 重新执行 login"}}}

# 看图，然后把验证码一起传回去
tianyi-app -c login password --username USER --password-stdin \
    --captcha ABCD --captcha-out /tmp/captcha.png
```

二维码：终端里用半块字符绘制（等宽字体即可扫），stdout 只留结果。
`--qr-file` 按扩展名决定格式（`.svg` 矢量图 / 其它为文本画法）。

登录态持久化在 `accounts.json`（与 GUI 同一份）。进程退出后再执行命令**不需要重新登录**；
会话 key 失效时会自动用 refresh token 续期，并把新 token 回写账号表。

### 4.2 文件

```bash
tianyi-app -c ls /                       # 列根目录
tianyi-app -c ls /Documents --long       # 带大小与修改时间
tianyi-app -c ls /Documents --all        # 不分页，列全部
tianyi-app -c ls /Documents --page 2 --limit 50
tianyi-app -c info /Documents/file.zip
tianyi-app -c search "报告"

tianyi-app -c mkdir /Documents/NewFolder
tianyi-app -c rename /old.txt new.txt
tianyi-app -c mv /file.txt /Documents/
tianyi-app -c cp /file.txt /Backup/
tianyi-app -c rm /file.txt --yes
tianyi-app -c rm /Folder --recursive --yes

tianyi-app -c link /Documents/file.zip     # 下载直链
tianyi-app -c quota                        # 容量
tianyi-app -c space                        # 显示当前空间
tianyi-app -c space family                 # 切到家庭云
```

路径规则：

* 以 `/` 开头是绝对路径，否则相对**当前工作目录**（一次性命令的当前目录是根目录，
  REPL 里是 `cd` 之后的位置）；
* `.` 忽略、`..` 折叠到上一级，**越过根目录直接报错**（而不是静默停在根目录——
  否则 `rm ../../x` 会变成 `rm /x`）；
* 文件名里的空格、中文、emoji、全角标点都原样保留，含空格的路径加引号即可。

安全约定：

* `rm` 先把**所有**路径解析成对象，任何一个解析失败就整体放弃，一个字节都不删；
* 删除目录必须显式 `--recursive`；
* 非交互环境（或 `--non-interactive`）必须显式 `--yes`，否则报 `USAGE` 并以 2 退出；
* 回答 `n` 视为取消，退出码 7（`CANCELED`）。

家庭云：`space family` 之后，服务端不支持的接口会明确报 `UNSUPPORTED` 并提示切回个人云，
**不会**静默改到别的空间上执行。

### 4.3 上传 / 下载

```bash
tianyi-app -c upload ./test.zip /                  # 目标目录在第 2 个位置参数
tianyi-app -c upload ./folder /Backup --recursive
tianyi-app -c upload ./a.zip --to /Backup          # 显式写目标目录

tianyi-app -c download /test.zip ./                # 落到 ./test.zip
tianyi-app -c download /test.zip --to ./backup/    # 落到 ./backup/test.zip
tianyi-app -c download /Documents ./backup --recursive
```

`upload` 的位置参数规则（可预测、无歧义）：给了 `--to` 就全部按源处理；否则**至少两个位置
参数且最后一个以 `/` 开头、本地磁盘上又不存在同路径**时，最后一个当目标目录；其余情况全部
按源处理，目标目录用当前目录。

`download` 的本地目标可以是目录（自动拼上云端文件名，文件名字符会被消毒）；不给则用配置里的
下载目录。目录下载必须显式 `--recursive`。

### 4.4 任务

```bash
tianyi-app -c tasks                       # 全部任务
tianyi-app -c tasks --filter active
tianyi-app -c task info 12
tianyi-app -c task pause 12
tianyi-app -c task resume 12              # 真的开始传（前台）
tianyi-app -c task resume 12 --no-wait    # 只改回「等待中」
tianyi-app -c task cancel 12
tianyi-app -c task remove 12
```

---

## 5. JSON 输出协议

`--json` 模式下 stdout **只有一条** JSON，日志、进度、诊断全部走 stderr。

成功：

```json
{
  "success": true,
  "data": { "authenticated": true },
  "error": null
}
```

失败：

```json
{
  "success": false,
  "data": null,
  "error": {
    "code": "AUTH_EXPIRED",
    "message": "Authentication expired",
    "retryable": true
  }
}
```

顶层永远是这三个键：`success` / `data` / `error`。带额外上下文时在 `error` 里多一个
`details` 对象（例如验证码路径、已删除项清单），没有就不出现。

### 5.1 错误码

| code | 含义 | 退出码 | 可重试 |
| --- | --- | --- | --- |
| `USAGE` | 参数错误 / 未知命令 / 缺值 / 非交互缺 `--yes` | 2 | 否 |
| `CONFIG` | 配置不可用或无法解析 | 6 | 否 |
| `AUTH_REQUIRED` | 尚未登录 | 3 | 否 |
| `AUTH_EXPIRED` | 登录态失效（刷新后可能恢复） | 3 | 是 |
| `LOGIN_FAILED` | 账号密码错误 / 二维码过期 | 3 | 否 |
| `CAPTCHA_REQUIRED` | 需要验证码 | 3 | 是 |
| `NETWORK` | 连接失败 / 超时 / 5xx / 429 | 4 | 是 |
| `REMOTE_API` | 天翼云返回业务错误 | 5 | 否 |
| `NOT_FOUND` | 路径 / 账号 / 任务不存在 | 8 | 否 |
| `CONFLICT` | 状态冲突（任务状态不允许该操作） | 5 | 是 |
| `PERMISSION_DENIED` | 401 / 403 | 5 | 否 |
| `LOCAL_IO` | 本地读写失败 | 6 | 否 |
| `BUSY` | 预留给「传输状态被另一进程占用」 | 9 | 是 |
| `CANCELED` | 被 Ctrl+C 中断 / 用户取消 | 7 | 否 |
| `TIMEOUT` | 超过 `--timeout` | 4 | 是 |
| `UNSUPPORTED` | 当前空间或本构建不支持 | 2 | 否 |
| `INTERNAL` | 其它（含内部 bug） | 1 | 否 |

`BUSY` 目前没有触发路径（并发写入靠跨进程锁与按任务合并解决），保留给将来的守护进程模式。

### 5.2 数据字段约定

* 大小：一律**整数字节**（`size`、`total`、`used`、`free`）；
* 时间：`modified` / `created` 是 ISO-8601（`2024-05-06T07:08:09`，秒级、不带时区——
  天翼云返回的就是服务器本地时间，这里不臆造偏移量），原始字符串另存 `*_raw`；
  任务时间戳 `created_at` 是带 `Z` 的 UTC（本地生成，可以确定时区）；
* 百分比：一律**千分比整数**（`progress_permille`、`used_permille`），不出现浮点数，
  因此永远不会有 `NaN` / `Infinity`；
* 目录项的 `type` 是 `dir` 或 `file`；目录的 `md5` 是 `null`，不是空串；
* 不输出任何 token / session key / 密码。

### 5.3 分页

列表类命令默认每页 200 条，`--page N` / `--limit N` 控制，`--all` 取全部：

```json
{
  "path": "/Documents",
  "folder_id": "-11",
  "space": "personal",
  "items": [ { "id": "123", "name": "a.zip", "type": "file", "size": 2048,
               "modified": "2024-05-06T07:08:09", "modified_raw": "2024-05-06 07:08:09",
               "created": "2024-05-01T00:00:00", "created_raw": "2024-05-01 00:00:00",
               "md5": "9f2c...", "parent_id": "-11" } ],
  "pagination": { "page": 1, "limit": 200, "total": 1, "returned": 1, "has_more": false }
}
```

---

## 6. NDJSON 会话协议（长连接 / Agent）

`tianyi-app -c --jsonl` 从 stdin 逐行读请求，逐条写响应到 stdout。

请求（三选一）：

```json
{"id": 1, "command": "ls / --long"}
{"id": 2, "argv": ["status"]}
{"id": 3, "cmd": ["account", "list"]}
```

也接受纯文本行（等价于 REPL 里敲的一行，`id` 记 0）。

响应：**一条请求恰好一条响应**，`id` 原样回填：

```json
{"success":true,"data":{...},"error":null,"id":1}
{"success":false,"data":null,"error":{"code":"NOT_FOUND","message":"...","retryable":false},"id":2}
```

消息边界与注意事项：

* 一条响应严格一行（内部不含换行），可以用任何行分隔的读取器解析；
* `event` 类型的中间消息目前**不产生**：传输进度不会污染协议（进度只在人类模式下写 stderr）；
  因此「一行请求 ↔ 一行响应」始终成立；
* 会话内 `cd` / `pwd` 会改变后续请求的当前目录（与 REPL 一致）；
* 收到 `exit` / `quit` 或 stdin EOF 时结束，退出码 0。

---

## 7. 交互式 Shell

`tianyi-app -c` 且 stdin 是终端时进入 REPL：

```text
TianyiCloud-Rust CLI 0.1.0
输入 help 查看命令，exit 退出。
tianyi> status
tianyi> ls /
tianyi:/Documents> cd /Documents
tianyi:/Documents> ls
tianyi:/Documents> download example.zip ./
tianyi:/Documents> exit
```

* 提示符显示当前云端目录；`cd ` 会真的去列一次目录，不存在就当场报错，不会把工作目录
  改到一个坏路径上；
* 命令与一次性模式**完全同构**（同一套 clap 解析），`--json` 也可以照用；
* 支持历史（`~/.config/tianyi-cloud-rust/cli-history.txt`，最多 2000 行）与命令名补全；
* `Ctrl+C` 中断当前操作（第一次暂停传输并保留断点，第二次取消），`Ctrl+D` 正常退出；
* `help` / `history` / `clear` / `pwd` / `cd` / `exit` / `quit` 是会话内命令。

---

## 8. 传输与任务

### 8.1 诚实的前台语义

本项目**没有**常驻守护进程，也没有跨进程传输。因此：

| 模式 | 行为 |
| --- | --- |
| `--wait`（默认） | **前台传输**：进程活着就在传，退出即停 |
| `--no-wait` | 只把任务写进 `tasks.json`（`pending`），**没有任何进程去传**；要用 `task resume` 启动 |

进程绝不会在退出时声称「任务仍在后台运行」。

### 8.2 断点续传与中断

* 下载沿用原有的 `.part` 机制：暂停后 `.part` 依然是**合法前缀**，下次从断点继续；
* 上传靠本地文件重来（服务端未提交的分片由服务端回收）；
* `Ctrl+C` 第一次 → 请求**暂停**（保留断点），第二次 → 取消；
* `--timeout` 到点视为超时错误（退出码 4），不会留下 `Running` 幽灵任务。

### 8.3 任务表与跨进程一致性

`tasks.json` 的格式与 GUI **完全一致**（`Vec<Task>`，字段不变）。两个前端共用同一个文件：

* 写入是原子的（临时文件 → fsync → rename），任何时刻都不会读到半截 JSON；
* 每次写入前抢 `tasks.json.lock` 排他锁，抢不到就跳过本次写入（下一次进度回调再试），
  绝不硬写；
* 写入前会合并磁盘上的任务表：**我动过的任务以我为准，我没动过的以磁盘为准**，
  所以两边都不会把对方刚写的进度或删除结果顶掉；
* 损坏的 `tasks.json` 会被改名留档（`.corrupt-*`），不会被静默丢弃。

已知限制：两个进程同时改**同一条**任务时，仍然是「后写者赢」。需要绝对互斥时，请只在一个
前端里操作传输。

---

## 9. 与 GUI 共存

* 配置目录、`config.json`、`accounts.json`、`tasks.json` 完全共用；
* 配置写入改成**读-改-写**（持锁重读后再改单个字段），所以 GUI 改主题、CLI 改
  `last_account` 不会互相覆盖；
* 账号表同理：命令行刷新 token 后回写，GUI 那边的会话仍然能用；
* 唯一需要人工注意的场景：两边同时传同一条任务（见 8.3）。

---

## 10. 典型场景

### 10.1 SSH / 无桌面的 Linux 服务器

```bash
# 一次性装好
cargo build --release -p tianyi-app --no-default-features --features cli
install -m755 target/release/tianyi-app /usr/local/bin/

# 登录（二维码在终端里画出来，手机扫）
tianyi-app -c login qr

# 日常
tianyi-app -c ls /Backup --long
tianyi-app -c upload ./nightly.tar.gz /Backup --yes
tianyi-app -c quota
```

### 10.2 Docker

```dockerfile
FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p tianyi-app --no-default-features --features cli

FROM debian:stable-slim
COPY --from=build /src/target/release/tianyi-app /usr/local/bin/tianyi-app
# 配置目录挂在卷上，登录态才能持久化
ENV XDG_CONFIG_HOME=/data
VOLUME ["/data"]
ENTRYPOINT ["tianyi-app"]
CMD ["-c", "status"]
```

```bash
docker run --rm -it -v "$PWD/data:/data" \
    -e XDG_CONFIG_HOME=/data tianyi-cli -c login qr
docker run --rm -v "$PWD/data:/data" -e XDG_CONFIG_HOME=/data \
    -v "$PWD/upload:/upload" tianyi-cli -c upload /upload/a.zip /Backup --yes
```

### 10.3 AI Agent / 脚本调用

```bash
# 1) 探测登录状态
state=$(tianyi-app -c --json --non-interactive status)
echo "$state" | jq -r '.data.authenticated'

# 2) 列目录并取第一个文件 id
tianyi-app -c --json --non-interactive ls /Documents --limit 10 \
  | jq -r '.data.items[] | select(.type=="file") | .id'

# 3) 下载
tianyi-app -c --json --non-interactive download /Documents/a.zip ./out/ --to ./out/
```

Python：

```python
import json, subprocess

def run(*args):
    p = subprocess.run(["tianyi-app", "-c", "--json", "--non-interactive", *args],
                       capture_output=True, text=True, timeout=600)
    payload = json.loads(p.stdout)          # stdout 永远是合法 JSON
    if not payload["success"]:
        err = payload["error"]
        raise RuntimeError(f"{err['code']}: {err['message']} (retryable={err['retryable']})")
    return payload["data"]

print(run("status"))
for item in run("ls", "/", "--all")["items"]:
    print(item["type"], item["size"], item["name"])
```

常驻会话（一次进程，多次请求）：

```python
import json, subprocess
p = subprocess.Popen(["tianyi-app", "-c", "--jsonl"],
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
def call(i, command):
    p.stdin.write(json.dumps({"id": i, "command": command}) + "\n"); p.stdin.flush()
    while True:
        line = json.loads(p.stdout.readline())
        if line.get("id") == i:
            return line
print(call(1, "status"))
print(call(2, "ls /"))
p.stdin.write('{"id":3,"command":"exit"}\n'); p.stdin.flush()
```

---

## 11. 已知限制

1. **没有后台守护进程**：`--no-wait` 只是登记任务，不会自己跑（见 8.1）。
2. **`task pause` 对其它进程无效**：状态会被记为暂停，但真正在传的那个进程不会因此停下来；
   要停请在那个进程里操作（GUI 里点暂停，或在那条 CLI 命令上按 Ctrl+C）。
3. **同一任务被两个进程同时修改**时后写者赢（见 8.3）。
4. **家庭云**只开放个人云支持的那部分接口，其余操作明确报 `UNSUPPORTED`。
5. **JSON 模式下 NDJSON 不产出进度事件**：只保证「一行请求一行响应」。
6. GUI 的实时进度推送依赖进程内的定时器；CLI 不依赖它，进度直接写 stderr。

---

## 12. 测试与 CI

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked

# 纯 CLI 构建必须能过（无图形依赖）
cargo check -p tianyi-app --no-default-features --features cli --locked
cargo build -p tianyi-app --no-default-features --features cli --locked
```

命令行的端到端测试在 `crates/tianyi-app/tests/cli_e2e.rs`：直接驱动真实二进制，每个用例
都在自己的临时 `XDG_CONFIG_HOME` 里跑，并清掉 `DISPLAY` / `WAYLAND_DISPLAY`，
覆盖退出码、stdout 纯净度、无 TTY 不挂住、JSON 协议形状、凭据不泄露。

CI（`.github/workflows/ci.yml`）里额外有：

* `cli-headless` 作业：`--no-default-features --features cli` 的 x86_64 与 aarch64 编译；
* `cli-e2e` 作业：在**不装任何 GUI 系统库**的 runner 上跑 CLI 集成测试；
* Release 流程额外产出 CLI-only 产物（见 `.github/workflows/build.yml`）。
