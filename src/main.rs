// jj-workspace: a Herdr plugin to create/remove Jujutsu (jj) workspaces.
//
// One binary, dispatched by subcommand (set in herdr-plugin.toml):
//   open <workspace|tab>  action: resolve the focused repo, open the wizard pane
//   wizard                pane:   prompt a name, `jj workspace add`, then open it
//   remove                action: `jj workspace forget` + delete dir + close in Herdr
//
// No external crates: install-time `cargo build --release` stays fast/offline.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{self, Command};

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
/// pane, handing it the repo and the open-mode via `--env`.
fn cmd_open(mode: &str) -> ! {
    let ctx = env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default();
    let repo = json_string_field(&ctx, "workspace_cwd")
        .or_else(|| json_string_field(&ctx, "focused_pane_cwd"))
        .unwrap_or_default();

    let mut cmd = Command::new(herdr_bin());
    cmd.args(["plugin", "pane", "open", "--plugin", &plugin_id(), "--entrypoint", "wizard"])
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

/// Pane (interactive, has a TTY): create the jj workspace and open it in Herdr.
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

    let name = prompt("new workspace name: ");
    if !valid_name(&name) {
        fail("name must be non-empty and match [A-Za-z0-9._-]");
    }

    let dest = format!("{}/{}.{}", workspace_root(&repo), basename(&repo), name);
    if Path::new(&dest).exists() {
        fail(&format!("destination already exists: {dest}"));
    }

    eprintln!("+ jj workspace add --name {name} {dest}");
    let mut add = Command::new("jj");
    add.current_dir(&repo)
        .args(["workspace", "add", "--name", &name, &dest]);
    run_or(add, "jj workspace add", fail);

    let herdr = herdr_bin();
    let mut open = Command::new(&herdr);
    if mode == "tab" {
        eprintln!("+ herdr tab create --cwd {dest}");
        open.args(["tab", "create", "--cwd", &dest, "--label", &name, "--focus"]);
        run_or(open, "herdr tab create", fail);
    } else {
        eprintln!("+ herdr workspace create --cwd {dest}");
        open.args(["workspace", "create", "--cwd", &dest, "--label", &name, "--focus"]);
        run_or(open, "herdr workspace create", fail);
    }

    println!("done.");
    pause();
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

    // Resolve symlinks / `..` before any destructive operation.
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
        die(&format!("refusing to remove the MAIN jj workspace ({})", canon.display()));
    }
    // Belt-and-suspenders against deleting root / a filesystem boundary.
    if canon == Path::new("/") || canon.parent().is_none() {
        die(&format!("refusing to remove unsafe path: {}", canon.display()));
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
        None => eprintln!(
            "warning: no workspace id in context; Herdr workspace left open (orphaned)"
        ),
    }
    println!("removed jj workspace: {}", canon.display());
    process::exit(0);
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

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo")
        .to_string()
}

/// Where new workspaces are created: $JJ_WORKSPACE_ROOT (env or plugin `.env`),
/// else a sibling directory of the repo.
fn workspace_root(repo: &str) -> String {
    if let Some(root) = config_value("JJ_WORKSPACE_ROOT") {
        return root.trim_end_matches('/').to_string();
    }
    Path::new(repo)
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".".into())
}

/// Look up a config value: process env first, then `$HERDR_PLUGIN_CONFIG_DIR/.env`.
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

fn pause() {
    print!("\npress enter to close...");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
}

/// Run a command; on failure call `on_err` (which never returns).
fn run_or(mut cmd: Command, what: &str, on_err: fn(&str) -> !) {
    match cmd.status() {
        Ok(status) if status.success() => {}
        Ok(status) => on_err(&format!("{what} failed (exit {})", status.code().unwrap_or(-1))),
        Err(err) => on_err(&format!("{what} failed to start: {err}")),
    }
}

/// Interactive failure (in a pane): show the error, wait, then exit non-zero.
fn fail(message: &str) -> ! {
    eprintln!("error: {message}");
    pause();
    process::exit(1);
}

/// Headless failure (in an action): error to the plugin log, exit non-zero.
fn die(message: &str) -> ! {
    eprintln!("error: {message}");
    process::exit(1);
}

/// Minimal string-field extractor for the flat HERDR_PLUGIN_CONTEXT_JSON object.
/// Mirrors the approach in herdr's own rust-release-check example (no serde dep).
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
