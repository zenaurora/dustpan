//! Persistent preferences for the argument-free workflow.
//!
//! The file is intentionally a tiny key/value format so the binary remains
//! dependency-free and a broken or hand-edited value can fall back safely.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use crate::targets::Category;

#[derive(Clone)]
pub struct Settings {
    pub categories: Vec<Category>,
    pub recycle_bin: bool,
    pub confirm_clean: bool,
    pub color: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            categories: Category::ALL.to_vec(),
            recycle_bin: false,
            confirm_clean: true,
            color: true,
        }
    }
}

impl Settings {
    pub fn load() -> Settings {
        let mut settings = Settings::default();
        let Some(path) = path() else {
            return settings;
        };
        let Ok(content) = fs::read_to_string(path) else {
            return settings;
        };
        for line in content.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "categories" => {
                    let parsed: Vec<Category> = value
                        .split(',')
                        .filter_map(|item| Category::parse(item.trim()))
                        .fold(Vec::new(), |mut categories, category| {
                            if !categories.contains(&category) {
                                categories.push(category);
                            }
                            categories
                        });
                    if value.trim().is_empty() {
                        settings.categories.clear();
                    } else if !parsed.is_empty() {
                        settings.categories = parsed;
                    }
                }
                "recycle_bin" => {
                    if let Some(value) = parse_bool(value) {
                        settings.recycle_bin = value;
                    }
                }
                "confirm_clean" => {
                    if let Some(value) = parse_bool(value) {
                        settings.confirm_clean = value;
                    }
                }
                "color" => {
                    if let Some(value) = parse_bool(value) {
                        settings.color = value;
                    }
                }
                _ => {}
            }
        }
        settings
    }

    pub fn save(&self) -> io::Result<()> {
        let path = path().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no settings directory available")
        })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let categories = self
            .categories
            .iter()
            .map(|category| category.key())
            .collect::<Vec<_>>()
            .join(",");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
        writeln!(file, "categories={categories}")?;
        writeln!(file, "recycle_bin={}", self.recycle_bin)?;
        writeln!(file, "confirm_clean={}", self.confirm_clean)?;
        writeln!(file, "color={}", self.color)
    }
}

pub fn path() -> Option<PathBuf> {
    if let Ok(appdata) = std::env::var("APPDATA") {
        return Some(PathBuf::from(appdata).join("dustpan").join("settings.conf"));
    }
    std::env::var("HOME").ok().map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("dustpan")
            .join("settings.conf")
    })
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" | "on" => Some(true),
        "false" | "no" | "0" | "off" => Some(false),
        _ => None,
    }
}
