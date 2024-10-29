#![allow(
    unused_doc_comments,
    unused_variables,
    unused_imports,
    unused_mut,
    dead_code,
    unused_assignments,
    non_camel_case_types,
    unreachable_code,
    unused_macros,
    bare_trait_objects,
    non_snake_case,
    invalid_value
)]

macro_rules! p {
    ($a:expr) => {
        println!("{:?}", $a)
    };
}
macro_rules! pp {
    ($a:expr,$b:expr) => {
        println!("{:?}, {:?}", $a, $b)
    };
}
macro_rules! ppp {
    ($a:expr,$b:expr,$c:expr) => {
        println!("{:?}, {:?}, {:?}", $a, $b, $c)
    };
}

use ::errno::{errno, set_errno};
use colored::{ColoredString, Colorize};
use errno::Errno as LibErrno;
use nix::{
    errno::Errno,
    libc::user_regs_struct,
    sys::{
        ptrace::{self},
        signal::{kill, Signal},
        wait::waitpid,
    },
    unistd::{fork, ForkResult, Pid},
};
use procfs::process::{MMapPath, MemoryMap};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    env::args,
    error::Error,
    fmt::Debug,
    mem::{self, transmute, MaybeUninit},
    os::{raw::c_void, unix::process::CommandExt},
    path::PathBuf,
    process::{exit, Command, Stdio},
    ptr::null,
    time::Duration,
};
use syscalls::Sysno;
use utilities::{
    display_unsupported, errno_check, parse_args, set_memory_break, ATTACH, EXITERS, FAILED_ONLY,
    FOLLOW_FORKS, OUTPUT, OUTPUT_FOLLOW_FORKS, QUIET, SUMMARY,
};

mod syscalls_map;
mod syscall_object;
mod types;
use syscall_object::{SyscallObject, SyscallState};
mod one_line_formatter;
mod utilities;

// TODO!
// consider humansize crate for human readable byte amounts
use pete::{Ptracer, Restart, Stop, Tracee};

fn main() {
    let command = parse_args();
    runner(command);
}

fn runner(command: Vec<String>) {
    if FOLLOW_FORKS.get() {
        if ATTACH.get().0 {
            follow_forks(None);
        } else {
            follow_forks(Some(command));
        }
    } else {
        if ATTACH.get().0 {
            parent(None);
        } else {
            unsafe {
                match fork() {
                    Ok(ForkResult::Parent { child }) => {
                        parent(Some(child));
                    }
                    Ok(ForkResult::Child) => {
                        child_trace_me(command);
                    }
                    Err(errno) => {
                        println!("error: {errno}")
                    }
                }
            }
        }
    }
}

fn child_trace_me(comm: Vec<String>) {
    let mut command = Command::new(&comm[0]);
    command.args(&comm[1..]);

    if QUIET.get() {
        command.stdout(Stdio::null());
    }

    // TRACE ME
    let _ = ptrace::traceme().unwrap();
    // EXECUTE
    let res = command.exec();
}

fn follow_forks(command_to_run: Option<Vec<String>>) {
    match command_to_run {
        // COMMANDLINE PROGRAM
        Some(comm) => {
            let mut command = Command::new(&comm[0]);
            command.args(&comm[1..]);

            if QUIET.get() {
                command.stdout(Stdio::null());
            }

            let mut ptracer = Ptracer::new();
            *ptracer.poll_delay_mut() = Duration::from_nanos(1);
            let child = ptracer.spawn(command).unwrap();
            ptrace_ptracer(ptracer, Pid::from_raw(child.id() as i32));
        }
        // ATTACHING TO PID
        None => {
            if ATTACH.get().0 {
                let mut ptracer = Ptracer::new();
                *ptracer.poll_delay_mut() = Duration::from_nanos(1);
                let child = ptracer
                    .attach(pete::Pid::from_raw(ATTACH.get().1.unwrap() as i32))
                    .unwrap();
                ptrace_ptracer(ptracer, Pid::from_raw(ATTACH.get().1.unwrap() as i32));
            } else {
                eprintln!("Usage: invalid arguments\n");
            }
        }
    }
}

