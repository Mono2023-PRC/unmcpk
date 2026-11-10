# unmcpk

网易 MCPK 容器（`vanilla.mcp` / `vanilla_old.mcp` 这类热更新包）的解包、还原与反编译工具。

一个 **纯 Rust 单二进制**：读容器 → 逐层解码 → 还原成 CPython 2.7 `.pyc` → 按原始源码路径建树 →
用内置的 Python 2.7 反编译器还原 `.py`，并附一份可跨版本 diff 的 `manifest.json`。全程无人工干预，
也不需要任何外部程序。

## 特性

- **自包含**：内置 py2.7 反编译器（`src/pydec/`），没有 pycdc 之类的第三方二进制依赖，也不需要装 python。
- **自动化**：一条命令从 `.mcp` 直接得到可读源码树，不需要手工挑 key、拆层、修 pyc 或挑 opcode 表。
- **通用**：层变换、尾部填充长度、opcode 表全部按实测判定，不写死版本；新旧两个容器用同一份二进制跑通。
- **快**：`vanilla.mcp`（26.9 MB / 4354 个文件）全量解码 + 反编译约 **12 秒**（12 线程，含写盘）。
- **容错**：未知层变换、未知 tag、未知 opcode 表只记录进 manifest，不中断；结构无法建模的模块整体回退成
  「注释 + 原因 + `pass`」，绝不输出半截源码；损坏的容器给出明确错误而非崩溃。
- **可回归**：manifest 逐文件记录命中的层、消费字节数、尾部长度、code 对象数、诱饵数、`co_filename`、
  输出路径与反编译状态，两个版本的产物可以直接对比。
- **不破坏代码**：网易塞进每个模块的诱饵 code 对象（字节码是噪声）被识别后桩化成 `return None`，
  真实代码原样保留；需要原样时用 `--keep-decoys`。

## 快速开始

```bash
cargo build --release

# 容器元信息（不解码）
./target/release/unmcpk.exe info vanilla.mcp

# 全量：pyc + 反编译源码 + manifest
./target/release/unmcpk.exe unpack vanilla.mcp -o unpacked

# 冒烟测试
./target/release/unmcpk.exe unpack vanilla.mcp -o /tmp/smoke --limit 50
```

仓库里的 `bin/unmcpk.exe` 是构建产物，可直接运行，不需要先 `cargo build`。

## 反编译后端

| 后端 | 说明 |
| --- | --- |
| `native` | **默认**，内置的 Rust 实现。读回刚写出的 `.pyc`，解析成 code 对象后重建源码。无外部依赖，随二进制一起走。 |
| `pycdc` | 可选对照后端。把 `pycdc.exe` 放在 exe 同级、`bin/` 或 `PATH` 上即被自动找到，也可用 `UNMCPK_PYCDC` 指定；工具会一次进程处理一批文件。它更快，但相对导入会丢掉相对点。 |
| `uncompyle6` | 可选对照后端。任意装了 `uncompyle6` 的 python 3.8+ 解释器，用 `--python` / `UNMCPK_PYTHON` 指定；工具用常驻 worker 池，避免每文件重启解释器。已在 `uncompyle6 3.9.3` + CPython 3.11 上实测可用（`--limit 20` 全成功）。 |
| `none` | 只产出 `.pyc`。 |

`--decompiler auto`（默认）使用内置实现；显式写 `pycdc` / `uncompyle6` / `none` 可切换。

## 输出结构

```
unpacked/
  pyc/<原始源码路径>.pyc      CPython 2.7 pyc（8 字节头，magic 03 F3 0D 0A）
  src/<原始源码路径>.py       反编译源码；失败时写一行注释说明原因
  raw/<dirId>_<fileId>.bin    仅 --raw：解码后的 payload
  manifest.json               逐文件报告
```

`manifest.json` 每个条目：`dir_id` / `file_id` / `raw_size` / `layer` / `payload_size` / `marshal_size` /
`trailer` / `trailer_marker` / `code_objects` / `decoys` / `opcode_type` / `co_filename` / `pyc` / `source` /
`decompiled` / `error`；顶层汇总层分布、尾部分布、opcode 表、未知 opcode 与各阶段耗时。

