// jj-workspace: a Herdr plugin to create/remove Jujutsu (jj) workspaces,
// mirroring Herdr's own git-worktree flow and dialog.
//
// One binary, dispatched by subcommand (set in herdr-plugin.toml):
//   open <workspace|tab>  action: resolve the focused repo, open the wizard pane
//   wizard                pane:   the worktree-style modal, `jj workspace add`, open it
//   remove                action: `jj workspace forget` + delete dir + close in Herdr
//
// The wizard renders the actual "new worktree" modal using the same TUI stack as
// Herdr (ratatui + crossterm), ported from herdr's src/ui/dialogs.rs and
// src/ui/widgets.rs so it looks and behaves like the built-in dialog.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame, Terminal,
};

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("open") => cmd_open(args.get(2).map(String::as_str).unwrap_or("workspace")),
        Some("wizard") => cmd_wizard(),
        Some("remove") => cmd_remove(),
        other => {
            eprintln!("usage: jj-workspace <open [workspace|tab] | wizard | remove>");
            eprintln!("got: {other:?}");
            process::exit(2);
        }
    }
}

/// Action (headless): figure out which repo is focused, then open the wizard
/// pane, handing it the repo and open-mode via `--env`.
fn cmd_open(mode: &str) -> ! {
    let ctx = env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default();
    let repo = json_string_field(&ctx, "workspace_cwd")
        .or_else(|| json_string_field(&ctx, "focused_pane_cwd"))
        .unwrap_or_default();

    let mut cmd = Command::new(herdr_bin());
    cmd.args([
        "plugin",
        "pane",
        "open",
        "--plugin",
        &plugin_id(),
        "--entrypoint",
        "wizard",
    ])
    .arg("--env")
    .arg(format!("JJ_REPO={repo}"))
    .arg("--env")
    .arg(format!("JJ_OPEN={mode}"))
    .arg("--focus");
    match cmd.status() {
        Ok(status) => process::exit(status.code().unwrap_or(0)),
        Err(err) => {
            eprintln!("error: failed to open wizard pane: {err}");
            process::exit(1);
        }
    }
}

/// Pane (interactive TTY): the worktree-style modal, then create + open.
fn cmd_wizard() -> ! {
    if which("jj").is_none() {
        fail("jj not found on PATH");
    }
    let mode = env::var("JJ_OPEN").unwrap_or_else(|_| "workspace".into());

    let mut repo = env::var("JJ_REPO").unwrap_or_default();
    if repo.is_empty() || !is_jj_workspace(&repo) {
        repo = prompt("jj repo path: ");
    }
    let repo = repo.trim_end_matches('/').to_string();
    if !is_jj_workspace(&repo) {
        fail(&format!("{repo} is not a jj workspace"));
    }

    let repo_name = basename(&repo);
    let root = workspaces_root();
    let default_name = generated_name(seed());
    let bookmarks = match load_bookmarks(&repo) {
        Ok(bookmarks) if !bookmarks.is_empty() => bookmarks,
        Ok(_) => fail("no jj bookmarks found to base the workspace on"),
        Err(err) => fail(&err),
    };

    // Run the ported worktree modal; None = the user pressed esc.
    let WizardOutcome {
        workspace_name,
        base_revset,
    } = match run_wizard(&repo_name, &root, default_name, bookmarks) {
        Ok(Some(outcome)) => outcome,
        Ok(None) => process::exit(0),
        Err(err) => fail(&format!("terminal error: {err}")),
    };

    let slug = workspace_name_to_path_slug(&workspace_name);
    let dest_path = root.join(&repo_name).join(&slug);
    if dest_path.exists() {
        fail(&format!("checkout already exists: {}", dest_path.display()));
    }
    // jj workspace add does not create intermediate dirs (Herdr create_dir_all's
    // the parent before `git worktree add` for the same reason).
    if let Some(parent) = dest_path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            fail(&format!("could not create {}: {err}", parent.display()));
        }
    }
    let dest = dest_path.display().to_string();

    // Base the new workspace's working-copy commit on the selected bookmark.
    eprintln!("+ jj workspace add --name {workspace_name} --revision {base_revset} {dest}");
    let mut add = Command::new("jj");
    add.current_dir(&repo).args([
        "workspace",
        "add",
        "--name",
        &workspace_name,
        "--revision",
        &base_revset,
        &dest,
    ]);
    run_or(add, "jj workspace add", fail);

    let target = if mode == "tab" { "tab" } else { "workspace" };
    eprintln!("+ herdr {target} create --cwd {dest}");
    let mut open = Command::new(herdr_bin());
    open.args([
        target,
        "create",
        "--cwd",
        &dest,
        "--label",
        &workspace_name,
        "--focus",
    ]);
    run_or(open, &format!("herdr {target} create"), fail);
    process::exit(0);
}

