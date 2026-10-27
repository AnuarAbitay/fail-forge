#[cfg(unix)]
#[path = "microbench/unix.rs"]
mod unix;

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    unix::main()
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    eprintln!("microbench requires Unix control sockets; use the native Windows process tests");
    std::process::ExitCode::from(2)
}
