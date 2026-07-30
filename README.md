# dustpan

Windows `clean`（缓存清理）+ `analyze`（磁盘占用浏览）的 CLI。

- **单二进制、零依赖**：纯 Rust 标准库，release 构建约几百 KB
- **只读默认安全**：先扫描预览，确认后才删除；`--dry-run` 全程不落一刀
- 三个核心机制：
  1. **数据与逻辑分离**：清理目标是一张静态表（`src/targets.rs`），`%VAR%` 模板 + `*` 通配符
  2. **删除单点收敛**：所有删除都走 `Cleaner::remove_one` 的四重闸门——用户白名单 → 路径校验（绝对路径 / 无 `..` / 最小深度）→ 系统保护树拒绝表 → 允许基目录白名单（fail-closed）
  3. **审计日志**：每次删除追加记录到 `%LOCALAPPDATA%\dustpan\operations.log`

## 用法

```console
dpan                     # 扫描 → 预览 → 确认 → 清理
dpan --dry-run           # 只看能清多少，不删
dpan --only dev,browser  # 限定类别：temp / system / browser / dev / apps
dpan --list              # 列出本机命中的清理目标
dpan --recycle-bin       # 顺便清空回收站
dpan -y -v                # 跳过确认 + 显示每条跳过/失败原因

dpan analyze              # 分析用户目录，交互式逐层浏览
dpan analyze D:\projects  # 分析指定目录
dpan analyze --json       # JSON 输出供脚本使用；管道时自动降为一次性列表
dpan analyze --top 20     # 每层最多显示 20 条

dpan apps                 # 列出已安装应用（注册表 + 便携应用扫描）
dpan apps chrome          # 名称过滤
dpan apps --sort size -v  # 按占用排序，显示发布者/安装位置
dpan apps --store --json  # 含 Store/UWP 应用，JSON 输出
```

## analyze（只读磁盘占用浏览）

启动时并发扫描整棵目录树建立大小索引，之后逐层浏览零延迟。全屏 vim 式交互，单键即时响应（raw 模式，无需回车）：

| 键 | 动作 |
| --- | --- |
| `j` / `k` / ↑↓ | 上下移动光标 |
| `l` / `Enter` / → | 进入目录（文件则显示大小/修改时间） |
| `h` / `u` / ← | 返回上级（光标自动落在刚离开的目录上） |
| `g` / `G` | 跳到顶部 / 底部 |
| `Ctrl-U` / `Ctrl-F` / PgUp / PgDn | 翻页 |
| `q` / `ESC` / `Ctrl-C` | 退出（完整还原终端） |

每层文件/文件夹按大小降序 + 比例条，列表随终端高度自动滚动。全程只读、不跟随符号链接。raw 模式零依赖实现：Unix 用 `stty`，Windows 直接 FFI kernel32 `SetConsoleMode`（并启用 VT 输入，两端按键解析统一走 CSI 序列）。输出被管道时自动降为一次性列表。

## 清理范围

| 类别 | 内容 |
| --- | --- |
| Temp | 用户 `%TEMP%`、`C:\Windows\Temp` |
| System | 缩略图/图标缓存、INetCache、崩溃转储、WER 报告、DirectX/NVIDIA/AMD 着色器缓存 |
| Browser | Chrome / Edge / Firefox 各 profile 的 Cache、Code Cache、GPUCache |
| Dev | npm / pnpm / yarn / pip / uv / cargo registry / go-build / NuGet HTTP 缓存、VS Code 缓存、JetBrains caches |
| Apps | Discord / Slack / Teams(classic) / Spotify 缓存 |

刻意不清（重新下载代价高）：`.m2/repository`、`.gradle/caches`、`.nuget/packages`、pnpm store。

被占用/锁定的文件自动跳过并计数（Windows 上应用运行时很常见），不会中断整体清理。

## 白名单

`%APPDATA%\dustpan\whitelist.txt`，每行一个路径或 glob，`#` 注释，支持 `~` 和 `%VAR%`：

```
# 保住整棵子树
%LOCALAPPDATA%\npm-cache\important-pkg
# glob 匹配完整路径
*node_modules\.cache*
```

## 构建与测试

```console
cargo build --release          # 在 Windows 上构建产出 dpan.exe
cargo test                     # 单元测试（跨平台可跑）
sh scripts/smoke.sh            # 端到端冒烟测试（伪造 Windows 环境变量）
cargo check --target x86_64-pc-windows-msvc   # macOS/Linux 上做 Windows 目标类型检查
```

模板路径统一用 `\` 书写，展开时按宿主 OS 转换分隔符，因此全部逻辑在 macOS/Linux 上可测试，实际清理目标只在 Windows 环境变量存在时才会命中。

## apps（应用清单，只读）

三个来源合并，解决“Windows 上装的东西很杂”的问题：

1. **注册表 Uninstall 键**（advapi32 FFI 直读）：HKLM 64 位 / HKLM WOW6432Node（32 位程序）/ HKCU（用户级安装）三个位置，并套用标准隐藏规则（`SystemComponent=1`、补丁条目、无名条目）——和 Geek Uninstaller 读的是同一份数据
2. **Store/UWP 应用**（`--store`，可选）：PowerShell `Get-AppxPackage`，较慢
3. **便携/绿色应用扫描**：注册表里没有的解压即用软件。默认扫 `%LOCALAPPDATA%\Programs`、`scoop\apps`（自动识别版本号）、`PortableApps`；自定义目录写在 `%APPDATA%\dustpan\portable_dirs.txt`（每行一个）。目录内两层以内含 `.exe` 才算应用，已在注册表出现的路径/同名应用自动去重

输出列：名称、版本、占用（注册表 `EstimatedSize` 或实算目录大小）、安装日期、来源（system / sys32 / user / store / portable）。JSON 输出额外含 `UninstallString`，为将来的卸载功能预留。

## 致谢

实现参考了 [Mole](https://github.com/tw93/Mole) CLI。
