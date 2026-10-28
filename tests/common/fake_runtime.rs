//! The fake container runtime the sandbox tests share (`tests/fixtures/sandbox/fake-runtime.rs`),
//! compiled once for as long as its source stays what it is, and copied in as `docker` and
//! `podman`. A real program, so tmux names a pane's command after it.

// Each test file that includes `common` uses some of this, or none of it.
#![allow(dead_code)]

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const SOURCE: &str = include_str!("../fixtures/sandbox/fake-runtime.rs");

fn binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fake-sandbox-runtime");
        std::fs::create_dir_all(&dir).unwrap();
        let mut hasher = DefaultHasher::new();
        SOURCE.hash(&mut hasher);
        let binary = dir.join(format!("runtime-{:016x}", hasher.finish()));
        if !binary.exists() {
            let pid = std::process::id();
            let source = dir.join(format!("runtime-{pid}.rs"));
            let partial = dir.join(format!("runtime-{pid}.partial"));
            std::fs::write(&source, SOURCE).unwrap();
            let rustc = Path::new(env!("CARGO")).with_file_name("rustc");
            let rustc = if rustc.exists() {
                rustc
            } else {
                PathBuf::from("rustc")
            };
            let status = Command::new(rustc)
                .args(["--edition", "2021", "-o"])
                .arg(&partial)
                .arg(&source)
                .status()
                .expect("running rustc for the fake sandbox runtime");
            assert!(
                status.success(),
                "rustc could not build the fake sandbox runtime"
            );
            std::fs::rename(&partial, &binary).unwrap();
            let _ = std::fs::remove_file(&source);
        }
        binary
    })
}

/// A fake runtime installed under one root: its `bin` (to put first on `PATH`), its state and log.
pub struct FakeRuntime {
    pub bin: PathBuf,
    pub state: PathBuf,
    pub log: PathBuf,
}

impl FakeRuntime {
    pub fn install(root: &Path) -> FakeRuntime {
        let fake = FakeRuntime {
            bin: root.join("fake-bin"),
            state: root.join("fake-state"),
            log: root.join("fake-runtime.log"),
        };
        std::fs::create_dir_all(&fake.bin).unwrap();
        std::fs::create_dir_all(&fake.state).unwrap();
        for name in ["docker", "podman"] {
            std::fs::copy(binary(), fake.bin.join(name)).unwrap();
            super::exec::make_executable(fake.bin.join(name));
        }
        fake
    }

    /// What a process running the fake needs in its environment.
    pub fn env(&self) -> Vec<(String, String)> {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sandbox");
        vec![
            (
                "FAKE_RUNTIME_LOG".to_string(),
                self.log.display().to_string(),
            ),
            (
                "FAKE_RUNTIME_STATE".to_string(),
                self.state.display().to_string(),
            ),
            (
                "FAKE_RUNTIME_FIXTURES".to_string(),
                fixtures.display().to_string(),
            ),
        ]
    }

    /// Every call so far, in order: the program's name, then its arguments.
    pub fn calls(&self) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        text.split("\x1e\n")
            .filter(|call| !call.is_empty())
            .map(|call| {
                call.split('\x1f')
                    .filter(|word| !word.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .collect()
    }

    /// The calls that started an agent's container: `run`, not `run -d`.
    pub fn runs(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|call| call.get(1).map(String::as_str) == Some("run"))
            .filter(|call| !call.iter().any(|word| word == "-d"))
            .collect()
    }

    pub fn down(&self, down: bool) {
        let file = self.state.join("down");
        if down {
            std::fs::write(file, "").unwrap();
        } else {
            let _ = std::fs::remove_file(file);
        }
    }

    /// Down for the next `times` times it is asked `info`, then up: a runtime that comes up after
    /// a while, after exactly so many tries whatever the time they take.
    pub fn down_for(&self, times: u32) {
        std::fs::write(self.state.join("down"), times.to_string()).unwrap();
    }

    pub fn image(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(
            self.state
                .join(format!("image-{}", name.replace(['/', ':'], "_"))),
        )
        .ok()
    }

    pub fn built(&self, file: &str) -> String {
        std::fs::read_to_string(self.state.join("build-context").join(file)).unwrap_or_default()
    }

    /// For each call so far, its verb and the DOCKER_CONTEXT and CONTAINER_CONNECTION it was
    /// given (empty when unset): which daemon it went to.
    pub fn daemons(&self) -> Vec<(String, String, String)> {
        std::fs::read_to_string(self.state.join("daemons"))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let mut parts = line.split('\t').map(str::to_string);
                let mut next = || parts.next().unwrap_or_default();
                (next(), next(), next())
            })
            .collect()
    }

    /// When each `info` was asked, in milliseconds of the wall clock, in order.
    pub fn info_times(&self) -> Vec<u128> {
        std::fs::read_to_string(self.state.join("info-at"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.parse().ok())
            .collect()
    }

    /// The file whose appearance ends a `run` started with `FAKE_RUNTIME_STOP` pointing at it.
    pub fn stop_file(&self) -> PathBuf {
        self.state.join("stop")
    }
}