## 选项

| 选项 | 作用 |
| --- | --- |
| `-o, --output <DIR>` | 输出目录（默认 `unpacked`） |
| `-j, --jobs <N>` | 并行度（默认 CPU 核数） |
| `--limit <N>` | 只处理前 N 个文件，用于冒烟 |
| `--no-pyc` / `--no-source` | 只写源码 / 只写 pyc |
| `--raw` | 额外保存解码后的 payload |
| `--decompiler <auto\|native\|pycdc\|uncompyle6\|none>` | 反编译后端 |
| `--python <PATH>` | uncompyle6 使用的解释器 |
| `--keep-decoys` | 保留诱饵对象的噪声字节码（会被反编译器拒绝） |
| `--old-compat` | 复现历史工具对诱饵的处理方式（删常量 + 砍 9 字节），会破坏含自更新校验的模块 |
| `--no-text-fix` | 关闭 GBK docstring 乱码的启发式修复 |
| `--verify` | 解包后立即复核：重读每个 pyc、校验 magic、重新解析、核对对象数与 `co_filename`，不一致就报错退出 |
| `-q, --quiet` | 不打印进度 |

## 实测

同一台机器（12 线程）：

| 容器 | 大小 | 目录 / 文件 | 文件成功 | code 对象 | 诱饵 | 模块级重建 | 函数级重建 | 耗时 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `vanilla.mcp` | 26.9 MB | 505 / 4354 | 4353 + 1 资产 | 91157 | 4015 | 4353/4353（100%） | 91122/91157（99.96%） | ≈12 s |
| `vanilla_old.mcp` | 22.8 MB | 474 / 3904 | 3903 + 1 资产 | 85483 | 3570 | 3903/3903（100%） | 85447/85483（99.96%） | ≈10 s |

分阶段（`vanilla.mcp`）：解码 0.47 s、写盘 3.2 s、反编译 8.1 s。写盘耗时随磁盘、杀软和机器负载波动明显。

「模块级」按整个模块能否重建计；「函数级」按 code 对象逐个计——单个函数重建失败时只在原地留
「注释 + 原因 + 指令清单 + `pass`」，模块其余部分照常输出，所以函数级指标才是真实覆盖率。
统计命令：

```bash
grep -rh "could not rebuild this code object" unpacked/src --include=*.py | wc -l
```

模块内个别函数无法重建时，就地写成注释（原因 + 该函数的指令清单）+ `pass`，模块其余部分照常输出。
`vanilla.mcp` 上 91157 个 code 对象里 35 个（0.04%）以这种方式回退，`vanilla_old.mcp` 是 36 / 85483（0.04%）；
剩下的形态很零散：推导式元素里再嵌条件表达式、`try` 体内嵌 `while`/`with` 的清理序列、
闭包变量下标、以及 `break` 与 `else` 交织的循环尾部。
`cargo test --release` 41 项通过；加 `--verify` 时每个样本都会打印 `verify: ok`。

跨平台：除 Windows 上的构建外，`cargo check --target x86_64-unknown-linux-gnu --all-targets`
也能通过（源码里没有平台专有的系统调用；路径处理、进制与并发都只依赖 `std`），
在有对应链接器的机器上可直接产出 Linux 二进制。

## 注意事项

- 内置反编译器覆盖 CPython 2.7 的常用结构；两个样本的**所有模块**都能重建（模块级 100%），只有个别函数会退回成「注释 + 原因 + `pass`」，原因写在同一文件里，
  也可用 `examples/probe.exe <container> <fileId> <dirId>` 逐对象定位。
