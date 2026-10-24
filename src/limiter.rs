use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};

const SLOT: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Identity {
    pub pid: i32,
    start: u64,
}

#[derive(Debug)]
struct Process {
    id: Identity,
    parent: Option<i32>,
    cpu_ms: u64,
    exe: Option<PathBuf>,
}

type Snapshot = HashMap<i32, Process>;

#[derive(Debug)]
struct Member {
    id: Identity,
    cpu_ms: u64,
}

fn refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .without_tasks()
        .with_cpu()
        .with_exe(UpdateKind::Always)
}

fn live(status: ProcessStatus) -> bool {
    !matches!(status, ProcessStatus::Zombie | ProcessStatus::Dead)
}

fn matches_exe(exe: &Path, requested: &OsStr) -> bool {
    if Path::new(requested).components().count() > 1 || requested.as_encoded_bytes().contains(&b'/')
    {
        exe.as_os_str() == requested
    } else {
        exe.file_name() == Some(requested)
    }
}

fn update_members(
    snapshot: &Snapshot,
    root: Identity,
    children: bool,
    members: &mut HashMap<i32, Member>,
) -> Option<u64> {
    if snapshot.get(&root.pid).map(|p| p.id) != Some(root) {
        return None;
    }
    members.retain(|pid, member| snapshot.get(pid).map(|p| p.id) == Some(member.id));
    members.entry(root.pid).or_insert(Member {
        id: root,
        cpu_ms: snapshot[&root.pid].cpu_ms,
    });
    if children {
        let mut by_parent: HashMap<i32, Vec<&Process>> = HashMap::new();
        for process in snapshot.values() {
            if let Some(parent) = process.parent {
                by_parent.entry(parent).or_default().push(process);
            }
        }
        let mut queue: VecDeque<_> = members.keys().copied().collect();
        while let Some(parent) = queue.pop_front() {
            for process in by_parent.get(&parent).into_iter().flatten() {
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    members.entry(process.id.pid)
                {
                    entry.insert(Member {
                        id: process.id,
                        cpu_ms: process.cpu_ms,
                    });
                    queue.push_back(process.id.pid);
                }
            }
        }
    }
    let mut delta = 0_u64;
    for (pid, member) in members {
        let cpu_ms = snapshot[pid].cpu_ms;
        delta = delta.saturating_add(cpu_ms.saturating_sub(member.cpu_ms));
        member.cpu_ms = cpu_ms;
    }
    Some(delta)
}

fn work_duration(limit: u32, capacity: f64) -> Duration {
    if limit == 0 {
        Duration::ZERO
    } else {
        SLOT.mul_f64((f64::from(limit) / 100.0 / capacity.max(0.001)).clamp(0.000001, 1.0))
    }
}

