//! A throwaway git repository for tests.

use std::{
    path::PathBuf,
    process::{Command, Stdio},
};

pub struct TempRepo {
    pub dir: PathBuf,
}

impl TempRepo {
    /// A new repository on branch `main` with no commits. `name` must be
    /// unique among tests, since they run in parallel.
    pub fn new(name: &str) -> TempRepo {
        let dir = std::env::temp_dir().join(format!("rex-git-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // rex-git runs in the canonical path, and so must the tests.
        let repo = TempRepo { dir: dir.canonicalize().unwrap() };
        repo.git(&["init", "-q", "-b", "main", "."]);
        repo
    }

    /// Run git here and return its stdout. Panics when git fails.
    pub fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Like `git`, for commands that are expected to fail (a merge with conflicts).
    #[allow(dead_code)]
    pub fn git_may_fail(&self, args: &[&str]) {
        let _ = Command::new("git")
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .output();
    }

    pub fn write(&self, path: &str, content: &str) {
        let path = self.dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// Stage everything and commit it.
    pub fn commit_all(&self, msg: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", msg]);
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
