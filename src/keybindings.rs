use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Action {
    CycleMode,
    Submit,
    Cancel,
    DeleteForward,
    DeleteBackward,
    DeleteLine,
    DeleteWord,
    CursorLeft,
    CursorRight,
    CursorHome,
    CursorEnd,
    HistoryPrevious,
    HistoryNext,
    ScrollUp,
    ScrollDown,
    ScrollTop,
    ScrollBottom,
}

impl Action {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cycle_mode" | "mode_cycle" | "cycle_permission_mode" | "cycle_modes" => {
                Some(Self::CycleMode)
            }
            "submit" => Some(Self::Submit),
            "cancel" => Some(Self::Cancel),
            "delete_forward" | "forward_delete" | "delete_to_end" => Some(Self::DeleteForward),
            "delete_backward" | "backward_delete" => Some(Self::DeleteBackward),
            "delete_line" | "line_delete" | "delete_to_start" | "delete_to_line_start" => {
                Some(Self::DeleteLine)
            }
            "delete_word" | "word_delete" | "delete_word_backward" => Some(Self::DeleteWord),
            "cursor_left" | "move_left" | "move_cursor_left" => Some(Self::CursorLeft),
            "cursor_right" | "move_right" | "move_cursor_right" => Some(Self::CursorRight),
            "cursor_home" | "move_home" | "move_cursor_home" => Some(Self::CursorHome),
            "cursor_end" | "move_end" | "move_cursor_end" => Some(Self::CursorEnd),
            "history_previous" | "history_up" | "history_prev" => Some(Self::HistoryPrevious),
            "history_next" | "history_down" => Some(Self::HistoryNext),
            "scroll_up" | "transcript_scroll_up" => Some(Self::ScrollUp),
            "scroll_down" | "transcript_scroll_down" => Some(Self::ScrollDown),
            "scroll_top" | "transcript_scroll_top" => Some(Self::ScrollTop),
            "scroll_bottom" | "transcript_scroll_bottom" => Some(Self::ScrollBottom),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::CycleMode => "cycle_mode",
            Self::Submit => "submit",
            Self::Cancel => "cancel",
            Self::DeleteForward => "delete_forward",
            Self::DeleteBackward => "delete_backward",
            Self::DeleteLine => "delete_line",
            Self::DeleteWord => "delete_word",
            Self::CursorLeft => "cursor_left",
            Self::CursorRight => "cursor_right",
            Self::CursorHome => "cursor_home",
            Self::CursorEnd => "cursor_end",
            Self::HistoryPrevious => "history_previous",
            Self::HistoryNext => "history_next",
            Self::ScrollUp => "scroll_up",
            Self::ScrollDown => "scroll_down",
            Self::ScrollTop => "scroll_top",
            Self::ScrollBottom => "scroll_bottom",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyCombination {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyCombination {
    pub fn parse(value: &str) -> Result<Self> {
        let parts = value.split('+').collect::<Vec<_>>();
        if parts.iter().any(|part| part.trim().is_empty()) {
            anyhow::bail!("key combination {value:?} contains an empty component");
        }
        let key = parts
            .last()
            .map(|part| part.trim())
            .context("key combination is empty")?;
        let mut modifiers = KeyModifiers::NONE;
        for modifier in &parts[..parts.len().saturating_sub(1)] {
            let modifier = modifier.trim().to_ascii_lowercase();
            let flag = match modifier.as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "shift" => KeyModifiers::SHIFT,
                "alt" | "option" => KeyModifiers::ALT,
                "meta" | "cmd" | "command" | "super" => KeyModifiers::SUPER,
                _ => anyhow::bail!("unknown key modifier {modifier:?} in {value:?}"),
            };
            if modifiers.contains(flag) {
                anyhow::bail!("duplicate key modifier {modifier:?} in {value:?}");
            }
            modifiers.insert(flag);
        }

        let code = match key.to_ascii_lowercase().as_str() {
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "page_up" => KeyCode::PageUp,
            "pagedown" | "page_down" => KeyCode::PageDown,
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "insert" => KeyCode::Insert,
            "space" => KeyCode::Char(' '),
            name if name.starts_with('f') && name[1..].parse::<u8>().is_ok() => {
                let number = name[1..].parse::<u8>().unwrap_or_default();
                match number {
                    1..=12 => KeyCode::F(number),
                    _ => anyhow::bail!("unknown function key {key:?}"),
                }
            }
            _ if key.chars().count() == 1 => {
                let character = key.chars().next().unwrap_or_default();
                if modifiers.is_empty() && !character.is_control() {
                    anyhow::bail!(
                        "unmodified printable-character binding {value:?} is not allowed"
                    );
                }
                KeyCode::Char(character.to_ascii_lowercase())
            }
            _ => anyhow::bail!("unknown key {key:?} in {value:?}"),
        };
        let (code, modifiers) = if code == KeyCode::BackTab {
            (KeyCode::Tab, modifiers | KeyModifiers::SHIFT)
        } else {
            (code, modifiers)
        };
        if let KeyCode::Char(character) = code
            && modifiers.is_empty()
            && !character.is_control()
        {
            anyhow::bail!("unmodified printable-character binding {value:?} is not allowed");
        }
        Ok(Self { code, modifiers })
    }

    pub fn from_event(event: KeyEvent) -> Self {
        let (code, mut modifiers) = (event.code, event.modifiers);
        let code = if code == KeyCode::BackTab {
            modifiers.insert(KeyModifiers::SHIFT);
            KeyCode::Tab
        } else {
            code
        };
        if let KeyCode::Char(character) = code {
            return Self {
                code: KeyCode::Char(character.to_ascii_lowercase()),
                modifiers,
            };
        }
        Self { code, modifiers }
    }

    pub fn label(self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("Ctrl".into());
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            parts.push("Shift".into());
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            parts.push("Alt".into());
        }
        if self.modifiers.contains(KeyModifiers::SUPER) {
            parts.push("Meta".into());
        }
        parts.push(match self.code {
            KeyCode::Enter => "Enter".into(),
            KeyCode::Esc => "Esc".into(),
            KeyCode::Backspace => "Backspace".into(),
            KeyCode::Delete => "Delete".into(),
            KeyCode::Left => "Left".into(),
            KeyCode::Right => "Right".into(),
            KeyCode::Up => "Up".into(),
            KeyCode::Down => "Down".into(),
            KeyCode::Home => "Home".into(),
            KeyCode::End => "End".into(),
            KeyCode::PageUp => "PageUp".into(),
            KeyCode::PageDown => "PageDown".into(),
            KeyCode::Tab | KeyCode::BackTab => "Tab".into(),
            KeyCode::Insert => "Insert".into(),
            KeyCode::F(number) => format!("F{number}"),
            KeyCode::Char(' ') => "Space".into(),
            KeyCode::Char(character) => character.to_ascii_uppercase().to_string(),
            _ => format!("{:?}", self.code),
        });
        parts.join("+")
    }
}

