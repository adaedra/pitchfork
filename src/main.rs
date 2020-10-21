use nix::libc::pid_t;
use nix::sys::signal::{kill, Signal};
use nix::sys::stat::Mode;
use nix::unistd::{mkfifo, unlink, Pid};
use std::error::Error;
use std::fs::{create_dir, read, write, File};
use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{exit, Child, Command};
use std::str::from_utf8;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::spawn;
use std::time::Duration;

enum Event {
    ChildStopped(i32),
    RestartRequested,
    StopRequested,
    FifoErrored,
}

struct StopResult {
    status: Option<i32>,
    event: Option<Event>,
}

fn show_error<T: Error>(message: &str, error: &T) {
    eprintln!("[pitchfork] {}", message);
    eprintln!("[pitchfork] Error was: {}", error.to_string());
}

fn file_names(name: &str, run_dir: &Path) -> (PathBuf, PathBuf) {
    let mut pid = run_dir.join(name);
    let mut ipc = pid.clone();
    pid.set_extension("pid");
    ipc.set_extension("ipc");

    return (pid, ipc);
}

fn run_fifo_thread(tx: Sender<Event>, ipc_file: &Path) {
    let ipc_file = ipc_file.to_owned();

    spawn(move || {
        'main: loop {
            let fifo = match File::open(&ipc_file) {
                Ok(file) => file,
                Err(error) => {
                    show_error("Could not reopen IPC file", &error);
                    break 'main;
                }
            };

            for byte in fifo.bytes() {
                match byte {
                    Ok(b'r') => {
                        tx.send(Event::RestartRequested).ok();
                    }
                    Ok(b's') => {
                        tx.send(Event::StopRequested).ok();
                    }
                    Ok(byte) => {
                        eprintln!("[pitchfork] Unknown order '{}'", byte as char);
                    }
                    Err(error) => {
                        show_error("Error while reading IPC file", &error);
                        break 'main;
                    }
                }
            }
        }

        tx.send(Event::FifoErrored).ok();
    });
}

fn run_child_thread(tx: Sender<Event>, mut child: Child) {
    spawn(move || {
        let status = child.wait().unwrap();

        if let Some(code) = status.code() {
            println!(
                "[pitchfork] Process {} ended with status {}",
                child.id(),
                code
            );
            tx.send(Event::ChildStopped(code)).ok();
        } else if let Some(signal) = status.signal() {
            println!(
                "[pitchfork] Process {} ended by signal {}",
                child.id(),
                signal
            );
            tx.send(Event::ChildStopped(-1)).ok();
        } else {
            println!("[pitchfork] Process {} disappeared", child.id());
            tx.send(Event::ChildStopped(-1)).ok();
        }
    });
}

fn try_kill(pid: pid_t, signal: Signal, timeout: u8, rx: &Receiver<Event>) -> StopResult {
    println!(
        "[pitchfork] Sending signal {} to {}",
        signal.to_string(),
        pid
    );

    if let Err(_) = kill(Pid::from_raw(pid), Some(signal)) {
        // Maybe we should try wait to see if we can salvage a result
        return StopResult {
            status: Some(-1),
            event: None,
        };
    }

    match rx.recv_timeout(Duration::from_secs(timeout as u64)) {
        Err(_) => StopResult {
            status: None,
            event: None,
        },
        Ok(Event::ChildStopped(code)) => StopResult {
            status: Some(code),
            event: None,
        },
        // We should try to run again, calculating the remaining time.
        Ok(other) => StopResult {
            status: None,
            event: Some(other),
        },
    }
}

fn stop_process(pid: pid_t, rx: &Receiver<Event>) -> StopResult {
    let mut interrupting_event = None;
    let tries = [
        (Signal::SIGINT, 20),
        (Signal::SIGTERM, 20),
        (Signal::SIGKILL, 1),
    ];
    let mut status = -1;

    for (signal, timeout) in tries.iter() {
        let StopResult {
            status: stop_status,
            event,
        } = try_kill(pid, *signal, *timeout, &rx);

        if let Some(stop_status) = stop_status {
            status = stop_status;
            break;
        }

        if let Some(event) = event {
            interrupting_event = Some(event);
        }
    }

    StopResult {
        status: Some(status),
        event: interrupting_event,
    }
}

fn boot_process(tx: Sender<Event>, command: &[&str]) -> pid_t {
    let child = match Command::new(command[0]).args(&command[1..]).spawn() {
        Ok(child) => child,
        Err(error) => {
            show_error(&format!("Could not start command {}", command[0]), &error);
            exit(1);
        }
    };

    let pid = child.id() as pid_t;
    run_child_thread(tx, child);

    println!("[pitchfork] Process started as PID {}", pid);
    pid
}

