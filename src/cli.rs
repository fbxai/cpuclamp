use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

#[derive(Debug, PartialEq)]
pub enum Target {
    Pid(i32),
    Exe(OsString),
    Command(Vec<OsString>),
}

#[derive(Debug, PartialEq)]
pub struct Config {
    pub limit: u32,
    pub target: Target,
    pub verbose: bool,
    pub lazy: bool,
    pub children: bool,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Help,
    List,
    Limit(Config),
}

pub fn help(cpus: u32) -> String {
    format!(
        "Usage: cpulimit -l N [OPTIONS] (-p PID | -e FILE | COMMAND [ARGS])\n\
         \x20      cpulimit --list\n\
         -l, --limit=N          CPU percent, 0..{} (100 = one core)\n\
         -p, --pid=PID          attach to PID (implies --lazy)\n\
         -e, --exe=FILE         exact executable basename or path\n\
         -i, --include-children share the budget with recursive descendants\n\
         -z, --lazy             exit when no target remains\n\
         -v, --verbose          show control statistics on stderr\n\
         --list                list configured live sessions (use alone)\n\
         -h, --help             show help\n\
         --                    end options; command arguments are passed verbatim\n\n\
         --list is a best-effort snapshot at this executable path, including\n\
         waiting executable sessions. OS permissions can hide sessions. No history,\n\
         enforcement state, or selected/retained descendants are reported.\n",
        cpus.saturating_mul(100)
    )
}

fn number(value: &OsStr, option: &str) -> Result<u32, String> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(format!("{option} requires a nonnegative integer"));
    }
    value
        .to_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("{option} is too large"))
}

pub fn parse(args: Vec<OsString>, cpus: u32) -> Result<Action, String> {
    let mut limit = None;
    let mut target = None;
    let (mut verbose, mut lazy, mut children) = (false, false, false);
    let mut list = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_bytes();
        if arg == b"--" || !arg.starts_with(b"-") || arg == b"-" {
            if arg == b"--" {
                index += 1;
            }
            if index < args.len() {
                if target.is_some() {
                    return Err("choose exactly one target mode".into());
                }
                target = Some(Target::Command(args[index..].to_vec()));
            }
            break;
        }
        if arg.starts_with(b"--") {
            let split = arg.iter().position(|b| *b == b'=');
            let name = &arg[..split.unwrap_or(arg.len())];
            match name {
                b"--help" | b"--list" | b"--verbose" | b"--lazy" | b"--include-children" => {
                    if split.is_some() {
                        return Err("this option does not take a value".into());
                    }
                    match name {
                        b"--help" => return Ok(Action::Help),
                        b"--list" => list = true,
                        b"--verbose" => verbose = true,
                        b"--lazy" => lazy = true,
                        _ => children = true,
                    }
                }
                b"--limit" | b"--pid" | b"--exe" => {
                    let value = if let Some(at) = split {
                        OsString::from_vec(arg[at + 1..].to_vec())
                    } else {
                        index += 1;
                        args.get(index).ok_or("missing option value")?.clone()
                    };
                    set_value(name, value, &mut limit, &mut target)?;
                }
                _ => return Err(format!("unknown option: {}", args[index].to_string_lossy())),
            }
        } else {
            let mut at = 1;
            while at < arg.len() {
                match arg[at] {
                    b'h' => return Ok(Action::Help),
                    b'v' => verbose = true,
                    b'z' => lazy = true,
                    b'i' => children = true,
                    flag @ (b'l' | b'p' | b'e') => {
                        let value = if at + 1 < arg.len() {
                            OsString::from_vec(arg[at + 1..].to_vec())
                        } else {
                            index += 1;
                            args.get(index).ok_or("missing option value")?.clone()
                        };
                        let name: &[u8] = match flag {
                            b'l' => b"--limit",
                            b'p' => b"--pid",
                            _ => b"--exe",
                        };
                        set_value(name, value, &mut limit, &mut target)?;
                        break;
                    }
                    _ => return Err(format!("unknown option: -{}", arg[at] as char)),
                }
                at += 1;
            }
        }
        index += 1;
    }
    if list {
        return if args.len() == 1 {
            Ok(Action::List)
        } else {
            Err("--list must be used alone".into())
        };
    }
    let limit = limit.ok_or("--limit is required")?;
    if limit > cpus.saturating_mul(100) {
        return Err(format!(
            "--limit must be between 0 and {}",
            cpus.saturating_mul(100)
        ));
    }
    Ok(Action::Limit(Config {
        limit,
        target: target.ok_or("choose exactly one target mode")?,
        verbose,
        lazy,
        children,
    }))
}