/// Action (headless): forget the current jj workspace, delete it, close in Herdr.
fn cmd_remove() -> ! {
    if which("jj").is_none() {
        die("jj not found on PATH");
    }
    let ctx = env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default();
    let ws = env::var("HERDR_WORKSPACE_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| json_string_field(&ctx, "workspace_id"));
    let cwd = json_string_field(&ctx, "workspace_cwd").unwrap_or_default();
    if cwd.is_empty() {
        die("no workspace cwd in context");
    }

    let canon = match fs::canonicalize(&cwd) {
        Ok(p) => p,
        Err(err) => die(&format!("cannot resolve {cwd}: {err}")),
    };
    if !canon.join(".jj").exists() {
        die(&format!("{} is not a jj workspace", canon.display()));
    }
    // The MAIN workspace stores .jj/repo as a directory; a secondary workspace
    // stores it as a file pointer. Never remove the main workspace.
    if canon.join(".jj").join("repo").is_dir() {
        die(&format!(
            "refusing to remove the MAIN jj workspace ({})",
            canon.display()
        ));
    }
    if canon == Path::new("/") || canon.parent().is_none() {
        die(&format!(
            "refusing to remove unsafe path: {}",
            canon.display()
        ));
    }

    let mut forget = Command::new("jj");
    forget.current_dir(&canon).args(["workspace", "forget"]);
    run_or(forget, "jj workspace forget", die);

    if let Err(err) = fs::remove_dir_all(&canon) {
        die(&format!("failed to delete {}: {err}", canon.display()));
    }

    match ws {
        Some(ws) => {
            let mut close = Command::new(herdr_bin());
            close.args(["workspace", "close", &ws]);
            run_or(close, "herdr workspace close", die);
        }
        None => eprintln!("warning: no workspace id in context; Herdr workspace left open"),
    }
    println!("removed jj workspace: {}", canon.display());
    process::exit(0);
}

// --- wizard TUI (ported from herdr src/ui/dialogs.rs + widgets.rs) ----------

/// Herdr's catppuccin palette (src/app/state.rs `Palette::catppuccin`).
struct Palette {
    accent: Color,
    panel_bg: Color,
    surface0: Color,
    surface_dim: Color,
    overlay0: Color,
    text: Color,
    subtext0: Color,
    red: Color,
}

fn catppuccin() -> Palette {
    Palette {
        accent: Color::Rgb(137, 180, 250),
        panel_bg: Color::Rgb(24, 24, 37),
        surface0: Color::Rgb(49, 50, 68),
        surface_dim: Color::Rgb(30, 30, 46),
        overlay0: Color::Rgb(108, 112, 134),
        text: Color::Rgb(205, 214, 244),
        subtext0: Color::Rgb(166, 173, 200),
        red: Color::Rgb(243, 139, 168),
    }
}

struct WizardOutcome {
    workspace_name: String,
    base_revset: String,
}

/// Returns Some(outcome) on "create and open", None on cancel (esc / ctrl-c).
fn run_wizard(
    repo_name: &str,
    root: &Path,
    initial: String,
    bookmarks: Vec<String>,
) -> io::Result<Option<WizardOutcome>> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;

    // The generated name is prefilled but acts as a placeholder: the first edit
    // replaces it wholesale (mirrors herdr's `name_input_replace_on_type`).
    let mut name = initial;
    let mut replace_on_type = true;
    let mut selected_bookmark = 0usize;
    let mut error: Option<String> = None;
    let outcome = loop {
        let _ = terminal.draw(|frame| {
            draw_wizard(
                frame,
                &name,
                repo_name,
                root,
                &bookmarks,
                selected_bookmark,
                error.as_deref(),
            )
        });
        match event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => break None,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break None,
                KeyCode::Enter => {
                    if bookmarks.is_empty() {
                        error = Some("select a base bookmark".into());
                    } else if valid_workspace_name(&name) {
                        break Some(WizardOutcome {
                            workspace_name: name.clone(),
                            base_revset: bookmarks[selected_bookmark].clone(),
                        });
                    } else {
                        error = Some("workspace name must match [A-Za-z0-9._/-]".into());
                    }
                }
                KeyCode::Up => {
                    selected_bookmark = selected_bookmark.saturating_sub(1);
                    error = None;
                }
                KeyCode::Down => {
                    if selected_bookmark + 1 < bookmarks.len() {
                        selected_bookmark += 1;
                    }
                    error = None;
                }
                KeyCode::PageUp => {
                    selected_bookmark = selected_bookmark.saturating_sub(5);
                    error = None;
                }
                KeyCode::PageDown => {
                    selected_bookmark =
                        (selected_bookmark + 5).min(bookmarks.len().saturating_sub(1));
                    error = None;
                }
                KeyCode::Home => {
                    selected_bookmark = 0;
                    error = None;
                }
                KeyCode::End => {
                    selected_bookmark = bookmarks.len().saturating_sub(1);
                    error = None;
                }
                KeyCode::Backspace => {
                    if replace_on_type {
                        name.clear();
                        replace_on_type = false;
                    } else {
                        name.pop();
                    }
                    error = None;
                }
                KeyCode::Char(c) => {
                    if replace_on_type {
                        name.clear();
                        replace_on_type = false;
                    }
                    name.push(c);
                    error = None;
                }
                _ => {}
            },
            Ok(_) => {}
            Err(err) => {
                let _ = restore_terminal(&mut terminal);
                return Err(err);
            }
        }
    };

    restore_terminal(&mut terminal)?;
    Ok(outcome)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

