//! Shared Clack-style single-select prompt.
//!
//! One interactive picker, used by both the env-var resolver and the
//! port-conflict preflight. On a real terminal it's an arrow-key / `j`,`k`
//! radio list (crossterm raw mode); without a controlling terminal (piped
//! stdin, CI, tests) it falls back to a numbered prompt that reads the
//! injected reader. `Enter` selects; `Esc` / `Ctrl-C` / EOF returns `None`.

use std::io::{BufRead, IsTerminal, Write};

// Clack-style glyphs
const S_BAR: &str = "│";
const S_RADIO_ACTIVE: &str = "●";
const S_RADIO_INACTIVE: &str = "○";

// Colors
const C_RESET: &str = "\x1b[0m";
const C_DIM: &str = "\x1b[2m";
const C_CYAN: &str = "\x1b[36m";
const C_YELLOW: &str = "\x1b[33m";

/// Pick one of `choices`, pre-selecting `default_idx`.
///
/// Dispatches to the arrow-key picker on a TTY and the numbered fallback
/// otherwise. Returns the chosen index, or `None` if the user aborted
/// (`Esc` / `Ctrl-C` / EOF).
pub fn select_one<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    choices: &[String],
    default_idx: usize,
) -> std::io::Result<Option<usize>> {
    if std::io::stdin().is_terminal() {
        pick_choice(output, choices, default_idx)
    } else {
        pick_choice_numbered(input, output, choices, default_idx)
    }
}

/// Render N choice lines into a string for the picker.
fn format_choices(choices: &[String], selected: usize, default_idx: usize) -> String {
    let mut buf = String::new();
    for (i, choice) in choices.iter().enumerate() {
        if i == selected {
            buf.push_str(&format!(
                "  {C_DIM}{S_BAR}{C_RESET}  {C_CYAN}{S_RADIO_ACTIVE}{C_RESET} {choice}"
            ));
        } else {
            buf.push_str(&format!(
                "  {C_DIM}{S_BAR}{C_RESET}  {C_DIM}{S_RADIO_INACTIVE} {choice}{C_RESET}"
            ));
        }
        if i == default_idx {
            buf.push_str(&format!(" {C_DIM}(default){C_RESET}"));
        }
        buf.push_str("\r\n");
    }
    buf
}

/// Interactive arrow-key picker using crossterm raw mode.
/// Uses Clack's rendering strategy: track line count, move up, erase down, redraw.
/// Returns the index of the selected choice, or None on Ctrl+C / Esc.
pub fn pick_choice<W: Write>(
    output: &mut W,
    choices: &[String],
    default_idx: usize,
) -> Result<Option<usize>, std::io::Error> {
    use crossterm::{
        event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
        terminal,
    };

    let mut selected = default_idx;
    let num_choices = choices.len();

    // Hide cursor, render initial frame
    write!(output, "\x1b[?25l")?;
    let frame = format_choices(choices, selected, default_idx);
    write!(output, "{frame}")?;
    output.flush()?;
    let prev_lines = num_choices;

    terminal::enable_raw_mode()?;
    let result = loop {
        if let Ok(Event::Key(KeyEvent {
            code, modifiers, ..
        })) = event::read()
        {
            match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = if selected == 0 {
                        num_choices - 1
                    } else {
                        selected - 1
                    };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1) % num_choices;
                }
                KeyCode::Enter => break Ok(Some(selected)),
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    break Ok(None);
                }
                KeyCode::Esc => break Ok(None),
                _ => continue,
            };

            // Redraw: move to col 0, up N lines, erase down, write new frame
            write!(output, "\r\x1b[{prev_lines}A\x1b[J")?;
            let frame = format_choices(choices, selected, default_idx);
            write!(output, "{frame}")?;
            output.flush()?;
        }
    };

    terminal::disable_raw_mode()?;
    // Erase the picker: move up, erase down, show cursor
    write!(output, "\r\x1b[{prev_lines}A\x1b[J\x1b[?25h")?;
    output.flush()?;
    result
}

/// Numbered-list fallback for choice selection when no controlling terminal
/// is available (piped stdin, CI, tests) — [`pick_choice`]'s crossterm raw
/// mode needs a real TTY and `event::read()` ignores any injected reader.
/// Prints a numbered list and reads a 1-based selection from `input`; an
/// empty line takes the default. Returns `None` on EOF.
pub fn pick_choice_numbered<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    choices: &[String],
    default_idx: usize,
) -> Result<Option<usize>, std::io::Error> {
    for (i, choice) in choices.iter().enumerate() {
        let marker = if i == default_idx { " (default)" } else { "" };
        writeln!(
            output,
            "  {C_DIM}{S_BAR}{C_RESET}  {C_DIM}{}){C_RESET} {choice}{C_DIM}{marker}{C_RESET}",
            i + 1
        )?;
    }
    write!(
        output,
        "  {C_DIM}{S_BAR}{C_RESET}  {C_DIM}Enter a number (1-{}), or Enter for default ›{C_RESET} ",
        choices.len()
    )?;
    output.flush()?;

    loop {
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) => break Ok(None),
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    break Ok(Some(default_idx));
                }
                match trimmed.parse::<usize>() {
                    Ok(n) if (1..=choices.len()).contains(&n) => break Ok(Some(n - 1)),
                    _ => {
                        write!(
                            output,
                            "  {C_DIM}{S_BAR}{C_RESET}  {C_YELLOW}▲{C_RESET} {C_DIM}Enter 1-{} ›{C_RESET} ",
                            choices.len()
                        )?;
                        output.flush()?;
                    }
                }
            }
            Err(e) => break Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn numbered_accepts_valid_selection() {
        let choices = vec!["a".into(), "b".into(), "c".into()];
        let mut input = Cursor::new(b"2\n");
        let mut output = Vec::new();
        let picked = pick_choice_numbered(&mut input, &mut output, &choices, 0).unwrap();
        assert_eq!(picked, Some(1));
    }

    #[test]
    fn numbered_empty_takes_default() {
        let choices = vec!["a".into(), "b".into(), "c".into()];
        let mut input = Cursor::new(b"\n");
        let mut output = Vec::new();
        let picked = pick_choice_numbered(&mut input, &mut output, &choices, 2).unwrap();
        assert_eq!(picked, Some(2));
    }

    #[test]
    fn numbered_eof_is_none() {
        let choices = vec!["a".into(), "b".into()];
        let mut input = Cursor::new(b"");
        let mut output = Vec::new();
        let picked = pick_choice_numbered(&mut input, &mut output, &choices, 0).unwrap();
        assert_eq!(picked, None);
    }

    #[test]
    fn numbered_reprompts_on_out_of_range() {
        let choices = vec!["a".into(), "b".into()];
        let mut input = Cursor::new(b"9\n1\n");
        let mut output = Vec::new();
        let picked = pick_choice_numbered(&mut input, &mut output, &choices, 0).unwrap();
        assert_eq!(picked, Some(0));
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Enter 1-2"));
    }
}
