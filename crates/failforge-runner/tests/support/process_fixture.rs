#[cfg(unix)]
#[path = "process_fixture_unix.rs"]
mod unix;
#[cfg(windows)]
#[path = "process_fixture_windows.rs"]
mod windows;

fn main() -> std::process::ExitCode {
    #[cfg(unix)]
    {
        unix::main()
    }
    #[cfg(windows)]
    {
        windows::main()
    }
}
