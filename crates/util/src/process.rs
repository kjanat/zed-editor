use anyhow::{Context as _, Result};
use std::process::Stdio;

/// A wrapper around `smol::process::Child` that attempts to clean up subprocess
/// trees: on Unix by using process groups, and on Windows by using job objects.
///
/// On Windows, if job setup and assignment succeed, dropping this struct
/// closes the job handle and terminates processes in the job. If either step
/// fails, the direct child remains usable but descendants are not tracked.
/// A descendant spawned before job assignment can also escape the job.
pub struct Child {
    process: smol::process::Child,
    #[cfg(windows)]
    job: Option<windows_job::JobObject>,
}

impl std::ops::Deref for Child {
    type Target = smol::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.process
    }
}

impl std::ops::DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.process
    }
}

impl Child {
    /// Starts a command in a new Unix session or an assigned Windows job before
    /// its program can run. Unlike `spawn`, containment setup cannot fall back.
    #[cfg(not(windows))]
    pub fn spawn_contained(
        mut command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        use std::os::unix::process::CommandExt as _;

        // SAFETY: setsid and errno access are async-signal-safe after fork.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let description = describe_command(&command);
        let process = smol::process::Command::from(command)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to spawn contained command {description}"))?;
        Ok(Self { process })
    }

    /// Starts a command suspended, assigns its job, then resumes its primary
    /// thread. Setup failure terminates and reaps the child without a fallback.
    #[cfg(windows)]
    pub fn spawn_contained(
        mut command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        use std::os::windows::process::CommandExt as _;
        use windows::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};

        let description = describe_command(&command);
        let job = windows_job::JobObject::new()?;
        command.creation_flags((CREATE_NO_WINDOW | CREATE_SUSPENDED).0);
        let mut process = smol::process::Command::from(command)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to spawn contained command {description}"))?;
        if let Err(error) = job
            .assign_process(process.id())
            .and_then(|()| windows_job::resume_primary_thread(process.id()))
        {
            let job_termination = job.terminate();
            process
                .kill()
                .or_else(|kill_error| process.try_status()?.map(|_| ()).ok_or(kill_error))
                .with_context(|| {
                    format!("terminating child after containment setup failed: {error:#}")
                })?;
            smol::block_on(process.status())
                .context("reaping child after containment setup failed")?;
            job_termination.context("terminating job after containment setup failed")?;
            while job.has_running_processes()? {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            return Err(error).with_context(|| format!("containing command {description}"));
        }
        Ok(Self {
            process,
            job: Some(job),
        })
    }

    #[cfg(not(windows))]
    pub fn spawn(
        mut command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        crate::set_pre_exec_to_start_new_session(&mut command);
        let description = describe_command(&command);
        let process = smol::process::Command::from(command)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .with_context(|| format!("failed to spawn command {description}"))?;
        Ok(Self { process })
    }

    #[cfg(windows)]
    pub fn spawn(
        command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        let description = describe_command(&command);
        let process = smol::process::Command::from(command)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .with_context(|| format!("failed to spawn command {description}"))?;

        // Assign the child to a job object configured to kill the entire
        // process tree when the last job handle is closed, so descendants
        // (e.g. node workers and MCP servers spawned by agent servers) are
        // reaped even if the direct child doesn't clean them up. Any process
        // the child spawns after this assignment is automatically part of the
        // job.
        //
        // Descendants spawned before this assignment escape the job. This
        // compatibility path permits job-setup failure; callers requiring
        // containment before execution must use `spawn_contained` instead.
        let job = windows_job::JobObject::new()
            .and_then(|job| {
                job.assign_process(process.id())?;
                Ok(job)
            })
            .map_err(|error| {
                log::error!("failed to assign spawned process to a job object: {error:#}");
            })
            .ok();

        Ok(Self { process, job })
    }

    /// Consumes the child, draining its stdout/stderr and waiting for it to
    /// exit, then returns the collected output.
    pub async fn output(self) -> Result<std::process::Output> {
        // NOTE: Keep `self` alive across this await, do not destructure it to
        // pull `process` out first. On Windows that drops the job object early,
        // which triggers `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and kills the
        // child before `output()` finishes collecting its stdout/stderr.
        Ok(self.process.output().await?)
    }

