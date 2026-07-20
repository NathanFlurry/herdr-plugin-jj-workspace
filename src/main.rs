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
    layout::{Alignment, Constraint, Layout, Rect},
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
        Some("open-abandon") => cmd_open_abandon(),
        Some("abandon") => cmd_abandon(),
        _ => {
            eprintln!(
                "usage: jj-workspace <open [workspace|tab] | wizard | remove | open-abandon | abandon>"
            );
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
    // Resolve to the MAIN workspace root. The wizard may be launched from a
    // secondary workspace (e.g. ~/.herdr/workspaces/agent-os/pkg-perf); without
    // this, repo_name would be the leaf ("pkg-perf") and new workspaces would be
    // scattered under workspaces/pkg-perf/ instead of workspaces/agent-os/.
    let repo = repo_root(&repo);

    let repo_name = basename(&repo);
    let root = workspaces_root();
    let default_branch = generated_name(seed());
    let default_base = config_value("JJ_BASE_REV").unwrap_or_else(|| "trunk()".into());

    // Run the ported worktree modal; None = the user pressed esc.
    let spec = match run_wizard(&repo_name, &root, default_branch, default_base) {
        Ok(Some(spec)) => spec,
        Ok(None) => process::exit(0),
        Err(err) => fail(&format!("terminal error: {err}")),
    };

    if let Err(err) = create_workspace(&repo, &root, &spec.name, &spec.base, &mode) {
        fail(&err);
    }
    process::exit(0);
}

fn create_workspace(
    repo: &str,
    root: &Path,
    branch: &str,
    base: &str,
    mode: &str,
) -> Result<(), String> {
    if !valid_workspace_name(branch) {
        return Err(workspace_name_error());
    }
    if base.trim().is_empty() {
        return Err("base revision cannot be empty".into());
    }
    let repo_name = basename(repo);
    let slug = branch_to_path_slug(branch);
    let dest_path = root.join(&repo_name).join(&slug);
    if dest_path.exists() {
        return Err(format!("checkout already exists: {}", dest_path.display()));
    }
    // jj workspace add does not create intermediate dirs (Herdr create_dir_all's
    // the parent before `git worktree add` for the same reason).
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
    }
    let dest = dest_path.display().to_string();

    // Use the revision selected in the wizard without changing remote-tracking
    // state. Fetching is reserved for the explicit publish workflow.
    eprintln!("+ jj workspace add --name {branch} -r {base} {dest}");
    let mut add = Command::new("jj");
    add.current_dir(repo)
        .args(["workspace", "add", "--name", branch, "-r", base, &dest]);
    if !run(add) {
        return Err("jj workspace add failed".into());
    }

    // The task bookmark is the workspace identity and must exist before the
    // checkout is exposed through Herdr.
    let mut bookmark = Command::new("jj");
    bookmark
        .current_dir(&dest)
        .args(["bookmark", "create", branch, "-r", "@"]);
    if !run(bookmark) {
        return Err(rollback_failure_message(
            repo,
            &dest_path,
            branch,
            false,
            "could not create the task bookmark",
        ));
    }

    let herdr = herdr_bin();
    let mut open = Command::new(&herdr);
    if mode == "tab" {
        eprintln!("+ herdr tab create --cwd {dest}");
        open.args([
            "tab", "create", "--cwd", &dest, "--label", branch, "--focus",
        ]);
        let output = open.output().map_err(|err| {
            rollback_failure_message(
                repo,
                &dest_path,
                branch,
                true,
                &format!("herdr tab create failed to start: {err}"),
            )
        })?;
        io::stderr().write_all(&output.stderr).ok();
        if !output.status.success() {
            return Err(rollback_failure_message(
                repo,
                &dest_path,
                branch,
                true,
                &format!(
                    "herdr tab create failed (exit {})",
                    output.status.code().unwrap_or(-1)
                ),
            ));
        }
        // When this wizard's overlay pane exits, herdr restores focus to the
        // tab the overlay was opened from, clobbering --focus. Re-focus the new
        // tab from a detached helper that outlives the overlay.
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(tab_id) = json_string_field(&stdout, "tab_id") {
            let script = format!(
                "for _ in 1 2 3 4 5 6; do sleep 0.2; '{herdr}' tab focus '{tab_id}' >/dev/null 2>&1; done"
            );
            let mut helper = Command::new("sh");
            helper
                .args(["-c", &script])
                .stdin(process::Stdio::null())
                .stdout(process::Stdio::null())
                .stderr(process::Stdio::null());
            // Detach into its own process group: the wizard's group gets SIGHUP
            // when the overlay PTY closes, which would kill the helper first.
            {
                use std::os::unix::process::CommandExt;
                helper.process_group(0);
            }
            let _ = helper.spawn();
        }
    } else {
        eprintln!("+ herdr workspace create --cwd {dest}");
        open.args([
            "workspace",
            "create",
            "--cwd",
            &dest,
            "--label",
            branch,
            "--focus",
        ]);
        if !run(open) {
            return Err(rollback_failure_message(
                repo,
                &dest_path,
                branch,
                true,
                "herdr workspace create failed",
            ));
        }
    }
    Ok(())
}

