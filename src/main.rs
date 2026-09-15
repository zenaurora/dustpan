//! dpan — lightweight Windows cache cleaner inspired by tw93/Mole.
//!
//! `clean` flow: resolve targets -> parallel size scan -> preview table ->
//! confirm -> clean (all deletions pass the safety gate) -> report + audit log.
//! `analyze` flow: one parallel tree scan -> read-only interactive explorer.

mod analyze;
mod apps;
mod clean;
mod ctxmenu;
mod fsutil;
mod menu;
#[cfg(windows)]
mod reg;
mod safety;
mod scan;
mod settings;
mod steam;
mod targets;
mod term;
mod ui;
mod uninstall;

use std::io::IsTerminal;
use std::process::ExitCode;

use clean::{CleanPlan, CleanStats, Cleaner};
use safety::Safety;
use targets::{Category, ResolvedTarget};
use ui::{fmt_size, Style};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Default)]
struct Options {
    dry_run: bool,
    yes: bool,
    verbose: bool,
    list: bool,
    recycle_bin: bool,
    only: Option<Vec<Category>>,
}

fn main() -> ExitCode {
    match parse_args() {
        Ok(Some(Cli::Menu)) => run_menu(),
        Ok(Some(Cli::Clean(opts))) => run(&opts),
        Ok(Some(Cli::Analyze(opts))) => analyze::run(&opts),
        Ok(Some(Cli::Apps(opts))) => apps::run(&opts),
        Ok(Some(Cli::Uninstall(opts))) => uninstall::run(&opts),
        Ok(Some(Cli::Ctxmenu(opts))) => ctxmenu::run(&opts),
        Ok(None) => ExitCode::SUCCESS, // --help / --version
        Err(msg) => {
            eprintln!("error: {msg}\n\nrun `dpan --help` for usage");
            ExitCode::FAILURE
        }
    }
}

enum Cli {
    Menu,
    Clean(Options),
    Analyze(analyze::AnalyzeOptions),
    Apps(apps::AppsOptions),
    Uninstall(uninstall::UninstallOptions),
    Ctxmenu(ctxmenu::CtxOptions),
}

fn parse_args() -> Result<Option<Cli>, String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let no_color = args.iter().any(|arg| arg == "--no-color");
    args.retain(|arg| arg != "--no-color");
    if no_color {
        std::env::set_var("DPAN_NO_COLOR", "1");
    }
    if args.is_empty() {
        return Ok(Some(Cli::Menu));
    }
    if let Some(first) = args.first() {
        if first == "analyze" || first == "analyse" {
            return parse_analyze_args(&args[1..]);
        }
        if first == "apps" {
            return parse_apps_args(&args[1..]);
        }
        if first == "uninstall" {
            return parse_uninstall_args(&args[1..]);
        }
        if first == "ctxmenu" {
            return parse_ctxmenu_args(&args[1..]);
        }
    }
    parse_clean_args(&args)
}

fn run_menu() -> ExitCode {
    let mut settings = settings::Settings::load();
    loop {
        match menu::choose() {
            Some(menu::Action::Clean) => {
                // 主菜单默认高亮 Clean，按一次 Enter 就进入扫描；首次运行
                // 不需要了解分类，也不要求先创建配置文件。
                let only = (settings.categories.len() != Category::ALL.len())
                    .then(|| settings.categories.clone());
                let result = run(&Options {
                    yes: !settings.confirm_clean,
                    recycle_bin: settings.recycle_bin,
                    only,
                    ..Options::default()
                });
                if result != ExitCode::SUCCESS {
                    eprintln!("cleaning completed with errors");
                }
                ui::pause("\nPress Enter to return to the menu...");
            }
            Some(menu::Action::Apps) => {
                let result = apps::run(&apps::AppsOptions::default());
                if result != ExitCode::SUCCESS {
                    ui::pause("\nPress Enter to return to the menu...");
                }
            }
            Some(menu::Action::Analyze) => match menu::choose_analyze() {
                Some(selection) => {
                    let result = analyze::run(&analyze::AnalyzeOptions {
                        path: Some(selection.path),
                        ..analyze::AnalyzeOptions::default()
                    });
                    if result != ExitCode::SUCCESS {
                        ui::pause("\nPress Enter to return to the menu...");
                    }
                }
                None => continue,
            },
            Some(menu::Action::Ctxmenu) => {
                let _ = ctxmenu::interactive();
            }
            Some(menu::Action::Settings) => {
                if let Some(updated) = menu::choose_settings(&settings) {
                    match updated.save() {
                        Ok(()) => {
                            settings = updated;
                            println!("Settings saved.");
                        }
                        Err(error) => eprintln!("could not save settings: {error}"),
                    }
                    ui::pause("Press Enter to return to the menu...");
                }
            }
            None => return ExitCode::SUCCESS,
        }
    }
}