#[derive(Clone, Debug, Default)]
pub struct KeyMap {
    bindings: BTreeMap<Action, Vec<KeyCombination>>,
    lookup: HashMap<KeyCombination, Action>,
}

impl KeyMap {
    pub fn built_in() -> Self {
        Self::from_pairs([
            ("cycle_mode", "Shift+Tab"),
            ("submit", "Enter"),
            ("cancel", "Esc"),
            ("delete_forward", "Delete"),
            ("delete_forward", "Ctrl+D"),
            ("delete_backward", "Backspace"),
            ("delete_line", "Ctrl+U"),
            ("delete_word", "Ctrl+W"),
            ("cursor_left", "Left"),
            ("cursor_right", "Right"),
            ("cursor_home", "Home"),
            ("cursor_end", "End"),
            ("history_previous", "Up"),
            ("history_next", "Down"),
            ("scroll_up", "PageUp"),
            ("scroll_down", "PageDown"),
            ("scroll_top", "Ctrl+Home"),
            ("scroll_bottom", "Ctrl+End"),
        ])
        .expect("built-in keybindings are valid")
    }

    fn from_pairs<const N: usize>(pairs: [(&str, &str); N]) -> Result<Self> {
        let mut map = Self::default();
        for (command, key) in pairs {
            map.insert(
                Action::parse(command).context("unknown built-in action")?,
                KeyCombination::parse(key)?,
            );
        }
        map.validate()
    }

    fn insert(&mut self, action: Action, key: KeyCombination) {
        self.lookup.insert(key, action);
        self.bindings.entry(action).or_default().push(key);
    }