fn exec(name: &str, command: Vec<&str>, run_dir: &Path) {
    if !run_dir.exists() {
        if let Err(error) = create_dir(run_dir) {
            show_error(
                &format!(
                    "{} does not exist and it was impossible to create it",
                    run_dir.display()
                ),
                &error,
            );

            exit(1);
        }
    } else if !run_dir.is_dir() {
        eprintln!("[pitchfork] {} is not a directory", run_dir.display());
        exit(1);
    }

    let (pid_file, ipc_file) = file_names(name, run_dir);

    if pid_file.exists() {
        let pid = from_utf8(&read(&pid_file).unwrap())
            .unwrap()
            .parse::<pid_t>()
            .unwrap();

        if let Ok(_) = kill(Pid::from_raw(pid), None) {
            eprintln!("[pitchfork] Command {} is running as PID {}", name, pid);
            eprintln!("[pitchfork] Stop it before running it again.");

            exit(1);
        }

        println!("[pitchfork] Clearing existing PID file");
        unlink(&pid_file).unwrap();
    }

    if ipc_file.exists() {
        println!("[pitchfork] Clearing existing IPC file");
        unlink(&ipc_file).unwrap();
    }

    mkfifo(&ipc_file, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();

    let (tx, rx) = channel();

    run_fifo_thread(tx.clone(), &ipc_file);

    let mut child_pid = boot_process(tx.clone(), &command[..]);
    write(&pid_file, format!("{}", child_pid)).unwrap();
    let mut final_status = 0;

    for event in rx.iter() {
        match event {
            Event::ChildStopped(child_status) => {
                final_status = child_status;
                break;
            }
            Event::FifoErrored => {
                println!("FIFO errored!");
                let StopResult { status, .. } = stop_process(child_pid, &rx);
                if let Some(code) = status {
                    final_status = code;
                }

                break;
            }
            Event::RestartRequested => {
                println!("Restart");
                let StopResult { status, event } = stop_process(child_pid, &rx);

                if let Some(Event::StopRequested) = event {
                    if let Some(code) = status {
                        final_status = code
                    }
                    break;
                }

                child_pid = boot_process(tx.clone(), &command[..]);
                write(&pid_file, format!("{}", child_pid)).unwrap();
            }
            Event::StopRequested => {
                println!("Stop");
                let StopResult { status, event } = stop_process(child_pid, &rx);

                if let Some(code) = status {
                    final_status = code
                }

                if let Some(Event::RestartRequested) = event {
                    child_pid = boot_process(tx.clone(), &command[..]);
                    write(&pid_file, format!("{}", child_pid)).unwrap();
                } else {
                    break;
                }
            }
        }
    }

    unlink(&ipc_file).ok();
    unlink(&pid_file).ok();

    exit(final_status);
}

fn restart(name: &str, run_dir: &Path) {
    let (_, ipc_path) = file_names(name, run_dir);
    if !ipc_path.exists() {
        eprintln!("No processes running under this name.");
        exit(1);
    }

    write(ipc_path, "r").unwrap();
}

fn stop(name: &str, run_dir: &Path) {
    let (_, ipc_path) = file_names(name, run_dir);
    if !ipc_path.exists() {
        eprintln!("No processes running under this name.");
        exit(1);
    }

    write(ipc_path, "s").unwrap();
}

fn main() {
    use clap::{App, AppSettings, Arg, SubCommand};

    let exec_command = SubCommand::with_name("exec")
        .about("Runs a command under pitchfork")
        .arg(
            Arg::with_name("name")
                .required(true)
                .index(1)
                .help("The process name to run this command under"),
        )
        .arg(
            Arg::with_name("command")
                .required(true)
                .multiple(true)
                .index(2)
                .help("The command to run under supervision"),
        );
    let stop_command = SubCommand::with_name("stop")
        .about("Stop a command currently running")
        .arg(
            Arg::with_name("name")
                .help("The process to stop")
                .required(true),
        );
    let restart_command = SubCommand::with_name("restart")
        .about("Stops and starts again a command running")
        .arg(
            Arg::with_name("name")
                .help("The process to restart")
                .required(true),
        );

    let app = App::new(env!("CARGO_PKG_NAME"))
        .setting(AppSettings::SubcommandRequired)
        .arg(
            Arg::with_name("run_dir")
                .long("run-directory")
                .value_name("directory")
                .help("Override directory where to save IPC and PID files (default /var/run/pitchfork)")
        )
        .subcommand(exec_command)
        .subcommand(stop_command)
        .subcommand(restart_command);
    let matches = app.get_matches();

    let run_dir = Path::new(matches.value_of("run_dir").unwrap_or("/var/run/pitchfork"));

    match matches.subcommand() {
        ("exec", Some(args)) => exec(
            args.value_of("name").unwrap(),
            args.values_of("command").unwrap().collect(),
            run_dir,
        ),
        ("stop", Some(args)) => stop(args.value_of("name").unwrap(), run_dir),
        ("restart", Some(args)) => restart(args.value_of("name").unwrap(), run_dir),
        _ => panic!("Should not happen"),
    }
}
