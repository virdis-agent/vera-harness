use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::Result;
use crossterm::{
    cursor::{self, MoveTo, RestorePosition, SavePosition},
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    style::{Color, Stylize},
    terminal::{self, Clear, ClearType},
};

use crate::safety::PermissionMode;
use crate::sessions::Message;

pub struct Dashboard<'a> {
    pub version: &'a str,
    pub root: &'a PathBuf,
    pub instructions: &'a [String],
    pub skills: &'a [String],
    pub prompts: &'a [String],
    pub extensions: &'a [String],
    pub mcp_servers: usize,
    pub mcp_available: usize,
    pub provider: &'a str,
    pub model: &'a str,
    pub effort: &'a str,
    pub context_tokens: usize,
    pub context_limit: usize,
    pub context_estimated: bool,
    pub mode: PermissionMode,
    pub messages: &'a [Message],
}

#[derive(Default)]
pub struct UiState {
    pub draft: String,
    /// Number of wrapped transcript lines hidden below the viewport.
    pub scroll_offset: usize,
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
const PLAN: Color = Color::Rgb {
    r: 112,
    g: 166,
    b: 232,
};
const YOLO: Color = Color::Rgb {
    r: 224,
    g: 108,
    b: 117,
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

fn mode_color(mode: PermissionMode) -> Color {
    match mode {
        PermissionMode::Plan => PLAN,
        PermissionMode::Confirm => GOLD,
        PermissionMode::Auto => TEAL,
        PermissionMode::Yolo => YOLO,
    }
}

pub struct DashboardFrame {
    line: String,
    footer_line: String,
    mcp_servers: usize,
    mode: PermissionMode,
    output_row: u16,
    interactive: bool,
}

impl DashboardFrame {
    pub fn finish_input(self) -> Result<()> {
        let mut stdout = io::stdout();
        if self.interactive {
            execute!(stdout, MoveTo(0, self.output_row))?;
        } else {
            draw_decorations(
                &self.line,
                &self.footer_line,
                self.mcp_servers,
                self.mode,
                false,
            );
        }
        stdout.flush()?;
        Ok(())
    }
}

pub fn render_dashboard(dashboard: &Dashboard<'_>, state: &UiState) -> Result<DashboardFrame> {
    let mut stdout = io::stdout();
    let (terminal_width, terminal_height) = terminal::size().unwrap_or((100, 40));
    let width = terminal_width as usize;
    execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;

    println!(
        "{} {}",
        "vera".with(TEAL),
        format!("v{}", dashboard.version).with(MUTED)
    );
    println!(
        "{}",
        "shift+tab switch mode · ctrl+c twice or ctrl+d exit · /commands for help · /compact context"
            .with(MUTED)
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

    let transcript = transcript_lines(dashboard.messages, width.saturating_sub(2).max(20));
    if !transcript.is_empty() {
        let current_row = cursor::position().map(|(_, row)| row).unwrap_or(0);
        let reserved = 5_u16;
        let available = terminal_height
            .saturating_sub(current_row)
            .saturating_sub(reserved)
            .max(2) as usize;
        let max_offset = transcript.len().saturating_sub(available);
        let end = transcript
            .len()
            .saturating_sub(state.scroll_offset.min(max_offset));
        let start = end.saturating_sub(available);
        for line in &transcript[start..end] {
            println!("{line}");
        }
        if start > 0 || end < transcript.len() {
            println!(
                "{}",
                format!(
                    "  history {}/{} · PgUp/PgDn · Home/End",
                    end,
                    transcript.len()
                )
                .with(MUTED)
            );
        }
    }
    println!(
        "{}",
        format!(
            "MCP: {}/{} server{} active",
            dashboard.mcp_servers,
            dashboard.mcp_available,
            if dashboard.mcp_servers == 1 { "" } else { "s" }
        )
        .with(TEAL)
    );
    println!();

    let line = "─".repeat(width.max(24));

    let percent = if dashboard.context_limit == 0 {
        0.0
    } else {
        dashboard.context_tokens as f64 / dashboard.context_limit as f64 * 100.0
    };
    let estimate = if dashboard.context_estimated {
        "estimated"
    } else {
        "provider"
    };
    let footer_left = format!(
        "$0.000 (sub)  {}/{}k ({estimate}, {:.1}%)",
        dashboard.context_tokens,
        dashboard.context_limit / 1_000,
        percent
    );
    let footer_right = format!(
        "({}) {} • {}",
        dashboard.provider, dashboard.model, dashboard.effort
    );
    let footer_gap = width.saturating_sub(footer_left.len() + footer_right.len());
    let footer_line = format!("{}{}{}", footer_left, " ".repeat(footer_gap), footer_right);
    let interactive = stdout.is_terminal();
    if interactive {
        println!("{}", line.clone().with(mode_color(dashboard.mode)));
    } else {
        println!("{line}");
    }
    print!("{}{}", "> ".with(mode_color(dashboard.mode)), state.draft);
    stdout.flush()?;
    let (_, prompt_row) = if interactive {
        cursor::position()?
    } else {
        (0, 0)
    };
    let output_row = prompt_row.saturating_add(4);
    if interactive {
        execute!(
            stdout,
            SavePosition,
            MoveTo(0, prompt_row.saturating_add(1))
        )?;
        draw_decorations(
            &line,
            &footer_line,
            dashboard.mcp_servers,
            dashboard.mode,
            true,
        );
        execute!(stdout, RestorePosition)?;
        stdout.flush()?;
    }
    Ok(DashboardFrame {
        line,
        footer_line,
        mcp_servers: dashboard.mcp_servers,
        mode: dashboard.mode,
        output_row,
        interactive,
    })
}

fn draw_decorations(
    line: &str,
    footer_line: &str,
    mcp_servers: usize,
    mode: PermissionMode,
    colored: bool,
) {
    if colored {
        let color = mode_color(mode);
        println!("{}", line.with(color));
        println!("{}", footer_line.with(MUTED));
        print!("{} ", format!("MCP: {mcp_servers}/4 servers").with(TEAL));
        println!("{}", format!("⏵ {}", mode.label()).with(color));
    } else {
        println!("{line}");
        println!("{footer_line}");
        println!("MCP: {mcp_servers} active ⏵ {}", mode.label());
    }
}

pub enum InputAction {
    Submit(String),
    CycleMode,
    Cancel,
    Exit,
    ScrollUp,
    ScrollDown,
    ScrollTop,
    ScrollBottom,
    Resize,
}

pub fn read_input(ctrl_c_pending: &mut bool) -> Result<InputAction> {
    read_chat_input(ctrl_c_pending, &mut UiState::default())
}

pub fn read_chat_input(ctrl_c_pending: &mut bool, state: &mut UiState) -> Result<InputAction> {
    let stdin = io::stdin();
    if !stdin.is_terminal() || !io::stdout().is_terminal() {
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            *ctrl_c_pending = false;
            return Ok(InputAction::Exit);
        }
        *ctrl_c_pending = false;
        return Ok(InputAction::Submit(line));
    }

    terminal::enable_raw_mode()?;
    let result = read_raw_input(ctrl_c_pending, state);
    terminal::disable_raw_mode()?;
    result
}

fn read_raw_input(ctrl_c_pending: &mut bool, state: &mut UiState) -> Result<InputAction> {
    let mut stdout = io::stdout();
    loop {
        match event::read()? {
            Event::Paste(text) => {
                *ctrl_c_pending = false;
                state.draft.push_str(&text);
                print!("{text}");
                stdout.flush()?;
            }
            Event::Key(key) => {
                let is_ctrl_c = matches!(key.code, KeyCode::Char('c'))
                    && key.modifiers.contains(KeyModifiers::CONTROL);
                if !is_ctrl_c {
                    *ctrl_c_pending = false;
                }
                match key.code {
                    KeyCode::BackTab => {
                        return Ok(InputAction::CycleMode);
                    }
                    KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        return Ok(InputAction::CycleMode);
                    }
                    KeyCode::Enter => {
                        print!("\r\n");
                        stdout.flush()?;
                        state.scroll_offset = 0;
                        return Ok(InputAction::Submit(std::mem::take(&mut state.draft)));
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        print!("^C\r\n");
                        stdout.flush()?;
                        return Ok(handle_ctrl_c(ctrl_c_pending));
                    }
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        print!("\r\n");
                        stdout.flush()?;
                        return Ok(InputAction::Exit);
                    }
                    KeyCode::Char(character) => {
                        state.draft.push(character);
                        print!("{character}");
                        stdout.flush()?;
                    }
                    KeyCode::Backspace => {
                        if state.draft.pop().is_some() {
                            print!("\x08 \x08");
                            stdout.flush()?;
                        }
                    }
                    KeyCode::Esc => {
                        return Ok(InputAction::Cancel);
                    }
                    KeyCode::PageUp => return Ok(InputAction::ScrollUp),
                    KeyCode::PageDown => return Ok(InputAction::ScrollDown),
                    KeyCode::Home => return Ok(InputAction::ScrollTop),
                    KeyCode::End => return Ok(InputAction::ScrollBottom),
                    _ => {}
                }
            }
            Event::Resize(_, _) => return Ok(InputAction::Resize),
            _ => {}
        }
    }
}

