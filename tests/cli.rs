use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

const BIN: &str = env!("CARGO_BIN_EXE_cpulimit");
static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cpulimit-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// Each test owns a new process group, including any fixture descendants.
struct Owned(Child);

impl Owned {
    fn spawn(command: &mut Command) -> Self {
        Self(
            command
                .process_group(0)
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    fn pid(&self) -> i32 {
        self.0.id() as i32
    }

    fn signal(&self, signal: i32) {
        assert!(self.pid() > 1);
        assert_eq!(unsafe { libc::kill(self.pid(), signal) }, 0);
    }

    fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "PID {} timed out", self.pid());
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // Negative here addresses only the disposable group this guard created, never -1.
        assert!(self.pid() > 1);
        unsafe {
            libc::kill(-self.pid(), libc::SIGCONT);
            libc::kill(-self.pid(), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

fn fixture_command(mode: &str, dir: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "fixture", "--ignored", "--nocapture"])
        .env("CPULIMIT_FIXTURE", mode)
        .env("CPULIMIT_TEST_DIR", dir);
    command
}

fn limiter(args: &[&OsStr]) -> Command {
    let mut command = Command::new(BIN);
    command.args(args);
    command
}

fn eventually(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !condition() {
        assert!(Instant::now() < deadline, "condition timed out");
        thread::sleep(Duration::from_millis(20));
    }
}

fn try_sample(pid: i32) -> Option<(u64, ProcessStatus)> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid as u32)]),
        true,
        ProcessRefreshKind::nothing().without_tasks().with_cpu(),
    );
    system
        .process(Pid::from_u32(pid as u32))
        .map(|process| (process.accumulated_cpu_time(), process.status()))
}

fn sample(pid: i32) -> (u64, ProcessStatus) {
    try_sample(pid).expect("fixture must be visible")
}

fn pid_file(path: &Path) -> i32 {
    eventually(|| {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| s.parse::<i32>().ok())
            .is_some()
    });
    fs::read_to_string(path).unwrap().parse().unwrap()
}

#[test]
fn help_errors_status_and_argument_bytes() {
    let mut help = Owned::spawn(Command::new(BIN).arg("--help"));
    assert!(help.wait(Duration::from_secs(3)).success());
    for args in [
        vec!["-l", "1", "-p", "0"],
        vec!["-l", "1", "-p", "1"],
        vec!["-l", "1", "-p", "-1"],
        vec!["-l", "1", "-p", "2", "-e", "x"],
        vec!["-l", "1", "--not-an-option"],
    ] {
        let mut child = Owned::spawn(Command::new(BIN).args(args).stderr(Stdio::null()));
        assert!(!child.wait(Duration::from_secs(3)).success());
    }
    for pid in [std::process::id() as i32, unsafe { libc::getppid() }] {
        let mut child = Owned::spawn(
            Command::new(BIN)
                .args(["-l", "1", "-p", &pid.to_string()])
                .stderr(Stdio::null()),
        );
        assert!(!child.wait(Duration::from_secs(3)).success());
    }
    let mut absent = Owned::spawn(Command::new(BIN).args(["-l", "1", "-p", "2147483647"]));
    assert!(absent.wait(Duration::from_secs(3)).success());
    let mut spawn_error = Owned::spawn(
        Command::new(BIN)
            .args(["-l", "1", "--", "/nonexistent/cpulimit-test-command"])
            .stderr(Stdio::null()),
    );
    assert!(!spawn_error.wait(Duration::from_secs(3)).success());
    let temp = Temp::new();
    let bytes = OsString::from_vec(vec![0xff, b' ', b'x']);
    let mut command = limiter(&[
        OsStr::new("-l100"),
        OsStr::new("--"),
        OsStr::new("/bin/sh"),
        OsStr::new("-c"),
        OsStr::new("printf '%s' \"$1\" > \"$2\"; exit 37"),
        OsStr::new("sh"),
        &bytes,
        temp.0.join("args").as_os_str(),
    ]);
    let mut child = Owned::spawn(&mut command);
    assert_eq!(child.wait(Duration::from_secs(4)).code(), Some(37));
    assert_eq!(
        fs::read(temp.0.join("args")).unwrap(),
        vec![0xff, b' ', b'x']
    );
    let mut child =
        Owned::spawn(Command::new(BIN).args(["-l100", "/bin/sh", "-c", "kill -TERM $$"]));
    assert_eq!(child.wait(Duration::from_secs(4)).code(), Some(143));
}