fn rollback_failure_message(
    repo: &str,
    destination: &Path,
    branch: &str,
    bookmark_exists: bool,
    reason: &str,
) -> String {
    match rollback_empty_workspace(repo, destination, branch, bookmark_exists) {
        Ok(()) => format!(
            "{reason}; rolled back the empty workspace at {}",
            destination.display()
        ),
        Err(rollback) => format!("{reason}; {rollback}"),
    }
}

fn rollback_empty_workspace(
    repo: &str,
    destination: &Path,
    branch: &str,
    bookmark_exists: bool,
) -> Result<(), String> {
    if !workspace_is_empty(destination) {
        return Err(format!(
            "preserved non-empty workspace at {} for recovery",
            destination.display()
        ));
    }

    let mut forget = Command::new("jj");
    forget
        .current_dir(repo)
        .args(["workspace", "forget", branch]);
    if !run(forget) {
        return Err(format!(
            "could not forget empty workspace {}; preserved {}",
            branch,
            destination.display()
        ));
    }

    let bookmark_error = if bookmark_exists {
        let mut bookmark = Command::new("jj");
        bookmark
            .current_dir(repo)
            .args(["bookmark", "delete", branch]);
        (!run(bookmark)).then(|| format!("could not delete task bookmark {branch}"))
    } else {
        None
    };
    fs::remove_dir_all(destination).map_err(|err| {
        format!(
            "could not delete empty workspace {}: {err}",
            destination.display()
        )
    })?;
    match bookmark_error {
        Some(error) => Err(format!(
            "{error}; removed the empty workspace at {}",
            destination.display()
        )),
        None => Ok(()),
    }
}

fn workspace_is_empty(cwd: &Path) -> bool {
    let output = Command::new("jj")
        .current_dir(cwd)
        .args(["log", "-r", "@ ~ empty()", "--no-graph", "-T", "commit_id"])
        .output();
    matches!(output, Ok(output) if output.status.success() && output.stdout.iter().all(u8::is_ascii_whitespace))
}

enum RemovalPolicy<'a> {
    ClosedPullRequest,
    Abandon { typed_name: &'a str },
}

fn removal_context() -> (String, Option<String>) {
    let ctx = env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default();
    let workspace_id = env::var("HERDR_WORKSPACE_ID")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| json_string_field(&ctx, "workspace_id"));
    let cwd = json_string_field(&ctx, "workspace_cwd").unwrap_or_default();
    (cwd, workspace_id)
}

fn cmd_open_abandon() -> ! {
    let (cwd, workspace_id) = removal_context();
    let mut command = Command::new(herdr_bin());
    command
        .args([
            "plugin",
            "pane",
            "open",
            "--plugin",
            &plugin_id(),
            "--entrypoint",
            "abandon-wizard",
        ])
        .arg("--env")
        .arg(format!("JJ_ABANDON_CWD={cwd}"))
        .arg("--env")
        .arg(format!(
            "JJ_ABANDON_WORKSPACE_ID={}",
            workspace_id.unwrap_or_default()
        ))
        .arg("--focus");
    match command.status() {
        Ok(status) => process::exit(status.code().unwrap_or(0)),
        Err(err) => die(&format!("failed to open abandon confirmation: {err}")),
    }
}