fn parse_ctxmenu_args(args: &[String]) -> Result<Option<Cli>, String> {
    let mut action = ctxmenu::Action::List;
    let mut filter: Option<String> = None;
    let mut json = false;
    let mut rest = args;
    if let Some(first) = rest.first() {
        match first.as_str() {
            "off" => {
                action = ctxmenu::Action::Off;
                rest = &rest[1..];
            }
            "on" => {
                action = ctxmenu::Action::On;
                rest = &rest[1..];
            }
            _ => {}
        }
    }
    for arg in rest {
        match arg.as_str() {
            "--json" => json = true,
            "-h" | "--help" => {
                print_help();
                return Ok(None);
            }
            s if s.starts_with('-') => return Err(format!("unknown argument: {s}")),
            name => {
                if filter.is_some() {
                    return Err(format!("unexpected extra argument: {name}"));
                }
                filter = Some(name.to_string());
            }
        }
    }
    if action != ctxmenu::Action::List && filter.is_none() {
        return Err("ctxmenu on/off needs an entry name, e.g. `dpan ctxmenu off 百度`".into());
    }
    Ok(Some(Cli::Ctxmenu(ctxmenu::CtxOptions {
        action,
        filter,
        json,
    })))
}

fn parse_uninstall_args(args: &[String]) -> Result<Option<Cli>, String> {
    let mut filter: Option<String> = None;
    let mut dry_run = false;
    let mut yes = false;
    for arg in args {
        match arg.as_str() {
            "-n" | "--dry-run" => dry_run = true,
            "-y" | "--yes" => yes = true,
            "-h" | "--help" => {
                print_help();
                return Ok(None);
            }
            s if s.starts_with('-') => return Err(format!("unknown argument: {s}")),
            name => {
                if filter.is_some() {
                    return Err(format!("unexpected extra argument: {name}"));
                }
                filter = Some(name.to_string());
            }
        }
    }
    let filter = filter.ok_or("uninstall needs an app name, e.g. `dpan uninstall 7-zip`")?;
    Ok(Some(Cli::Uninstall(uninstall::UninstallOptions {
        filter,
        dry_run,
        yes,
    })))
}

fn parse_apps_args(args: &[String]) -> Result<Option<Cli>, String> {
    let mut opts = apps::AppsOptions::default();
    for arg in args {
        match arg.as_str() {
            "--json" => opts.json = true,
            "-h" | "--help" => {
                print_help();
                return Ok(None);
            }
            s if s.starts_with('-') => return Err(format!("unknown argument: {s}")),
            filter => {
                if opts.filter.is_some() {
                    return Err(format!("unexpected extra filter: {filter}"));
                }
                opts.filter = Some(filter.to_string());
            }
        }
    }
    Ok(Some(Cli::Apps(opts)))
}

fn parse_analyze_args(args: &[String]) -> Result<Option<Cli>, String> {
    let mut opts = analyze::AnalyzeOptions::default();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => opts.json = true,
            "--top" => {
                let value = iter.next().ok_or("--top needs a number")?;
                opts.top = value
                    .parse()
                    .map_err(|_| format!("invalid --top value: {value}"))?;
            }
            s if s.starts_with("--top=") => {
                let value = &s["--top=".len()..];
                opts.top = value
                    .parse()
                    .map_err(|_| format!("invalid --top value: {value}"))?;
            }
            "-h" | "--help" => {
                print_help();
                return Ok(None);
            }
            s if s.starts_with('-') => return Err(format!("unknown argument: {s}")),
            path => {
                if opts.path.is_some() {
                    return Err(format!("unexpected extra path: {path}"));
                }
                opts.path = Some(std::path::PathBuf::from(path));
            }
        }
    }
    Ok(Some(Cli::Analyze(opts)))
}

fn parse_clean_args(args: &[String]) -> Result<Option<Cli>, String> {
    let mut opts = Options::default();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "clean" => {} // implicit default subcommand, tolerated
            "-n" | "--dry-run" => opts.dry_run = true,
            "-y" | "--yes" => opts.yes = true,
            "-v" | "--verbose" => opts.verbose = true,
            "--list" => opts.list = true,
            "--recycle-bin" => opts.recycle_bin = true,
            "-V" | "--version" => {
                println!("dpan {VERSION}");
                return Ok(None);
            }
            "-h" | "--help" => {
                print_help();
                return Ok(None);
            }
            "--only" => {
                let value = iter
                    .next()
                    .ok_or("--only needs a value, e.g. --only dev,browser")?;
                opts.only = Some(parse_categories(value)?);
            }
            s if s.starts_with("--only=") => {
                opts.only = Some(parse_categories(&s["--only=".len()..])?);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Some(Cli::Clean(opts)))
}

