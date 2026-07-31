//! dpan — lightweight Windows cache cleaner inspired by tw93/Mole.
//!
//! `clean` flow: resolve targets -> parallel size scan -> preview table ->
//! confirm -> clean (all deletions pass the safety gate) -> report + audit log.
//! `analyze` flow: one parallel tree scan -> read-only interactive explorer.

mod analyze;
mod apps;
mod clean;
mod ctxmenu;
#[cfg(windows)]
mod reg;
mod safety;
mod scan;
mod targets;
mod term;
mod ui;
mod uninstall;

use std::process::ExitCode;

use clean::{CleanStats, Cleaner};
use safety::Safety;
use targets::{Category, ResolvedTarget};
use ui::{fmt_size, Style};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Default)]
struct Options {
    dry_run: bool,
    yes: bool,
    verbose: bool,
    no_color: bool,
    list: bool,
    recycle_bin: bool,
    only: Option<Vec<Category>>,
}

fn main() -> ExitCode {
    match parse_args() {
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
    Clean(Options),
    Analyze(analyze::AnalyzeOptions),
    Apps(apps::AppsOptions),
    Uninstall(uninstall::UninstallOptions),
    Ctxmenu(ctxmenu::CtxOptions),
}

fn parse_args() -> Result<Option<Cli>, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
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

fn parse_ctxmenu_args(args: &[String]) -> Result<Option<Cli>, String> {
    let mut action = ctxmenu::Action::List;
    let mut filter: Option<String> = None;
    let mut json = false;
    let mut no_color = false;
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
            "--no-color" => no_color = true,
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
        no_color,
    })))
}

fn parse_uninstall_args(args: &[String]) -> Result<Option<Cli>, String> {
    let mut filter: Option<String> = None;
    let mut dry_run = false;
    let mut yes = false;
    let mut no_color = false;
    for arg in args {
        match arg.as_str() {
            "-n" | "--dry-run" => dry_run = true,
            "-y" | "--yes" => yes = true,
            "--no-color" => no_color = true,
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
        no_color,
    })))
}

fn parse_apps_args(args: &[String]) -> Result<Option<Cli>, String> {
    let mut opts = apps::AppsOptions::default();
    for arg in args {
        match arg.as_str() {
            "--json" => opts.json = true,
            "--no-color" => opts.no_color = true,
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
            "--no-color" => opts.no_color = true,
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
            "--no-color" => opts.no_color = true,
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
    dpan [clean] [OPTIONS]
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

ANALYZE OPTIONS (read-only disk usage explorer):
    [PATH]               Directory to analyze (default: home directory)
        --json           Print sizes as JSON and exit (for scripting)
        --top <n>        Entries shown per directory (default: 40)

APPS OPTIONS (installed application inventory, biggest first):
    [FILTER]             Only show apps whose name contains FILTER
        --json           Print the inventory as JSON (adds publisher,
                         location, uninstall string)

UNINSTALL OPTIONS (runs the vendor uninstaller, then sweeps leftovers):
    <FILTER>             App to uninstall (must match exactly one)
    -n, --dry-run        Show what would run and what would be deleted
    -y, --yes            Skip both confirmation prompts

CTXMENU OPTIONS (right-click menu manager, no admin needed):
    dpan ctxmenu                 List all context-menu entries
    dpan ctxmenu off <name>      Hide an entry (reversible, HKCU only)
    dpan ctxmenu on <name>       Restore a hidden entry
        --json                   List as JSON

COMMON OPTIONS:
        --no-color       Disable colored output
    -h, --help           Show this help
    -V, --version        Show version

FILES:
    whitelist       %APPDATA%\\dustpan\\whitelist.txt       (one path/glob per line)
    portable dirs   %APPDATA%\\dustpan\\portable_dirs.txt   (extra apps scan roots)
    audit log       %LOCALAPPDATA%\\dustpan\\operations.log"
    );
}

fn run(opts: &Options) -> ExitCode {
    let style = Style::auto(opts.no_color);
    let resolved = targets::resolve_targets(opts.only.as_deref());

    if resolved.is_empty() {
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
        print_target_list(&resolved, &style);
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
    let sizes = scan::scan_sizes(&resolved);
    let total: u64 = sizes.iter().sum();

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
    let mut cleaner = Cleaner::new(&safety, opts.dry_run);
    let mut stats = CleanStats::default();
    for (target, size) in resolved.iter().zip(&sizes) {
        if *size == 0 {
            continue;
        }
        let s = cleaner.clean_target(target);
        stats.freed += s.freed;
        stats.deleted += s.deleted;
        stats.failed += s.failed;
        stats.skipped += s.skipped;
    }

    if opts.verbose {
        for line in &cleaner.verbose_lines {
            println!("{}", style.dim(line));
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
            style.yellow(&format!(
                "  {} entries locked or in use, skipped",
                stats.failed
            ))
        );
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

    if opts.recycle_bin {
        empty_recycle_bin(opts.dry_run, &style);
    }
    ExitCode::SUCCESS
}

fn print_target_list(resolved: &[ResolvedTarget], style: &Style) {
    for cat in Category::ALL {
        let rows: Vec<&ResolvedTarget> = resolved.iter().filter(|t| t.category == cat).collect();
        if rows.is_empty() {
            continue;
        }
        println!("{}", style.bold(&style.cyan(cat.label())));
        for t in rows {
            println!(
                "  {:<28} {}",
                t.name,
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
            println!("  {:<28} {:>10}", t.name, fmt_size(sizes[i]));
            if verbose {
                println!("      {}", style.dim(&t.path.display().to_string()));
            }
        }
    }
}

/// Lite approach: shell out to PowerShell instead of linking shell32.
fn empty_recycle_bin(dry_run: bool, style: &Style) {
    if !cfg!(windows) {
        println!(
            "{}",
            style.dim("--recycle-bin is only available on Windows, skipped")
        );
        return;
    }
    if dry_run {
        println!("{}", style.dim("dry run: would empty the Recycle Bin"));
        return;
    }
    let status = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Clear-RecycleBin -Force -ErrorAction SilentlyContinue",
        ])
        .status();
    match status {
        Ok(s) if s.success() => println!("{} Recycle Bin emptied", style.green("✓")),
        _ => println!("{}", style.yellow("could not empty the Recycle Bin")),
    }
}
