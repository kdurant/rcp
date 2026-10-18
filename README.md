# rcp —— 带进度条与速率的 `cp`

`rcp` 是一个 GNU `cp` 的替代实现（Rust），在复制文件/目录时显示**当前文件**、
**已复制字节**、**实时速率**和 **ETA**，其余语义尽量对齐 GNU coreutils 的 `cp`。

```
⠹ big.iso   (1/1)
      768.00 MiB/1.00 GiB     [████████████████████████░░░░░░░░]  75%     2.39 GiB/s ETA 0s
total 768.00 MiB/1.00 GiB     [████████████████████████░░░░░░░░]  75%     2.39 GiB/s ETA 0s
rcp: 1.00 GiB in 0.40s (2.53 GiB/s), 1 files, 0 dirs, 0 links, 0 special, 0 skipped, 0 failed
```

## 构建与安装

```bash
cargo build --release            # 产出 target/release/rcp
cargo install --path .           # 安装到 ~/.cargo/bin
```

## 用法

```bash
rcp src.txt dst.txt              # 单文件
rcp a b c dir/                   # 多源 → 目录
rcp -r src_dir/ dst_dir/         # 递归（保留符号链接本身）
rcp -a src_dir/ dst_dir/         # 归档：保留权限/属主/时间戳/硬链接
rcp -p big.bin /backup/          # 保留属性
rcp -l a b                       # 硬链接
rcp --parents a/b/c.txt out/     # 在 out 下重建 a/b/c.txt
rcp -t out/ a b                  # 指定目标目录
rcp -n / -u / -i ...             # 不覆盖 / 仅更新 / 覆盖前询问
rcp --progress=always big.bin .  # 强制输出进度（管道/日志里退化为每秒一行文本）
rcp -q ...                       # 关闭进度与汇总
```

进度条画在 **stderr**，不会污染 stdout 重定向出来的数据。stderr 不是终端时
默认**自动关闭**（`auto`）。

## 支持的选项

| 类别 | 选项 |
| --- | --- |
| 递归 | `-r` / `-R` / `--recursive`、`-a` / `--archive`、`-x` / `--one-file-system`、`--parents` |
| 符号链接 | `-d`、`-P` / `--no-dereference`、`-L` / `--dereference`、`-H` |
| 覆盖策略 | `-f` / `--force`、`-i` / `--interactive`、`-n` / `--no-clobber`、`-u` / `--update` |
| 属性 | `-p`、`--preserve[=mode,ownership,timestamps,links,all]` |
| 目标形式 | `-t` / `--target-directory`、`-T` / `--no-target-directory` |
| 链接模式 | `-l` / `--link`、`-s` / `--symbolic-link` |
| 其它 | `-v` / `--verbose`、`-h` / `--help`、`-V` / `--version` |
| 本工具新增 | `--progress[=auto\|always\|never]`、`-q` / `--quiet` |

`-i`/`-n`、`-L`/`-P`/`-H`/`-d` 都遵循 GNU 的「**后者胜出**」规则
（`-f` 不参与覆盖提示的优先级）；`-a` = `-dR --preserve=all`；
`-p` = `--preserve=mode,ownership,timestamps`。

## 与 GNU cp 的已知差异