fn cmd_abandon() -> ! {
    let cwd = env::var("JJ_ABANDON_CWD").unwrap_or_default();
    let workspace_id = env::var("JJ_ABANDON_WORKSPACE_ID")
        .ok()
        .filter(|value| !value.is_empty());
    let typed_name = prompt("type the exact task name to abandon: ");
    let canon = match remove_workspace(
        &cwd,
        workspace_id.as_deref(),
        RemovalPolicy::Abandon {
            typed_name: &typed_name,
        },
    ) {
        Ok(canon) => canon,
        Err(err) => fail(&err),
    };
    println!("abandoned jj workspace: {}", canon.display());
    process::exit(0);
}

/// Action (headless): forget the current clean jj workspace, delete it, and close in Herdr.
fn cmd_remove() -> ! {
    if which("jj").is_none() {
        die("jj not found on PATH");
    }
    let (cwd, workspace_id) = removal_context();
    let canon = match remove_workspace(
        &cwd,
        workspace_id.as_deref(),
        RemovalPolicy::ClosedPullRequest,
    ) {
        Ok(canon) => canon,
        Err(err) => die(&err),
    };
    println!("removed jj workspace: {}", canon.display());
    process::exit(0);
}

fn remove_workspace(
    cwd: &str,
    workspace_id: Option<&str>,
    policy: RemovalPolicy<'_>,
) -> Result<PathBuf, String> {
    if cwd.is_empty() {
        return Err("no workspace cwd in context".into());
    }
    let canon = fs::canonicalize(cwd).map_err(|err| format!("cannot resolve {cwd}: {err}"))?;
    if !canon.join(".jj").exists() {
        return Err(format!("{} is not a jj workspace", canon.display()));
    }
    // The main workspace owns the jj store at .jj/repo. This structural guard
    // remains authoritative even if that workspace is renamed.
    if canon.join(".jj").join("repo").is_dir() {
        return Err(format!(
            "refusing to remove the MAIN jj workspace ({})",
            canon.display()
        ));
    }
    let name = current_workspace_name(&canon)?;
    if name == "default" {
        return Err(format!(
            "refusing to remove the MAIN jj workspace ({})",
            canon.display()
        ));
    }
    let expected_slug = branch_to_path_slug(&name);
    if !valid_branch(&name)
        || canon.file_name().and_then(|part| part.to_str()) != Some(expected_slug.as_str())
    {
        return Err(format!(
            "refusing to remove workspace with mismatched task identity ({})",
            canon.display()
        ));
    }
    if !workspace_is_empty(&canon) {
        return Err(format!(
            "refusing to remove changed workspace ({})",
            canon.display()
        ));
    }
    match policy {
        RemovalPolicy::ClosedPullRequest if !pull_request_is_closed(&canon, &name)? => {
            return Err(format!(
                "refusing to remove {name}: its pull request is still open"
            ));
        }
        RemovalPolicy::Abandon { typed_name } if typed_name != name => {
            return Err(format!(
                "refusing to abandon {name}: typed task name did not match"
            ));
        }
        _ => {}
    }

    let mut forget = Command::new("jj");
    forget.current_dir(&canon).args(["workspace", "forget"]);
    if !run(forget) {
        return Err("jj workspace forget failed".into());
    }
    fs::remove_dir_all(&canon)
        .map_err(|err| format!("failed to delete {}: {err}", canon.display()))?;

    if let Some(workspace_id) = workspace_id {
        let mut close = Command::new(herdr_bin());
        close.args(["workspace", "close", workspace_id]);
        if !run(close) {
            return Err(format!(
                "herdr workspace close failed after deleting {}",
                canon.display()
            ));
        }
    } else {
        eprintln!("warning: no workspace id in context; Herdr workspace left open");
    }
    Ok(canon)
}

fn pull_request_is_closed(cwd: &Path, branch: &str) -> Result<bool, String> {
    let output = Command::new("gh")
        .current_dir(cwd)
        .args([
            "pr",
            "view",
            branch,
            "--json",
            "state,mergedAt",
            "--jq",
            "if .mergedAt != null then \"MERGED\" else .state end",
        ])
        .output()
        .map_err(|err| format!("could not verify pull request state: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "could not verify a closed pull request for {branch}; use explicit abandon if no PR exists"
        ));
    }
    Ok(matches!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "CLOSED" | "MERGED"
    ))
}

