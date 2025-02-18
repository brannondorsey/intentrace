use crate::{syscalls_map::initialize_syscall_map, types::SysDetails};
use lazy_static::lazy_static;
use nix::{errno::Errno, libc::__errno_location, unistd::Pid};
use phf::phf_set;
use procfs::process::{MMapPath, MemoryMap};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    time::Duration,
};
use syscalls::Sysno;

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

pub static mut UNSUPPORTED: Vec<&'static str> = Vec::new();

pub static EXITERS: phf::Set<&'static str> = phf_set! {
    "exit",
    "exit_group",
};

thread_local! {
    pub static PRE_CALL_PROGRAM_BREAK_POINT: Cell<usize> = Cell::new(0);
    pub static INTENT: Cell<bool> = Cell::new(true);
    pub static SUMMARY: Cell<bool> = Cell::new(false);
    pub static STRING_LIMIT: Cell<usize> = Cell::new(36);
    pub static FOLLOW_FORKS: Cell<bool> = Cell::new(false);
    pub static QUIET: Cell<bool> = Cell::new(false);
    pub static FAILED_ONLY: Cell<bool> = Cell::new(false);
    pub static ATTACH: Cell<(bool,Option<usize>)> = Cell::new((false,None));
    pub static OUTPUT: RefCell<HashMap<Sysno, (usize, Duration)>> = RefCell::new(HashMap::new());
    pub static OUTPUT_FOLLOW_FORKS: RefCell<HashMap<Sysno, usize>> = RefCell::new(HashMap::new());
    // TODO! Time blocks feature
    // pub static TIME_BLOCKS: Cell<bool> = Cell::new(false);
}

lazy_static! {
    pub static ref SYSCALL_MAP: HashMap<Sysno, SysDetails> = initialize_syscall_map();
    pub static ref PAGE_SIZE: usize = page_size::get();
}