fn transcript_lines(messages: &[Message], width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for message in messages {
        let label = if message.role == "user" {
            "You"
        } else {
            "Vera"
        };
        let indent = " ".repeat(label.len() + 2);
        let content_width = width.saturating_sub(indent.len()).max(10);
        let mut first = true;
        for paragraph in message
            .content
            .lines()
            .chain(message.content.is_empty().then_some(""))
        {
            let wrapped = wrap_text(paragraph, content_width);
            for part in wrapped {
                if first {
                    lines.push(format!("{label}: {part}"));
                    first = false;
                } else {
                    lines.push(format!("{indent}{part}"));
                }
            }
        }
        lines.push(String::new());
    }
    lines
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn handle_ctrl_c(ctrl_c_pending: &mut bool) -> InputAction {
    if *ctrl_c_pending {
        *ctrl_c_pending = false;
        InputAction::Exit
    } else {
        *ctrl_c_pending = true;
        InputAction::Cancel
    }
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
    use super::{InputAction, mode_color, transcript_lines, wrap};
    use crate::safety::PermissionMode;
    use crate::sessions::Message;

    #[test]
    fn wraps_context_lists() {
        let entries = vec!["one".into(), "two".into(), "three".into(), "four".into()];
        let lines = wrap(&entries, 10);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| line.len() <= 10));
    }

    #[test]
    fn transcript_labels_and_wraps_conversation_messages() {
        let messages = vec![
            Message {
                role: "user".into(),
                content: "a message that must wrap".into(),
            },
            Message {
                role: "assistant".into(),
                content: "visible response".into(),
            },
        ];
        let lines = transcript_lines(&messages, 16);
        assert!(lines.iter().any(|line| line.starts_with("You: ")));
        assert!(lines.iter().any(|line| line.starts_with("Vera: ")));
        assert!(lines.iter().all(|line| line.chars().count() <= 16));
    }

    #[test]
    fn permission_modes_have_distinct_colors() {
        let colors = [
            mode_color(PermissionMode::Plan),
            mode_color(PermissionMode::Confirm),
            mode_color(PermissionMode::Auto),
            mode_color(PermissionMode::Yolo),
        ];
        for (index, color) in colors.iter().enumerate() {
            assert!(!colors[index + 1..].contains(color));
        }
    }

    #[test]
    fn second_ctrl_c_exits() {
        let mut ctrl_c_pending = false;

        assert!(matches!(
            super::handle_ctrl_c(&mut ctrl_c_pending),
            InputAction::Cancel
        ));
        assert!(ctrl_c_pending);
        assert!(matches!(
            super::handle_ctrl_c(&mut ctrl_c_pending),
            InputAction::Exit
        ));
        assert!(!ctrl_c_pending);
    }
}