fn parent(child_or_attach: Option<Pid>) {
    let child = if child_or_attach.is_some() {
        child_or_attach.unwrap()
    } else {
        let child = Pid::from_raw(ATTACH.get().1.unwrap() as i32);
        let _ = ptrace::attach(child).unwrap();
        child
    };
    // skip first execve
    let _res = waitpid(child, None).unwrap();
    let mut syscall_entering = true;
    let (mut start, mut end) = (None, None);
    let mut syscall = SyscallObject::default();
    'main_loop: loop {
        match ptrace::syscall(child, None) {
            Ok(_void) => {
                let _res = waitpid(child, None).expect("Failed waiting for child.");
                match syscall_entering {
                    true => {
                        // SYSCALL ABOUT TO RUN
                        match nix::sys::ptrace::getregs(child) {
                            Ok(registers) => {
                                syscall = SyscallObject::build(&registers, child);
                                // p!(syscall.sysno.name());
                                syscall_will_run(&mut syscall, &registers, child);
                                if syscall.is_exiting() {
                                    break 'main_loop;
                                }
                            }
                            Err(errno) => {}
                        }
                        syscall_entering = false;
                        start = Some(std::time::Instant::now());
                        continue 'main_loop;
                    }
                    false => {
                        // SYSCALL RETURNED
                        end = Some(std::time::Instant::now());
                        match nix::sys::ptrace::getregs(child) {
                            Ok(registers) => {
                                OUTPUT.with_borrow_mut(|ref mut output| {
                                    output
                                        .entry(syscall.sysno)
                                        .and_modify(|value| {
                                            value.0 += 1;
                                            value.1 = value.1.saturating_add(
                                                end.unwrap().duration_since(start.unwrap()),
                                            );
                                        })
                                        .or_insert((
                                            1,
                                            end.unwrap().duration_since(start.unwrap()),
                                        ));
                                });
                                start = None;
                                end = None;
                                syscall_returned(&mut syscall, &registers)
                            }
                            Err(errno) => {
                                handle_getting_registers_error(errno, "exit", syscall.sysno);
                                break 'main_loop;
                            }
                        }
                        syscall_entering = true;
                    }
                }
            }
            Err(errno) => {
                println!(
                    "\n\n ptrace-syscall Error: {errno}, last syscall: {} \n\n",
                    syscall.sysno
                );
                break 'main_loop;
            }
        }
    }
    if SUMMARY.get() {
        print_table();
    }
}

fn ptrace_ptracer(mut ptracer: Ptracer, child: Pid) {
    let mut last_sysno: Sysno = unsafe { mem::zeroed() };
    let mut last_pid = unsafe { mem::zeroed() };
    let mut pid_syscall_map: HashMap<Pid, SyscallObject> = HashMap::new();

    while let Some(mut tracee) = ptracer.wait().unwrap() {
        let syscall_pid = Pid::from_raw(tracee.pid.as_raw());
        match tracee.stop {
            Stop::SyscallEnter => 'label_for_early_break: {
                match nix::sys::ptrace::getregs(syscall_pid) {
                    Ok(registers) => {
                        // p!(tracee.registers().unwrap());
                        if syscall_pid != last_pid {
                            if let Some(last_syscall) = pid_syscall_map.get_mut(&last_pid) {
                                last_syscall.paused = true;
                                let paused = " STOPPED ".on_bright_green();
                                print!(" ├ {paused}");
                            }
                        }
                        let mut syscall = SyscallObject::build(&registers, syscall_pid);
                        if SUMMARY.get() {
                            OUTPUT_FOLLOW_FORKS.with_borrow_mut(|ref mut output| {
                                output
                                    .entry(syscall.sysno)
                                    .and_modify(|value| {
                                        *value += 1;
                                    })
                                    .or_insert(1);
                            });
                        }
                        syscall_will_run(&mut syscall, &registers, syscall_pid);
                        if syscall.is_exiting() {
                            break 'label_for_early_break;
                        }
                        last_sysno = syscall.sysno;
                        syscall.state = SyscallState::Exiting;
                        pid_syscall_map.insert(syscall_pid, syscall);
                    }
                    Err(errno) => handle_getting_registers_error(errno, "enter", last_sysno),
                }
                last_pid = syscall_pid;
            }
            Stop::SyscallExit => {
                if syscall_pid != last_pid {
                    if let Some(last_syscall) = pid_syscall_map.get_mut(&last_pid) {
                        last_syscall.paused = true;
                        let paused = " STOPPED ".on_bright_green();
                        print!(" ├ {paused}");
                    }
                }
                match nix::sys::ptrace::getregs(syscall_pid) {
                    Ok(registers) => {
                        if let Some(mut syscall) = pid_syscall_map.get_mut(&syscall_pid) {
                            syscall_returned(&mut syscall, &registers);
                            pid_syscall_map.remove(&syscall_pid).unwrap();
                        }
                    }
                    Err(errno) => handle_getting_registers_error(errno, "exit", last_sysno),
                }
                last_pid = syscall_pid;
            }
            _ => {
                let Tracee { pid, stop, .. } = tracee;
            }
        }
        ptracer.restart(tracee, Restart::Syscall).unwrap();
    }
    if SUMMARY.get() {
        print_table();
    }
}