    #[cfg(not(windows))]
    pub fn kill(&mut self) -> Result<()> {
        let pid = self.process.id();
        // SAFETY: the child starts its own session, whose process group is its PID.
        if unsafe { libc::killpg(pid.try_into()?, libc::SIGKILL) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error).context("terminating child process group");
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    pub fn kill(&mut self) -> Result<()> {
        if let Some(job) = &self.job {
            job.terminate()
        } else {
            self.process.kill()?;
            Ok(())
        }
    }

    /// Terminates the group/job and reaps the direct child. Windows additionally
    /// waits for all job members to exit. Callers must drain captured pipes too.
    /// Unix does not wait for group disappearance: orphaned zombies can retain
    /// their group indefinitely even though they can no longer execute.
    pub async fn terminate_and_wait(&mut self) -> Result<()> {
        self.kill()?;
        self.process
            .status()
            .await
            .context("reaping terminated child")?;
        #[cfg(windows)]
        while self.has_running_processes()? == Some(true) {
            smol::unblock(|| std::thread::sleep(std::time::Duration::from_millis(10))).await;
        }
        Ok(())
    }

    /// Reports whether the Unix process group still exists, including zombies.
    /// This is not a teardown barrier; use `terminate_and_wait` and drain pipes.
    #[cfg(not(windows))]
    pub fn has_running_processes(&self) -> Result<Option<bool>> {
        // SAFETY: signal zero queries the child's process group without signaling it.
        if unsafe { libc::killpg(self.process.id().try_into()?, 0) } == 0 {
            return Ok(Some(true));
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(Some(false)),
            Some(libc::EPERM) => Ok(Some(true)),
            _ => Err(error).context("querying child process group"),
        }
    }

    /// Returns job activity, or `None` when descendant cleanup is unavailable.
    ///
    /// The count includes descendants after the direct child exits. `None`
    /// means job setup failed, not that the process tree is empty.
    ///
    /// # Errors
    ///
    /// Returns an error if Windows cannot query the owned job's activity.
    #[cfg(windows)]
    pub fn has_running_processes(&self) -> Result<Option<bool>> {
        self.job
            .as_ref()
            .map(windows_job::JobObject::has_running_processes)
            .transpose()
    }
}

fn describe_command(command: &std::process::Command) -> String {
    crate::redact::describe_command(
        command.get_program(),
        command.get_args(),
        command.get_current_dir(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn test_contained_kill_closes_descendant_pipes() {
        use futures_lite::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};

        smol::block_on(async {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "sleep 60 & child=$!; printf '%s\\n' \"$child\"; wait"]);
            let mut child =
                Child::spawn_contained(command, Stdio::null(), Stdio::piped(), Stdio::null())
                    .expect("spawn contained process tree");
            let mut output = BufReader::new(child.stdout.take().expect("captured stdout"));
            let mut line = String::new();
            output
                .read_line(&mut line)
                .await
                .expect("descendant readiness");
            let descendant = line.trim().parse::<u32>().expect("descendant PID");
            assert_ne!(descendant, child.id());
            assert_eq!(
                child.has_running_processes().expect("live group"),
                Some(true)
            );
            child
                .terminate_and_wait()
                .await
                .expect("terminate and reap process group");
            // Native process exit needs a wall-clock deadline. Dropping the sender
            // cancels the blocking wait when pipe draining wins the race.
            let (cancel_deadline, deadline) = std::sync::mpsc::channel::<()>();
            let closed = futures_lite::future::race(
                async {
                    output
                        .read_to_end(&mut Vec::new())
                        .await
                        .expect("drain descendant pipe");
                    true
                },
                async {
                    smol::unblock(move || {
                        matches!(
                            deadline.recv_timeout(std::time::Duration::from_secs(5)),
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
                        )
                    })
                    .await
                },
            )
            .await;
            drop(cancel_deadline);
            assert!(closed, "a surviving descendant retained the captured pipe");
        });
    }

    #[test]
    fn test_spawn_error_names_the_command_without_its_environment() {
        let mut command = std::process::Command::new("zed-test-missing-program");
        command
            .args([
                "--stdio",
                "-eTOKEN=option-secret",
                "-c",
                "API_KEY=argument-secret run",
            ])
            .current_dir(std::env::temp_dir().join("API_KEY=directory-secret"))
            .env("DATABASE_URL", "postgres://user:hunter2@db/app");
        let Err(error) = Child::spawn(command, Stdio::null(), Stdio::null(), Stdio::null()) else {
            panic!("spawning a missing program should fail");
        };
        let message = format!("{error:#}");
        assert!(
            message.contains(
                r#""zed-test-missing-program" "--stdio" "-eTOKEN=\"[REDACTED]\"" "-c" "API_KEY=\"[REDACTED]\" run" in "#
            ),
            "{message}"
        );
        assert!(message.contains(r#"API_KEY=\"[REDACTED]\""#), "{message}");
        for secret in ["option-secret", "argument-secret", "directory-secret"] {
            assert!(!message.contains(secret), "{message}");
        }
        assert!(!message.contains("DATABASE_URL"), "{message}");
        assert!(!message.contains("hunter2"), "{message}");
    }
}

#[cfg(windows)]
mod windows_job {
    use crate::ResultExt as _;
    use anyhow::{Context as _, Result};
    use windows::Win32::{
        Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
                QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
            },
            Threading::{
                OpenProcess, OpenThread, PROCESS_SET_QUOTA, PROCESS_TERMINATE, ResumeThread,
                THREAD_SUSPEND_RESUME,
            },
        },
    };

