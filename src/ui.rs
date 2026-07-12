use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    cursor::{self, MoveTo, MoveToColumn, RestorePosition, SavePosition},
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyModifiers},
    execute,
    style::{Color, ResetColor, SetBackgroundColor, Stylize},
    terminal::{self, Clear, ClearType},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::safety::PermissionMode;
use crate::sessions::{DisplayMode, Message};

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
    pub display_mode: DisplayMode,
}

#[derive(Default)]
pub struct UiState {
    pub draft: String,
    cursor_index: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
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
    mcp_available: usize,
    mode: PermissionMode,
    display_mode: DisplayMode,
    output_row: u16,
    interactive: bool,
}

impl DashboardFrame {
    pub fn finish_input(self) -> Result<()> {
        let mut stdout = io::stdout();
        if self.interactive {
            execute!(stdout, ResetColor, MoveTo(0, self.output_row))?;
        } else {
            draw_decorations(
                Decorations {
                    line: &self.line,
                    footer_line: &self.footer_line,
                    mcp_servers: self.mcp_servers,
                    mcp_available: self.mcp_available,
                    mode: self.mode,
                    display_mode: self.display_mode,
                },
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

    let line = "─".repeat(width.saturating_sub(1).max(23));

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
        "{} / {} ({estimate}, {:.1}%)",
        format_tokens(dashboard.context_tokens),
        format_tokens(dashboard.context_limit),
        percent
    );
    let footer_right = format!(
        "({}) {} • {}",
        dashboard.provider, dashboard.model, dashboard.effort
    );
    let footer_line = fit_status_line(&footer_left, &footer_right, width);
    let interactive = stdout.is_terminal();
    let input_bg = adaptive_input_background();
    if interactive {
        execute!(
            stdout,
            SetBackgroundColor(input_bg),
            Clear(ClearType::CurrentLine)
        )?;
        println!();
        execute!(
            stdout,
            SetBackgroundColor(input_bg),
            Clear(ClearType::CurrentLine)
        )?;
        print!("  {} {}", "›".with(mode_color(dashboard.mode)), state.draft);
    } else {
        println!("{line}");
        print!("{}{}", "> ".with(mode_color(dashboard.mode)), state.draft);
    }
    stdout.flush()?;
    let (_, prompt_row) = if interactive {
        cursor::position()?
    } else {
        (0, 0)
    };
    let output_row = prompt_row.saturating_add(5);
    if interactive {
        execute!(
            stdout,
            SavePosition,
            MoveTo(0, prompt_row.saturating_add(1)),
            SetBackgroundColor(input_bg),
            Clear(ClearType::CurrentLine)
        )?;
        println!();
        execute!(stdout, ResetColor)?;
        draw_decorations(
            Decorations {
                line: &line,
                footer_line: &footer_line,
                mcp_servers: dashboard.mcp_servers,
                mcp_available: dashboard.mcp_available,
                mode: dashboard.mode,
                display_mode: dashboard.display_mode,
            },
            true,
        );
        execute!(stdout, RestorePosition, SetBackgroundColor(input_bg))?;
        stdout.flush()?;
    }
    Ok(DashboardFrame {
        line,
        footer_line,
        mcp_servers: dashboard.mcp_servers,
        mcp_available: dashboard.mcp_available,
        mode: dashboard.mode,
        display_mode: dashboard.display_mode,
        output_row,
        interactive,
    })
}

struct Decorations<'a> {
    line: &'a str,
    footer_line: &'a str,
    mcp_servers: usize,
    mcp_available: usize,
    mode: PermissionMode,
    display_mode: DisplayMode,
}

fn draw_decorations(decorations: Decorations<'_>, colored: bool) {
    let Decorations {
        line,
        footer_line,
        mcp_servers,
        mcp_available,
        mode,
        display_mode,
    } = decorations;
    if colored {
        let color = mode_color(mode);
        println!("{}", line.with(color));
        println!("{}", footer_line.with(MUTED));
        print!(
            "{} ",
            format!("MCP: {mcp_servers}/{mcp_available}").with(TEAL)
        );
        println!(
            "{}",
            format!("⏵ {} · tools {}", mode.label(), display_mode.label()).with(color)
        );
    } else {
        println!("{line}");
        println!("{footer_line}");
        println!(
            "MCP: {mcp_servers}/{mcp_available} ⏵ {} · tools {}",
            mode.label(),
            display_mode.label()
        );
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
    if let Err(error) = execute!(io::stdout(), EnableBracketedPaste) {
        terminal::disable_raw_mode()?;
        return Err(error.into());
    }
    let result = read_raw_input(ctrl_c_pending, state);
    let paste_disable = execute!(io::stdout(), DisableBracketedPaste);
    let raw_disable = terminal::disable_raw_mode();
    match (result, paste_disable, raw_disable) {
        (Ok(action), Ok(()), Ok(())) => Ok(action),
        (Err(error), _, _) => Err(error),
        (_, Err(error), _) => Err(error.into()),
        (_, _, Err(error)) => Err(error.into()),
    }
}

fn read_raw_input(ctrl_c_pending: &mut bool, state: &mut UiState) -> Result<InputAction> {
    let mut stdout = io::stdout();
    state.cursor_index = state.cursor_index.min(state.draft.chars().count());
    let prompt_row = cursor::position()?.1;
    loop {
        match event::read()? {
            Event::Paste(text) => {
                *ctrl_c_pending = false;
                insert_at_cursor(state, &text.replace(['\r', '\n'], " "));
                redraw_draft(&mut stdout, prompt_row, state)?;
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
                        state.scroll_offset = 0;
                        let submitted = std::mem::take(&mut state.draft);
                        if !submitted.trim().is_empty() && state.history.last() != Some(&submitted)
                        {
                            state.history.push(submitted.clone());
                        }
                        state.cursor_index = 0;
                        state.history_index = None;
                        state.history_draft.clear();
                        return Ok(InputAction::Submit(submitted));
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(handle_ctrl_c(ctrl_c_pending));
                    }
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if state.draft.is_empty() {
                            return Ok(InputAction::Exit);
                        }
                        delete_at_cursor(state);
                        redraw_draft(&mut stdout, prompt_row, state)?;
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        delete_before_cursor(state, true);
                        redraw_draft(&mut stdout, prompt_row, state)?;
                    }
                    KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        delete_word_before_cursor(state);
                        redraw_draft(&mut stdout, prompt_row, state)?;
                    }
                    KeyCode::Char(character) => {
                        insert_at_cursor(state, &character.to_string());
                        redraw_draft(&mut stdout, prompt_row, state)?;
                    }
                    KeyCode::Backspace => {
                        delete_before_cursor(state, false);
                        redraw_draft(&mut stdout, prompt_row, state)?;
                    }
                    KeyCode::Delete => {
                        delete_at_cursor(state);
                        redraw_draft(&mut stdout, prompt_row, state)?;
                    }
                    KeyCode::Left => {
                        state.cursor_index = state.cursor_index.saturating_sub(1);
                        redraw_draft(&mut stdout, prompt_row, state)?;
                    }
                    KeyCode::Right => {
                        state.cursor_index =
                            (state.cursor_index + 1).min(state.draft.chars().count());
                        redraw_draft(&mut stdout, prompt_row, state)?;
                    }
                    KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(InputAction::ScrollTop);
                    }
                    KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(InputAction::ScrollBottom);
                    }
                    KeyCode::Home => {
                        state.cursor_index = 0;
                        redraw_draft(&mut stdout, prompt_row, state)?;
                    }
                    KeyCode::End => {
                        state.cursor_index = state.draft.chars().count();
                        redraw_draft(&mut stdout, prompt_row, state)?;
                    }
                    KeyCode::Up => {
                        history_up(state);
                        redraw_draft(&mut stdout, prompt_row, state)?;
                    }
                    KeyCode::Down => {
                        history_down(state);
                        redraw_draft(&mut stdout, prompt_row, state)?;
                    }
                    KeyCode::Esc => {
                        return Ok(InputAction::Cancel);
                    }
                    KeyCode::PageUp => return Ok(InputAction::ScrollUp),
                    KeyCode::PageDown => return Ok(InputAction::ScrollDown),
                    _ => {}
                }
            }
            Event::Resize(_, _) => return Ok(InputAction::Resize),
            _ => {}
        }
    }
}