pub fn sleep_until(deadline: Instant, signal: &AtomicUsize) {
    while signal.load(Ordering::Relaxed) == 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

pub struct Controller {
    system: System,
    snapshot: Snapshot,
    forbidden: HashSet<i32>,
    stopped: HashSet<Identity>,
}

impl Controller {
    pub fn new() -> Result<Self, String> {
        let mut controller = Self {
            system: System::new(),
            snapshot: HashMap::new(),
            forbidden: HashSet::from([0, 1]),
            stopped: HashSet::new(),
        };
        controller.refresh();
        let mut pid = std::process::id() as i32;
        while pid > 1 && controller.forbidden.insert(pid) {
            pid = controller
                .snapshot
                .get(&pid)
                .and_then(|p| p.parent)
                .ok_or("cannot inspect the limiter's ancestry safely")?;
        }
        Ok(controller)
    }

    pub fn refresh(&mut self) {
        self.system
            .refresh_processes_specifics(ProcessesToUpdate::All, true, refresh_kind());
        self.snapshot = self
            .system
            .processes()
            .iter()
            .filter_map(|(pid, process)| {
                let pid = i32::try_from(pid.as_u32()).ok()?;
                live(process.status()).then(|| {
                    (
                        pid,
                        Process {
                            id: Identity {
                                pid,
                                start: process.start_time(),
                            },
                            parent: process
                                .parent()
                                .and_then(|p| i32::try_from(p.as_u32()).ok()),
                            cpu_ms: process.accumulated_cpu_time(),
                            exe: process.exe().map(Path::to_path_buf),
                        },
                    )
                })
            })
            .collect();
    }

    pub fn validate_pid(&self, pid: i32) -> Result<(), String> {
        if pid <= 1 || self.forbidden.contains(&pid) {
            Err(format!(
                "refusing unsafe PID {pid} (system, self, or ancestor)"
            ))
        } else {
            Ok(())
        }
    }

    pub fn find_pid(
        &self,
        pid: i32,
        child: &mut Option<std::process::Child>,
    ) -> Result<Option<Identity>, String> {
        self.validate_pid(pid)?;
        if let Some(process) = self.snapshot.get(&pid) {
            return Ok(Some(process.id));
        }
        // Some kernels omit zombies from snapshots; an owned child handle can still reap them.
        if let Some(child) = child.as_mut()
            && child.id() as i32 == pid
            && child
                .try_wait()
                .map_err(|e| format!("wait failed: {e}"))?
                .is_some()
        {
            return Ok(None);
        }
        if probe(pid)? {
            // A zombie can still answer kill(0), but it is no longer a runnable target.
            if self
                .system
                .process(Pid::from_u32(pid as u32))
                .is_some_and(|p| !live(p.status()))
            {
                return Ok(None);
            }
            return Err(format!(
                "cannot inspect live PID {pid}; check process visibility/permissions"
            ));
        }
        Ok(None)
    }

    pub fn find_exe(&self, requested: &OsStr) -> Option<Identity> {
        self.snapshot
            .values()
            .filter(|p| !self.forbidden.contains(&p.id.pid) && p.id.pid > 1)
            .filter(|p| {
                p.exe
                    .as_deref()
                    .is_some_and(|exe| matches_exe(exe, requested))
            })
            .map(|p| p.id)
            .min_by_key(|id| id.pid)
    }

    fn signal(&mut self, id: Identity, signal: i32) -> Result<bool, String> {
        self.validate_pid(id.pid)?;
        let pids = [Pid::from_u32(id.pid as u32)];
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing().without_tasks(),
        );
        match self.system.process(pids[0]) {
            Some(process) if process.start_time() != id.start || !live(process.status()) => {
                return Ok(false);
            }
            Some(_) => {}
            None => {
                if probe(id.pid)? {
                    return Err(format!(
                        "cannot revalidate live PID {} before signal {signal}",
                        id.pid
                    ));
                }
                return Ok(false);
            }
        }
        // shortcut: portable start times have second granularity and kill races reuse; use OS handles if race-free control is required.
        send(id.pid, signal)
    }

    pub fn resume_all(&mut self) -> Result<(), String> {
        let stopped: Vec<_> = self.stopped.iter().copied().collect();
        let mut errors = Vec::new();
        for id in stopped {
            match self.signal(id, libc::SIGCONT) {
                Ok(_) => {
                    self.stopped.remove(&id);
                }
                Err(error) => errors.push(error),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn stop(&mut self, members: &HashMap<i32, Member>) -> Result<(), String> {
        for member in members.values() {
            if !self.stopped.contains(&member.id) && self.signal(member.id, libc::SIGSTOP)? {
                self.stopped.insert(member.id);
            }
        }
        Ok(())
    }

    pub fn control(
        &mut self,
        root: Identity,
        limit: u32,
        children: bool,
        verbose: bool,
        signal: &AtomicUsize,
        child: &mut Option<std::process::Child>,
    ) -> Result<(), String> {
        let mut members = HashMap::new();
        let mut capacity = 1.0;
        let mut last_run = Duration::ZERO;
        let mut last_sample = Instant::now();
        let mut last_report = Instant::now();
        loop {
            if signal.load(Ordering::Relaxed) != 0 {
                return Ok(());
            }
            self.refresh();
            let Some(cpu_ms) = update_members(&self.snapshot, root, children, &mut members) else {
                // A hidden, still-live root is an error, rather than a successful disappearance.
                if !self.snapshot.contains_key(&root.pid) {
                    self.find_pid(root.pid, child)?;
                }
                return Ok(());
            };
            let now = Instant::now();
            let elapsed = now.duration_since(last_sample);
            last_sample = now;
            if !last_run.is_zero() {
                let measured = cpu_ms as f64 / last_run.as_secs_f64() / 1000.0;
                capacity = capacity * 0.7 + measured * 0.3;
            }
            if verbose && last_report.elapsed() >= Duration::from_secs(1) {
                eprintln!(
                    "PID {}: {} members, {:.1}% CPU, limit {}%",
                    root.pid,
                    members.len(),
                    cpu_ms as f64 / elapsed.as_secs_f64().max(0.001) / 10.0,
                    limit
                );
                last_report = now;
            }
            let work = work_duration(limit, capacity);
            let cycle = Instant::now();
            if work.is_zero() {
                self.stop(&members)?;
                sleep_until(cycle + SLOT, signal);
                continue;
            }
            self.resume_all()?;
            let run_start = Instant::now();
            sleep_until(cycle + work, signal);
            last_run = run_start.elapsed();
            if signal.load(Ordering::Relaxed) != 0 {
                return Ok(());
            }
            if work < SLOT {
                self.stop(&members)?;
            }
            sleep_until(cycle + SLOT, signal);
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        if let Err(error) = self.resume_all() {
            eprintln!("cpuclamp: cleanup failed: {error}");
        }
    }
}

pub fn send(pid: i32, signal: i32) -> Result<bool, String> {
    if pid <= 1 || pid == std::process::id() as i32 {
        return Err(format!("refusing unsafe PID {pid}"));
    }
    // SAFETY: kill takes integer arguments; positive PIDs address one process only.
    if unsafe { libc::kill(pid, signal) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(format!("signal {signal} to PID {pid} failed: {error}"))
    }
}

fn probe(pid: i32) -> Result<bool, String> {
    send(pid, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: i32, parent: i32, start: u64, cpu_ms: u64) -> Process {
        Process {
            id: Identity { pid, start },
            parent: Some(parent),
            cpu_ms,
            exe: None,
        }
    }

    #[test]
    fn exact_executable_matching() {
        assert!(matches_exe(Path::new("/tmp/worker"), OsStr::new("worker")));
        assert!(matches_exe(
            Path::new("/tmp/worker"),
            OsStr::new("/tmp/worker")
        ));
        assert!(!matches_exe(
            Path::new("/tmp/worker-long"),
            OsStr::new("worker")
        ));
        assert!(!matches_exe(
            Path::new("/other/worker"),
            OsStr::new("/tmp/worker")
        ));
    }

    #[test]
    fn recursive_retained_membership_and_cpu_deltas() {
        let mut snapshot = HashMap::from([
            (10, process(10, 1, 1, 100)),
            (20, process(20, 10, 2, 200)),
            (30, process(30, 20, 3, 300)),
            (40, process(40, 1, 4, 400)),
        ]);
        let root = snapshot[&10].id;
        let mut members = HashMap::new();
        assert_eq!(update_members(&snapshot, root, true, &mut members), Some(0));
        assert_eq!(members.len(), 3);
        snapshot.get_mut(&30).unwrap().parent = Some(1);
        snapshot.remove(&20);
        snapshot.get_mut(&10).unwrap().cpu_ms += 10;
        snapshot.get_mut(&30).unwrap().cpu_ms += 20;
        assert_eq!(
            update_members(&snapshot, root, true, &mut members),
            Some(30)
        );
        assert!(members.contains_key(&30));
        snapshot.get_mut(&30).unwrap().id.start += 1;
        assert_eq!(update_members(&snapshot, root, true, &mut members), Some(0));
        assert!(!members.contains_key(&30));
        snapshot.get_mut(&10).unwrap().id.start += 1;
        assert_eq!(update_members(&snapshot, root, true, &mut members), None);
    }

    #[test]
    fn bounded_multicore_work_and_zero() {
        assert_eq!(work_duration(0, 0.0), Duration::ZERO);
        assert_eq!(work_duration(25, 1.0), Duration::from_millis(25));
        assert_eq!(work_duration(100, 4.0), Duration::from_millis(25));
        assert_eq!(work_duration(200, 4.0), Duration::from_millis(50));
        assert_eq!(work_duration(400, 0.0), SLOT);
        assert_eq!(work_duration(1, 1e12), Duration::from_nanos(100));
    }

    #[test]
    fn unsafe_pids_are_rejected_without_signals() {
        let controller = Controller::new().unwrap();
        for pid in [-1, 0, 1, std::process::id() as i32, unsafe {
            libc::getppid()
        }] {
            assert!(controller.validate_pid(pid).is_err());
        }
    }
}
