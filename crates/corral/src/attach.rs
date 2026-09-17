//! The `corral attach` picker.
//!
//! A daemon serves one workspace, so there is never more than one row to
//! choose from today. The listing is still what tells a user what is
//! already running and how busy it is. The formatting and the parse are
//! pure and tested; the two launchers at the bottom are process I/O.

use corrald::protocol::WorkspaceInfo;
use std::io::Write as _;

/// One display line per workspace, in listing order, for both the fzf
/// feed and the numbered prompt. The leading `N. ` is the index the
/// numbered prompt accepts and `parse_selection` reads back.
pub fn lines(workspaces: &[WorkspaceInfo]) -> Vec<String> {
    workspaces
        .iter()
        .enumerate()
        .map(|(i, w)| {
            format!(
                "{}. {}  {}  {}",
                i + 1,
                w.id,
                count(w.panes, "pane"),
                count(w.clients, "client"),
            )
        })
        .collect()
}

fn count(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// The workspace a selection names, or `None` for no choice: blank input,
/// an out-of-range index, or a line that names nothing in the list.
///
/// Accepts a bare index (`2`, or `2.` as displayed), a bare workspace id,
/// or a whole display line, which is what fzf echoes back for the row the
/// user highlighted.
pub fn parse_selection(input: &str, workspaces: &[WorkspaceInfo]) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(index) = trimmed.trim_end_matches('.').parse::<usize>() {
        return index
            .checked_sub(1)
            .and_then(|i| workspaces.get(i))
            .map(|w| w.id.clone());
    }
    workspaces
        .iter()
        .find(|w| trimmed == w.id || id_of_line(trimmed) == Some(w.id.as_str()))
        .map(|w| w.id.clone())
}

/// The id field of a display line: the token after the `N. ` index, which
/// is where `lines` puts it. Workspace ids come from socket file stems, so
/// they carry no whitespace and cannot be split across tokens.
fn id_of_line(line: &str) -> Option<&str> {
    line.split_whitespace().nth(1)
}

/// Ask which workspace to attach to: fzf when it is installed, a numbered
/// prompt otherwise. `None` means the user declined, from a blank answer
/// or an fzf cancel, and is not an error.
pub fn choose(workspaces: &[WorkspaceInfo]) -> Option<String> {
    let display = lines(workspaces);
    let selection = match run_fzf(&display) {
        Ok(selection) => selection,
        // Not installed: read a number instead.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => run_prompt(&display),
        Err(e) => {
            eprintln!("corral: fzf failed: {e}");
            None
        }
    };
    parse_selection(selection.as_deref().unwrap_or_default(), workspaces)
}

/// Feed the display lines to fzf and read back the highlighted one.
/// `Ok(None)` is the user cancelling (Esc or Ctrl-C); `Err(NotFound)` is
/// fzf not being installed at all, which is the caller's cue to prompt.
fn run_fzf(lines: &[String]) -> std::io::Result<Option<String>> {
    let mut child = std::process::Command::new("fzf")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let fed = (|| -> std::io::Result<()> {
            for line in lines {
                writeln!(stdin, "{line}")?;
            }
            Ok(())
        })();
        // Dropping stdin closes the feed, which is fzf's signal that the
        // list is complete. A cancel closes the far end instead, so the
        // write hits a closed pipe: that is the user choosing nothing.
        if let Err(e) = fed
            && e.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(e);
        }
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let picked = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!picked.is_empty()).then_some(picked))
}

/// Print the numbered list and read one line from stdin. `None` on blank
/// input or EOF.
fn run_prompt(lines: &[String]) -> Option<String> {
    for line in lines {
        println!("{line}");
    }
    print!("attach to which? [1-{}] ", lines.len());
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    match std::io::stdin().read_line(&mut input) {
        // EOF (Ctrl-D) is a cancel; a blank line is no choice either, and
        // `parse_selection` reads both as `None`.
        Ok(0) | Err(_) => None,
        Ok(_) => Some(input),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(id: &str, panes: usize, clients: usize) -> WorkspaceInfo {
        WorkspaceInfo {
            id: id.into(),
            panes,
            clients,
        }
    }

    #[test]
    fn lines_number_each_workspace_and_count_its_panes_and_clients() {
        let listed = vec![workspace("corral-501", 2, 1), workspace("corral-777", 0, 3)];
        assert_eq!(
            lines(&listed),
            vec![
                "1. corral-501  2 panes  1 client".to_string(),
                "2. corral-777  0 panes  3 clients".to_string(),
            ]
        );
    }

    #[test]
    fn a_lone_pane_and_client_are_not_pluralized() {
        assert_eq!(
            lines(&[workspace("w", 1, 1)]),
            vec!["1. w  1 pane  1 client".to_string()]
        );
    }

    #[test]
    fn every_display_line_names_exactly_one_workspace() {
        // The picker hands fzf these lines and parses what comes back, so
        // the two halves have to agree.
        let listed = vec![workspace("corral-501", 2, 1), workspace("corral-777", 0, 3)];
        for (i, line) in lines(&listed).iter().enumerate() {
            assert_eq!(
                parse_selection(line, &listed).as_deref(),
                Some(listed[i].id.as_str()),
                "line {line:?} must name {}",
                listed[i].id
            );
        }
    }

    #[test]
    fn a_bare_index_selects_by_position() {
        let listed = vec![workspace("corral-501", 1, 1), workspace("corral-777", 1, 1)];
        assert_eq!(parse_selection("1", &listed).as_deref(), Some("corral-501"));
        assert_eq!(parse_selection("2", &listed).as_deref(), Some("corral-777"));
        // The displayed form carries a trailing dot; pasting it back works.
        assert_eq!(
            parse_selection("2.", &listed).as_deref(),
            Some("corral-777")
        );
        assert_eq!(
            parse_selection(" 2 \n", &listed).as_deref(),
            Some("corral-777")
        );
    }

    #[test]
    fn a_bare_id_selects_that_workspace() {
        let listed = vec![workspace("corral-501", 1, 1), workspace("corral-777", 1, 1)];
        assert_eq!(
            parse_selection("corral-777", &listed).as_deref(),
            Some("corral-777")
        );
    }

    #[test]
    fn no_choice_is_none_rather_than_an_error() {
        let listed = vec![workspace("corral-501", 1, 1)];
        for input in ["", "   ", "\n", "0", "2", "99", "nonsense"] {
            assert_eq!(
                parse_selection(input, &listed),
                None,
                "input {input:?} names no workspace"
            );
        }
        // Nothing to choose from is also no choice, not a panic.
        assert_eq!(parse_selection("1", &[]), None);
    }
}
