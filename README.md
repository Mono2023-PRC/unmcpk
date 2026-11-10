# unmcpk

网易 MCPK 容器（`vanilla.mcp` / `vanilla_old.mcp` 这类热更新包）的解包、还原与反编译工具。

一个 Rust 单二进制：读容器 → 逐层解码 → 还原成 CPython 2.7 `.pyc` → 按原始源码路径建树 →
反编译成 `.py`，并附一份可跨版本 diff 的 `manifest.json`。全程无人工干预。

## 构建

```bash
cargo build --release            # 本工具本身
bash scripts/build_pycdc.sh      # 随仓库一起构建 pycdc 反编译器（写入 bin/）
```

`pycdc/` 是随仓库一起分发的反编译器源码（见下）。`scripts/build_pycdc.sh` 按顺序尝试
`$CXX`、`python -m ziglang c++`（`pip install ziglang`）、`g++`、`clang++`，把产物写到
`bin/pycdc.exe`（Windows）或 `bin/pycdc`。mingw 工具链缺少两参数的 `mkdir`，脚本用生成的
shim 头补上，不改动 `pycdc/` 里的任何源文件。

## 分支

| 分支 | 内容 | 状态 |
| --- | --- | --- |
| `master` | 本分支：**默认用 `pycdc` 反编译**。解密、还原、建树全部是 Rust，反编译交给同为 exe 同级的 `pycdc.exe`（构建产物随 `bin/` 分发）。`vanilla.mcp` 4353/4353 模块、`vanilla_old.mcp` 3903/3903，全量约 10 秒。 | 发布版 |
| `todo` | 纯 Rust 自包含：额外带一套自己写的 CPython 2.7 反编译器（`src/pydec/`，约 6 千行），不需要任何外部程序就能出源码，`unpack --verify` 会重新解析每个 pyc 交叉核对。当前模块级 4353/4353、函数级 91122/91157（99.96%），剩余 35 个对象以「注释 + 原始指令清单 + `pass`」保留。 | 开发中 |

两个分支共用容器解析、层解码、marshal 还原与目录重建部分，只有「pyc → 源码」那一步不同。

## 特性

- **自动化**：一条命令从 `.mcp` 直接得到可读的目录树，不需要手工挑 key、拆层或修 pyc。
- **快**：`vanilla.mcp`（26.9 MB / 4354 个文件）全量含反编译约 **9 秒**（12 线程，其中写盘约 4.8 秒、反编译约 3.4 秒）；`vanilla_old.mcp`（22.8 MB / 3904 个文件）约 7.5 秒。两个容器都是 0 失败。
- **通用**：层变换、尾部填充长度、opcode 表全部实测判定，不写死版本；新旧两个容器用同一份二进制跑通。
- **容错**：未知层变换、未知 tag、未知 opcode 表只记录进 manifest，不中断；损坏的容器给出明确错误而非崩溃。
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

仓库里的 `bin/` 已经放好构建产物（`unmcpk.exe` 与用于反编译的 `pycdc.exe`），可直接运行 `./bin/unmcpk.exe`。

## 反编译后端

| 后端 | 说明 |
| --- | --- |
| `pycdc` | 默认，最快。放在 exe 同级、`bin/`、`<项目>/pycdc/` 或在 `PATH` 上都会被自动找到，也可用 `UNMCPK_PYCDC` 指定。工具会一次进程处理一批文件，比逐文件快一个数量级。 |
| `uncompyle6` | 可选，产物更贴近原始写法。任意装了 `uncompyle6` 的 python 3.8+ 解释器，用 `--python` / `UNMCPK_PYTHON` 指定；工具用常驻 worker 池，避免每文件重启解释器。 |
| `none` | 只产出 `.pyc`。 |