/// Mirrors herdr's `render_new_linked_worktree_overlay`, with a bookmark picker.
fn draw_wizard(
    frame: &mut Frame,
    name: &str,
    repo_name: &str,
    root: &Path,
    bookmarks: &[String],
    selected_bookmark: usize,
    error: Option<&str>,
) {
    let p = catppuccin();
    let area = frame.area();
    dim_background(frame, area);
    let Some(inner) = render_modal_shell(frame, area, 76, 18, &p) else {
        return;
    };
    if inner.height < 12 {
        return;
    }

    let list_height = inner.height.saturating_sub(11).max(1).min(5);
    let checkout_label_y = 7 + list_height;
    let checkout_value_y = 8 + list_height;
    let status_y = inner.height.saturating_sub(2);
    let line = |offset: u16| Rect::new(inner.x, inner.y + offset, inner.width, 1);
    render_modal_header(frame, line(0), "new jj workspace", &p);

    frame.render_widget(
        Paragraph::new(" workspace").style(Style::default().fg(p.overlay0)),
        line(2),
    );
    let input_rect = line(3);
    frame.render_widget(Clear, input_rect);
    frame.render_widget(
        Paragraph::new(format!(" {name}█")).style(Style::default().fg(p.text).bg(p.surface0)),
        input_rect,
    );

    let selected = selected_bookmark.min(bookmarks.len().saturating_sub(1));
    let base_label = if bookmarks.is_empty() {
        " base bookmark".to_string()
    } else {
        format!(" base bookmark ({}/{})  ↑/↓", selected + 1, bookmarks.len())
    };
    frame.render_widget(
        Paragraph::new(base_label).style(Style::default().fg(p.overlay0)),
        line(5),
    );
    render_bookmark_list(
        frame,
        Rect::new(inner.x, inner.y + 6, inner.width, list_height),
        bookmarks,
        selected,
        &p,
    );

    let checkout = root
        .join(repo_name)
        .join(workspace_name_to_path_slug(name))
        .display()
        .to_string();
    frame.render_widget(
        Paragraph::new(" checkout").style(Style::default().fg(p.overlay0)),
        line(checkout_label_y),
    );
    frame.render_widget(
        Paragraph::new(format!(
            " {}",
            fit_text(&checkout, inner.width.saturating_sub(1))
        ))
        .style(Style::default().fg(p.subtext0)),
        line(checkout_value_y),
    );

    let status = if let Some(error) = error {
        Paragraph::new(format!(" {error}")).style(Style::default().fg(p.red))
    } else {
        Paragraph::new(" enter: create  esc: cancel  arrows: change base bookmark")
            .style(Style::default().fg(p.overlay0))
    };
    frame.render_widget(status, line(status_y));

    let (create_rect, cancel_rect) = button_rects(inner);
    render_action_button(
        frame,
        create_rect,
        Some("↵"),
        "create and open",
        Style::default()
            .fg(panel_contrast_fg(&p))
            .bg(p.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(p.text)
            .bg(p.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

fn render_bookmark_list(
    frame: &mut Frame,
    area: Rect,
    bookmarks: &[String],
    selected_bookmark: usize,
    p: &Palette,
) {
    frame.render_widget(Clear, area);
    if bookmarks.is_empty() {
        frame.render_widget(
            Paragraph::new(" no bookmarks found").style(Style::default().fg(p.red)),
            Rect::new(area.x, area.y, area.width, 1),
        );
        return;
    }

    let visible_rows = area.height as usize;
    let selected = selected_bookmark.min(bookmarks.len() - 1);
    let mut start = selected.saturating_sub(visible_rows / 2);
    if start + visible_rows > bookmarks.len() {
        start = bookmarks.len().saturating_sub(visible_rows);
    }

    for row in 0..visible_rows {
        let idx = start + row;
        if idx >= bookmarks.len() {
            break;
        }
        let selected_row = idx == selected;
        let marker = if selected_row { "›" } else { " " };
        let more_marker = if row == 0 && start > 0 {
            "↑"
        } else if row + 1 == visible_rows && idx + 1 < bookmarks.len() {
            "↓"
        } else {
            marker
        };
        let text = fit_text(&format!(" {more_marker} {}", bookmarks[idx]), area.width);
        let style = if selected_row {
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text).bg(p.surface0)
        };
        frame.render_widget(
            Paragraph::new(text).style(style),
            Rect::new(area.x, area.y + row as u16, area.width, 1),
        );
    }
}

fn fit_text(text: &str, width: u16) -> String {
    let max = width as usize;
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return "…".into();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

// Ported verbatim from herdr's src/ui/widgets.rs / src/ui.rs.

fn dim_background(frame: &mut Frame, area: Rect) {
    let buf = frame.buffer_mut();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            let cell = &mut buf[(x, y)];
            cell.set_style(cell.style().add_modifier(Modifier::DIM));
        }
    }
}

fn render_modal_shell(frame: &mut Frame, area: Rect, w: u16, h: u16, p: &Palette) -> Option<Rect> {
    let popup = centered_popup_rect(area, w, h)?;
    render_panel_shell(frame, popup, p.accent, p.panel_bg)
}

fn render_panel_shell(frame: &mut Frame, area: Rect, border: Color, bg: Color) -> Option<Rect> {
    if area.width < 2 || area.height < 2 {
        return None;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .border_set(symbols::border::PLAIN)
        .style(Style::default().bg(bg));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    Some(inner)
}

fn centered_popup_rect(area: Rect, popup_w: u16, popup_h: u16) -> Option<Rect> {
    let popup_w = popup_w.min(area.width.saturating_sub(4));
    let popup_h = popup_h.min(area.height.saturating_sub(2));
    if popup_w < 4 || popup_h < 4 {
        return None;
    }
    let popup_x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let popup_y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    Some(Rect::new(popup_x, popup_y, popup_w, popup_h))
}

fn render_modal_header(frame: &mut Frame, area: Rect, title: &str, p: &Palette) {
    let line = Line::from(vec![Span::styled(
        title,
        Style::default().fg(p.text).add_modifier(Modifier::BOLD),
    )]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_action_button(
    frame: &mut Frame,
    rect: Rect,
    hint: Option<&str>,
    label: &str,
    style: Style,
) {
    frame.render_widget(
        Paragraph::new(action_button_text(hint, label))
            .style(style)
            .alignment(Alignment::Center),
        rect,
    );
}

fn action_button_text(hint: Option<&str>, label: &str) -> String {
    match hint {
        Some(hint) => format!(" {hint} {label} "),
        None => format!(" {label} "),
    }
}

fn panel_contrast_fg(p: &Palette) -> Color {
    match p.panel_bg {
        Color::Reset => p.surface_dim,
        color => color,
    }
}

/// Herdr's `new_linked_worktree_button_rects`: a centered "create / cancel" row.
fn button_rects(inner: Rect) -> (Rect, Rect) {
    let create = action_button_text(Some("↵"), "create and open")
        .chars()
        .count() as u16;
    let cancel = action_button_text(Some("esc"), "cancel").chars().count() as u16;
    let gap = 2u16;
    let total = create + cancel + gap;
    let mut x = inner.x + inner.width.saturating_sub(total) / 2;
    let y = inner.y + inner.height.saturating_sub(1);
    let create_rect = Rect::new(x, y, create, 1);
    x = x.saturating_add(create).saturating_add(gap);
    let cancel_rect = Rect::new(x, y, cancel, 1);
    (create_rect, cancel_rect)
}

// --- naming (mirrors src/worktree.rs in herdr) -----------------------------

const ADJECTIVES: [&str; 8] = [
    "brave", "calm", "clear", "green", "lucky", "quiet", "rapid", "silver",
];
const NOUNS: [&str; 8] = [
    "river", "cloud", "field", "forest", "harbor", "meadow", "stone", "valley",
];

fn generated_name(seed: u64) -> String {
    let adjective = ADJECTIVES[(seed as usize) % ADJECTIVES.len()];
    let noun = NOUNS[((seed / ADJECTIVES.len() as u64) as usize) % NOUNS.len()];
    let suffix = seed & 0xffff;
    format!("workspace/{adjective}-{noun}-{suffix:04x}")
}

fn workspace_name_to_path_slug(name: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "workspace".into()
    } else {
        trimmed
    }
}

fn seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Checkout root: $JJ_WORKSPACE_ROOT override, else ~/.herdr/workspaces.
fn workspaces_root() -> PathBuf {
    if let Some(root) = config_value("JJ_WORKSPACE_ROOT") {
        return PathBuf::from(expand_tilde(root.trim_end_matches('/')));
    }
    PathBuf::from(expand_tilde("~/.herdr/workspaces"))
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

// --- helpers ---------------------------------------------------------------

fn load_bookmarks(repo: &str) -> Result<Vec<String>, String> {
    let template = r#"name ++ "\t" ++ if(remote, remote, "") ++ "\0""#;
    let output = Command::new("jj")
        .current_dir(repo)
        .args([
            "--color",
            "never",
            "--no-pager",
            "bookmark",
            "list",
            "--all-remotes",
            "-T",
            template,
        ])
        .output()
        .map_err(|err| format!("jj bookmark list failed to start: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let details = stderr.trim();
        if details.is_empty() {
            return Err(format!(
                "jj bookmark list failed (exit {})",
                output.status.code().unwrap_or(-1)
            ));
        }
        return Err(format!("jj bookmark list failed: {details}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut bookmarks = Vec::new();
    for entry in stdout.split('\0') {
        if entry.is_empty() {
            continue;
        }
        let (name, remote) = entry.split_once('\t').unwrap_or((entry, ""));
        let name = name.trim();
        let remote = remote.trim();
        if name.is_empty() || remote == "git" {
            continue;
        }
        let label = if remote.is_empty() {
            name.to_string()
        } else {
            format!("{name}@{remote}")
        };
        if bookmarks.contains(&label) {
            continue;
        }
        bookmarks.push(label);
    }
    Ok(bookmarks)
}

fn herdr_bin() -> String {
    env::var("HERDR_BIN_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "herdr".into())
}

fn plugin_id() -> String {
    env::var("HERDR_PLUGIN_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "nathanflurry.jj-workspace".into())
}

fn is_jj_workspace(repo: &str) -> bool {
    !repo.is_empty() && Path::new(repo).join(".jj").exists()
}

fn valid_workspace_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo")
        .to_string()
}

fn config_value(key: &str) -> Option<String> {
    if let Ok(value) = env::var(key) {
        if !value.is_empty() {
            return Some(value);
        }
    }
    let dir = env::var("HERDR_PLUGIN_CONFIG_DIR").ok()?;
    let content = fs::read_to_string(Path::new(&dir).join(".env")).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                let v = v.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

fn which(cmd: &str) -> Option<()> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .find(|dir| dir.join(cmd).is_file())
        .map(|_| ())
}

fn prompt(message: &str) -> String {
    print!("{message}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
    line.trim().to_string()
}

fn run_or(cmd: Command, what: &str, on_err: fn(&str) -> !) {
    let mut cmd = cmd;
    match cmd.status() {
        Ok(status) if status.success() => {}
        Ok(status) => on_err(&format!(
            "{what} failed (exit {})",
            status.code().unwrap_or(-1)
        )),
        Err(err) => on_err(&format!("{what} failed to start: {err}")),
    }
}

fn fail(message: &str) -> ! {
    eprintln!("error: {message}");
    print!("\npress enter to close...");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
    process::exit(1);
}

fn die(message: &str) -> ! {
    eprintln!("error: {message}");
    process::exit(1);
}

fn json_string_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let after_key = json.split_once(&needle)?.1;
    let after_colon = after_key.split_once(':')?.1.trim_start();
    let value = after_colon.strip_prefix('"')?;
    let mut out = String::new();
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            out.push(match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(out);
        } else {
            out.push(ch);
        }
    }
    None
}