fn current_workspace_name(cwd: &Path) -> Result<String, String> {
    let current = Command::new("jj")
        .current_dir(cwd)
        .args(["log", "-r", "@", "--no-graph", "-T", "commit_id"])
        .output()
        .map_err(|err| format!("could not identify jj workspace: {err}"))?;
    if !current.status.success() {
        return Err("could not identify jj workspace".into());
    }
    let current_target = String::from_utf8_lossy(&current.stdout).trim().to_string();
    if current_target.is_empty() {
        return Err("could not identify the current jj target".into());
    }

    let workspaces = Command::new("jj")
        .current_dir(cwd)
        .args([
            "workspace",
            "list",
            "-T",
            "name ++ \"\\t\" ++ target.commit_id() ++ \"\\n\"",
        ])
        .output()
        .map_err(|err| format!("could not list jj workspaces: {err}"))?;
    if !workspaces.status.success() {
        return Err("could not list jj workspaces".into());
    }
    let expected_slug = cwd.file_name().and_then(|part| part.to_str());
    let names: Vec<_> = String::from_utf8_lossy(&workspaces.stdout)
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(name, target)| {
            *target == current_target && Some(branch_to_path_slug(name).as_str()) == expected_slug
        })
        .map(|(name, _)| name.to_string())
        .collect();
    match names.as_slice() {
        [name] => Ok(name.clone()),
        [] => Err("could not match the current jj target to a workspace".into()),
        _ => Err("could not identify a unique jj workspace name".into()),
    }
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

/// Returns Some(branch) on "create and open", None on cancel (esc / ctrl-c).
#[derive(Clone, Copy, PartialEq, Eq)]
enum WizardField {
    Name,
    Base,
}

struct WorkspaceSpec {
    name: String,
    base: String,
}

