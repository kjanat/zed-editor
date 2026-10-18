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
    #[cfg(not(windows))]
    group: std::sync::Arc<std::sync::Mutex<ProcessGroupState>>,
    #[cfg(windows)]
    job: Option<windows_job::JobObject>,
}

#[cfg(not(windows))]
#[derive(Default)]
enum ProcessGroupState {
    #[default]
    Unreaped,
    Signalled,
    Reaped,
}

#[cfg(not(windows))]
impl ProcessGroupState {
    fn signal(&mut self, signal: impl FnOnce() -> Result<()>) -> Result<()> {
        if matches!(self, Self::Unreaped) {
            signal()?;
            *self = Self::Signalled;
        }
        Ok(())
    }
}

/// Owns containment and the only access to the child's reaping operations.
pub struct ContainedChild {
    process: smol::process::Child,
    executor: scheduler::BackgroundExecutor,
    pub stdout: Option<smol::process::ChildStdout>,
    pub stderr: Option<smol::process::ChildStderr>,
    #[cfg(not(windows))]
    group: ProcessGroupState,
    #[cfg(windows)]
    job: windows_job::JobObject,
    status: Option<std::process::ExitStatus>,
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
    pub fn spawn_contained(
        executor: scheduler::BackgroundExecutor,
        command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<ContainedChild> {
        ContainedChild::spawn(executor, command, stdin, stdout, stderr)
    }

    // Status futures can outlive the mutable borrow. Serialize every reaping
    // poll with group signalling, including when callers poll on another thread.
    #[cfg(not(windows))]
    pub fn status(
        &mut self,
    ) -> impl std::future::Future<Output = std::io::Result<std::process::ExitStatus>> + use<> {
        let future = self.process.status();
        let group = self.group.clone();
        async move {
            let mut future = std::pin::pin!(future);
            futures::future::poll_fn(move |cx| {
                let mut group = match group.lock() {
                    Ok(group) => group,
                    Err(_) => return std::task::Poll::Ready(Err(std::io::Error::other("process lifecycle lock poisoned"))),
                };
                let result = future.as_mut().poll(cx);
                if matches!(&result, std::task::Poll::Ready(Ok(_)))
                    || matches!(&result, std::task::Poll::Ready(Err(error)) if error.raw_os_error() == Some(libc::ECHILD))
                {
                    *group = ProcessGroupState::Reaped;
                }
                result
            })
            .await
        }
    }

    #[cfg(not(windows))]
    pub fn try_status(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        let mut group = self
            .group
            .lock()
            .map_err(|_| std::io::Error::other("process lifecycle lock poisoned"))?;
        let status = self.process.try_status();
        if matches!(&status, Ok(Some(_)))
            || matches!(&status, Err(error) if error.raw_os_error() == Some(libc::ECHILD))
        {
            *group = ProcessGroupState::Reaped;
        }
        status
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
        Ok(Self {
            process,
            group: Default::default(),
        })
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

    /// Consumes the child, draining its stdout/stderr and waiting for it to exit.
    pub async fn output(self) -> Result<std::process::Output> {
        // Keep the remaining fields of self alive across the await: dropping
        // the Windows job early would kill the child before output is collected.
        Ok(self.process.output().await?)
    }

    #[cfg(not(windows))]
    pub fn kill(&mut self) -> Result<()> {
        let mut group = self
            .group
            .lock()
            .map_err(|_| anyhow::anyhow!("process lifecycle lock poisoned"))?;
        group.signal(|| terminate_process_group(self.process.id().try_into()?))
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

    #[cfg(windows)]
    pub fn has_running_processes(&self) -> Result<Option<bool>> {
        self.job
            .as_ref()
            .map(windows_job::JobObject::has_running_processes)
            .transpose()
    }
}

impl ContainedChild {
    pub fn id(&self) -> u32 {
        self.process.id()
    }

    pub fn exit_status(&self) -> Option<std::process::ExitStatus> {
        self.status
    }

    #[cfg(not(windows))]
    async fn wait_for_exit(&mut self) -> std::io::Result<()> {
        self.process.stdin.take();
        let process_id = self.process.id();
        #[cfg(target_os = "freebsd")]
        let process_id = i64::from(process_id);
        loop {
            let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
            // SAFETY: waitid initializes siginfo; WNOWAIT reserves the leader PID
            // until group signalling succeeds. No other API can reap this child.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    process_id,
                    information.as_mut_ptr(),
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            if result == -1 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                if error.raw_os_error() == Some(libc::ECHILD) {
                    self.group = ProcessGroupState::Reaped;
                }
                return Err(error);
            }
            // SAFETY: waitid succeeded; zeroed si_pid represents a pending exit.
            let information = unsafe { information.assume_init() };
            // SAFETY: waitid populated the child-status fields of siginfo.
            if unsafe { information.si_pid() } != 0 {
                return Ok(());
            }
            self.executor
                .timer(std::time::Duration::from_millis(10))
                .await;
        }
    }

    #[cfg(windows)]
    async fn wait_for_exit(&mut self) -> std::io::Result<()> {
        self.process.status().await.map(drop)
    }

    /// Observes exit, signals remaining descendants, then reaps the leader.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        self.wait_for_exit()
            .await
            .context("observing contained child exit")?;
        self.terminate_and_wait().await?;
        self.status.context("missing reaped child status")
    }

    #[cfg(not(windows))]
    fn spawn(
        executor: scheduler::BackgroundExecutor,
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
        let mut process = smol::process::Command::from(command)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to spawn contained command {description}"))?;
        Ok(Self {
            stdout: process.stdout.take(),
            stderr: process.stderr.take(),
            process,
            executor,
            group: ProcessGroupState::Unreaped,
            status: None,
        })
    }

    /// Starts a command suspended, assigns its job, then resumes its primary
    /// thread. Setup failure terminates and reaps the child without a fallback.
    #[cfg(windows)]
    fn spawn(
        executor: scheduler::BackgroundExecutor,
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
            stdout: process.stdout.take(),
            stderr: process.stderr.take(),
            process,
            executor,
            job,
            status: None,
        })
    }

    /// Collects output until the process exits or cancellation supplies an error.
    /// Both paths terminate remaining descendants, reap the child, and drain its
    /// pipes before returning. The caller must keep this future alive through
    /// cleanup: signal cancellation instead of dropping it, retaining any owners
    /// of directories and locks used by the command until it returns.
    pub async fn output_until_cancelled(
        &mut self,
        cancellation: impl std::future::Future<Output = anyhow::Error>,
    ) -> Result<std::process::Output> {
        use futures::{AsyncReadExt as _, FutureExt as _};

        let mut stdout = self.stdout.take();
        let mut stderr = self.stderr.take();
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();
        let result = {
            let output = async {
                let (status, _, _) = futures::try_join!(
                    self.wait(),
                    async {
                        match stdout.as_mut() {
                            Some(stdout) => stdout
                                .read_to_end(&mut stdout_bytes)
                                .await
                                .context("reading command stdout"),
                            None => Ok(0),
                        }
                    },
                    async {
                        match stderr.as_mut() {
                            Some(stderr) => stderr
                                .read_to_end(&mut stderr_bytes)
                                .await
                                .context("reading command stderr"),
                            None => Ok(0),
                        }
                    },
                )?;
                Ok(std::process::Output {
                    status,
                    stdout: stdout_bytes,
                    stderr: stderr_bytes,
                })
            }
            .fuse();
            let cancellation = cancellation.fuse();
            futures::pin_mut!(output, cancellation);
            futures::select_biased! {
                result = output => result,
                error = cancellation => Err(error),
            }
        };
        // A failed signal can leave a writer alive indefinitely. Close our pipe
        // handles on that path instead of waiting for an EOF that cannot arrive.
        if let Err(error) = self.kill() {
            return Err(error).with_context(|| match &result {
                Ok(_) => "cleaning up completed command".to_owned(),
                Err(command_error) => format!("cleaning up failed command: {command_error:#}"),
            });
        }
        let (termination, stdout, stderr) = futures::join!(
            self.terminate_and_wait(),
            async {
                match stdout.as_mut() {
                    Some(stdout) => futures::io::copy(stdout, &mut futures::io::sink()).await,
                    None => Ok(0),
                }
            },
            async {
                match stderr.as_mut() {
                    Some(stderr) => futures::io::copy(stderr, &mut futures::io::sink()).await,
                    None => Ok(0),
                }
            },
        );
        let cleanup = termination
            .and_then(|()| {
                stdout
                    .context("draining terminated command stdout")
                    .map(drop)
            })
            .and_then(|()| {
                stderr
                    .context("draining terminated command stderr")
                    .map(drop)
            });
        if let Err(error) = cleanup {
            return match result {
                Ok(_) => Err(error),
                Err(command_error) => Err(error)
                    .with_context(|| format!("cleaning up failed command: {command_error:#}")),
            };
        }
        result
    }

    #[cfg(not(windows))]
    pub fn kill(&mut self) -> Result<()> {
        self.group
            .signal(|| terminate_process_group(self.process.id().try_into()?))
    }

    #[cfg(windows)]
    pub fn kill(&mut self) -> Result<()> {
        self.job.terminate()
    }

    /// Terminates the group/job and reaps the direct child. Windows additionally
    /// waits for all job members to exit. Callers must drain captured pipes too.
    /// Unix does not wait for group disappearance: orphaned zombies can retain
    /// their group indefinitely even though they can no longer execute.
    pub async fn terminate_and_wait(&mut self) -> Result<()> {
        self.kill()?;
        if self.status.is_none() {
            self.status = Some(
                self.process
                    .status()
                    .await
                    .context("reaping terminated child")?,
            );
            #[cfg(not(windows))]
            {
                self.group = ProcessGroupState::Reaped;
            }
        }
        #[cfg(windows)]
        while self.has_running_processes()? == Some(true) {
            self.executor
                .timer(std::time::Duration::from_millis(10))
                .await;
        }
        Ok(())
    }

    /// Reports whether the owned Unix process group exists, including zombies.
    /// Returns `None` after reaping releases ownership of the group ID.
    /// This is not a teardown barrier; use `terminate_and_wait` and drain pipes.
    #[cfg(not(windows))]
    pub fn has_running_processes(&self) -> Result<Option<bool>> {
        if matches!(self.group, ProcessGroupState::Reaped) {
            return Ok(None);
        }
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

    /// Returns activity of the job established before the child started running.
    ///
    /// # Errors
    ///
    /// Returns an error if Windows cannot query the owned job's activity.
    #[cfg(windows)]
    pub fn has_running_processes(&self) -> Result<Option<bool>> {
        self.job.has_running_processes().map(Some)
    }
}