    /// A Win32 job object configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`:
    /// all processes assigned to the job (and their descendants) are terminated
    /// when the last handle to the job is closed, which happens when this struct
    /// is dropped, or when the OS closes the owning process's handles after it
    /// exits for any reason.
    pub(crate) struct JobObject(HANDLE);

    // SAFETY: Job object handles can be used from any thread.
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    pub(crate) fn resume_primary_thread(pid: u32) -> Result<()> {
        // std closes CreateProcess's primary-thread handle. The suspended child
        // has not executed, so its sole thread can be recovered by its owner PID.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)
                .context("enumerating suspended child threads")?;
            let _close_snapshot = crate::defer(|| {
                CloseHandle(snapshot).log_err();
            });
            let mut entry = THREADENTRY32 {
                dwSize: size_of::<THREADENTRY32>().try_into()?,
                ..Default::default()
            };
            Thread32First(snapshot, &mut entry).context("reading first thread entry")?;
            let mut primary_thread = None;
            loop {
                if entry.th32OwnerProcessID == pid {
                    anyhow::ensure!(
                        primary_thread.is_none(),
                        "suspended child has multiple threads"
                    );
                    primary_thread = Some(entry.th32ThreadID);
                }
                match Thread32Next(snapshot, &mut entry) {
                    Ok(()) => {}
                    Err(error)
                        if error.code()
                            == windows::core::HRESULT::from_win32(ERROR_NO_MORE_FILES.0) =>
                    {
                        break;
                    }
                    Err(error) => return Err(error).context("reading child thread entries"),
                }
            }
            let thread = OpenThread(
                THREAD_SUSPEND_RESUME,
                false,
                primary_thread.context("suspended child has no primary thread")?,
            )
            .context("opening suspended child thread")?;
            let _close_thread = crate::defer(|| {
                CloseHandle(thread).log_err();
            });
            let previous_count = ResumeThread(thread);
            if previous_count == u32::MAX {
                return Err(windows::core::Error::from_thread())
                    .context("resuming contained child");
            }
            anyhow::ensure!(
                previous_count == 1,
                "unexpected suspended child thread count: {previous_count}"
            );
            Ok(())
        }
    }

    impl JobObject {
        pub(crate) fn new() -> Result<Self> {
            unsafe {
                let job =
                    Self(CreateJobObjectW(None, None).context("failed to create job object")?);
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .context("failed to set job object limits")?;
                Ok(job)
            }
        }

        pub(crate) fn assign_process(&self, pid: u32) -> Result<()> {
            unsafe {
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid)
                    .context("failed to open process")?;
                let result = AssignProcessToJobObject(self.0, process)
                    .context("failed to assign process to job object");
                CloseHandle(process).log_err();
                result
            }
        }

        pub(crate) fn terminate(&self) -> Result<()> {
            unsafe { TerminateJobObject(self.0, 1).context("failed to terminate job object") }
        }

        pub(crate) fn has_running_processes(&self) -> Result<bool> {
            let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            // SAFETY: self owns a valid job handle, and accounting is writable
            // for the exact buffer size passed to this synchronous query.
            unsafe {
                QueryInformationJobObject(
                    Some(self.0),
                    JobObjectBasicAccountingInformation,
                    &mut accounting as *mut _ as *mut _,
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    None,
                )
                .context("failed to query active job processes")?;
            }
            Ok(accounting.ActiveProcesses > 0)
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0).log_err();
            }
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn test_unmanaged_child_reports_unavailable_job_activity() {
        let mut command = gpui_util::new_std_command("ping.exe");
        command.args(["-n", "60", "127.0.0.1"]);
        let process = smol::process::Command::from(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn unmanaged process");
        let mut child = Child { process, job: None };

        let pid = child.id();
        assert!(
            process_is_alive(pid),
            "direct child should still be running"
        );
        assert_eq!(
            child.has_running_processes().expect("failed to query job"),
            None
        );
        child
            .kill()
            .expect("failed to kill unassigned direct child");
        smol::block_on(child.status()).expect("failed to reap direct child");
        assert_process_exits(pid, "direct child should be terminated");
    }

    #[test]
    fn test_contained_termination_waits_for_descendants() {
        let temp_dir = tempfile::tempdir().expect("process tree directory");
        let (mut child, descendant) = spawn_process_tree(temp_dir.path(), true);
        assert_eq!(
            child.has_running_processes().expect("query contained job"),
            Some(true)
        );
        smol::block_on(child.terminate_and_wait()).expect("terminate and reap contained tree");
        assert_eq!(
            child.has_running_processes().expect("query terminated job"),
            Some(false)
        );
        assert!(
            !process_is_alive(descendant),
            "descendant survived completed teardown"
        );
    }

    #[test]
    fn test_job_tracks_grandchild_after_direct_child_exits() {
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("grandchild_pid");
        let mut command = gpui_util::new_std_command("powershell.exe");
        command.args(["-NoProfile", "-Command"]).arg(format!(
            "$p = Start-Process -FilePath ping.exe -ArgumentList @('-n','60','127.0.0.1') -PassThru -WindowStyle Hidden; \
             Set-Content -LiteralPath '{}' -Value $p.Id",
            pid_file.display()
        ));
        let mut child =
            Child::spawn_contained(command, Stdio::null(), Stdio::null(), Stdio::null())
                .expect("failed to spawn powershell");
        let status = smol::block_on(child.status()).expect("failed to await powershell");
        assert!(status.success(), "powershell failed: {status}");

        let grandchild_pid = std::fs::read_to_string(&pid_file)
            .expect("powershell did not record grandchild pid")
            .trim()
            .parse::<u32>()
            .expect("invalid grandchild pid");
        assert!(process_is_alive(grandchild_pid));
        assert_eq!(
            child.has_running_processes().expect("failed to query job"),
            Some(true),
            "job should remain active after its direct child exits"
        );

        child.kill().expect("failed to terminate job");
        let deadline = Instant::now() + Duration::from_secs(2);
        while child.has_running_processes().expect("failed to query job") != Some(false) {
            assert!(Instant::now() < deadline, "job did not become inactive");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_process_exits(grandchild_pid, "job grandchild should be terminated");
    }

    /// Spawns a process tree `powershell -> ping` via `Child::spawn` and
    /// returns the `Child` along with the pid of the grandchild (`ping`).
    fn spawn_process_tree(temp_dir: &std::path::Path, contained: bool) -> (Child, u32) {
        let pid_file = temp_dir.join("grandchild_pid");
        let partial_pid_file = temp_dir.join("grandchild_pid.partial");
        let mut command = std::process::Command::new("powershell.exe");
        command.args(["-NoProfile", "-Command"]).arg(format!(
            "$p = Start-Process -FilePath ping.exe -ArgumentList @('-n','60','127.0.0.1') -PassThru -WindowStyle Hidden; \
             Set-Content -LiteralPath '{0}' -Value $p.Id; \
             Move-Item -LiteralPath '{0}' -Destination '{1}'; \
             Wait-Process -Id $p.Id",
            partial_pid_file.display(),
            pid_file.display()
        ));
        let spawn = if contained {
            Child::spawn_contained
        } else {
            Child::spawn
        };
        let child = spawn(command, Stdio::null(), Stdio::null(), Stdio::null())
            .expect("failed to spawn powershell");

        // PowerShell startup exceeded 5 seconds on Windows CI runners under nextest's parallel load.
        let deadline = Instant::now() + Duration::from_secs(30);
        let grandchild_pid = loop {
            if let Ok(contents) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for grandchild pid file"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(
            process_is_alive(grandchild_pid),
            "grandchild should be alive after spawning"
        );
        (child, grandchild_pid)
    }

    fn process_is_alive(pid: u32) -> bool {
        use windows::Win32::{
            Foundation::{CloseHandle, STILL_ACTIVE},
            System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };

        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false;
            };
            let mut exit_code = 0u32;
            let alive = GetExitCodeProcess(handle, &mut exit_code).is_ok()
                && exit_code == STILL_ACTIVE.0 as u32;
            CloseHandle(handle).expect("failed to close process handle");
            alive
        }
    }

    fn assert_process_exits(pid: u32, message: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_is_alive(pid) {
            assert!(Instant::now() < deadline, "{message} (pid {pid})");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn test_kill_terminates_grandchildren() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (mut child, grandchild_pid) = spawn_process_tree(temp_dir.path(), false);

        child.kill().expect("failed to kill child");

        assert_process_exits(
            grandchild_pid,
            "grandchild should be terminated after killing the child",
        );
    }

    #[test]
    fn test_drop_terminates_grandchildren() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (child, grandchild_pid) = spawn_process_tree(temp_dir.path(), false);

        drop(child);

        assert_process_exits(
            grandchild_pid,
            "grandchild should be terminated after dropping the child",
        );
    }
}
