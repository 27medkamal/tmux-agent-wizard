// The fake container runtime of taw's sandbox tests. A real program, so tmux names a pane's
// command after it (`docker`), that records every call and answers as the real one does, with
// the shapes beside this file. No test ever runs a real runtime.
//
// FAKE_RUNTIME_LOG       the file each call is added to: the program's name and each argument,
//                        each ended by \x1f, the call by \x1e and a newline, in one write
// FAKE_RUNTIME_STATE     a folder: `down` there makes `info` fail as a stopped daemon does,
//                        and only that many more times when it holds a number;
//                        `image-<name>` holds the recipe label an image was built with;
//                        `ps.jsonl` and `network-subnets.txt` are what `ps` and
//                        `network inspect` print; `build-context/` gets the last build's files;
//                        `context` and `connection` are the current Docker context and Podman's
//                        default connection; `daemons` gets a line for each call: its verb, then
//                        the DOCKER_CONTEXT and CONTAINER_CONNECTION it was given, tab-separated;
//                        `info-at` gets the time each `info` was asked, in milliseconds
// FAKE_RUNTIME_FIXTURES  the folder of runtime shapes (tests/fixtures/sandbox)
// FAKE_RUNTIME_REPORT    for `run`: written to <folder>/session.json, for a `-v <folder>:/run/taw`
// FAKE_RUNTIME_STOP      for `run`: it runs until this file exists
// FAKE_RUNTIME_RUN_STATUS for `run` without FAKE_RUNTIME_STOP: the status it ends with (0)
// FAKE_RUNTIME_INFO_DELAY_MS for `info`: how long it takes to answer
// FAKE_RUNTIME_BUILD_STATUS for `build`: the status it ends with (0), its image kept only on 0
// FAKE_RUNTIME_DETACH_STATUS for `run -d`: the status it ends with (0), a container's id printed
//                        only on 0

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn fixture(name: &str) -> String {
    var("FAKE_RUNTIME_FIXTURES")
        .and_then(|dir| std::fs::read_to_string(Path::new(&dir).join(name)).ok())
        .unwrap_or_default()
}

fn state(name: &str) -> Option<PathBuf> {
    var("FAKE_RUNTIME_STATE").map(|dir| Path::new(&dir).join(name))
}

fn image_file(name: &str) -> Option<PathBuf> {
    state(&format!("image-{}", name.replace(['/', ':'], "_")))
}

fn record(program: &str, args: &[String]) {
    let Some(log) = var("FAKE_RUNTIME_LOG") else {
        return;
    };
    let mut line = String::new();
    for word in std::iter::once(program).chain(args.iter().map(String::as_str)) {
        line.push_str(word);
        line.push('\x1f');
    }
    line.push_str("\x1e\n");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// The daemon a call was sent to, as its environment names it.
fn record_daemon(args: &[String]) {
    let Some(file) = state("daemons") else {
        return;
    };
    let line = format!(
        "{}\t{}\t{}\n",
        args.first().map(String::as_str).unwrap_or(""),
        std::env::var("DOCKER_CONTEXT").unwrap_or_default(),
        std::env::var("CONTAINER_CONNECTION").unwrap_or_default()
    );
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// What the state file `name` holds, the current one, else the shape of the fixture.
fn current(name: &str, shape: &str) -> String {
    state(name)
        .and_then(|file| std::fs::read_to_string(file).ok())
        .unwrap_or_else(|| fixture(shape))
}

fn value_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].as_str())
}

