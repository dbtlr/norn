//! anstyle styles + color-when resolution.

use std::env;
use std::io::IsTerminal;

use anstyle::{AnsiColor, Style};

use crate::cli::ColorWhen;

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub key: Style,
    pub separator: Style,
    pub footer: Style,
    // Reserved for future JSON/JSONL coloring (deferred polish).
    #[allow(dead_code)]
    pub json_key: Style,
    #[allow(dead_code)]
    pub json_string: Style,
    #[allow(dead_code)]
    pub enabled: bool,
}

impl Palette {
    pub fn none() -> Self {
        Self {
            key: Style::new(),
            separator: Style::new(),
            footer: Style::new(),
            json_key: Style::new(),
            json_string: Style::new(),
            enabled: false,
        }
    }

    pub fn ansi() -> Self {
        Self {
            key: Style::new().fg_color(Some(AnsiColor::Cyan.into())).dimmed(),
            separator: Style::new().dimmed(),
            footer: Style::new().dimmed().italic(),
            json_key: Style::new().fg_color(Some(AnsiColor::Cyan.into())),
            json_string: Style::new().fg_color(Some(AnsiColor::Green.into())),
            enabled: true,
        }
    }
}

pub fn resolve_palette(when: ColorWhen) -> Palette {
    match when {
        ColorWhen::Always => Palette::ansi(),
        ColorWhen::Never => Palette::none(),
        ColorWhen::Auto => {
            if env::var_os("NO_COLOR").is_some() {
                return Palette::none();
            }
            if env::var_os("CLICOLOR_FORCE").is_some() {
                return Palette::ansi();
            }
            if std::io::stdout().is_terminal() {
                Palette::ansi()
            } else {
                Palette::none()
            }
        }
    }
}