fn set_value(
    name: &[u8],
    value: OsString,
    limit: &mut Option<u32>,
    target: &mut Option<Target>,
) -> Result<(), String> {
    if name == b"--limit" {
        if limit.is_some() {
            return Err("--limit specified more than once".into());
        }
        *limit = Some(number(&value, "--limit")?);
    } else {
        if target.is_some() {
            return Err("choose exactly one target mode".into());
        }
        *target = Some(if name == b"--pid" {
            let pid = number(&value, "--pid")?;
            if !(2..=i32::MAX as u32).contains(&pid) {
                return Err("--pid must be between 2 and 2147483647".into());
            }
            Target::Pid(pid as i32)
        } else {
            if value.is_empty() {
                return Err("--exe must not be empty".into());
            }
            Target::Exe(value)
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn config(input: Vec<OsString>, cpus: u32) -> Config {
        let Action::Limit(config) = parse(input, cpus).unwrap() else {
            panic!("expected a limit action");
        };
        config
    }

    #[test]
    fn modes_flags_and_boundaries() {
        let parsed = config(args(&["-vzi", "-l25", "--pid=42"]), 4);
        assert_eq!(
            parsed,
            Config {
                limit: 25,
                target: Target::Pid(42),
                verbose: true,
                lazy: true,
                children: true
            }
        );
        assert_eq!(
            config(args(&["--limit=0", "--exe", "worker"]), 1).target,
            Target::Exe("worker".into())
        );
        let mut input = args(&["-l", "100", "--", "cmd", "-p", "99", ""]);
        input.push(OsString::from_vec(vec![0xff, b'x']));
        assert_eq!(
            config(input.clone(), 1).target,
            Target::Command(input[3..].to_vec())
        );
        assert_eq!(
            config(args(&["-l1", "cmd", "--help"]), 1).target,
            Target::Command(args(&["cmd", "--help"]))
        );
        assert_eq!(parse(args(&["--help"]), 1), Ok(Action::Help));
    }

    #[test]
    fn list_action_and_command_boundaries() {
        assert_eq!(parse(args(&["--list"]), 1), Ok(Action::List));
        for values in [
            vec!["--help", "--list", "-l1"],
            vec!["--list", "--help"],
            vec!["--list", "-h"],
        ] {
            assert_eq!(parse(args(&values), 1), Ok(Action::Help));
        }
        for values in [
            vec!["--list=1"],
            vec!["--list="],
            vec!["--list", "--list"],
            vec!["--list", "--"],
            vec!["--list", "cmd"],
            vec!["--list", "-l1"],
            vec!["-l1", "--list", "cmd"],
            vec!["--list", "-p2"],
            vec!["--list", "-efoo"],
            vec!["--list", "-v"],
            vec!["--list", "-z"],
            vec!["--list", "-i"],
        ] {
            assert!(parse(args(&values), 1).is_err(), "{values:?}");
        }
        for values in [
            vec!["-l1", "cmd", "--list"],
            vec!["-l1", "--", "cmd", "--list"],
        ] {
            assert_eq!(
                config(args(&values), 1).target,
                Target::Command(args(&["cmd", "--list"]))
            );
        }
        assert_eq!(
            config(args(&["-l1", "--", "--list"]), 1).target,
            Target::Command(args(&["--list"]))
        );
    }

    #[test]
    fn invalid_inputs_fail() {
        for values in [
            vec![],
            vec!["-p2"],
            vec!["-l1"],
            vec!["-l-1", "-p2"],
            vec!["-l1.5", "-p2"],
            vec!["-l101", "-p2"],
            vec!["-l1", "-p0"],
            vec!["-l1", "-p1"],
            vec!["-l1", "-p-2"],
            vec!["-l1", "-p2147483648"],
            vec!["-l1", "-p2", "-efoo"],
            vec!["-l1", "-p2", "cmd"],
            vec!["-l1", "-e"],
            vec!["-l1", "--exe="],
            vec!["-l1", "--verbose=yes", "cmd"],
            vec!["-l1", "-l2", "cmd"],
            vec!["-l1", "--wat"],
        ] {
            assert!(parse(args(&values), 1).is_err(), "{values:?}");
        }
    }
}