fn parse_categories(value: &str) -> Result<Vec<Category>, String> {
    let mut cats = Vec::new();
    for part in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let cat = Category::parse(part).ok_or_else(|| {
            format!("unknown category '{part}' (temp, system, browser, dev, apps)")
        })?;
        if !cats.contains(&cat) {
            cats.push(cat);
        }
    }
    if cats.is_empty() {
        return Err("--only needs at least one category".into());
    }
    Ok(cats)
}

fn print_help() {
    println!(
        "dpan {VERSION} — lightweight Windows cache cleaner

USAGE:
    dpan                         Open the interactive menu (recommended)
    dpan clean [OPTIONS]
    dpan analyze [PATH] [OPTIONS]
    dpan apps [FILTER] [OPTIONS]
    dpan uninstall <FILTER> [OPTIONS]
    dpan ctxmenu [off|on <NAME>] [OPTIONS]

CLEAN OPTIONS:
    -n, --dry-run        Preview what would be removed, delete nothing
    -y, --yes            Skip the confirmation prompt
        --only <cats>    Limit categories: temp,system,browser,dev,apps
        --list           List resolved targets without scanning sizes
        --recycle-bin    Also empty the Recycle Bin (Windows only)
    -v, --verbose        Show per-entry decisions (skips, failures)
    When started without arguments, Clean opens a category picker first.

ANALYZE OPTIONS (read-only disk usage explorer):
    [PATH]               Directory to analyze (default: home directory)
        --json           Print sizes as JSON and exit (for scripting)
        --top <n>        Entries shown per directory (default: 40)
    Without arguments, Analyze offers common folders, drives, and custom paths.

APPS OPTIONS (interactive app list, biggest first):
    [FILTER]             Only show apps whose name contains FILTER
        --json           Print the inventory as JSON (adds publisher,
                         location, uninstall string)
    Interactive keys: j/k move, Space multi-select (with live hint),
    Enter uninstalls marked apps (or the highlighted one), g/G jump, q quit;
    sources: registry, Steam libraries (ACF manifests), portable dirs.

UNINSTALL OPTIONS (runs the vendor uninstaller, then sweeps leftovers):
    <FILTER>             App to uninstall (must match exactly one)
    -n, --dry-run        Show what would run and what would be deleted
    -y, --yes            Skip both confirmation prompts

CTXMENU OPTIONS (right-click menu manager, no admin needed):
    dpan ctxmenu                 List all context-menu entries
    dpan ctxmenu off <name>      Hide an entry (reversible, HKCU only)
    dpan ctxmenu on <name>       Restore a hidden entry
        --json                   List as JSON
    Without arguments, Context Menu opens a keyboard-driven on/off browser.

INTERACTIVE SETTINGS:
    The Settings screen remembers cleaning areas, Recycle Bin, confirmation,
    and color preferences for future argument-free runs.

COMMON OPTIONS:
    -h, --help           Show this help
    -V, --version        Show version
        --no-color       Disable ANSI color output

FILES:
    settings        %APPDATA%\\dustpan\\settings.conf
    whitelist       %APPDATA%\\dustpan\\whitelist.txt       (one path/glob per line)
    portable dirs   %APPDATA%\\dustpan\\portable_dirs.txt   (extra apps scan roots)
    audit log       %LOCALAPPDATA%\\dustpan\\operations.log"
    );
}