`--decompiler auto`（默认）按可用性选择：有 pycdc 用 pycdc，否则 uncompyle6，都没有就只输出 pyc。

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
| `--decompiler <auto\|pycdc\|uncompyle6\|none>` | 反编译后端 |
| `--python <PATH>` | uncompyle6 使用的解释器 |
| `--keep-decoys` | 保留诱饵对象的噪声字节码（会被反编译器拒绝） |
| `--old-compat` | 复现历史工具对诱饵的处理方式（删常量 + 砍 9 字节），会破坏含自更新校验的模块 |
| `--no-text-fix` | 关闭 GBK docstring 乱码的启发式修复 |
| `-q, --quiet` | 不打印进度 |

## 实测

同一台机器（12 线程），同一份二进制：

| 容器 | 大小 | 目录 / 文件 | 文件成功 | code 对象 | 诱饵 | 反编译 | 耗时 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `vanilla.mcp` | 26.9 MB | 505 / 4354 | 4353 + 1 资产 | 91157 | 4015 | 4353/4353 | 6.4 s |
| `vanilla_old.mcp` | 22.8 MB | 474 / 3904 | 3903 + 1 资产 | 85483 | 3570 | 3903/3903 | 5.5 s |

分阶段：解码 ~0.5 s、写盘 ~2.8 s、反编译 ~3.2 s。4353 个 pyc 全部能被 `xdis` 载入，`cargo test` 16 项通过。

## 注意事项

- 诱饵对象的字节码本来就是噪声，无法反编译，只能桩化；桩化后模块其余部分才可读。
- pycdc 会丢掉相对导入的相对点（`from .X import Y` → `from X import Y`），需要更贴近原始写法时用
  `--decompiler uncompyle6`。
- 同名模块（不同目录但 `co_filename` 相同）会加 `__<dirId>_<fileId>` 后缀避免互相覆盖，全量下约 500 个。
- GBK docstring 修复是启发式的：只在一行里出现「本不该出现的希腊/西里尔区字符」时才尝试按 GBK 重解。
- `--old-compat` 仅为对齐历史产物，默认关闭。

## 目录

```
src/mcpk.rs        容器解析
src/rotor.rs       6-rotor 变换（Wichmann-Hill 种子、逐 id 密钥调度）
src/payload.rs     层候选、尾部判定、StringID
src/nemarshal/     网易 marshal 解析器 + 校验/桩化 + opcode 表
src/pyc.rs         标准 py2.7 pyc 发射
src/pathmap.rs     co_filename → 相对路径、冲突消解
src/decompile.rs   pycdc / uncompyle6 后端、批量切分、乱码修复
src/pipeline.rs    三阶段流水线（解码 → 写盘 → 反编译）+ manifest
src/main.rs        CLI
examples/dbg.rs    单文件/全库诊断工具
docs/FORMAT.md     容器与 marshal 格式参考
pycdc/             随仓库分发的反编译器源码（GPL-3.0，见「许可证」；只保留构建所需子集，
                   上游的 tests/ 与 scripts/ 未纳入）
scripts/           build_pycdc.sh：构建 pycdc/
bin/               构建产物（unmcpk.exe、pycdc.exe，不入库）
```

## 许可证

本仓库整体按 **GNU GPL v3** 分发（见 `LICENSE`），因为 `pycdc/` 目录内内嵌了：

- `pycdc/` —— CPython 字节码反编译器，来自 [zrax/pycdc](https://github.com/zrax/pycdc) 的
  [HonLaderDev/pycdc](https://github.com/HonLaderDev/pycdc) 分支（commit `17e9cf8`，含 Py2.7
  反编译崩溃与操作数错位的修复），GPL-3.0。这里分发的是**构建所需的子集**：`*.cpp`/`*.h`/
  `bytecode_ops.inl`/`bytes/`/`LICENSE`/`README.markdown`/`CMakeLists.txt`；上游的 `tests/`
  （测试用 pyc fixture）与 `scripts/`（重建各版本 CPython 的辅助脚本）**未纳入**。
  纳入的源文件与上游逐字节一致，未做任何修改。
- 其余代码（容器解析、层解码、marshal 还原、目录重建、CLI）为本项目原创，为与 `pycdc/`
  合并分发，同样以 GPL-3.0 授权。
