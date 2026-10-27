//! Native Windows ownership: create suspended inside a kill-on-close job,
//! resume, observe using owned handles, terminate the job, then retire handles.
//! No PID-based termination or executable shell fallback is used.
use crate::prepare::{PreparedCommand, wide};
use crate::result::GroupState;
use std::fs::File;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::os::windows::process::ExitStatusExt;
use std::process::ExitStatus;
use std::ptr::{null, null_mut};
use std::time::Instant;
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::*;

pub(crate) const INTERRUPT: i32 = 2;
pub(crate) const TERMINATE: i32 = 15;
pub(crate) const KILL: i32 = 9;
pub(crate) type OutputPipe = OwnedHandle;

pub(crate) struct ManagedChild {
    process: OwnedHandle,
    job: OwnedHandle,
    pid: u32,
    stdout: Option<OutputPipe>,
    stderr: Option<OutputPipe>,
    status: Option<ExitStatus>,
    retired: bool,
}

fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle().cast()
}

// SAFETY: successful Win32 creation/duplication returns a uniquely owned,
// valid, non-pseudo handle. Only callers with that guarantee use this helper.
unsafe fn owned(handle: HANDLE) -> OwnedHandle {
    unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) }
}

fn check(result: i32) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn duplicate_inheritable(file: &File) -> io::Result<OwnedHandle> {
    let mut output = null_mut();
    // SAFETY: borrowed File and pseudo current-process handles remain live;
    // output points to writable HANDLE storage and receives independent ownership.
    unsafe {
        check(DuplicateHandle(
            GetCurrentProcess(),
            file.as_raw_handle().cast(),
            GetCurrentProcess(),
            &mut output,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        ))?;
        Ok(owned(output))
    }
}

fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    // SAFETY: zero initializes a valid SECURITY_ATTRIBUTES; nLength states ABI.
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let (mut read, mut write) = (null_mut(), null_mut());
    // SAFETY: both out pointers and the security record live through CreatePipe.
    unsafe {
        check(CreatePipe(&mut read, &mut write, &attributes, 0))?;
        let (read, write) = (owned(read), owned(write));
        check(SetHandleInformation(raw(&read), HANDLE_FLAG_INHERIT, 0))?;
        Ok((read, write))
    }
}

/// Restricted handle inheritance prevents a concurrent child from receiving
/// unrelated control/lease capabilities. Attribute values outlive CreateProcess.
struct Attributes {
    storage: Vec<usize>,
}
impl Attributes {
    fn new(handles: &[HANDLE], jobs: &[HANDLE]) -> io::Result<Self> {
        let mut bytes = 0;
        // SAFETY: a null list queries allocation size; bytes is valid output.
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut bytes);
        }
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut storage = vec![0_usize; bytes.div_ceil(size_of::<usize>())];
        // SAFETY: storage is aligned and at least the size queried above. On
        // initialization failure no list exists, so no Delete call is made.
        unsafe {
            check(InitializeProcThreadAttributeList(
                storage.as_mut_ptr().cast(),
                2,
                0,
                &mut bytes,
            ))?;
        }
        let mut result = Self { storage };
        // SAFETY: pointers refer to the caller's live handle arrays, retained
        // until after this list is dropped. Counts describe their byte sizes.
        unsafe {
            check(UpdateProcThreadAttribute(
                result.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                size_of_val(handles),
                null_mut(),
                null(),
            ))?;
            check(UpdateProcThreadAttribute(
                result.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                jobs.as_ptr().cast(),
                size_of_val(jobs),
                null_mut(),
                null(),
            ))?;
        }
        Ok(result)
    }
    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: this list was initialized into live aligned owned storage.
        unsafe {
            DeleteProcThreadAttributeList(self.ptr());
        }
    }
}

