//! Interactive command picker shown when dpan starts without arguments.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use crate::settings::Settings;
use crate::targets::Category;
use crate::term::{self, AltScreen, Key, RawMode};
use crate::ui::Style;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Clean,
    Apps,
    Analyze,
    Ctxmenu,
    Settings,
}

pub struct AnalyzeSelection {
    pub path: PathBuf,
}

const ITEMS: [(Action, &str, &str); 5] = [
    (Action::Clean, "Clean", "Free up disk space"),
    (Action::Apps, "Apps", "Manage or uninstall applications"),
    (Action::Analyze, "Analyze", "Explore disk usage"),
    (
        Action::Ctxmenu,
        "Context Menu",
        "Inspect right-click menu entries",
    ),
    (
        Action::Settings,
        "Settings",
        "Change remembered preferences",
    ),
];

pub fn choose() -> Option<Action> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("dpan needs an interactive terminal when run without a command.");
        eprintln!("Run `dpan --help` for usage or `dpan clean` to clean.");
        return None;
    }

    let raw = RawMode::enter()?;
    let screen = AltScreen::enter();
    let style = Style::auto();
    let selected = choose_inner(&style);
    // Restore the terminal before the selected command starts. Some commands
    // use their own full-screen view or a normal line-based confirmation.
    drop(screen);
    drop(raw);
    selected
}

fn choose_inner(style: &Style) -> Option<Action> {
    let mut selected = 0usize;
    let mut stdin = io::stdin().lock();
    loop {
        draw(selected, style);
        match term::read_key(&mut stdin) {
            Key::Up => selected = selected.saturating_sub(1),
            Key::Down => selected = (selected + 1).min(ITEMS.len() - 1),
            Key::Top => selected = 0,
            Key::Bottom => selected = ITEMS.len() - 1,
            Key::Number(n) if (1..=ITEMS.len() as u8).contains(&n) => {
                return Some(ITEMS[n as usize - 1].0);
            }
            Key::Enter | Key::Right => return Some(ITEMS[selected].0),
            Key::Quit | Key::Left => return None,
            Key::Number(_) | Key::Space | Key::PageUp | Key::PageDown | Key::Other => {}
        }
    }
}

fn draw(selected: usize, style: &Style) {
    let mut out = String::from("\x1b[H\x1b[2J\n");
    out.push_str(&format!("  {}\n", style.bold("dustpan")));
    out.push_str(&format!(
        "  {}\n\n",
        style.dim("Clean and inspect your Windows PC")
    ));

    for (i, (_, title, description)) in ITEMS.iter().enumerate() {
        let row = format!("  {}  {:<14} {}  ", i + 1, title, description);
        if i == selected {
            out.push_str(&format!("  {}\n", style.invert(&row)));
        } else {
            out.push_str(&format!("  {row}\n"));
        }
    }

    out.push_str(&format!(
        "\n  {}\n",
        style.dim("↑↓/j/k move  •  Enter select  •  1-5 quick select  •  q quit")
    ));
    print!("{out}");
    let _ = io::stdout().flush();
}

const SETTINGS_ITEMS: [&str; 8] = [
    "Temporary files",
    "System caches",
    "Browser caches",
    "Developer caches",
    "Application caches",
    "Recycle Bin",
    "Confirm before deleting",
    "Color output",
];

pub fn choose_settings(settings: &Settings) -> Option<Settings> {
    let _raw = RawMode::enter()?;
    let _screen = AltScreen::enter();
    let style = Style::auto();
    let mut selected = [false; 8];
    for (i, category) in Category::ALL.iter().enumerate() {
        selected[i] = settings.categories.contains(category);
    }
    selected[5] = settings.recycle_bin;
    selected[6] = settings.confirm_clean;
    selected[7] = settings.color;
    let mut cursor = 0usize;
    let mut message = String::new();
    let mut stdin = io::stdin().lock();
    loop {
        draw_settings(cursor, &selected, &style, &message);
        message.clear();
        match term::read_key(&mut stdin) {
            Key::Up => cursor = cursor.saturating_sub(1),
            Key::Down => cursor = (cursor + 1).min(SETTINGS_ITEMS.len() - 1),
            Key::Top => cursor = 0,
            Key::Bottom => cursor = SETTINGS_ITEMS.len() - 1,
            Key::Space => selected[cursor] = !selected[cursor],
            Key::Number(n) if (1..=SETTINGS_ITEMS.len() as u8).contains(&n) => {
                cursor = n as usize - 1;
                selected[cursor] = !selected[cursor];
            }
            Key::Enter | Key::Right => {
                let categories: Vec<Category> = Category::ALL
                    .iter()
                    .enumerate()
                    .filter_map(|(i, category)| selected[i].then_some(*category))
                    .collect();
                if categories.is_empty() && !selected[5] {
                    message = "Keep at least one cleaning area enabled".into();
                    continue;
                }
                return Some(Settings {
                    categories,
                    recycle_bin: selected[5],
                    confirm_clean: selected[6],
                    color: selected[7],
                });
            }
            Key::Quit | Key::Left => return None,
            Key::PageUp | Key::PageDown | Key::Other | Key::Number(_) => {}
        }
    }
}

