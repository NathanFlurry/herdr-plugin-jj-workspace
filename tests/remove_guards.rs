use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!("jj-workspace-{label}-{}-{nonce}", process::id()));
        fs::create_dir_all(&path).expect("create temporary directory");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
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
fn remove_refuses_changed_workspace_without_forgetting_or_deleting_it() {
    let temp = TempDir::new("remove-dirty");
    let bin = temp.path.join("bin");
    let workspace = temp.path.join("task-safety");
    let log = temp.path.join("commands.log");
    fs::create_dir_all(workspace.join(".jj")).expect("create secondary workspace metadata");
    fs::write(workspace.join(".jj/repo"), "../repo\n").expect("create jj workspace pointer");
    fs::create_dir_all(&bin).expect("create fake bin");
    write_executable(
        &bin.join("jj"),
        "#!/bin/sh\nprintf 'jj %s\\n' \"$*\" >> \"$TEST_LOG\"\nif [ \"$1\" = log ] && [ \"$3\" = '@ ~ empty()' ]; then\n  printf '%s\\n' 'deadbeef'\nelif [ \"$1\" = log ]; then\n  printf '%s\\n' 'task-safety'\nelif [ \"$1\" = workspace ] && [ \"$2\" = list ]; then\n  printf 'task-safety\\ttask-safety\\n'\nfi\nexit 0\n",
    );
    write_executable(
        &bin.join("herdr"),
        "#!/bin/sh\nprintf 'herdr %s\\n' \"$*\" >> \"$TEST_LOG\"\nexit 0\n",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_jj-workspace"))
        .arg("remove")
        .env("PATH", &bin)
        .env("TEST_LOG", &log)
        .env("HERDR_BIN_PATH", bin.join("herdr"))
        .env(
            "HERDR_PLUGIN_CONTEXT_JSON",
            format!(
                r#"{{"workspace_id":"workspace-1","workspace_cwd":"{}"}}"#,
                workspace.display()
            ),
        )
        .output()
        .expect("run plugin");

    assert!(
        !output.status.success(),
        "changed workspaces must be refused"
    );
    assert!(workspace.exists(), "refusal must preserve the checkout");
    let commands = fs::read_to_string(log).expect("read fake command log");
    assert!(!commands.contains("jj workspace forget"));
}