#[test]
fn list_configured_live_sessions() {
    let temp = Temp::new();
    let binary = temp.0.join("cpulimit");
    fs::copy(BIN, &binary).unwrap();
    let list = || {
        let output = Command::new(&binary).arg("--list").output().unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        String::from_utf8(output.stdout).unwrap()
    };
    let empty = "No configured live CPU limit sessions found.\n";
    assert_eq!(list(), empty);

    let target = Owned::spawn(&mut fixture_command("busy", &temp.0));
    let mut pid_limit =
        Owned::spawn(Command::new(&binary).args(["-l0", "-p", &target.pid().to_string()]));
    eventually(|| sample(target.pid()).1 == ProcessStatus::Stop);

    let program = std::env::current_exe().unwrap();
    let secret = "cpulimit-list-must-never-print-this-argument";
    let mut command_limit = Owned::spawn(
        Command::new(&binary)
            .args(["-il25", "--"])
            .arg(&program)
            .args([
                "--exact",
                "fixture",
                "--ignored",
                "--nocapture",
                "--skip",
                secret,
            ])
            .env("CPULIMIT_FIXTURE", "handle")
            .env("CPULIMIT_TEST_DIR", &temp.0),
    );
    pid_file(&temp.0.join("command-pid"));

    let selector = temp.0.join("absent\n\t\x1bworker");
    let mut waiting = Owned::spawn(Command::new(&binary).arg("-l50").arg("-e").arg(&selector));
    let other_binary = temp.0.join("another-cpulimit");
    fs::copy(BIN, &other_binary).unwrap();
    let _other = Owned::spawn(Command::new(&other_binary).args(["-l75", "-e", "absent-other"]));

    let mut expected = [
        (
            pid_limit.pid(),
            format!("{}\t0%\tno\tpid:{}", pid_limit.pid(), target.pid()),
        ),
        (
            command_limit.pid(),
            format!(
                "{}\t25%\tyes\tcommand:{}",
                command_limit.pid(),
                program.as_os_str().as_bytes().escape_ascii()
            ),
        ),
        (
            waiting.pid(),
            format!(
                "{}\t50%\tno\texe:{}",
                waiting.pid(),
                selector.as_os_str().as_bytes().escape_ascii()
            ),
        ),
    ];
    expected.sort_unstable_by_key(|(pid, _)| *pid);
    let expected = format!(
        "LIMITER_PID\tLIMIT\tCHILDREN\tTARGET\n{}\n",
        expected
            .iter()
            .map(|(_, row)| row.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
    eventually(|| list() == expected);
    assert!(!list().contains(secret));

    for control in [&mut pid_limit, &mut command_limit, &mut waiting] {
        control.signal(libc::SIGTERM);
        assert_eq!(control.wait(Duration::from_secs(4)).code(), Some(143));
    }
    assert_eq!(list(), empty);
}

#[test]
fn zero_freezes_then_interrupt_resumes_attached_process() {
    let temp = Temp::new();
    let target = Owned::spawn(&mut fixture_command("busy", &temp.0));
    eventually(|| sample(target.pid()).0 > 50);
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let mut control =
            Owned::spawn(Command::new(BIN).args(["-l0", "-p", &target.pid().to_string()]));
        eventually(|| sample(target.pid()).1 == ProcessStatus::Stop);
        let before = sample(target.pid()).0;
        thread::sleep(Duration::from_millis(450));
        assert!(
            sample(target.pid()).0.saturating_sub(before) <= 10,
            "zero limit must not pulse SIGCONT"
        );
        control.signal(signal);
        assert_eq!(
            control.wait(Duration::from_secs(4)).code(),
            Some(128 + signal)
        );
        eventually(|| sample(target.pid()).0 > before + 50);
    }
}

#[test]
fn command_shutdown_forwards_and_bounds_ignoring_child() {
    let temp = Temp::new();
    for (mode, signal) in [("handle", libc::SIGINT), ("ignore", libc::SIGTERM)] {
        let mut command = Command::new(BIN);
        command
            .args(["-l25", "--"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "fixture", "--ignored", "--nocapture"])
            .env("CPULIMIT_FIXTURE", mode)
            .env("CPULIMIT_TEST_DIR", &temp.0);
        let mut control = Owned::spawn(&mut command);
        let child_pid = pid_file(&temp.0.join("command-pid"));
        eventually(|| sample(child_pid).1 == ProcessStatus::Stop);
        let started = Instant::now();
        control.signal(signal);
        assert_eq!(
            control.wait(Duration::from_secs(5)).code(),
            Some(128 + signal)
        );
        assert!(started.elapsed() < Duration::from_secs(4));
        if mode == "handle" {
            assert!(temp.0.join("handled").exists());
        }
        assert_eq!(
            unsafe { libc::kill(child_pid, 0) },
            -1,
            "owned command must be reaped"
        );
        fs::remove_file(temp.0.join("command-pid")).unwrap();
    }
}

#[test]
fn executable_wait_reacquire_exact_name_and_lazy() {
    let temp = Temp::new();
    let path = temp.0.join(format!("worker-{}", std::process::id()));
    fs::copy(std::env::current_exe().unwrap(), &path).unwrap();
    let mut lazy = Owned::spawn(Command::new(BIN).args([
        OsStr::new("-zl0"),
        OsStr::new("--exe"),
        path.as_os_str(),
    ]));
    assert!(lazy.wait(Duration::from_secs(3)).success());
    let longer = temp.0.join(format!(
        "{}-long",
        path.file_name().unwrap().to_string_lossy()
    ));
    fs::copy(&path, &longer).unwrap();
    let mut unrelated_command = Command::new(&longer);
    unrelated_command
        .args(["--exact", "fixture", "--ignored", "--nocapture"])
        .env("CPULIMIT_FIXTURE", "busy")
        .env("CPULIMIT_TEST_DIR", &temp.0);
    let unrelated = Owned::spawn(&mut unrelated_command);
    let mut control = Owned::spawn(Command::new(BIN).args([
        OsStr::new("-l0"),
        OsStr::new("-e"),
        path.file_name().unwrap(),
    ]));
    thread::sleep(Duration::from_millis(250));
    assert!(control.0.try_wait().unwrap().is_none());
    assert_ne!(sample(unrelated.pid()).1, ProcessStatus::Stop);
    for _ in 0..2 {
        let mut target_command = Command::new(&path);
        target_command
            .args(["--exact", "fixture", "--ignored", "--nocapture"])
            .env("CPULIMIT_FIXTURE", "busy")
            .env("CPULIMIT_TEST_DIR", &temp.0);
        let mut target = Owned::spawn(&mut target_command);
        eventually(|| sample(target.pid()).1 == ProcessStatus::Stop);
        target.signal(libc::SIGKILL);
        target.wait(Duration::from_secs(2));
    }
    control.signal(libc::SIGTERM);
    assert_eq!(control.wait(Duration::from_secs(4)).code(), Some(143));
}

#[test]
fn recursive_and_reparented_descendants_resume_on_root_exit() {
    let temp = Temp::new();
    let mut root = Owned::spawn(&mut fixture_command("tree", &temp.0));
    let leaf = pid_file(&temp.0.join("leaf"));
    let bridge = pid_file(&temp.0.join("bridge"));
    let mut control = Owned::spawn(Command::new(BIN).args(["-il0", "-p", &root.pid().to_string()]));
    eventually(|| sample(leaf).1 == ProcessStatus::Stop && sample(bridge).1 == ProcessStatus::Stop);
    // Let the bridge exit after the leaf was observed, retaining the reparented leaf.
    fs::write(temp.0.join("release"), b"").unwrap();
    unsafe {
        libc::kill(bridge, libc::SIGCONT);
    }
    eventually(|| try_sample(bridge).is_none_or(|(_, status)| status == ProcessStatus::Zombie));
    let before = sample(leaf).0;
    thread::sleep(Duration::from_millis(350));
    assert!(sample(leaf).0.saturating_sub(before) <= 10);
    root.signal(libc::SIGKILL);
    root.wait(Duration::from_secs(2));
    assert!(control.wait(Duration::from_secs(4)).success());
    eventually(|| sample(leaf).0 > before + 50);
}

#[test]
fn sustained_cpu_budget_includes_threads() {
    let temp = Temp::new();
    let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    for threads in [1, if cpus >= 4 { 4 } else { 1 }] {
        let target = Owned::spawn(
            fixture_command("busy", &temp.0).env("CPULIMIT_THREADS", threads.to_string()),
        );
        eventually(|| sample(target.pid()).0 > 100);
        let before = sample(target.pid()).0;
        let started = Instant::now();
        thread::sleep(Duration::from_millis(700));
        let baseline = (sample(target.pid()).0 - before) as f64 / started.elapsed().as_secs_f64();
        let mut control =
            Owned::spawn(Command::new(BIN).args(["-l25", "-p", &target.pid().to_string()]));
        thread::sleep(Duration::from_secs(2));
        let before = sample(target.pid()).0;
        let started = Instant::now();
        thread::sleep(Duration::from_secs(3));
        let actual = (sample(target.pid()).0 - before) as f64 / started.elapsed().as_secs_f64();
        eprintln!(
            "{threads} threads: baseline {baseline:.0} CPU-ms/s, limited {actual:.0} CPU-ms/s"
        );
        assert!(
            (100.0..=420.0).contains(&actual),
            "25% budget outside broad scheduling tolerance: {actual}"
        );
        assert!(actual < baseline * 0.65, "CPU use must measurably fall");
        control.signal(libc::SIGTERM);
        assert_eq!(control.wait(Duration::from_secs(4)).code(), Some(143));
    }
}

#[test]
#[ignore = "disposable subprocess entry point, invoked by runtime tests"]
fn fixture() {
    let dir = PathBuf::from(std::env::var_os("CPULIMIT_TEST_DIR").unwrap());
    match std::env::var("CPULIMIT_FIXTURE").unwrap().as_str() {
        "busy" => {
            // Survive POSIX SIGHUP when a stopped disposable group becomes orphaned.
            unsafe {
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
            }
            let threads = std::env::var("CPULIMIT_THREADS")
                .ok()
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(1);
            let spin = || loop {
                std::hint::black_box(42_u64.wrapping_mul(13));
            };
            for _ in 1..threads {
                thread::spawn(spin);
            }
            spin();
        }
        "tree" => {
            let mut child = fixture_command("bridge", &dir)
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            fs::write(dir.join("bridge"), child.id().to_string()).unwrap();
            thread::spawn(move || child.wait().unwrap());
            loop {
                thread::sleep(Duration::from_millis(20));
            }
        }
        "bridge" => {
            let mut child = fixture_command("busy", &dir)
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            fs::write(dir.join("leaf"), child.id().to_string()).unwrap();
            // The bridge exits deliberately; its leaf is then adopted by the OS.
            thread::spawn(move || child.wait().unwrap());
            while !dir.join("release").exists() {
                thread::sleep(Duration::from_millis(10));
            }
        }
        mode @ ("handle" | "ignore") => {
            let signal = std::sync::Arc::new(AtomicUsize::new(0));
            for sig in [libc::SIGINT, libc::SIGTERM] {
                signal_hook::flag::register_usize(
                    sig,
                    std::sync::Arc::clone(&signal),
                    sig as usize,
                )
                .unwrap();
            }
            fs::write(dir.join("command-pid"), std::process::id().to_string()).unwrap();
            loop {
                if mode == "handle" && signal.load(Ordering::Relaxed) != 0 {
                    fs::write(dir.join("handled"), b"").unwrap();
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        other => panic!("unknown fixture {other}"),
    }
}