fn draw_settings(cursor: usize, selected: &[bool; 8], style: &Style, message: &str) {
    let mut out = String::from("\x1b[H\x1b[2J\n");
    out.push_str(&format!("  {}\n", style.bold("Settings")));
    out.push_str(&format!(
        "  {}\n\n",
        style.dim("Defaults used by the argument-free workflow")
    ));
    for (i, title) in SETTINGS_ITEMS.iter().enumerate() {
        let mark = if selected[i] { "[x]" } else { "[ ]" };
        let row = format!("  {}  {}  {}  ", i + 1, mark, title);
        if i == cursor {
            out.push_str(&format!("  {}\n", style.invert(&row)));
        } else {
            out.push_str(&format!("{row}\n"));
        }
    }
    out.push_str(&format!(
        "\n  {}\n",
        style.dim("↑↓/j/k move  •  Space toggle  •  Enter save  •  q cancel")
    ));
    if !message.is_empty() {
        out.push_str(&format!("\n  {}\n", style.yellow(message)));
    }
    print!("{out}");
    let _ = io::stdout().flush();
}

/// Pick a useful starting directory for the disk-usage explorer. The command
/// line still accepts arbitrary paths, while the common no-argument flow does
/// not require the user to type a Windows path.
pub fn choose_analyze() -> Option<AnalyzeSelection> {
    let mut items: Vec<(String, Option<PathBuf>)> = Vec::new();
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(PathBuf::from);
    if let Some(home) = home {
        items.push(("Home".into(), Some(home.clone())));
        for (label, name) in [
            ("Desktop", "Desktop"),
            ("Downloads", "Downloads"),
            ("Documents", "Documents"),
            ("Pictures", "Pictures"),
        ] {
            let path = home.join(name);
            if path.is_dir() {
                items.push((label.into(), Some(path)));
            }
        }
    }
    #[cfg(windows)]
    for letter in b'A'..=b'Z' {
        let path = PathBuf::from(format!("{}:\\", letter as char));
        if path.is_dir()
            && !items
                .iter()
                .any(|(_, existing)| existing.as_ref() == Some(&path))
        {
            items.push((format!("Drive {}:", letter as char), Some(path)));
        }
    }
    items.push(("Custom path...".into(), None));
    let raw = RawMode::enter()?;
    let screen = AltScreen::enter();
    let style = Style::auto();
    let mut cursor = 0usize;
    let mut stdin = io::stdin().lock();
    loop {
        let mut out = String::from("\x1b[H\x1b[2J\n");
        out.push_str(&format!("  {}\n", style.bold("Analyze disk usage")));
        out.push_str(&format!(
            "  {}\n\n",
            style.dim("Choose a directory to explore")
        ));
        for (i, (label, path)) in items.iter().enumerate() {
            let display = path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            let row = format!("  {}  {:<16} {}  ", i + 1, label, display);
            if i == cursor {
                out.push_str(&format!("  {}\n", style.invert(&row)));
            } else {
                out.push_str(&format!("{row}\n"));
            }
        }
        out.push_str(&format!(
            "\n  {}\n",
            style.dim("↑↓/j/k move  •  Enter open  •  q cancel")
        ));
        print!("{out}");
        let _ = io::stdout().flush();
        let picked = match term::read_key(&mut stdin) {
            Key::Up => {
                cursor = cursor.saturating_sub(1);
                None
            }
            Key::Down => {
                cursor = (cursor + 1).min(items.len() - 1);
                None
            }
            Key::Top => {
                cursor = 0;
                None
            }
            Key::Bottom => {
                cursor = items.len() - 1;
                None
            }
            Key::Number(n) if (1..=items.len() as u8).contains(&n) => {
                cursor = n as usize - 1;
                Some(cursor)
            }
            Key::Enter | Key::Right => Some(cursor),
            Key::Quit | Key::Left => {
                drop(stdin);
                drop(screen);
                drop(raw);
                return None;
            }
            Key::Space | Key::PageUp | Key::PageDown | Key::Other | Key::Number(_) => None,
        };
        if let Some(index) = picked {
            let path = items[index].1.clone();
            drop(stdin);
            drop(screen);
            drop(raw);
            return match path {
                Some(path) => Some(AnalyzeSelection { path }),
                None => prompt_analyze_path(),
            };
        }
    }
}

fn prompt_analyze_path() -> Option<AnalyzeSelection> {
    loop {
        print!("Directory to analyze (blank to cancel): ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            return None;
        }
        let value = line.trim().trim_matches('"');
        if value.is_empty() {
            return None;
        }
        let path = PathBuf::from(value);
        if path.is_dir() {
            return Some(AnalyzeSelection { path });
        }
        println!("Not a readable directory: {}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_actions_stay_in_display_order() {
        assert_eq!(ITEMS[0].0, Action::Clean);
        assert_eq!(ITEMS[1].0, Action::Apps);
        assert_eq!(ITEMS[2].0, Action::Analyze);
        assert_eq!(ITEMS[3].0, Action::Ctxmenu);
        assert_eq!(ITEMS[4].0, Action::Settings);
    }
}