fn insert_at_cursor(state: &mut UiState, value: &str) {
    let mut chars = state.draft.chars().collect::<Vec<_>>();
    let inserted = value.chars().collect::<Vec<_>>();
    chars.splice(
        state.cursor_index..state.cursor_index,
        inserted.iter().copied(),
    );
    state.cursor_index += inserted.len();
    state.draft = chars.into_iter().collect();
}

fn delete_before_cursor(state: &mut UiState, to_start: bool) {
    let mut chars = state.draft.chars().collect::<Vec<_>>();
    let start = if to_start {
        0
    } else {
        state.cursor_index.saturating_sub(1)
    };
    if start < state.cursor_index {
        chars.drain(start..state.cursor_index);
        state.cursor_index = start;
        state.draft = chars.into_iter().collect();
    }
}

fn delete_word_before_cursor(state: &mut UiState) {
    let mut chars = state.draft.chars().collect::<Vec<_>>();
    let mut start = state.cursor_index;
    while start > 0 && chars[start - 1].is_whitespace() {
        start -= 1;
    }
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    chars.drain(start..state.cursor_index);
    state.cursor_index = start;
    state.draft = chars.into_iter().collect();
}

fn delete_at_cursor(state: &mut UiState) {
    let mut chars = state.draft.chars().collect::<Vec<_>>();
    if state.cursor_index < chars.len() {
        chars.remove(state.cursor_index);
        state.draft = chars.into_iter().collect();
    }
}