impl Drop for ContainedChild {
    fn drop(&mut self) {
        if let Err(error) = self.kill() {
            log::error!("failed to terminate contained child: {error:#}");
        }
    }
}

#[cfg(not(windows))]
fn terminate_process_group(process_group: libc::pid_t) -> Result<()> {
    // SAFETY: contained children start their own session and process group.
    if unsafe { libc::killpg(process_group, libc::SIGKILL) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    if error.raw_os_error() == Some(libc::EPERM) && macos_process_group_has_exited(process_group)? {
        // Darwin skips zombies in killpg and reports EPERM for a group containing
        // only zombies. A live member or failed inspection must still be an error.
        return Ok(());
    }
    Err(error).context("terminating child process group")
}

#[cfg(target_os = "macos")]
fn macos_process_group_members(process_group: libc::pid_t) -> Result<Vec<libc::pid_t>> {
    // SAFETY: null requests the capacity in PIDs; no buffer is written.
    let capacity = unsafe { libc::proc_listpgrppids(process_group, std::ptr::null_mut(), 0) };
    anyhow::ensure!(
        capacity > 0,
        "failed to size child process group: {}",
        std::io::Error::last_os_error()
    );
    let mut members: Vec<libc::pid_t> = vec![0; usize::try_from(capacity)?];
    let bytes = std::mem::size_of_val(members.as_slice()).try_into()?;
    // libproc maps errors to zero, which also represents an empty group.
    // SAFETY: errno is thread-local; the allocated buffer has exactly `bytes` bytes.
    let count = unsafe {
        *libc::__error() = 0;
        libc::proc_listpgrppids(process_group, members.as_mut_ptr().cast(), bytes)
    };
    let error = std::io::Error::last_os_error();
    anyhow::ensure!(
        count >= 0 && (count != 0 || error.raw_os_error() == Some(0)),
        "failed to list child process group: {error}"
    );
    let count = usize::try_from(count)?;
    anyhow::ensure!(
        count < members.len(),
        "child process group listing was truncated"
    );
    members.truncate(count);
    Ok(members)
}

#[cfg(target_os = "macos")]
fn macos_process_group_has_exited(process_group: libc::pid_t) -> Result<bool> {
    let mut zombies = std::collections::HashSet::new();
    for member in macos_process_group_members(process_group)? {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdshortinfo>::uninit();
        let bytes = std::mem::size_of::<libc::proc_bsdshortinfo>().try_into()?;
        // SAFETY: libproc fills this correctly sized structure. Argument 1 includes
        // zombies, which are omitted when argument 0 is used for this flavor.
        let read = unsafe {
            *libc::__error() = 0;
            libc::proc_pidinfo(
                member,
                libc::PROC_PIDT_SHORTBSDINFO,
                1,
                info.as_mut_ptr().cast(),
                bytes,
            )
        };
        if read == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(error).context("inspecting child process group member");
        }
        anyhow::ensure!(
            read == bytes,
            "incomplete child process group member information"
        );
        // SAFETY: the successful call wrote the entire structure.
        let info = unsafe { info.assume_init() };
        if info.pbsi_pgid == u32::try_from(process_group)? {
            if info.pbsi_status != libc::SZOMB {
                return Ok(false);
            }
            zombies.insert(member);
        }
    }
    // A member can fork and then exit between enumeration and inspection. Reject
    // new members rather than treating that incomplete snapshot as an empty group.
    Ok(macos_process_group_members(process_group)?
        .into_iter()
        .all(|member| zombies.contains(&member)))
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

    pub(super) fn with_executor<T>(
        test: impl AsyncFnOnce(scheduler::BackgroundExecutor) -> T,
    ) -> T {
        scheduler::TestScheduler::once(async |scheduler| {
            scheduler.allow_parking();
            test(scheduler.background()).await
        })
    }

    #[cfg(unix)]
    #[test]
    fn test_exit_observation_reserves_leader_until_group_signal() {
        with_executor(async |executor| {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "exit 23"]);
            let mut child = Child::spawn_contained(
                executor.clone(),
                command,
                Stdio::null(),
                Stdio::null(),
                Stdio::null(),
            )
            .expect("spawn group leader");
            child.wait_for_exit().await.expect("observe leader exit");
            let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
            let process_id = child.id();
            #[cfg(target_os = "freebsd")]
            let process_id = i64::from(process_id);
            // SAFETY: WNOWAIT observes this owned child without releasing its PID.
            let observation = unsafe {
                libc::waitid(
                    libc::P_PID,
                    process_id,
                    information.as_mut_ptr(),
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            assert_eq!(
                observation,
                0,
                "leader was reaped before group signalling: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: waitid succeeded and initialized the structure.
            let information = unsafe { information.assume_init() };
            // SAFETY: waitid initialized siginfo for an exited child.
            assert_eq!(
                unsafe { information.si_pid() },
                libc::pid_t::try_from(child.id()).expect("PID")
            );
            child.terminate_and_wait().await.expect("signal then reap");
            assert_eq!(
                child.exit_status().and_then(|status| status.code()),
                Some(23)
            );
            child.terminate_and_wait().await.expect("repeat cleanup");
            child.kill().expect("repeat signal after reaping");
            assert_eq!(
                child
                    .has_running_processes()
                    .expect("released group ownership"),
                None
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_signal_failure_preserves_ownership_for_cleanup_retry() {
        with_executor(async |executor| {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "exit 23"]);
            let mut child = Child::spawn_contained(
                executor.clone(),
                command,
                Stdio::null(),
                Stdio::null(),
                Stdio::null(),
            )
            .expect("spawn leader");
            child.wait_for_exit().await.expect("observe exited leader");
            let error = child
                .group
                .signal(|| anyhow::bail!("injected signal failure"))
                .expect_err("signal error must propagate");
            assert_eq!(error.to_string(), "injected signal failure");
            assert!(child.exit_status().is_none());
            child
                .wait_for_exit()
                .await
                .expect("failed signal must retain waitable child");
            child
                .terminate_and_wait()
                .await
                .expect("retry real cleanup");
            child
                .group
                .signal(|| anyhow::bail!("signalled twice"))
                .expect("reaped ownership must never signal again");
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_ordinary_owned_status_future_prevents_signalling_reaped_group() {
        with_executor(async |_executor| {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "exit 23"]);
            let mut child = Child::spawn(command, Stdio::null(), Stdio::null(), Stdio::null())
                .expect("spawn ordinary child");
            let status = child.status();
            // ACP and kernel watchers poll this future after releasing the child lock.
            assert_eq!(status.await.expect("reap watched child").code(), Some(23));
            let mut group = child.group.lock().expect("lifecycle lock");
            group
                .signal(|| anyhow::bail!("signalled reaped group"))
                .expect("owned wait must revoke group signalling");
            drop(group);
            child.kill().expect("safe cleanup after detached wait");
            assert!(child.try_status().expect("cached status").is_some());
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_natural_exit_closes_descendant_pipes_and_preserves_output() {
        with_executor(async |executor| {
            let mut command = std::process::Command::new("sh");
            command.args([
                "-c",
                "sleep 60 & printf output; printf diagnostic >&2; exit 23",
            ]);
            let mut child = Child::spawn_contained(
                executor.clone(),
                command,
                Stdio::null(),
                Stdio::piped(),
                Stdio::piped(),
            )
            .expect("spawn pipe-holding descendant");
            let cancellation = async {
                executor.timer(std::time::Duration::from_secs(5)).await;
                anyhow::anyhow!("descendant retained pipes after leader exit")
            };
            let output = child
                .output_until_cancelled(cancellation)
                .await
                .expect("natural exit cleanup");
            assert_eq!(output.status.code(), Some(23));
            assert_eq!(output.stdout, b"output");
            assert_eq!(output.stderr, b"diagnostic");
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_deadline_reaps_process_and_closes_pipes() {
        with_executor(async |executor| {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "sleep 60 & wait"]);
            let mut child = Child::spawn_contained(
                executor.clone(),
                command,
                Stdio::null(),
                Stdio::piped(),
                Stdio::piped(),
            )
            .expect("spawn process for deadline");
            let deadline = async {
                executor.timer(std::time::Duration::from_millis(20)).await;
                anyhow::anyhow!("deadline expired")
            };
            let error = child
                .output_until_cancelled(deadline)
                .await
                .expect_err("deadline must cancel");
            assert_eq!(error.to_string(), "deadline expired");
            assert!(child.exit_status().is_some());
            child
                .terminate_and_wait()
                .await
                .expect("repeat deadline cleanup");
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_termination_accepts_zombie_only_group_but_detects_live_members() {
        // SAFETY: the child uses only async-signal-safe calls before _exit.
        let child = unsafe { libc::fork() };
        assert!(
            child >= 0,
            "fork failed: {}",
            std::io::Error::last_os_error()
        );
        if child == 0 {
            // SAFETY: setsid, getpid, kill, and _exit are async-signal-safe.
            unsafe {
                if libc::setsid() == -1 {
                    libc::_exit(1);
                }
                if libc::kill(libc::getpid(), libc::SIGSTOP) == -1 {
                    libc::_exit(2);
                }
                libc::_exit(0);
            }
        }
        let _reap = crate::defer(move || {
            // SAFETY: this test owns the unreaped child PID until this guard runs.
            if unsafe { libc::kill(child, libc::SIGKILL) } == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    log::error!("failed to terminate test child: {error}");
                }
            }
            // SAFETY: null discards status; waitpid reaps only the owned child.
            if unsafe { libc::waitpid(child, std::ptr::null_mut(), 0) } == -1 {
                log::error!(
                    "failed to reap test child: {}",
                    std::io::Error::last_os_error()
                );
            }
        });
        let mut status = 0;
        // SAFETY: status points to writable memory; WUNTRACED observes the stop.
        assert_eq!(
            unsafe { libc::waitpid(child, &mut status, libc::WUNTRACED) },
            child
        );
        assert!(
            libc::WIFSTOPPED(status),
            "child did not establish its group"
        );
        assert!(!macos_process_group_has_exited(child).expect("inspect live group"));
        // SAFETY: this is the stopped child owned by the test.
        assert_eq!(unsafe { libc::kill(child, libc::SIGCONT) }, 0);
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
        // SAFETY: waitid initializes siginfo and WNOWAIT deliberately retains the
        // zombie so the cleanup result cannot depend on launchd reaping timing.
        assert_eq!(
            unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.try_into().expect("child PID"),
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOWAIT,
                )
            },
            0
        );
        assert!(macos_process_group_has_exited(child).expect("inspect zombie group"));
        terminate_process_group(child).expect("an already exited group must not fail teardown");
    }

    #[cfg(unix)]
    #[test]
    fn test_completed_output_preserves_status_and_descendant_output() {
        with_executor(async |executor| {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "sh -c 'printf child'; printf parent"]);
            let mut child = Child::spawn_contained(
                executor.clone(),
                command,
                Stdio::null(),
                Stdio::piped(),
                Stdio::piped(),
            )
            .expect("spawn contained process tree");
            let output = child
                .output_until_cancelled(futures::future::pending())
                .await
                .expect("naturally completed command must succeed");
            assert!(output.status.success());
            assert_eq!(output.stdout, b"childparent");
            assert!(output.stderr.is_empty());
            assert!(child.exit_status().is_some());
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_cancelled_output_reaps_child_and_closes_descendant_pipes() {
        use futures_lite::io::{AsyncBufReadExt as _, BufReader};

        with_executor(async |executor| {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "sleep 60 & printf 'ready\\n'; wait"]);
            let mut child = Child::spawn_contained(
                executor.clone(),
                command,
                Stdio::null(),
                Stdio::piped(),
                Stdio::piped(),
            )
            .expect("spawn contained process tree");
            let mut stdout = BufReader::new(child.stdout.take().expect("captured stdout"));
            let mut readiness = String::new();
            stdout
                .read_line(&mut readiness)
                .await
                .expect("descendant ready");
            assert_eq!(readiness, "ready\n");
            child.stdout = Some(stdout.into_inner());
            let error = child
                .output_until_cancelled(futures::future::ready(anyhow::anyhow!(
                    "test cancellation"
                )))
                .await
                .expect_err("cancelled process must fail after cleanup");
            assert_eq!(error.to_string(), "test cancellation");
            assert!(child.exit_status().is_some());
            assert!(child.stdout.is_none());
            assert!(child.stderr.is_none());
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_contained_kill_closes_descendant_pipes() {
        use futures_lite::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};

        with_executor(async |executor| {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "sleep 60 & child=$!; printf '%s\\n' \"$child\"; wait"]);
            let mut child = Child::spawn_contained(
                executor.clone(),
                command,
                Stdio::null(),
                Stdio::piped(),
                Stdio::null(),
            )
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
            child.kill().expect("signal process group");
            child.wait().await.expect("reap signalled child");
            child
                .terminate_and_wait()
                .await
                .expect("repeat termination after child exit");
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
    use super::tests::with_executor;
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn test_completed_output_preserves_status_and_descendant_output() {
        with_executor(async |executor| {
            let mut command = gpui_util::new_std_command("cmd.exe");
            command.args(["/d", "/c", "cmd /d /c echo child&echo parent"]);
            let mut child = Child::spawn_contained(
                executor.clone(),
                command,
                Stdio::null(),
                Stdio::piped(),
                Stdio::piped(),
            )
            .expect("spawn contained process tree");
            let output = child
                .output_until_cancelled(futures::future::pending())
                .await
                .expect("naturally completed command must succeed");
            assert!(output.status.success());
            assert_eq!(output.stdout, b"child\r\nparent\r\n");
            assert!(output.stderr.is_empty());
            assert_eq!(
                child.has_running_processes().expect("inactive job"),
                Some(false)
            );
        });
    }

    #[test]
    fn test_cancelled_output_waits_for_descendants() {
        with_executor(async |executor| {
            let temp_dir = tempfile::tempdir().expect("process tree directory");
            let (mut child, descendant) =
                spawn_contained_process_tree(temp_dir.path(), executor.clone());
            let error = child
                .output_until_cancelled(futures::future::ready(anyhow::anyhow!(
                    "test cancellation"
                )))
                .await
                .expect_err("cancelled process must fail after cleanup");
            assert_eq!(error.to_string(), "test cancellation");
            assert!(child.exit_status().is_some());
            assert_eq!(
                child.has_running_processes().expect("inactive job"),
                Some(false)
            );
            assert!(
                !process_is_alive(descendant),
                "descendant survived cancellation"
            );
        });
    }

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
        with_executor(async |executor| {
            let temp_dir = tempfile::tempdir().expect("process tree directory");
            let (mut child, descendant) =
                spawn_contained_process_tree(temp_dir.path(), executor.clone());
            assert_eq!(
                child.has_running_processes().expect("query contained job"),
                Some(true)
            );
            child
                .terminate_and_wait()
                .await
                .expect("terminate and reap contained tree");
            assert_eq!(
                child.has_running_processes().expect("query terminated job"),
                Some(false)
            );
            assert!(
                !process_is_alive(descendant),
                "descendant survived completed teardown"
            );
        });
    }

    #[test]
    fn test_job_tracks_grandchild_after_direct_child_exits() {
        with_executor(async |executor| {
            let temp_dir = tempfile::tempdir().unwrap();
            let pid_file = temp_dir.path().join("grandchild_pid");
            let mut command = gpui_util::new_std_command("powershell.exe");
            command.args(["-NoProfile", "-Command"]).arg(format!(
            "$p = Start-Process -FilePath ping.exe -ArgumentList @('-n','60','127.0.0.1') -PassThru -WindowStyle Hidden; \
             Set-Content -LiteralPath '{}' -Value $p.Id",
            pid_file.display()
        ));
            let mut child = Child::spawn_contained(
                executor.clone(),
                command,
                Stdio::null(),
                Stdio::null(),
                Stdio::null(),
            )
            .expect("failed to spawn powershell");
            child
                .wait_for_exit()
                .await
                .expect("failed to await powershell");

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
        });
    }

    /// Spawns a process tree `powershell -> ping` via `Child::spawn` and
    /// returns the `Child` along with the pid of the grandchild (`ping`).
    fn spawn_contained_process_tree(
        temp_dir: &std::path::Path,
        executor: scheduler::BackgroundExecutor,
    ) -> (ContainedChild, u32) {
        spawn_process_tree(temp_dir, |command, stdin, stdout, stderr| {
            Child::spawn_contained(executor, command, stdin, stdout, stderr)
        })
    }

    fn spawn_process_tree<T>(
        temp_dir: &std::path::Path,
        spawn: impl FnOnce(std::process::Command, Stdio, Stdio, Stdio) -> Result<T>,
    ) -> (T, u32) {
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
        let (mut child, grandchild_pid) = spawn_process_tree(temp_dir.path(), Child::spawn);

        child.kill().expect("failed to kill child");

        assert_process_exits(
            grandchild_pid,
            "grandchild should be terminated after killing the child",
        );
    }

    #[test]
    fn test_drop_terminates_grandchildren() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (child, grandchild_pid) = spawn_process_tree(temp_dir.path(), Child::spawn);

        drop(child);

        assert_process_exits(
            grandchild_pid,
            "grandchild should be terminated after dropping the child",
        );
    }
}
