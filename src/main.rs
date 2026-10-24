#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
compile_error!("cpuclamp supports Linux, macOS, and FreeBSD");

mod cli;
mod limiter;

use cli::{Action, Config, Target};
use limiter::{Controller, send, sleep_until};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};

fn list(cpus: u32) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate executable: {e}"))?;
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .without_tasks()
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );
    let mut rows = Vec::new();
    for (pid, process) in system.processes() {
        if pid.as_u32() == std::process::id()
            || matches!(
                process.status(),
                ProcessStatus::Zombie | ProcessStatus::Dead
            )
            || process.exe() != Some(exe.as_path())
            || process.cmd().is_empty()
        {
            continue;
        }
        let Ok(Action::Limit(config)) = cli::parse(process.cmd()[1..].to_vec(), cpus) else {
            continue;
        };
        let target = match config.target {
            Target::Pid(pid) => format!("pid:{pid}"),
            Target::Exe(exe) => format!("exe:{}", exe.as_bytes().escape_ascii()),
            Target::Command(args) => format!("command:{}", args[0].as_bytes().escape_ascii()),
        };
        rows.push((
            pid.as_u32(),
            format!(
                "{}\t{}%\t{}\t{target}",
                pid,
                config.limit,
                if config.children { "yes" } else { "no" },
            ),
        ));
    }
    rows.sort_unstable_by_key(|(pid, _)| *pid);
    if rows.is_empty() {
        println!("No configured live CPU limit sessions found.");
    } else {
        println!("LIMITER_PID\tLIMIT\tCHILDREN\tTARGET");
        for (_, row) in rows {
            println!("{row}");
        }
    }
    Ok(())
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

fn cleanup_child(child: &mut Child, signal: i32) -> Result<(), String> {
    if child
        .try_wait()
        .map_err(|e| format!("wait failed: {e}"))?
        .is_some()
    {
        return Ok(());
    }
    // An owned, unreaped direct child cannot have its PID reused; try_wait precedes each signal.
    send(child.id() as i32, signal)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if child
            .try_wait()
            .map_err(|e| format!("wait failed: {e}"))?
            .is_some()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            child
                .kill()
                .map_err(|e| format!("kill child failed: {e}"))?;
            child
                .wait()
                .map_err(|e| format!("reap child failed: {e}"))?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn run(config: Config, signal: &AtomicUsize) -> Result<i32, String> {
    let mut controller = Controller::new()?;
    if let Target::Pid(pid) = config.target {
        controller.validate_pid(pid)?;
    }
    let mut child = if let Target::Command(args) = &config.target {
        Some(
            Command::new(&args[0])
                .args(&args[1..])
                .spawn()
                .map_err(|e| format!("cannot spawn {:?}: {e}", args[0]))?,
        )
    } else {
        None
    };
    let result = (|| {
        loop {
            if signal.load(Ordering::Relaxed) != 0 {
                return Ok(0);
            }
            if let Some(child) = child.as_mut()
                && let Some(status) = child.try_wait().map_err(|e| format!("wait failed: {e}"))?
            {
                return Ok(exit_code(status));
            }
            controller.refresh();
            let root = match &config.target {
                Target::Pid(pid) => controller.find_pid(*pid, &mut child)?,
                Target::Exe(exe) => controller.find_exe(exe),
                Target::Command(_) => {
                    controller.find_pid(child.as_ref().unwrap().id() as i32, &mut child)?
                }
            };
            if let Some(root) = root {
                if config.verbose {
                    eprintln!("controlling PID {}", root.pid);
                }
                controller.control(
                    root,
                    config.limit,
                    config.children,
                    config.verbose,
                    signal,
                    &mut child,
                )?;
                controller.resume_all()?;
            }
            if signal.load(Ordering::Relaxed) != 0 {
                return Ok(0);
            }
            match &config.target {
                Target::Pid(_) => return Ok(0),
                Target::Exe(_) if config.lazy => return Ok(0),
                Target::Command(_) => {
                    // Root exit normally means a zombie; wait reaps it and preserves its status.
                    return child
                        .as_mut()
                        .unwrap()
                        .wait()
                        .map(exit_code)
                        .map_err(|e| format!("wait failed: {e}"));
                }
                Target::Exe(_) => {
                    if config.verbose {
                        eprintln!("waiting for executable");
                    }
                    sleep_until(Instant::now() + Duration::from_secs(2), signal);
                }
            }
        }
    })();
    let resumed = controller.resume_all();
    let interrupted = signal.load(Ordering::Relaxed) as i32;
    let child_cleanup = if let Some(child) = child.as_mut()
        && (interrupted != 0 || result.is_err() || resumed.is_err())
    {
        cleanup_child(
            child,
            if interrupted != 0 {
                interrupted
            } else {
                libc::SIGTERM
            },
        )
    } else {
        Ok(())
    };
    let mut errors = Vec::new();
    let code = match result {
        Ok(code) => code,
        Err(e) => {
            errors.push(e);
            1
        }
    };
    if let Err(e) = resumed {
        errors.push(format!("resume cleanup failed: {e}"));
    }
    if let Err(e) = child_cleanup {
        errors.push(e);
    }
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(if interrupted != 0 {
        128 + interrupted
    } else {
        code
    })
}

fn main() {
    // SAFETY: sysconf has no pointer arguments for this query.
    let count = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    let cpus = match u32::try_from(count) {
        Ok(n) if n > 0 => n,
        _ => {
            eprintln!("cpuclamp: cannot determine online CPU count");
            std::process::exit(1);
        }
    };
    let config = match cli::parse(std::env::args_os().skip(1).collect(), cpus) {
        Ok(Action::Limit(config)) => config,
        Ok(Action::Help) => {
            print!("{}", cli::help(cpus));
            return;
        }
        Ok(Action::Version) => {
            println!("cpuclamp {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Ok(Action::List) => {
            if let Err(e) = list(cpus) {
                eprintln!("cpuclamp: {e}");
                std::process::exit(1);
            }
            return;
        }
        Err(e) => {
            eprintln!("cpuclamp: {e}\nUse --help for usage.");
            std::process::exit(1);
        }
    };
    let signal = Arc::new(AtomicUsize::new(0));
    for number in [libc::SIGINT, libc::SIGTERM] {
        if let Err(e) =
            signal_hook::flag::register_usize(number, Arc::clone(&signal), number as usize)
        {
            eprintln!("cpuclamp: cannot install signal handler: {e}");
            std::process::exit(1);
        }
    }
    let code = match run(config, &signal) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("cpuclamp: {e}");
            1
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_exit_statuses() {
        assert_eq!(exit_code(ExitStatus::from_raw(37 << 8)), 37);
        assert_eq!(exit_code(ExitStatus::from_raw(libc::SIGTERM)), 143);
    }
}
