//! Interactive command picker shown when dpan starts without arguments.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use crate::targets::Category;
use crate::term::{self, AltScreen, Key, RawMode};
use crate::ui::Style;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Clean,
    Apps,
    Analyze,
    Ctxmenu,
}

/// The small amount of state the main cleaner needs from the interactive
/// picker. Execution stays in `main::run`, so the menu cannot accidentally
/// grow a second cleaning implementation.
pub struct CleanSelection {
    pub only: Option<Vec<Category>>,
    pub recycle_bin: bool,
}

pub struct AnalyzeSelection {
    pub path: PathBuf,
}

const ITEMS: [(Action, &str, &str); 4] = [
    (Action::Clean, "Clean", "Free up disk space"),
    (Action::Apps, "Apps", "Manage or uninstall applications"),
    (Action::Analyze, "Analyze", "Explore disk usage"),
    (
        Action::Ctxmenu,
        "Context Menu",
        "Inspect right-click menu entries",
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
        style.dim("↑↓/j/k move  •  Enter select  •  1-4 quick select  •  q quit")
    ));
    print!("{out}");
    let _ = io::stdout().flush();
}

const CLEAN_ITEMS: [(&str, Option<Category>); 6] = [
    ("Temporary files", Some(Category::Temp)),
    ("System caches", Some(Category::System)),
    ("Browser caches", Some(Category::Browser)),
    ("Developer caches", Some(Category::Dev)),
    ("Application caches", Some(Category::Apps)),
    ("Recycle Bin", None),
];

/// Select cleaning areas without remembering category flags. Categories are
/// selected by default; Recycle Bin is deliberately opt-in.
pub fn choose_clean() -> Option<CleanSelection> {
    let raw = RawMode::enter()?;
    let screen = AltScreen::enter();
    let style = Style::auto();
    let result = choose_clean_inner(&style);
    drop(screen);
    drop(raw);
    result
}

fn choose_clean_inner(style: &Style) -> Option<CleanSelection> {
    let mut selected = [true, true, true, true, true, false];
    let mut cursor = 0usize;
    let mut message = String::new();
    let mut stdin = io::stdin().lock();
    loop {
        draw_clean(cursor, &selected, style, &message);
        message.clear();
        match term::read_key(&mut stdin) {
            Key::Up => cursor = cursor.saturating_sub(1),
            Key::Down => cursor = (cursor + 1).min(CLEAN_ITEMS.len() - 1),
            Key::Top => cursor = 0,
            Key::Bottom => cursor = CLEAN_ITEMS.len() - 1,
            Key::Space => selected[cursor] = !selected[cursor],
            Key::Number(n) if (1..=CLEAN_ITEMS.len() as u8).contains(&n) => {
                selected[n as usize - 1] = !selected[n as usize - 1];
                cursor = n as usize - 1;
            }
            Key::Enter | Key::Right => {
                let categories: Vec<Category> = CLEAN_ITEMS
                    .iter()
                    .enumerate()
                    .filter_map(|(i, (_, category))| selected[i].then_some(*category).flatten())
                    .collect();
                let recycle_bin = selected[5];
                if categories.is_empty() && !recycle_bin {
                    message = "Select at least one cleaning area".into();
                    continue;
                }
                let only = (categories.len() != Category::ALL.len()).then_some(categories);
                return Some(CleanSelection { only, recycle_bin });
            }
            Key::Quit | Key::Left => return None,
            Key::PageUp | Key::PageDown | Key::Other | Key::Number(_) => {}
        }
    }
}

fn draw_clean(cursor: usize, selected: &[bool; 6], style: &Style, message: &str) {
    let mut out = String::from("\x1b[H\x1b[2J\n");
    out.push_str(&format!("  {}\n", style.bold("Clean your PC")));
    out.push_str(&format!(
        "  {}\n\n",
        style.dim("Choose what to scan and remove")
    ));
    for (i, (title, _)) in CLEAN_ITEMS.iter().enumerate() {
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
        style.dim("↑↓/j/k move  •  Space toggle  •  Enter scan  •  q cancel")
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
    let mut items = Vec::new();
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(PathBuf::from);
    if let Some(home) = home {
        items.push(("Home", home.clone()));
        for (label, name) in [
            ("Desktop", "Desktop"),
            ("Downloads", "Downloads"),
            ("Documents", "Documents"),
            ("Pictures", "Pictures"),
        ] {
            let path = home.join(name);
            if path.is_dir() {
                items.push((label, path));
            }
        }
    }
    if items.is_empty() {
        return None;
    }
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
            let row = format!("  {}  {:<12} {}  ", i + 1, label, path.display());
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
        match term::read_key(&mut stdin) {
            Key::Up => cursor = cursor.saturating_sub(1),
            Key::Down => cursor = (cursor + 1).min(items.len() - 1),
            Key::Top => cursor = 0,
            Key::Bottom => cursor = items.len() - 1,
            Key::Number(n) if (1..=items.len() as u8).contains(&n) => {
                cursor = n as usize - 1;
                let path = items[cursor].1.clone();
                drop(screen);
                drop(raw);
                return Some(AnalyzeSelection { path });
            }
            Key::Enter | Key::Right => {
                let path = items[cursor].1.clone();
                drop(screen);
                drop(raw);
                return Some(AnalyzeSelection { path });
            }
            Key::Quit | Key::Left => {
                drop(screen);
                drop(raw);
                return None;
            }
            Key::Space | Key::PageUp | Key::PageDown | Key::Other | Key::Number(_) => {}
        }
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
    }
}