impl ManagedChild {
    pub(crate) fn spawn(command: &mut PreparedCommand) -> io::Result<Self> {
        // SAFETY: null security/name produce a new unnamed noninheritable job.
        let job = unsafe { CreateJobObjectW(null(), null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a successful creation above transferred unique ownership.
        let job = unsafe { owned(job) };
        // SAFETY: native job structures support all-zero initialization.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: ABI type and byte size match the selected information class.
        unsafe {
            check(SetInformationJobObject(
                raw(&job),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of_val(&limits) as u32,
            ))?;
        }
        let (stdout, stdout_write) = pipe()?;
        let (stderr, stderr_write) = pipe()?;
        let input = if let Some(input) = &command.stdin {
            input.try_clone()?
        } else {
            File::open("NUL")?
        };
        let stdin = duplicate_inheritable(&input)?;
        let lease = command
            .lease
            .as_ref()
            .map(duplicate_inheritable)
            .transpose()?;
        let mut inherited = vec![raw(&stdin), raw(&stdout_write), raw(&stderr_write)];
        if let Some(lease) = &lease {
            inherited.push(raw(lease));
            // The copied handle value is identical in the inherited child, but
            // differs from the caller's source handle. Inject that exact value.
            command.set_lease_environment((raw(lease) as usize).to_string().into());
        }
        let jobs = [raw(&job)];
        let mut attributes = Attributes::new(&inherited, &jobs)?;
        // SAFETY: all-zero STARTUPINFOEXW and PROCESS_INFORMATION are valid.
        let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = raw(&stdin);
        startup.StartupInfo.hStdOutput = raw(&stdout_write);
        startup.StartupInfo.hStdError = raw(&stderr_write);
        startup.lpAttributeList = attributes.ptr();
        let mut information: PROCESS_INFORMATION = unsafe { zeroed() };
        let application = wide(command.program.as_os_str());
        // SAFETY: buffers are terminated and retained, command line is writable,
        // explicit application bypasses shell interpretation, and only the three
        // listed handles may be inherited. Job membership is atomic with child
        // creation, so controller death even before resume cannot orphan it.
        unsafe {
            check(CreateProcessW(
                application.as_ptr(),
                command.command_line.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_SUSPENDED
                    | CREATE_UNICODE_ENVIRONMENT
                    | CREATE_NEW_PROCESS_GROUP
                    | EXTENDED_STARTUPINFO_PRESENT,
                command.env.as_ptr().cast(),
                command.cwd.as_ptr(),
                &startup.StartupInfo,
                &mut information,
            ))?;
        }
        // SAFETY: successful CreateProcessW supplied unique process/thread handles.
        let process = unsafe { owned(information.hProcess) };
        let thread = unsafe { owned(information.hThread) };
        // Keep this guard alive before fallible resume. The process is already
        // a job member, including if the controller dies before this point.
        let mut child = Self {
            process,
            job,
            pid: information.dwProcessId,
            stdout: Some(stdout),
            stderr: Some(stderr),
            status: None,
            retired: false,
        };
        // SAFETY: job/process handles live; suspended process has executed no code.
        unsafe {
            check(AssignProcessToJobObject(
                raw(&child.job),
                raw(&child.process),
            ))?;
            if ResumeThread(raw(&thread)) == u32::MAX {
                return Err(io::Error::last_os_error());
            }
        }
        // Parent copies of pipe writers and input are dropped here before capture.
        child.observe()?;
        Ok(child)
    }
    pub(crate) fn id(&self) -> u32 {
        self.pid
    }
    pub(crate) fn take_stdout(&mut self) -> Option<OutputPipe> {
        self.stdout.take()
    }
    pub(crate) fn take_stderr(&mut self) -> Option<OutputPipe> {
        self.stderr.take()
    }
    pub(crate) fn is_retired(&self) -> bool {
        self.retired
    }
    pub(crate) fn observe(&mut self) -> io::Result<bool> {
        if self.retired {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if self.status.is_some() {
            return Ok(true);
        }
        // SAFETY: an owned handle pins process identity independently of PID reuse.
        match unsafe { WaitForSingleObject(raw(&self.process), 0) } {
            WAIT_TIMEOUT => Ok(false),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // SAFETY: process is signalled, code is writable u32 storage.
                unsafe {
                    check(GetExitCodeProcess(raw(&self.process), &mut code))?;
                }
                self.status = Some(ExitStatus::from_raw(code));
                Ok(true)
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
    pub(crate) fn group_state(&self) -> io::Result<GroupState> {
        if self.retired {
            return Ok(GroupState::Unknown);
        }
        // SAFETY: basic accounting supports zero initialization and the byte size
        // and information class match. Job handle never escapes to child code.
        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        unsafe {
            check(QueryInformationJobObject(
                raw(&self.job),
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of_val(&accounting) as u32,
                null_mut(),
            ))?;
        }
        Ok(if accounting.ActiveProcesses == 0 {
            GroupState::NoLiveMembers { zombies: 0 }
        } else {
            GroupState::LiveMembers {
                count: accounting.ActiveProcesses as usize,
            }
        })
    }
    pub(crate) async fn signal_until_or_cancelled(
        &mut self,
        signal: i32,
        _deadline: Instant,
        _stop_waiting: impl Fn() -> bool,
    ) -> io::Result<bool> {
        self.observe()?;
        if matches!(self.group_state()?, GroupState::NoLiveMembers { .. }) {
            return Ok(false);
        }
        // Windows has no general SIGTERM analogue. Every cancellation route
        // terminates the private job immediately rather than address numeric
        // console group IDs, which Windows can reuse after leader termination.
        let _ = signal;
        // SAFETY: a private owned job addresses only this command and descendants.
        unsafe {
            check(TerminateJobObject(raw(&self.job), 1))?;
        }
        Ok(true)
    }
    pub(crate) fn reap(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            self.retired = true;
            return Ok(status);
        }
        Err(io::ErrorKind::WouldBlock.into())
    }
}
impl Drop for ManagedChild {
    fn drop(&mut self) {
        // SAFETY: these handles remain owned until after Drop; terminate is
        // asynchronous, never blocks runtime teardown. Closing the job provides
        // a second kill-on-close guarantee for all non-breakaway descendants.
        unsafe {
            let _ = TerminateJobObject(raw(&self.job), 1);
            let _ = TerminateProcess(raw(&self.process), 1);
        }
    }
}
