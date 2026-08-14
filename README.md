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
dpan                     # 进入交互菜单，选择清理 / 应用 / 分析 / 右键菜单
dpan clean               # 扫描 → 预览 → 确认 → 清理
dpan clean --dry-run     # 只看能清多少，不删
dpan clean --only dev,browser  # 限定类别：temp / system / browser / dev / apps
dpan clean --list        # 列出本机命中的清理目标
dpan clean --recycle-bin # 顺便清空回收站
dpan clean -y -v         # 跳过确认 + 显示每条跳过/失败原因

dpan analyze              # 分析用户目录，交互式逐层浏览
dpan analyze D:\projects  # 分析指定目录
dpan analyze --json       # JSON 输出供脚本使用；管道时自动降为一次性列表
dpan analyze --top 20     # 每层最多显示 20 条

dpan apps                 # 交互式应用列表：j/k 移动，空格多选，Enter 卸载，q 退出
dpan apps chrome          # 名称筛选
dpan apps --json          # JSON 输出（含发布者/安装位置/卸载命令；管道时自动降为纯文本表格）

dpan uninstall 7-zip      # 卸载：厂商卸载器 → 扫残留 → 确认后清理
dpan uninstall foo -n     # 只预览：会执行什么、会删什么

dpan ctxmenu              # 列出所有右键菜单项（含来源 DLL/命令）
dpan ctxmenu off 百度     # 隐藏某项（可逆，不删厂商键，免管理员）
dpan ctxmenu on 百度      # 恢复
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

## apps（应用清单 + 交互式卸载入口）

`dpan apps [关键词]`，占用从大到小排列。在终端里直接进入全屏列表：`j/k` 移动，底部实时显示选中应用的**磁盘位置、占用大小、发布者、安装日期**（注册表没写大小的应用会在选中时按安装目录实算并重新排序）。

**空格 = 多选标记**（fzf/LazyVim 习惯）：按下即时在底部提示 `● selected Dota 2 — 2 marked, 127.8 GB`，已标记行显示 `●` 并高亮，光标自动下移，再按取消。`Enter`：有标记时先列清单一次性确认后**批量卸载**；无标记时卸载当前行（单独确认）。`q` 退出。输出被管道时自动降为一次性表格。

三个来源合并，解决“Windows 上装的东西很杂”的问题：

1. **注册表 Uninstall 键**（advapi32 FFI 直读）：HKLM 64 位 / HKLM WOW6432Node（32 位程序）/ HKCU（用户级安装）三个位置，并套用标准隐藏规则（`SystemComponent=1`、补丁条目、无名条目）——和 Geek Uninstaller 读的是同一份数据
2. **Steam 游戏**：直接解析 Steam 自己的库清单（`libraryfolders.vdf` + `appmanifest_*.acf`），拿到**精确的 SizeOnDisk 和安装目录**。vdf 里记录了用户添加的所有库（不限盘符）；若根目录探测失败，还会扫描各盘符根部的 `X:\SteamLibrary` 兜底，库路径按大小写/分隔符归一化去重。注册表里那些没大小的 “Steam App XXX” 条目仅在 ACF 扫描成功时才被替换，扫不到则保留原条目不丢游戏
3. **便携/绿色应用扫描**：注册表里没有的解压即用软件。默认扫 `%LOCALAPPDATA%\Programs`、`scoop\apps`（自动识别版本号）、`PortableApps`；自定义目录写在 `%APPDATA%\dustpan\portable_dirs.txt`（每行一个）

表格列：名称、版本、占用、安装日期、来源（system / sys32 / user / steam / portable）。发布者、安装位置、`UninstallString` 在 `--json` 输出里（注意：`source` 枚举含 `steam`，Steam 条目的 `uninstall` 字段是 `steam://` 协议 URL 而非可执行命令）。

## uninstall（卸载 + 残留清理）

**卸载方式：优先走软件自己的卸载器，不是强制删除。**`dpan uninstall <关键词>`（或在 `dpan apps` 里按 Enter），必须唯一命中。三步：

1. **执行厂商卸载器**：MSI 条目统一规整为 `msiexec /x {GUID} /qb`（不管厂商写的是 `/I` 还是 `/X`）；其他程序优先用 `QuietUninstallString`，没有则用 `UninstallString`；Steam 游戏通过 `steam://uninstall/<appid>` 交给 Steam 自己处理（不跑残留扫描，避免误伤存档）；只有便携应用（本来就没卸载器）才直接删目录，且走安全闸门。退出码 3010（需重启）视为成功，1602（用户取消）则中止后续步骤
2. **残留扫描**：按应用名变体（去版本号、空格/连字符/下划线互换）扫 `%APPDATA%`、`%LOCALAPPDATA%`、`%PROGRAMDATA%` 及安装目录，支持 `发布者\应用` 两层布局
3. **确认后删除**：残留列表带大小展示，确认后经同一套安全闸门 + 审计日志删除

注册表残留只报告路径不动手——dustpan 对注册表的写入严格限制在 HKCU（见 ctxmenu）。`-n` 全程预览，`-y` 跳过两次确认。

## ctxmenu（右键菜单管理）

专治国产软件往右键菜单塞“XX扫描”“上传到XX网盘”的问题。枚举六个挂载点（`*`、`Directory`、`Directory\Background`、`Folder`、`Drive`、`AllFilesystemObjects`）下的两类条目：

- **静态 verb**（`shell\<名字>`）：禁用 = 在 `HKCU\Software\Classes` 同路径写入 `LegacyDisable` 遮罩值，不碰厂商在 HKLM 的原键
- **COM 扩展**（`shellex\ContextMenuHandlers`）：禁用 = 把 CLSID 加进微软官方的每用户拉黑键 `HKCU\...\Shell Extensions\Blocked`（ShellExView 同款机制）

三条安全约束：**只写 HKCU**（免管理员）、**只禁用不删除**（`on` 完整恢复）、**Windows 自带项拒绝禁用**（来源在 System32/SysWOW64 的条目带 `[windows]` 标记且 `off` 会拒绝）。生效需新开 Explorer 窗口或重启 Explorer；Win11 新式菜单（非“显示更多选项”）的条目不在此机制内。


## 致谢

实现参考了 [Mole](https://github.com/tw93/Mole) CLI。