/// Returns the requested workspace on "create and open", or None on cancel.
fn run_wizard(
    repo_name: &str,
    root: &Path,
    initial_name: String,
    initial_base: String,
) -> io::Result<Option<WorkspaceSpec>> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;

    let mut spec = WorkspaceSpec {
        name: initial_name,
        base: initial_base,
    };
    let mut field = WizardField::Name;
    let mut replace_name_on_type = true;
    let mut replace_base_on_type = true;
    let mut error: Option<String> = None;
    let outcome = loop {
        let _ = terminal
            .draw(|frame| draw_wizard(frame, &spec, field, repo_name, root, error.as_deref()));
        match event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => break None,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break None,
                KeyCode::Tab | KeyCode::BackTab => {
                    field = match field {
                        WizardField::Name => WizardField::Base,
                        WizardField::Base => WizardField::Name,
                    };
                    error = None;
                }
                KeyCode::Enter => {
                    if !valid_workspace_name(&spec.name) {
                        error = Some(workspace_name_error());
                    } else if spec.base.trim().is_empty() {
                        error = Some("base revision cannot be empty".into());
                    } else {
                        break Some(spec);
                    }
                }
                KeyCode::Backspace => {
                    match field {
                        WizardField::Name if replace_name_on_type => {
                            spec.name.clear();
                            replace_name_on_type = false;
                        }
                        WizardField::Name => {
                            spec.name.pop();
                        }
                        WizardField::Base if replace_base_on_type => {
                            spec.base.clear();
                            replace_base_on_type = false;
                        }
                        WizardField::Base => {
                            spec.base.pop();
                        }
                    }
                    error = None;
                }
                KeyCode::Char(c) => {
                    match field {
                        WizardField::Name => {
                            if replace_name_on_type {
                                spec.name.clear();
                                replace_name_on_type = false;
                            }
                            spec.name.push(c);
                        }
                        WizardField::Base => {
                            if replace_base_on_type {
                                spec.base.clear();
                                replace_base_on_type = false;
                            }
                            spec.base.push(c);
                        }
                    }
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

/// Mirrors herdr's `render_new_linked_worktree_overlay`.
fn draw_wizard(
    frame: &mut Frame,
    spec: &WorkspaceSpec,
    field: WizardField,
    repo_name: &str,
    root: &Path,
    error: Option<&str>,
) {
    let p = catppuccin();
    let area = frame.area();
    dim_background(frame, area);
    let Some(inner) = render_modal_shell(frame, area, 68, 12, &p) else {
        return;
    };
    if inner.height < 9 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<10>(inner);

    render_modal_header(frame, rows[0], "new jj workspace", &p);

    frame.render_widget(
        Paragraph::new(" workspace").style(Style::default().fg(p.overlay0)),
        rows[1],
    );
    let name_cursor = if field == WizardField::Name {
        "█"
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(format!(" {}{name_cursor}", spec.name))
            .style(Style::default().fg(p.text).bg(p.surface0)),
        rows[2],
    );

    frame.render_widget(
        Paragraph::new(" base revision  [tab switches fields]")
            .style(Style::default().fg(p.overlay0)),
        rows[3],
    );
    let base_cursor = if field == WizardField::Base {
        "█"
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(format!(" {}{base_cursor}", spec.base))
            .style(Style::default().fg(p.text).bg(p.surface0)),
        rows[4],
    );

    let checkout = root
        .join(repo_name)
        .join(branch_to_path_slug(&spec.name))
        .display()
        .to_string();
    frame.render_widget(
        Paragraph::new(" checkout").style(Style::default().fg(p.overlay0)),
        rows[5],
    );
    frame.render_widget(
        Paragraph::new(format!(" {checkout}")).style(Style::default().fg(p.subtext0)),
        rows[6],
    );

    if let Some(error) = error {
        frame.render_widget(
            Paragraph::new(format!(" {error}")).style(Style::default().fg(p.red)),
            rows[7],
        );
    }

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

fn valid_workspace_name(name: &str) -> bool {
    if !valid_branch(name) {
        return false;
    }
    let Some(prefixes) = config_value("JJ_WORKSPACE_NAME_PREFIXES") else {
        return true;
    };
    prefixes
        .split(',')
        .map(str::trim)
        .filter(|prefix| !prefix.is_empty())
        .any(|prefix| name.starts_with(prefix))
}

fn workspace_name_error() -> String {
    match config_value("JJ_WORKSPACE_NAME_PREFIXES") {
        Some(prefixes) => format!(
            "workspace must match [A-Za-z0-9._/-] and start with one of: {}",
            prefixes
        ),
        None => "workspace must match [A-Za-z0-9._/-]".into(),
    }
}

fn branch_to_path_slug(branch: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in branch.chars() {
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

/// Resolve any jj workspace path to its MAIN workspace root.
///
/// The main workspace stores `.jj/repo` as the store *directory*; a secondary
/// workspace stores `.jj/repo` as a *file* holding the path to the main store,
/// relative to `.jj/` (e.g. `../../../../../agent-os/.jj/repo`). Following that
/// pointer and stripping the trailing `.jj/repo` yields the repo's real root, so
/// naming + placement stay stable no matter which workspace launched the wizard.
/// Falls back to the input path if anything is unexpected.
fn repo_root(workspace: &str) -> String {
    let jj_dir = Path::new(workspace).join(".jj");
    let repo_ptr = jj_dir.join("repo");
    // Main workspace: `.jj/repo` is the store dir itself — already the root.
    if repo_ptr.is_dir() {
        return workspace.to_string();
    }
    let pointer = match fs::read_to_string(&repo_ptr) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return workspace.to_string(),
    };
    // Pointer is relative to `.jj/`; drop `repo` then `.jj` to reach the root.
    let root = jj_dir
        .join(&pointer)
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    match root.and_then(|r| fs::canonicalize(r).ok()) {
        Some(canon) => canon.display().to_string(),
        None => workspace.to_string(),
    }
}

fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && branch
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

fn run(mut cmd: Command) -> bool {
    matches!(cmd.status(), Ok(status) if status.success())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct TempDir {
        path: PathBuf,
    }

    #[test]
    fn bookmark_creation_failure_preserves_an_existing_bookmark_and_removes_the_empty_workspace() {
        let _env = ENV_LOCK.lock().expect("lock environment");
        let temp = TempDir::new("bookmark-failure");
        let repo = temp.path.join("repo");
        let workspace_root = temp.path.join("workspaces");
        fs::create_dir_all(&repo).expect("create repository");
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Test User"],
            vec!["config", "user.email", "test@example.invalid"],
        ] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .expect("run git")
                .success());
        }
        fs::write(repo.join("README.md"), "test\n").expect("write repository file");
        for args in [
            vec!["add", "README.md"],
            vec!["commit", "-qm", "base"],
            vec!["branch", "-M", "main"],
        ] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .expect("run git")
                .success());
        }
        assert!(Command::new("jj")
            .args(["git", "init", "--colocate"])
            .current_dir(&repo)
            .status()
            .expect("initialize jj")
            .success());
        assert!(Command::new("jj")
            .args(["bookmark", "create", "task-safety", "-r", "@"])
            .current_dir(&repo)
            .status()
            .expect("create existing bookmark")
            .success());

        let herdr = temp.path.join("herdr");
        write_executable(&herdr, "#!/bin/sh\nexit 1\n");
        let _herdr = EnvVarGuard::set("HERDR_BIN_PATH", &herdr);
        let result = create_workspace(
            repo.to_str().expect("utf-8 source repo"),
            &workspace_root,
            "task-safety",
            "@",
            "workspace",
        );

        assert!(result.is_err());
        assert!(!workspace_root.join("repo").join("task-safety").exists());
        assert!(Command::new("jj")
            .args(["bookmark", "list", "task-safety"])
            .current_dir(&repo)
            .output()
            .expect("list existing bookmark")
            .stdout
            .starts_with(b"task-safety:"));
    }

    #[test]
    fn configured_prefixes_enforce_stable_task_identity() {
        let _env = ENV_LOCK.lock().expect("lock environment");
        let _prefixes = EnvVarGuard::set("JJ_WORKSPACE_NAME_PREFIXES", "issue-,pr-,task-");

        assert!(valid_workspace_name("issue-42-fix-login"));
        assert!(valid_workspace_name("pr-73-review"));
        assert!(valid_workspace_name("task-docs"));
        assert!(!valid_workspace_name("workspace/random"));
        assert!(!valid_workspace_name("task bad"));
    }

    #[test]
    fn remove_refuses_a_renamed_main_workspace_before_invoking_jj() {
        let temp = TempDir::new("renamed-main");
        let repo = temp.path.join("task-safety");
        fs::create_dir_all(repo.join(".jj/repo")).expect("create main jj store");

        let error = remove_workspace(
            repo.to_str().expect("utf-8 repo"),
            None,
            RemovalPolicy::ClosedPullRequest,
        )
        .expect_err("renamed main workspace must be refused");

        assert!(error.contains("MAIN jj workspace"));
        assert!(repo.exists(), "refusal must preserve the repository");
    }

    #[test]
    fn current_workspace_name_uses_task_identity_when_workspaces_share_a_target() {
        let _env = ENV_LOCK.lock().expect("lock environment");
        let temp = TempDir::new("shared-target");
        let bin = temp.path.join("bin");
        let workspace = temp.path.join("task-safety");
        fs::create_dir_all(&bin).expect("create fake bin");
        fs::create_dir_all(&workspace).expect("create fake workspace");
        write_executable(
            &bin.join("jj"),
            "#!/bin/sh\nif [ \"$1\" = log ]; then\n  printf '%s\\n' 'target'\nelif [ \"$1\" = workspace ] && [ \"$2\" = list ]; then\n  printf 'task-safety\\ttarget\\n'\n  printf 'task-other\\ttarget\\n'\nfi\n",
        );
        let _path = EnvVarGuard::set("PATH", &bin);

        let name = current_workspace_name(&workspace).expect("identify workspace");

        assert_eq!(name, "task-safety");
    }

    #[test]
    fn cleanup_refuses_an_open_pull_request() {
        let _env = ENV_LOCK.lock().expect("lock environment");
        let temp = TempDir::new("open-pr");
        let bin = temp.path.join("bin");
        let workspace = temp.path.join("task-safety");
        let log = temp.path.join("commands.log");
        fs::create_dir_all(&bin).expect("create fake bin");
        fs::create_dir_all(workspace.join(".jj")).expect("create fake workspace");
        fs::write(workspace.join(".jj/repo"), "../repo\n").expect("create workspace pointer");
        write_executable(
            &bin.join("jj"),
            "#!/bin/sh\nprintf 'jj %s\\n' \"$*\" >> \"$TEST_LOG\"\nif [ \"$1\" = log ] && [ \"$3\" != '@ ~ empty()' ]; then\n  printf '%s\\n' 'target'\nelif [ \"$1\" = workspace ] && [ \"$2\" = list ]; then\n  printf 'task-safety\\ttarget\\n'\nfi\n",
        );
        write_executable(&bin.join("gh"), "#!/bin/sh\nprintf '%s\\n' 'OPEN'\n");
        let _path = EnvVarGuard::set("PATH", &bin);
        let _log = EnvVarGuard::set("TEST_LOG", &log);

        let error = remove_workspace(
            workspace.to_str().expect("utf-8 workspace"),
            None,
            RemovalPolicy::ClosedPullRequest,
        )
        .expect_err("open pull request must prevent cleanup");

        assert!(error.contains("still open"));
        assert!(workspace.exists());
        assert!(!fs::read_to_string(log)
            .expect("read command log")
            .contains("workspace forget"));
    }

    #[test]
    fn abandon_requires_the_exact_typed_task_name() {
        let _env = ENV_LOCK.lock().expect("lock environment");
        let temp = TempDir::new("typed-abandon");
        let bin = temp.path.join("bin");
        let workspace = temp.path.join("task-safety");
        fs::create_dir_all(&bin).expect("create fake bin");
        fs::create_dir_all(workspace.join(".jj")).expect("create fake workspace");
        fs::write(workspace.join(".jj/repo"), "../repo\n").expect("create workspace pointer");
        write_executable(
            &bin.join("jj"),
            "#!/bin/sh\nif [ \"$1\" = log ] && [ \"$3\" != '@ ~ empty()' ]; then\n  printf '%s\\n' 'target'\nelif [ \"$1\" = workspace ] && [ \"$2\" = list ]; then\n  printf 'task-safety\\ttarget\\n'\nfi\n",
        );
        let _path = EnvVarGuard::set("PATH", &bin);

        let error = remove_workspace(
            workspace.to_str().expect("utf-8 workspace"),
            None,
            RemovalPolicy::Abandon {
                typed_name: "task-other",
            },
        )
        .expect_err("mismatched typed task name must prevent abandon");

        assert!(error.contains("did not match"));
        assert!(workspace.exists());
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos();
            let path =
                env::temp_dir().join(format!("jj-workspace-{label}-{}-{nonce}", process::id()));
            fs::create_dir_all(&path).expect("create temporary directory");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let old = env::var_os(key);
            env::set_var(key, value);
            Self { key, old }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => env::set_var(self.key, value),
                None => env::remove_var(self.key),
            }
        }
    }

    fn write_executable(path: &Path, source: &str) {
        fs::write(path, source).expect("write executable");
        let mut permissions = fs::metadata(path)
            .expect("read executable metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("mark executable");
    }

    #[test]
    fn herdr_create_failure_rolls_back_only_the_untouched_workspace() {
        let _env = ENV_LOCK.lock().expect("lock environment");
        let temp = TempDir::new("rollback");
        let bin = temp.path.join("bin");
        let repo = temp.path.join("source-repo");
        let workspace_root = temp.path.join("workspaces");
        let log = temp.path.join("commands.log");
        fs::create_dir_all(&bin).expect("create fake bin");
        fs::create_dir_all(&repo).expect("create source repo");
        write_executable(
            &bin.join("jj"),
            "#!/bin/sh\nprintf 'jj %s\\n' \"$*\" >> \"$TEST_LOG\"\nif [ \"$1\" = workspace ] && [ \"$2\" = add ]; then\n  for destination; do :; done\n  /bin/mkdir -p \"$destination/.jj\"\n  : > \"$destination/.jj/repo\"\nfi\nif [ \"$1\" = status ]; then\n  printf '%s\\n' 'The working copy has no changes.'\nfi\nexit 0\n",
        );
        let herdr = bin.join("herdr");
        write_executable(
            &herdr,
            "#!/bin/sh\nprintf 'herdr %s\\n' \"$*\" >> \"$TEST_LOG\"\nexit 1\n",
        );

        let _path = EnvVarGuard::set("PATH", &bin);
        let _log = EnvVarGuard::set("TEST_LOG", &log);
        let _herdr = EnvVarGuard::set("HERDR_BIN_PATH", &herdr);
        let result = create_workspace(
            repo.to_str().expect("utf-8 source repo"),
            &workspace_root,
            "task-safety",
            "trunk()",
            "workspace",
        );

        let destination = workspace_root.join("source-repo").join("task-safety");
        assert!(result.is_err());
        assert!(
            !destination.exists(),
            "untouched workspace should be removed"
        );
        let commands = fs::read_to_string(log).expect("read fake command log");
        assert!(commands.contains("jj bookmark delete task-safety"));
        assert!(commands.contains("jj workspace forget"));
        assert!(!commands.contains("jj git fetch"));
    }
}