fn history_up(state: &mut UiState) {
    if state.history.is_empty() {
        return;
    }
    if state.history_index.is_none() {
        state.history_draft = state.draft.clone();
    }
    let index = state
        .history_index
        .unwrap_or(state.history.len())
        .saturating_sub(1);
    state.history_index = Some(index);
    state.draft = state.history[index].clone();
    state.cursor_index = state.draft.chars().count();
}

fn history_down(state: &mut UiState) {
    let Some(index) = state.history_index else {
        return;
    };
    if index + 1 < state.history.len() {
        let next = index + 1;
        state.history_index = Some(next);
        state.draft = state.history[next].clone();
    } else {
        state.history_index = None;
        state.draft = state.history_draft.clone();
    }
    state.cursor_index = state.draft.chars().count();
}

fn redraw_draft(stdout: &mut io::Stdout, prompt_row: u16, state: &UiState) -> Result<()> {
    let width = terminal::size()
        .map(|(width, _)| width as usize)
        .unwrap_or(80);
    let available = width.saturating_sub(5).max(1);
    let chars = state.draft.chars().collect::<Vec<_>>();
    let (visible, cursor_column) = visible_input(&chars, state.cursor_index, available);
    execute!(
        stdout,
        MoveTo(0, prompt_row),
        SetBackgroundColor(adaptive_input_background()),
        Clear(ClearType::CurrentLine)
    )?;
    write!(stdout, "  {} {visible}", "›".with(TEAL))?;
    execute!(stdout, MoveTo((4 + cursor_column) as u16, prompt_row))?;
    stdout.flush()?;
    Ok(())
}

fn visible_input(input: &[char], cursor_index: usize, width: usize) -> (String, usize) {
    let mut start = cursor_index;
    let mut cursor_width = 0;
    while start > 0 {
        let character_width = input[start - 1].width().unwrap_or(0);
        if cursor_width + character_width > width.saturating_sub(1) {
            break;
        }
        start -= 1;
        cursor_width += character_width;
    }
    let mut visible = String::new();
    let mut used = 0;
    for character in &input[start..] {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > width {
            break;
        }
        visible.push(*character);
        used += character_width;
    }
    (visible, cursor_width)
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

fn adaptive_input_background() -> Color {
    let light = std::env::var("COLORFGBG")
        .ok()
        .and_then(|value| value.rsplit(';').next()?.parse::<u8>().ok())
        .is_some_and(|background| background >= 7 && background != 8);
    if light {
        Color::Rgb {
            r: 238,
            g: 238,
            b: 238,
        }
    } else {
        Color::Rgb {
            r: 42,
            g: 45,
            b: 46,
        }
    }
}

fn format_tokens(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn fit_status_line(left: &str, right: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let required = left.width() + right.width() + 1;
    if required <= width {
        return format!(
            "{left}{}{right}",
            " ".repeat(width - left.width() - right.width())
        );
    }
    truncate_width(left, width)
}

fn truncate_width(value: &str, width: usize) -> String {
    if value.width() <= width {
        return value.to_owned();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let mut result = String::new();
    let mut used = 0;
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > width - 1 {
            break;
        }
        result.push(character);
        used += character_width;
    }
    result.push('…');
    result
}

pub struct Loader {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Loader {
    pub fn start(text: &str, enabled: bool) -> Self {
        if !enabled || !io::stdout().is_terminal() {
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        }
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let text = text.to_owned();
        let handle = thread::spawn(move || {
            const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let mut index = 0;
            let mut stdout = io::stdout();
            while !thread_stop.load(Ordering::Relaxed) {
                let _ = write!(
                    stdout,
                    "\r{}",
                    format!("{} {text}", FRAMES[index % FRAMES.len()]).with(MUTED)
                );
                let _ = stdout.flush();
                index += 1;
                thread::sleep(Duration::from_millis(80));
            }
            let _ = execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine));
            let _ = stdout.flush();
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.stop();
    }
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
    use super::{
        InputAction, fit_status_line, format_tokens, mode_color, transcript_lines, truncate_width,
        visible_input, wrap,
    };
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

    #[test]
    fn token_counts_use_compact_consistent_units() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_250), "1.2k");
        assert_eq!(format_tokens(2_500_000), "2.5m");
    }

    #[test]
    fn status_line_preserves_both_sides_when_they_fit() {
        let line = fit_status_line("1.2k / 128.0k", "model • high", 40);
        assert_eq!(unicode_width::UnicodeWidthStr::width(line.as_str()), 40);
        assert!(line.starts_with("1.2k"));
        assert!(line.ends_with("model • high"));
    }

    #[test]
    fn narrow_status_and_input_are_unicode_width_safe() {
        assert_eq!(truncate_width("provider/model", 6), "provi…");
        let input = "abc界def".chars().collect::<Vec<_>>();
        let (visible, cursor) = visible_input(&input, input.len(), 5);
        assert!(unicode_width::UnicodeWidthStr::width(visible.as_str()) <= 5);
        assert!(cursor <= 4);
    }
}
