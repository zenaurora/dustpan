//! Interactive command picker shown when dpan starts without arguments.

use std::io::{self, IsTerminal, Write};

use crate::term::{self, AltScreen, Key, RawMode};
use crate::ui::Style;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Clean,
    Apps,
    Analyze,
    Ctxmenu,
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