fn syscall_will_run(syscall: &mut SyscallObject, registers: &user_regs_struct, child: Pid) {
    // GET PRECALL DATA (some data will be lost if not saved in this time frame)
    syscall.get_precall_data();

    // handle program break point
    if syscall.is_mem_alloc_dealloc() {
        set_memory_break(syscall.child);
    }

    if FOLLOW_FORKS.get() || syscall.is_exiting() {
        syscall.format();
        if syscall.is_exiting() {
            let exited = " EXITED ".on_bright_red();
            let pid = format!(" {} ", syscall.child).on_black();
            print!("\n\n {pid}{exited}\n",);
        }
    }
}

fn syscall_returned(syscall: &mut SyscallObject, registers: &user_regs_struct) {
    // STORE SYSCALL RETURN VALUE
    syscall.result.0 = Some(registers.rax);

    // manual calculation of errno for now
    // TODO! make this cleaner
    syscall.errno = errno_check(registers.rax);

    // GET POSTCALL DATA (some data will be lost if not saved in this time frame)
    syscall.get_postcall_data();

    if !FOLLOW_FORKS.get() {
        if FAILED_ONLY.get() && !syscall.parse_return_value_one_line().is_err() {
            return;
        }
        syscall.state = SyscallState::Entering;
        syscall.format();
        syscall.state = SyscallState::Exiting;
    }
    syscall.format();
    // handle program exiting
}

fn handle_getting_registers_error(errno: Errno, syscall_enter_or_exit: &str, sysno: Sysno) {
    if sysno == Sysno::exit || sysno == Sysno::exit_group {
        println!("\n\nSuccessfully exited\n");
    } else {
        match errno {
            Errno::ESRCH => {
                println!(
                "\n\n getting registers: syscall-{syscall_enter_or_exit} error: process disappeared\nsyscall: {sysno}, error: {errno}"
            );
                exit(0);
            }
            _ => println!("can some error while getting registers"),
        }
    }
}

fn print_table() {
    if FOLLOW_FORKS.get() {
        OUTPUT_FOLLOW_FORKS.with_borrow_mut(|output| {
            let mut vec = Vec::from_iter(output);
            vec.sort_by(|(_sysno, count), (_sysno2, count2)| count2.cmp(count));

            use tabled::{builder::Builder, settings::Style};
            let mut builder = Builder::new();

            builder.push_record(["calls", "syscall"]);
            builder.push_record([""]);
            for (sys, count) in vec {
                builder.push_record([&count.to_string(), sys.name()]);
            }
            let table = builder.build().with(Style::ascii_rounded()).to_string();

            println!("\n{}", table);
        });
    } else {
        OUTPUT.with_borrow_mut(|output| {
            let mut vec = Vec::from_iter(output);
            vec.sort_by(
                |(_sysno, (count, duration)), (_sysno2, (count2, duration2))| {
                    duration2.cmp(duration)
                },
            );

            use tabled::{builder::Builder, settings::Style};
            let mut builder = Builder::new();

            builder.push_record(["% time", "seconds", "usecs/call", "calls", "syscall"]);
            builder.push_record([""]);
            let total_time = vec
                .iter()
                .map(|(_, (_, time))| time.as_micros())
                .sum::<u128>();
            for (sys, (count, time)) in vec {
                let time_MICROS = time.as_micros() as f64;
                let time = time_MICROS / 1_000_000.0;
                let usecs_call = (time_MICROS / *count as f64) as i64;
                let time_percent = time_MICROS / total_time as f64;
                builder.push_record([
                    &format!("{:.2}", time_percent * 100.0),
                    &format!("{:.6}", time),
                    &format!("{}", usecs_call),
                    &count.to_string(),
                    sys.name(),
                ]);
            }
            let table = builder.build().with(Style::ascii_rounded()).to_string();

            println!("\n{}", table);
        });
    }
}
