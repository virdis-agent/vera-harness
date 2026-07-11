use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Result;
use crossterm::{
    cursor::MoveTo,
    execute,
    style::{Color, Stylize},
    terminal::{self, Clear, ClearType},
};

pub struct Dashboard<'a> {
    pub version: &'a str,
    pub root: &'a PathBuf,
    pub instructions: &'a [String],
    pub skills: &'a [String],
    pub prompts: &'a [String],
    pub extensions: &'a [String],
    pub mcp_servers: usize,
    pub provider: &'a str,
    pub model: &'a str,
    pub context_tokens: usize,
    pub context_limit: usize,
    pub plan_mode: bool,
}

const TEAL: Color = Color::Rgb {
    r: 116,
    g: 202,
    b: 193,
};
const GOLD: Color = Color::Rgb {
    r: 238,
    g: 193,
    b: 91,
};
const MUTED: Color = Color::Rgb {
    r: 104,
    g: 104,
    b: 104,
};
const DIM: Color = Color::Rgb {
    r: 150,
    g: 150,
    b: 150,
};
const BORDER: Color = Color::Rgb {
    r: 177,
    g: 92,
    b: 96,
};

pub fn render_dashboard(dashboard: &Dashboard<'_>) -> Result<()> {
    let mut stdout = io::stdout();
    let width = terminal::size()
        .map(|(width, _)| width as usize)
        .unwrap_or(100);
    execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;

    println!(
        "{} {}",
        "vera".with(TEAL),
        format!("v{}", dashboard.version).with(MUTED)
    );
    println!(
        "{}",
        "ctrl+d exit · /commands for help · /compact context · /plan mode".with(MUTED)
    );
    println!(
        "{}\n",
        "Vera can explain its own features and inspect the current repository.".with(MUTED)
    );
    println!(
        "{}\n",
        format!("repo: {}", dashboard.root.display()).with(MUTED)
    );

    section("Context", dashboard.instructions, width);
    section("Skills", dashboard.skills, width);
    section("Prompts", dashboard.prompts, width);
    section("Extensions", dashboard.extensions, width);
    println!();
    println!(
        "{}",
        format!(
            "MCP: {} server{} connected",
            dashboard.mcp_servers,
            if dashboard.mcp_servers == 1 { "" } else { "s" }
        )
        .with(TEAL)
    );
    println!();

    let line = "─".repeat(width.max(24));
    println!("{}", line.clone().with(BORDER));
    print!("  ");
    stdout.flush()?;
    println!();
    println!("{}", line.with(BORDER));

    let percent = if dashboard.context_limit == 0 {
        0.0
    } else {
        dashboard.context_tokens as f64 / dashboard.context_limit as f64 * 100.0
    };
    let footer_left = format!(
        "$0.000 (sub)  {:.1}%/{}k (auto)",
        percent,
        dashboard.context_limit / 1_000
    );
    let footer_right = format!("({}) {} • high", dashboard.provider, dashboard.model);
    let footer_gap = width.saturating_sub(footer_left.len() + footer_right.len());
    println!(
        "{}{}{}",
        footer_left.with(MUTED),
        " ".repeat(footer_gap),
        footer_right.with(MUTED)
    );
    println!(
        "{} {}",
        format!("MCP: {}/4 servers", dashboard.mcp_servers).with(TEAL),
        if dashboard.plan_mode {
            "⏸ Plan".with(GOLD)
        } else {
            "⏵ Safe".with(GOLD)
        }
    );
    print!("{}", "> ".with(DIM));
    stdout.flush()?;
    Ok(())
}

fn section(title: &str, entries: &[String], width: usize) {
    println!("{}", format!("[{title}]").with(GOLD));
    if entries.is_empty() {
        println!("  {}", "none".with(MUTED));
        return;
    }
    for line in wrap(entries, width.saturating_sub(4).max(20)) {
        println!("  {}", line.with(DIM));
    }
}

fn wrap(entries: &[String], width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for entry in entries {
        let next_len = if current.is_empty() {
            entry.len()
        } else {
            current.len() + 2 + entry.len()
        };
        if next_len > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str(", ");
        }
        current.push_str(entry);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::wrap;

    #[test]
    fn wraps_context_lists() {
        let entries = vec!["one".into(), "two".into(), "three".into(), "four".into()];
        let lines = wrap(&entries, 10);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| line.len() <= 10));
    }
}
