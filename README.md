# cpuclamp

Limit a Unix process tree's CPU use with SIGSTOP/SIGCONT. Supports PID, executable, and command modes. Requires Rust 1.95 or newer; process APIs support Linux, macOS, and FreeBSD.

```sh
cargo build --release
./target/release/cpuclamp -l 25 -p 12345
./target/release/cpuclamp -l 50 -i -e worker
./target/release/cpuclamp -l 100 -- command argument
./target/release/cpuclamp --list
cargo test
```

`-l/--limit N` is required when limiting a target and accepts an integer from zero through 100 times the online logical CPU count. 100 means one fully occupied core, including all threads; 200 allows two cores. With `-i/--include-children`, the root and recursively discovered descendants share this budget. CPU milliseconds and monotonic elapsed time drive an approximately 100 ms SIGSTOP/SIGCONT duty cycle. Limits are approximate: scheduling, accounting resolution, process discovery, and workloads that block on I/O affect observed use. Zero stops targets continuously without periodic resume pulses.

`--list` is used alone and shows a best-effort snapshot of configured live sessions running at the same executable path. Rows are ordered by limiter PID and show that PID, configured CPU percentage, whether children share the budget, and the target PID, executable selector, or command program. Command arguments are omitted and target bytes are escaped for terminal output. Waiting executable sessions are included. OS permissions and missing process metadata can hide sessions, and copies of the binary at other paths are excluded. A process can exit during the snapshot; reaped sessions disappear from subsequent lists. This is not a history or a report of current enforcement, selected targets, or retained descendants.

Choose exactly one target: `-p/--pid PID`, `-e/--exe FILE`, or `COMMAND [ARGS]`. PID mode exits when that process identity disappears. Executable mode matches the complete executable basename or the literal full path, chooses the lowest eligible PID, and waits or reacquires every two seconds. It uses executable metadata rather than possibly truncated process names; executables hidden by OS permissions cannot be matched. `-z/--lazy` exits if no matching executable exists or the selected process exits. PID and command modes always exit with their target. No shell is inserted in command mode; argument boundaries and non-UTF-8 bytes are preserved. Parsing stops at the command; `--` also ends options. Long options accept `=`, short flags can be grouped, and `-v/--verbose` prints control statistics on stderr. `-h/--help` succeeds.

You need permission to inspect and signal the selected processes, normally processes owned by your user. PID 0, negative PIDs, PID 1, the limiter itself, and its ancestors are rejected. Visibility or signal-permission errors fail with a diagnostic. A detectably changed process start time is treated as disappearance. The portable start time has second granularity, and checking identity before `kill` still has a check-to-signal race: this is best-effort PID identity protection, not race-free control. Avoid controlling privileged or unrelated processes with this tool.

Observed descendants remain included after reparenting while the root lives. Root exit releases and resumes surviving descendants, then ends that session; executable mode can select a new root. Processes born and exited entirely between polls cannot be included. This tool controls individual processes, not process groups, and does not change scheduling priority.

SIGINT and SIGTERM handlers only record the signal. Ordinary shutdown and control-error cleanup resume all identities stopped by this limiter, including retained descendants. Attach mode then exits with 128 plus the signal. Command mode resumes first, forwards the received signal to its unreaped direct child, waits up to two seconds, then kills and reaps that child if necessary. A control error uses SIGTERM for the same bounded child cleanup and exits unsuccessfully. Normal command exit codes pass through; signal termination returns 128 plus the signal. Descendants are resumed rather than terminated. Resume failures are diagnosed and fail the limiter. SIGKILL, a host crash, or lost signal permissions can prevent cleanup; externally stopped targets can also be resumed if this limiter subsequently stopped them.

`sysinfo` supplies process enumeration, ancestry, executable paths, and cumulative CPU accounting across the three Unix platforms; its optional subsystems and task enumeration are disabled. `libc` supplies POSIX signals and the online CPU count. `signal-hook` supplies flag-only signal handlers. There is no CLI framework or additional test dependency. The runtime tests create disposable process groups with unconditional teardown and bounded waits; the ignored `fixture` test is their subprocess entry point.

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