fn main() {
    let program = std::env::args()
        .next()
        .and_then(|path| {
            Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    record(&program, &args);
    record_daemon(&args);
    let code = match args.first().map(String::as_str) {
        Some("info") => info(),
        Some("image") => image(&args),
        Some("build") => build(&args),
        Some("ps") => {
            if let Some(file) = state("ps.jsonl") {
                print!("{}", std::fs::read_to_string(file).unwrap_or_default());
            }
            0
        }
        Some("network") => network(&args),
        Some("context") if args.get(1).map(String::as_str) == Some("show") => {
            print!("{}", current("context", "context-show.txt"));
            0
        }
        Some("system") if args.iter().any(|arg| arg == "connection") => {
            print!("{}", current("connection", "connection-default.txt"));
            0
        }
        Some("stop") | Some("rm") => {
            if let Some(name) = args.last() {
                println!("{name}");
            }
            0
        }
        Some("run") => run(&args),
        _ => 0,
    };
    std::process::exit(code);
}

fn info() -> i32 {
    if let Some(file) = state("info-at") {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis())
            .unwrap_or(0);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
        {
            let _ = writeln!(file, "{now}");
        }
    }
    if let Some(delay) = var("FAKE_RUNTIME_INFO_DELAY_MS").and_then(|ms| ms.parse().ok()) {
        std::thread::sleep(Duration::from_millis(delay));
    }
    if let Some(down) = state("down").filter(|file| file.exists()) {
        let times = std::fs::read_to_string(&down)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok());
        match times {
            // Down that many more times: none left, and it answers.
            Some(0) => {
                let _ = std::fs::remove_file(&down);
            }
            Some(times) => {
                let _ = std::fs::write(&down, (times - 1).to_string());
                eprint!("{}", fixture("daemon-down.txt"));
                return 1;
            }
            None => {
                eprint!("{}", fixture("daemon-down.txt"));
                return 1;
            }
        }
    }
    print!("{}", fixture("info.txt"));
    0
}

fn image(args: &[String]) -> i32 {
    let name = args.last().cloned().unwrap_or_default();
    match image_file(&name).and_then(|file| std::fs::read_to_string(file).ok()) {
        Some(recipe) => {
            println!("{recipe}");
            0
        }
        None => {
            eprint!("{}", fixture("no-such-image.txt").replace("{name}", &name));
            1
        }
    }
}

fn build(args: &[String]) -> i32 {
    let name = value_after(args, "-t").unwrap_or("image").to_string();
    let recipe = args
        .iter()
        .find_map(|arg| arg.strip_prefix("taw.recipe="))
        .unwrap_or("")
        .to_string();
    let context = PathBuf::from(args.last().cloned().unwrap_or_default());
    if let Some(saved) = state("build-context") {
        let _ = std::fs::create_dir_all(&saved);
        for file in ["Dockerfile", "taw-sandbox-start"] {
            let _ = std::fs::copy(context.join(file), saved.join(file));
        }
    }
    eprint!("{}", fixture("build-progress.txt"));
    let status = var("FAKE_RUNTIME_BUILD_STATUS")
        .and_then(|status| status.parse().ok())
        .unwrap_or(0);
    if status == 0 {
        if let Some(file) = image_file(&name) {
            let _ = std::fs::write(file, &recipe);
        }
    }
    status
}

fn network(args: &[String]) -> i32 {
    match args.get(1).map(String::as_str) {
        Some("inspect") => {
            if let Some(file) = state("network-subnets.txt") {
                print!("{}", std::fs::read_to_string(file).unwrap_or_default());
            }
            0
        }
        Some("create") => {
            println!("4f6c2a9b1e3d");
            0
        }
        _ => 0,
    }
}

fn run(args: &[String]) -> i32 {
    if args.iter().any(|arg| arg == "-d" || arg == "--detach") {
        let status = var("FAKE_RUNTIME_DETACH_STATUS")
            .and_then(|status| status.parse().ok())
            .unwrap_or(0);
        if status == 0 {
            println!("9c1d4e7f2a5b");
        }
        return status;
    }
    if let Some(payload) = var("FAKE_RUNTIME_REPORT") {
        let folder = args
            .windows(2)
            .filter(|pair| pair[0] == "-v" || pair[0] == "--volume")
            .find_map(|pair| pair[1].strip_suffix(":/run/taw"));
        if let Some(folder) = folder {
            let staged = Path::new(folder).join(".session.json.fake");
            if std::fs::write(&staged, payload).is_ok() {
                let _ = std::fs::rename(&staged, Path::new(folder).join("session.json"));
            }
        }
    }
    if let Some(stop) = var("FAKE_RUNTIME_STOP") {
        while !Path::new(&stop).exists() {
            std::thread::sleep(Duration::from_millis(50));
        }
        return 0;
    }
    var("FAKE_RUNTIME_RUN_STATUS")
        .and_then(|status| status.parse().ok())
        .unwrap_or(0)
}