fn run(opts: &Options) -> ExitCode {
    let style = Style::auto();
    let mut plan = CleanPlan::discover(opts.only.as_deref());
    let discovered = plan.all_resolved();

    if discovered.is_empty() && !opts.recycle_bin {
        println!("No cleanable targets found on this system.");
        if !cfg!(windows) {
            println!(
                "{}",
                style.dim("(dpan targets Windows paths; nothing resolves on this OS)")
            );
        }
        return ExitCode::SUCCESS;
    }

    if opts.list {
        print_target_list(&discovered, &style);
        return ExitCode::SUCCESS;
    }

    println!(
        "{} {}",
        style.bold("dpan clean"),
        if opts.dry_run {
            style.yellow("(dry run)")
        } else {
            String::new()
        }
    );
    println!("{}", style.dim("scanning..."));
    plan.preview();
    choose_optional_targets(&mut plan, opts, &style);
    let resolved = plan.resolved();
    let sizes: Vec<u64> = plan.targets.iter().map(|p| p.size).collect();
    let total = plan.total();

    print_preview(&resolved, &sizes, opts.verbose, &style);
    println!(
        "\n{} {}",
        style.bold("Total reclaimable:"),
        style.cyan(&fmt_size(total))
    );

    if total == 0 && !opts.recycle_bin {
        println!("Nothing to clean.");
        return ExitCode::SUCCESS;
    }

    if !opts.dry_run && !opts.yes && !ui::confirm("\nProceed with cleaning?") {
        println!("Aborted, nothing was deleted.");
        return ExitCode::SUCCESS;
    }
    println!();

    let safety = Safety::load();
    if plan.requires_admin() && !clean::is_elevated() {
        println!("{}", style.yellow(clean::explain_elevation()));
    }
    let running_apps = clean::running_cache_apps(&plan);
    if !running_apps.is_empty() && !opts.dry_run {
        println!(
            "{} {}",
            style.yellow("Cache owners are running:"),
            running_apps.join(", ")
        );
        println!("Close them first so all cache files can be removed.");
        if !opts.yes && !ui::confirm("Continue and retry locked files later?") {
            println!("Aborted, nothing was deleted.");
            return ExitCode::SUCCESS;
        }
    }
    let mut cleaner = Cleaner::new(&safety, opts.dry_run);
    let mut stats = CleanStats::default();
    for planned in &plan.targets {
        // 提权权限按目标设置而不是全局开启：浏览器、开发工具和普通应用
        // 缓存始终使用当前用户令牌，只有 Windows 系统缓存允许触发 UAC。
        cleaner.set_elevation_for_target(planned.metadata.requires_admin);
        let s = cleaner.clean_target(&planned.target);
        stats.merge(&s);
    }

    if opts.verbose {
        for line in &cleaner.verbose_lines {
            println!("{}", style.dim(line));
        }
    }

    if stats.failed > 0 && !opts.dry_run && std::io::stdin().is_terminal() {
        let apps = clean::running_cache_apps(&plan);
        if !apps.is_empty() {
            println!("{} {}", style.yellow("Still in use:"), apps.join(", "));
            println!(
                "{}",
                style.dim("Close the listed programs, then choose retry.")
            );
        }
        if ui::confirm("Retry failed entries now?") {
            let retry = cleaner.retry_failed(&stats.failed_paths.clone());
            // retry 只覆盖首轮失败项，因此失败数和失败详情应替换而不是
            // 累加；释放空间和成功数仍需计入整次清理报告。
            stats.freed += retry.freed;
            stats.deleted += retry.deleted;
            stats.skipped += retry.skipped;
            stats.failed = retry.failed;
            stats.failed_paths = retry.failed_paths;
            stats.failure_reasons = retry.failure_reasons;
            if retry.deleted > 0 {
                println!("{} retried entries removed", style.green("✓"));
            }
        }
    }

    let verb = if opts.dry_run { "Would free" } else { "Freed" };
    println!(
        "{} {} {} ({} items)",
        style.green("✓"),
        style.bold(verb),
        style.cyan(&fmt_size(stats.freed)),
        stats.deleted
    );
    if stats.failed > 0 {
        println!(
            "{}",
            style.yellow(&format!("  {} entries could not be removed", stats.failed))
        );
        for reason in stats
            .failure_reasons
            .iter()
            .take(if opts.verbose { usize::MAX } else { 8 })
        {
            println!("{}", style.dim(&format!("  {reason}")));
        }
    }

    if stats.skipped > 0 {
        println!(
            "{}",
            style.dim(&format!(
                "  {} entries protected/whitelisted, kept",
                stats.skipped
            ))
        );
    }
    if !opts.dry_run {
        if let Some(log) = clean::log_path() {
            println!("{}", style.dim(&format!("  audit log: {}", log.display())));
        }
    }

    let recycle_ok = if opts.recycle_bin {
        empty_recycle_bin(opts.dry_run, &style)
    } else {
        true
    };
    if stats.failed > 0 || !recycle_ok {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// 展示 Smart Clean 发现但默认不选的目标。编号只对应实际占用空间大于 0
/// 的条目；直接回车保持安全默认，输入 `all` 或逗号分隔编号才会加入计划。
fn choose_optional_targets(plan: &mut CleanPlan, opts: &Options, style: &Style) {
    let choices: Vec<(usize, &clean::PlannedTarget)> = plan
        .optional
        .iter()
        .enumerate()
        .filter(|(_, planned)| planned.size > 0)
        .collect();
    if choices.is_empty() {
        return;
    }

    println!("\n{}", style.bold("Additional caches found (not selected)"));
    println!(
        "{}",
        style.dim("These are safe to inspect, but may be slow or expensive to download again.")
    );
    for (display_index, (_, planned)) in choices.iter().enumerate() {
        let reason = if planned.metadata.expensive {
            "high re-download cost"
        } else if planned.metadata.requires_admin {
            "needs administrator permission"
        } else {
            "needs review"
        };
        println!(
            "  {:>2}. {:<28} {:>10}  {}",
            display_index + 1,
            planned.target.name,
            fmt_size(planned.size),
            style.yellow(reason)
        );
    }

    // -y 和管道调用必须保持完全非交互；列表仍会输出，让用户知道这些
    // 缓存存在，但只有交互式明确选择才会把它们加入本次执行计划。
    if opts.yes || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        println!("{}", style.dim("Skipped by Smart Clean."));
        return;
    }

    use std::io::Write;
    print!("Select caches to include (e.g. 1,3 or all; Enter skips): ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return;
    }
    let answer = answer.trim();
    if answer.is_empty() {
        return;
    }

    let selected_display_indices: Vec<usize> = if answer.eq_ignore_ascii_case("all") {
        (0..choices.len()).collect()
    } else {
        answer
            .split([',', ' ', ';'])
            .filter(|part| !part.is_empty())
            .filter_map(|part| part.parse::<usize>().ok())
            .filter_map(|number| number.checked_sub(1))
            .filter(|index| *index < choices.len())
            .collect()
    };
    // choices 保存的是 optional 中的原始索引；不能直接使用展示编号，
    // 因为 size=0 的目标已从界面隐藏。
    let optional_indices: Vec<usize> = selected_display_indices
        .into_iter()
        .map(|index| choices[index].0)
        .collect();
    drop(choices);
    plan.include_optional(&optional_indices);
}

fn print_target_list(resolved: &[ResolvedTarget], style: &Style) {
    for cat in Category::ALL {
        let rows: Vec<&ResolvedTarget> = resolved.iter().filter(|t| t.category == cat).collect();
        if rows.is_empty() {
            continue;
        }
        println!("{}", style.bold(&style.cyan(cat.label())));
        for t in rows {
            let metadata = t.metadata();
            let policy = if metadata.risk == targets::Risk::Low && !metadata.expensive {
                "smart default"
            } else if metadata.expensive {
                "optional: high re-download cost"
            } else {
                "optional: review required"
            };
            println!(
                "  {:<28} {:<34} {}",
                t.name,
                style.yellow(policy),
                style.dim(&t.path.display().to_string())
            );
        }
    }
}

fn print_preview(resolved: &[ResolvedTarget], sizes: &[u64], verbose: bool, style: &Style) {
    for cat in Category::ALL {
        let rows: Vec<(usize, &ResolvedTarget)> = resolved
            .iter()
            .enumerate()
            .filter(|(i, t)| t.category == cat && (sizes[*i] > 0 || verbose))
            .collect();
        if rows.is_empty() {
            continue;
        }
        println!("\n{}", style.bold(&style.cyan(cat.label())));
        for (i, t) in rows {
            let meta = t.metadata();
            let risk = match meta.risk {
                targets::Risk::Low => "low",
                targets::Risk::Medium => "medium",
            };
            println!(
                "  {:<28} {:>10}  [{} risk, {} re-download]",
                t.name,
                fmt_size(sizes[i]),
                risk,
                meta.redownload_cost
            );
            if verbose {
                println!("      {}", style.dim(&t.path.display().to_string()));
            }
        }
    }
}

/// Empty all recycle bins through the native Shell API.
fn empty_recycle_bin(dry_run: bool, style: &Style) -> bool {
    if !cfg!(windows) {
        println!(
            "{}",
            style.dim("--recycle-bin is only available on Windows, skipped")
        );
        return true;
    }
    if dry_run {
        println!("{}", style.dim("dry run: would empty the Recycle Bin"));
        return true;
    }
    #[cfg(windows)]
    {
        use std::ptr;

        #[link(name = "shell32")]
        extern "system" {
            fn SHEmptyRecycleBinW(
                hwnd: *mut std::ffi::c_void,
                root_path: *const u16,
                flags: u32,
            ) -> i32;
        }

        // SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND.
        let result = unsafe { SHEmptyRecycleBinW(ptr::null_mut(), ptr::null(), 0x7) };
        if result == 0 {
            println!("{} Recycle Bin emptied", style.green("✓"));
            return true;
        }
    }
    println!("{}", style.yellow("could not empty the Recycle Bin"));
    false
}