| 项 | 说明 |
| --- | --- |
| `--progress` / `-q` / 汇总行 | 本工具新增。汇总行只在进度显示开启时打印；stderr 非终端时默认静默 |
| `-i` 回答 no | 退出码 **1**（与 GNU coreutils 9.x 实测行为一致），目标文件保持不变 |
| 目录复制到自身 | 在扫描阶段就拒绝（`cannot copy a directory ... into itself`），不会先建目录再报错。GNU 会先创建并复制一部分 |
| xattr / ACL / SELinux context | 不保留。显式写 `--preserve=xattr`/`context` 会给警告；`-a` 静默跳过 |
| 稀疏文件 | 不做空洞检测（`--sparse` 不支持），稀疏源会被写成实心 |
| `-s` 的链接目标 | 使用展开后的源路径字符串（GNU 用命令行原样的字符串）。相对/绝对语义一致 |
| 遍历顺序 | 目录条目按名字排序（GNU 用 readdir 顺序），`-v` 的输出顺序可能不同 |
| socket | 用 `mknod` 创建（普通用户即可）。GNU 在"目标是已存在目录"时的跳过行为不跟随 |
| 未实现 | `--reflink`、`--backup`/`-b`/`-S`、`--copy-contents`、`--attributes-only`、`--strip-trailing-slashes`、`-Z`/context |
| 并发 | 单线程顺序复制（进度语义简单、磁盘行为可预测） |
| 用法错误 | 退出码 1（与 GNU 一致）；任何条目失败或 `-i` 被拒也返回 1 |

## 实现要点

**两阶段模型**：先完整扫描源（`src/scan.rs`）得到条目列表与总字节数，再执行复制
（`src/copier.rs`）。这样进度条的"总量 + ETA"是确定的，而不是未知总量的假百分比。
扫描期显示 `scanning... N entries` 的 spinner。

**字节搬运**（`copier::pump`）：

1. 常规文件先分块调用 `copy_file_range`（4 MiB/次），同文件系统下走内核零拷贝路径；
2. 遇到 `EXDEV` / `EINVAL` / `ENOSYS` / `EOPNOTSUPP` / `EPERM` 时回退到 `read`/`write`
   （64 KiB/次，实测在 tmpfs 上比 128 KiB+ 快 45%，见 `examples/rwbench.rs`）；
3. `EPERM` / `EINVAL` 等都会**在当前位置**继续，不会重复搬运。

**元数据**（`src/meta.rs`）：权限按 `src_mode & ~umask`（`-p` 时精确复制），
目录先带 `u+wx` 创建、复制完成后再自底向上 `chmod`/`utimensat`/`lchown`
（否则只读目录里写不进子项）。`chown` 的 `EPERM` 静默忽略（与 GNU 相同）。
`--preserve=links` 用 `(dev, ino)` 映射把同一硬链接组的后续文件改成硬链接。

**其它细节**：不写穿悬空符号链接（`not writing through dangling symlink`）；
失败目录的子孙条目直接跳过，避免刷屏；SIGINT/SIGTERM 会补一个换行再 `_exit`
（退出码 130/143），终端不会停在进度条中间。

## 测试

```bash
cargo test                       # 8 个单元测试 + 47 个集成测试
```

集成测试（`tests/copy.rs`）覆盖内容/权限/时间戳/硬链接/符号链接/覆盖策略/
`-t -T --parents`/特殊文件/进度输出等，并包含跨设备回退用例。

另有一个开发用的**与系统 `cp` 对拍脚本**（85 个用例，比较目录树快照、
权限、mtime、链接组、退出码与 stderr 行数）：

```bash
./tests/compare_gnu_cp.sh                              # 需要系统装有 coreutils cp
RCP=target/release/rcp ./tests/compare_gnu_cp.sh
```

## 性能

单文件 1 GiB（本机 tmpfs/ btrfs 实测）：

| 场景 | 路径 | `cp` | `rcp` |
| --- | --- | --- | --- |
| tmpfs → tmpfs（跨挂载） | `read`/`write` 回退 | 0.39 s | 0.40 s |
| tmpfs → btrfs | `read`/`write` 回退 | 0.39 s | 0.40 s |
| tmpfs 同挂载 | `copy_file_range` | 0.37 s | 0.38 s |
| 183 MB 源码目录树（数千小文件） | 混合 | 0.29 s | 0.22 s |

`examples/rwbench.rs` 是用来给回退路径定分块大小的微基准：

```bash
cargo run --release --example rwbench -- /dev/shm/src.bin /tmp/dst.bin 65536
```