- 每个容器里各有 1 个条目不是 Python（`dirId 0x9ab49f06` / `fileId 0xba13e0bd`，两个容器里都有这一条）：
  解码后正好是 132 字节 = **0 字节 marshal 流 + 130 字节填充 + `FF FE` 尾标**。这 130 字节经卡方检验与
  均匀随机不可区分（χ²=252，自由度 255，字节直方图无偏），没有任何结构或可打印倾向，因此判断它是
  「空占位条目」——打包器把内容换成了随机填充，槽位仍保留。它按「资产」处理，不产 pyc；加 `--raw` 会把
  这 132 字节写到 `raw/<dirId>_<fileId>.bin`。它的原始文件名不可恢复：`fileId` 是文件名的单向哈希，
  而这条没有 code 对象能提供 `co_filename`。其余 4353 / 3903 个条目全部还原成 pyc。
- 诱饵对象的字节码本来就是噪声，无法反编译，只能桩化；桩化后模块其余部分才可读。
- 同名模块（不同目录但 `co_filename` 相同）会加 `__<dirId>_<fileId>` 后缀避免互相覆盖，全量下约 500 个。
- GBK docstring 修复是启发式的：只在一行里出现「本不该出现的希腊/西里尔区字符」时才尝试按 GBK 重解。
- `--old-compat` 仅为对齐历史产物，默认关闭。

## 诊断工具

`cargo build --release --examples` 会把它们编译到 `target/release/examples/`；参数里的
`<container>` 是 `.mcp` 路径，id 都是十六进制。

| 工具 | 用法 | 用途 |
| --- | --- | --- |
| `verify` | `verify <output-dir>` | 独立复核：重读每个 pyc，校验 magic、重新解析、核对对象数与 `co_filename`（等价于 `unpack --verify`） |
| `native` | `native <container> [limit]` | 只跑解码 + 内置反编译，打印模块级/函数级成功率与失败原因排行（`UNMCPK_PROGRESS=1` 打进度） |
| `one` | `one <container> <fileId> [dirId]` | 解单个模块，失败时递归列出每个嵌套 code 对象的原因 |
| `probe` | `probe <container> <fileId> <codeName> [dirId]` | 单个 code 对象的指令清单 + 重建结果 |
| `list` | `list <container> <fileId> [dirId]` | 逐对象打印 vars/names/flags 与指令清单 |
| `jump` | `jump <container> <fileId> <codeName> [dirId]` | 字节 hex + 每条指令的 offset/name/arg/绝对目标 |
| `scan` | `scan <container> [count] [--op <byte>]` | 按 opcode 在全库里搜样例 |
| `rt` | `rt <container> <fileId>` / `rt --pyc <file.pyc>` | 对比「直接反编译」与「pyc 往返后反编译」，隔离解码与发射的 bug |
| `dbg` | `dbg <container> <fileId>` | 打印某文件每个 code 对象的概览 |

`UNMCPK_TRACE=1` 会打开反编译器的逐步跟踪（`suite dispatch`、`stack[n]`、`eval_range`、
`loop head`、`terminate at` 等），排查个别函数时用，输出很长，建议重定向到文件。

## 分支

- `master`（当前）：纯 Rust 实现，反编译由 `src/pydec/` 完成，无外部依赖。
- `pycdc`：最初的版本，反编译交给外部的 `pycdc.exe`。只想看容器/字节码还原、或想用 pycdc 输出对照时切过去。

## 目录

```
src/mcpk.rs        容器解析
src/rotor.rs       6-rotor 变换（Wichmann-Hill 种子、逐 id 密钥调度）
src/payload.rs     层候选、尾部判定、StringID
src/nemarshal/     网易 marshal 解析器 + 校验/桩化 + opcode 表
src/pyc.rs         标准 py2.7 pyc 发射
src/pathmap.rs     co_filename → 相对路径、冲突消解
src/pydec/         内置 py2.7 反编译器（反汇编 / 栈机 / 控制流 / 发射）
src/decompile.rs   后端选择（native / pycdc / uncompyle6 / none）、乱码修复
src/pipeline.rs    三阶段流水线（解码 → 写盘 → 反编译）+ manifest
src/main.rs        CLI
examples/          诊断工具（见下）
docs/FORMAT.md     容器与 marshal 格式参考
LICENSE            MIT
bin/unmcpk.exe     构建产物
```

本项目以 MIT 许可证发布，见 [LICENSE](LICENSE)。