    pub fn load(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("inspect keybindings file {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            anyhow::bail!("keybindings file {} is not a regular file", path.display());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("read keybindings file {}", path.display()))?;
        let entries: Vec<RawBinding> = serde_json::from_str(&text).map_err(|error| {
            anyhow::anyhow!("parse keybindings file {}: {error}", path.display())
        })?;
        let mut map = Self::default();
        for (index, entry) in entries.into_iter().enumerate() {
            let action = Action::parse(&entry.command).with_context(|| {
                format!(
                    "keybindings file {} entry {index}: unknown command {:?}",
                    path.display(),
                    entry.command
                )
            })?;
            let key = KeyCombination::parse(&entry.key).map_err(|error| {
                anyhow::anyhow!(
                    "keybindings file {} entry {index}: {error:#}",
                    path.display()
                )
            })?;
            if key == KeyCombination::parse("Ctrl+C")? {
                anyhow::bail!(
                    "keybindings file {} entry {index}: Ctrl+C is reserved for emergency exit",
                    path.display()
                );
            }
            if map.lookup.contains_key(&key) {
                anyhow::bail!(
                    "keybindings file {} entry {index}: key {} is already bound",
                    path.display(),
                    key.label()
                );
            }
            map.insert(action, key);
        }
        map.validate().map_err(|error| {
            anyhow::anyhow!("validate keybindings file {}: {error:#}", path.display())
        })
    }

    pub fn validate(&self) -> Result<Self> {
        let required = [Action::Submit, Action::Cancel];
        for action in required {
            if !self.bindings.contains_key(&action) {
                anyhow::bail!("missing required keybinding for {}", action.label());
            }
        }
        let mut seen = HashSet::new();
        for (action, keys) in &self.bindings {
            if keys.is_empty() {
                anyhow::bail!("keybinding action {} has no keys", action.label());
            }
            for key in keys {
                if !seen.insert(*key) {
                    anyhow::bail!("key {} is bound more than once", key.label());
                }
            }
        }
        Ok(self.clone())
    }

    pub fn action_for(&self, event: KeyEvent) -> Option<Action> {
        self.lookup.get(&KeyCombination::from_event(event)).copied()
    }

    pub fn bindings(&self) -> impl Iterator<Item = (Action, &[KeyCombination])> {
        self.bindings
            .iter()
            .map(|(action, keys)| (*action, keys.as_slice()))
    }

    pub fn default_json() -> String {
        let entries = Self::built_in()
            .bindings()
            .flat_map(|(action, keys)| {
                keys.iter().map(move |key| {
                    serde_json::json!({
                        "command": action.label(),
                        "key": key.label(),
                    })
                })
            })
            .collect::<Vec<_>>();
        format!(
            "{}\n",
            serde_json::to_string_pretty(&entries).expect("keybindings serialize")
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBinding {
    command: String,
    key: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_combinations_and_maps_events() {
        let key = KeyCombination::parse("Shift+Tab").unwrap();
        assert_eq!(key.code, KeyCode::Tab);
        assert!(key.modifiers.contains(KeyModifiers::SHIFT));
        let map = KeyMap::built_in();
        assert_eq!(
            map.action_for(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Submit)
        );
        assert_eq!(
            map.action_for(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            Some(Action::DeleteLine)
        );
    }

    #[test]
    fn rejects_conflicts_reserved_keys_and_unmodified_printables() {
        let path = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            path.path(),
            r#"[{"command":"submit","key":"Enter"},{"command":"cancel","key":"Enter"}]"#,
        )
        .unwrap();
        assert!(KeyMap::load(path.path()).is_err());
        assert!(KeyCombination::parse("x").is_err());
        fs::write(path.path(), r#"[{"command":"submit","key":"Enter"},{"command":"cancel","key":"Esc"},{"command":"cursor_left","key":"Ctrl+C"}]"#).unwrap();
        assert!(KeyMap::load(path.path()).is_err());
    }

    #[test]
    fn requires_submit_and_cancel() {
        let path = tempfile::NamedTempFile::new().unwrap();
        fs::write(path.path(), r#"[{"command":"submit","key":"Enter"}]"#).unwrap();
        assert!(KeyMap::load(path.path()).is_err());
    }

    #[test]
    fn accepts_multiple_keys_for_one_action_when_they_do_not_conflict() {
        let path = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            path.path(),
            r#"[
  {"command":"submit","key":"Enter"},
  {"command":"submit","key":"Ctrl+J"},
  {"command":"cancel","key":"Esc"}
]"#,
        )
        .unwrap();
        let map = KeyMap::load(path.path()).unwrap();
        assert_eq!(
            map.action_for(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            Some(Action::Submit)
        );
    }

    #[test]
    fn malformed_file_error_names_path_and_location() {
        let path = tempfile::NamedTempFile::new().unwrap();
        fs::write(path.path(), "[\n  {\"command\": \"submit\"\n").unwrap();
        let error = KeyMap::load(path.path()).unwrap_err().to_string();
        assert!(error.contains(&path.path().display().to_string()));
        assert!(error.contains("line"));
    }
}