pub fn parse_args() -> Vec<String> {
    let arg_vector = std::env::args().collect::<Vec<String>>();
    if arg_vector.len() < 2 {
        eprintln!("Usage: {} prog args\n", arg_vector[0]);
        std::process::exit(1)
    }
    let mut args = arg_vector.into_iter().peekable();
    let _ = args.next().unwrap();

    while let Some(arg) = args.peek() {
        match arg.as_str() {
            "-h" | "--help" => {
                // TODO!
                // PENDING SWITCH TO CLAP
                println!("intentrace is a strace for everyone.

Usage: intentrace [OPTIONS] [-- <TRAILING_ARGUMENTS>...]

Options:
  -c, --summary                      provide a summary table at the end of tracing
  -p, --attach <pid>                 attach to an already running proceess
  -f, --follow-forks                 trace child processes when traced programs create them
  -z, --failed-only                  only print failed syscalls	
  -q, --mute-stdout                  mute the traced program's std output
  -h, --help                         print help
  -v, --version                      print version
                ");
                std::process::exit(0)
            }
            "-v" | "--version" => {
                // TODO!
                // PENDING SWITCH TO CLAP
                println!("intentrace {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0)
            }
            "-c" | "--summary" => {
                let _ = args.next().unwrap();
                // if FOLLOW_FORKS.get() {
                //     eprintln!(
                //         "Usage: summary retrieval and fork following are mutually exclusive\n"
                //     );
                //     std::process::exit(100);
                // }
                SUMMARY.set(true);
            }
            "-p" | "--attach" => {
                let _ = args.next().unwrap();
                if FOLLOW_FORKS.get() {
                    eprintln!(
                        "Usage: attaching to a running process and fork following are mutually exclusive\n"
                    );
                    std::process::exit(100);
                }
                let pid = match args.next() {
                    Some(pid_str) => match pid_str.parse::<usize>() {
                        Ok(pid) => {
                            ATTACH.set((true, Some(pid)));
                        }
                        Err(_) => {
                            eprintln!("Usage: pid is not valid\n");
                            std::process::exit(100);
                        }
                    },
                    None => {
                        eprintln!("Usage: pid is not valid\n");
                        std::process::exit(100);
                    }
                };
            }
            "-f" | "--follow-forks" => {
                let _ = args.next().unwrap();
                if ATTACH.get().0 {
                    eprintln!(
                        "Usage: attaching to a running process and fork following are mutually exclusive\n"
                    );
                    std::process::exit(100);
                }
                // if SUMMARY.get() {
                //     eprintln!(
                //         "Usage: summary retrieval and fork following are mutually exclusive\n"
                //     );
                //     std::process::exit(100);
                // }
                FOLLOW_FORKS.set(true);
            }
            "-z" | "--failed-only" => {
                let _ = args.next().unwrap();
                if FOLLOW_FORKS.get() {
                    eprintln!(
                        "Usage: failed only retrieval and fork following are mutually exclusive\n"
                    );
                    std::process::exit(100);
                }
                FAILED_ONLY.set(true);
            }
            // time blocks in stdout between and during syscalls (e.g. a box is printed every 10 ms)
            // "-t" | "--time-blocks" => {
            //     let _ = args.next().unwrap();
            //     if FOLLOW_FORKS.get() {
            //         eprintln!("Usage: time blocks and fork following are mutually exclusive\n");
            //         std::process::exit(100);
            //     }
            //     TIME_BLOCKS.set(true);
            // }
            "-q" | "--mute-stdout" => {
                let _ = args.next().unwrap();
                QUIET.set(true);
            }
            _ => break,
        }
    }

    args.collect::<Vec<String>>()
}

pub fn get_mem_difference_from_previous(post_call_brk: usize) -> isize {
    post_call_brk as isize - PRE_CALL_PROGRAM_BREAK_POINT.get() as isize
}

pub fn match_enum_with_libc_flag(flags: u64, discriminant: i32) -> bool {
    (flags & (discriminant as u64)) == discriminant as u64
}

pub fn set_memory_break(child: Pid) {
    let ptraced_process = procfs::process::Process::new(i32::from(child)).unwrap();
    let stat = ptraced_process.stat().unwrap();
    let pre_call_brk = stat.start_brk.unwrap() as usize;

    let old_stored_brk = PRE_CALL_PROGRAM_BREAK_POINT.get();
    PRE_CALL_PROGRAM_BREAK_POINT.set(pre_call_brk);
}

pub fn where_in_childs_memory(child: Pid, address: u64) -> Option<MemoryMap> {
    let ptraced_process = procfs::process::Process::new(i32::from(child)).unwrap();
    let maps = ptraced_process.maps().unwrap().0;
    maps.into_iter()
        .find(|x| (address >= x.address.0) && (address <= x.address.1))
}

pub fn get_child_memory_break(child: Pid) -> (usize, (u64, u64)) {
    let ptraced_process = procfs::process::Process::new(i32::from(child)).unwrap();
    let stat = ptraced_process.stat().unwrap();
    let aa = ptraced_process.maps().unwrap().0;
    let c = aa
        .into_iter()
        .find(|x| x.pathname == MMapPath::Stack)
        .map(|x| x.address)
        .unwrap_or((0, 0));
    (PRE_CALL_PROGRAM_BREAK_POINT.get(), c)
}

pub fn errno_check(rax: u64) -> Option<Errno> {
    // let a = unsafe { &*__errno_location() };
    // p!("ERRNO LOCATION");
    // p!(a);
    // p!("ERRNO LOCATION");

    // TODO! improve on this hack
    let max_errno = 4095;
    // strace does something similar to this
    // https://github.com/strace/strace/blob/0f9f46096fa8da84e2e6a6646cd1e326bf7e83c7/src/negated_errno.h#L17
    // https://github.com/strace/strace/blob/0f9f46096fa8da84e2e6a6646cd1e326bf7e83c7/src/linux/x86_64/get_error.c#L26
    if rax > max_errno {
        let errno = (u32::MAX - rax as u32).saturating_add(1);
        let Errno: Errno = Errno::from_raw(errno as i32);
        let errno_fmt = errno::Errno(errno as i32);
        if matches!(Errno, Errno::UnknownErrno) {
            // p!("Big number but not an error");
            None
        } else {
            // p!(errno_fmt);
            Some(Errno)
        }
    } else {
        // p!("Not an error");
        None
    }
}

pub fn display_unsupported() {
    unsafe {
        UNSUPPORTED.iter().for_each(|uns| println!(" - {}", uns));
    }
}

pub fn x86_signal_to_string(signum: u64) -> Option<&'static str> {
    match signum {
        1 => Some("SIGHUP"),
        2 => Some("SIGINT"),
        3 => Some("SIGQUIT"),
        4 => Some("SIGILL"),
        5 => Some("SIGTRAP"),
        6 => Some("SIGABRT/SIGIOT"),
        7 => Some("SIGBUS"),
        8 => Some("SIGFPE"),
        9 => Some("SIGKILL"),
        10 => Some("SIGUSR1"),
        11 => Some("SIGSEGV"),
        12 => Some("SIGUSR2"),
        13 => Some("SIGPIPE"),
        14 => Some("SIGALRM"),
        15 => Some("SIGTERM"),
        16 => Some("SIGSTKFLT"),
        17 => Some("SIGCHLD"),
        18 => Some("SIGCONT"),
        19 => Some("SIGSTOP"),
        20 => Some("SIGTSTP"),
        21 => Some("SIGTTIN"),
        22 => Some("SIGTTOU"),
        23 => Some("SIGURG"),
        24 => Some("SIGXCPU"),
        25 => Some("SIGXFSZ"),
        26 => Some("SIGVTALRM"),
        27 => Some("SIGPROF"),
        28 => Some("SIGWINCH"),
        29 => Some("SIGIO/SIGPOLL"),
        30 => Some("SIGPWR"),
        34..=64 => Some("SIGRT"),
        _ => Some("SIGSYS/SIGUNUSED"),
    }
}
pub fn errno_to_string(errno: Errno) -> &'static str {
    match errno {
        Errno::EPERM => "Operation not permitted",
        Errno::ENOENT => "No such file or directory",
        Errno::ESRCH => "No such process",
        Errno::EINTR => "Interrupted system call",
        Errno::EIO => "I/O error",
        Errno::ENXIO => "No such device or address",
        Errno::E2BIG => "Argument list too long",
        Errno::ENOEXEC => "Exec format error",
        Errno::EBADF => "Bad file number",
        Errno::ECHILD => "No child processes",
        Errno::EAGAIN => "Try again",
        Errno::ENOMEM => "Out of memory",
        Errno::EACCES => "Permission denied",
        Errno::EFAULT => "Bad address",
        Errno::ENOTBLK => "Block device required",
        Errno::EBUSY => "Device or resource busy",
        Errno::EEXIST => "File exists",
        Errno::EXDEV => "Cross-device link",
        Errno::ENODEV => "No such device",
        Errno::ENOTDIR => "Not a directory",
        Errno::EISDIR => "Is a directory",
        Errno::EINVAL => "Invalid argument",
        Errno::ENFILE => "File table overflow",
        Errno::EMFILE => "Too many open files",
        Errno::ENOTTY => "Not a typewriter",
        Errno::ETXTBSY => "Text file busy",
        Errno::EFBIG => "File too large",
        Errno::ENOSPC => "No space left on device",
        Errno::ESPIPE => "Illegal seek",
        Errno::EROFS => "Read-only file system",
        Errno::EMLINK => "Too many links",
        Errno::EPIPE => "Broken pipe",
        Errno::EDOM => "Math argument out of domain of func",
        Errno::ERANGE => "Math result not representable",
        Errno::EDEADLK => "Resource deadlock would occur",
        Errno::ENAMETOOLONG => "File name too long",
        Errno::ENOLCK => "No record locks available",
        Errno::ENOSYS => "Function not implemented",
        Errno::ENOTEMPTY => "Directory not empty",
        Errno::ELOOP => "Too many symbolic links encountered",
        Errno::ENOMSG => "No message of desired type",
        Errno::EIDRM => "Identifier removed",
        Errno::ECHRNG => "Channel number out of range",
        Errno::EL2NSYNC => "Level 2 not synchronized",
        Errno::EL3HLT => "Level 3 halted",
        Errno::EL3RST => "Level 3 reset",
        Errno::ELNRNG => "Link number out of range",
        Errno::EUNATCH => "Protocol driver not attached",
        Errno::ENOCSI => "No CSI structure available",
        Errno::EL2HLT => "Level 2 halted",
        Errno::EBADE => "Invalid exchange",
        Errno::EBADR => "Invalid request descriptor",
        Errno::EXFULL => "Exchange full",
        Errno::ENOANO => "No anode",
        Errno::EBADRQC => "Invalid request code",
        Errno::EBADSLT => "Invalid slot",
        Errno::EBFONT => "Bad font file format",
        Errno::ENOSTR => "Device not a stream",
        Errno::ENODATA => "No data available",
        Errno::ETIME => "Timer expired",
        Errno::ENOSR => "Out of streams resources",
        Errno::ENONET => "Machine is not on the network",
        Errno::ENOPKG => "Package not installed",
        Errno::EREMOTE => "Object is remote",
        Errno::ENOLINK => "Link has been severed",
        Errno::EADV => "Advertise error",
        Errno::ESRMNT => "Srmount error",
        Errno::ECOMM => "Communication error on send",
        Errno::EPROTO => "Protocol error",
        Errno::EMULTIHOP => "Multihop attempted",
        Errno::EDOTDOT => "RFS specific error",
        Errno::EBADMSG => "Not a data message",
        Errno::EOVERFLOW => "Value too large for defined data type",
        Errno::ENOTUNIQ => "Name not unique on network",
        Errno::EBADFD => "File descriptor in bad state",
        Errno::EREMCHG => "Remote address changed",
        Errno::ELIBACC => "Can not access a needed shared library",
        Errno::ELIBBAD => "Accessing a corrupted shared library",
        Errno::ELIBSCN => ".lib section in a.out corrupted",
        Errno::ELIBMAX => "Attempting to link in too many shared libraries",
        Errno::ELIBEXEC => "Cannot exec a shared library directly",
        Errno::EILSEQ => "Illegal byte sequence",
        Errno::ERESTART => "Interrupted system call should be restarted",
        Errno::ESTRPIPE => "Streams pipe error",
        Errno::EUSERS => "Too many users",
        Errno::ENOTSOCK => "Socket operation on non-socket",
        Errno::EDESTADDRREQ => "Destination address required",
        Errno::EMSGSIZE => "Message too long",
        Errno::EPROTOTYPE => "Protocol wrong type for socket",
        Errno::ENOPROTOOPT => "Protocol not available",
        Errno::EPROTONOSUPPORT => "Protocol not supported",
        Errno::ESOCKTNOSUPPORT => "Socket type not supported",
        Errno::EOPNOTSUPP => "Operation not supported on transport endpoint",
        Errno::EPFNOSUPPORT => "Protocol family not supported",
        Errno::EAFNOSUPPORT => "Address family not supported by protocol",
        Errno::EADDRINUSE => "Address already in use",
        Errno::EADDRNOTAVAIL => "Cannot assign requested address",
        Errno::ENETDOWN => "Network is down",
        Errno::ENETUNREACH => "Network is unreachable",
        Errno::ENETRESET => "Network dropped connection because of reset",
        Errno::ECONNABORTED => "Software caused connection abort",
        Errno::ECONNRESET => "Connection reset by peer",
        Errno::ENOBUFS => "No buffer space available",
        Errno::EISCONN => "Transport endpoint is already connected",
        Errno::ENOTCONN => "Transport endpoint is not connected",
        Errno::ESHUTDOWN => "Cannot send after transport endpoint shutdown",
        Errno::ETOOMANYREFS => "Too many references: cannot splice",
        Errno::ETIMEDOUT => "Connection timed out",
        Errno::ECONNREFUSED => "Connection refused",
        Errno::EHOSTDOWN => "Host is down",
        Errno::EHOSTUNREACH => "No route to host",
        Errno::EALREADY => "Operation already in progress",
        Errno::EINPROGRESS => "Operation now in progress",
        Errno::ESTALE => "Stale NFS file handle",
        Errno::EUCLEAN => "Structure needs cleaning",
        Errno::ENOTNAM => "Not a XENIX named type file",
        Errno::ENAVAIL => "No XENIX semaphores available",
        Errno::EISNAM => "Is a named type file",
        Errno::EREMOTEIO => "Remote I/O error",
        Errno::EDQUOT => "Quota exceeded",
        Errno::ENOMEDIUM => "No medium found",
        Errno::EMEDIUMTYPE => "Wrong medium type",
        Errno::ECANCELED => "Operation Canceled",
        Errno::ENOKEY => "Required key not available",
        Errno::EKEYEXPIRED => "Key has expired",
        Errno::EKEYREVOKED => "Key has been revoked",
        Errno::EKEYREJECTED => "Key was rejected by service",
        Errno::EOWNERDEAD => "Owner died",
        Errno::ENOTRECOVERABLE => "State not recoverable",
        Errno::ERFKILL => "Operation not possible due to RF-kill",
        // Errno::EWOULDBLOCK => "Operation would block",
        // Errno::EAGAIN => "Operation would block",
        // Errno::EDEADLOCK => "Resource deadlock would occur",
        Errno::EHWPOISON => "Memory page has hardware error",
        Errno::UnknownErrno => unreachable!(),
        _ => unreachable!(),
    }
}
