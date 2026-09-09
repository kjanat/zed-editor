use std::{ffi::OsStr, path::Path};

#[cfg(target_os = "macos")]
mod darwin;

#[cfg(target_os = "macos")]
pub use darwin::{Child, Command, Stdio};

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000_u32;

pub use gpui_util::new_std_command;

pub fn new_command(program: impl AsRef<OsStr>) -> Command {
    Command::new(program)
}

pub fn new_command_with_env(
    program: impl AsRef<OsStr>,
    working_directory: &Path,
    env: &collections::HashMap<String, String>,
) -> anyhow::Result<Command> {
    #[cfg(target_os = "windows")]
    let program = {
        // CreateProcess does not search PATHEXT for npm's .cmd shims, and
        // extensions may supply the path to the adjacent Unix shell script.
        resolve_command_path(program, working_directory, env)?
    };

    let mut command = new_command(program);
    command.current_dir(working_directory).envs(env);
    Ok(command)
}

pub fn resolve_command_path(
    program: impl AsRef<OsStr>,
    working_directory: &Path,
    env: &collections::HashMap<String, String>,
) -> which::Result<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    {
        which::WhichConfig::new_with_sys(WindowsCommandEnvironment {
            working_directory,
            env,
        })
        .binary_name(program.as_ref().to_owned())
        .first_result()
    }
    #[cfg(not(target_os = "windows"))]
    which::which_in(program, env.get("PATH"), working_directory)
}

#[cfg(target_os = "windows")]
// RealSys caches the process PATHEXT. Each command needs its own environment,
// without mutating process-wide variables during concurrent server launches.
struct WindowsCommandEnvironment<'a> {
    working_directory: &'a Path,
    env: &'a collections::HashMap<String, String>,
}

#[cfg(target_os = "windows")]
impl WindowsCommandEnvironment<'_> {
    fn var_os(&self, name: &str) -> Option<std::ffi::OsString> {
        // Match Command::envs: Windows keys are case-insensitive, and the last
        // override wins if the map contains multiple spellings of the same key.
        self.env
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| std::ffi::OsString::from(value))
            .last()
            .or_else(|| std::env::var_os(name))
    }
}

#[cfg(target_os = "windows")]
impl which::sys::Sys for WindowsCommandEnvironment<'_> {
    type ReadDirEntry = std::fs::DirEntry;
    type Metadata = std::fs::Metadata;

    fn is_windows(&self) -> bool {
        true
    }

    fn current_dir(&self) -> std::io::Result<std::path::PathBuf> {
        Ok(self.working_directory.to_path_buf())
    }

    fn home_dir(&self) -> Option<std::path::PathBuf> {
        which::sys::RealSys.home_dir()
    }

    fn env_split_paths(&self, paths: &OsStr) -> Vec<std::path::PathBuf> {
        which::sys::RealSys.env_split_paths(paths)
    }

    fn env_path(&self) -> Option<std::ffi::OsString> {
        self.var_os("PATH")
    }

    fn env_path_ext(&self) -> Option<std::ffi::OsString> {
        self.var_os("PATHEXT")
    }

    fn metadata(&self, path: &Path) -> std::io::Result<Self::Metadata> {
        which::sys::RealSys.metadata(path)
    }

    fn symlink_metadata(&self, path: &Path) -> std::io::Result<Self::Metadata> {
        which::sys::RealSys.symlink_metadata(path)
    }

    fn read_dir(
        &self,
        path: &Path,
    ) -> std::io::Result<Box<dyn Iterator<Item = std::io::Result<Self::ReadDirEntry>>>> {
        which::sys::RealSys.read_dir(path)
    }

    fn is_valid_executable(&self, path: &Path) -> std::io::Result<bool> {
        which::sys::RealSys.is_valid_executable(path)
    }
}

#[cfg(not(target_os = "macos"))]
pub type Child = smol::process::Child;

#[cfg(not(target_os = "macos"))]
pub use std::process::Stdio;

#[cfg(not(target_os = "macos"))]
#[derive(Debug)]
pub struct Command(smol::process::Command);

#[cfg(not(target_os = "macos"))]
impl Command {
    #[inline]
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        #[cfg(target_os = "windows")]
        {
            use smol::process::windows::CommandExt;
            let mut cmd = smol::process::Command::new(program);
            cmd.creation_flags(CREATE_NO_WINDOW);
            Self(cmd)
        }
        #[cfg(not(target_os = "windows"))]
        Self(smol::process::Command::new(program))
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.0.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.0.args(args);
        self
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.0.get_args()
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.0.env(key, val);
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.0.envs(vars);
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.0.env_remove(key);
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.0.env_clear();
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.0.current_dir(dir);
        self
    }

    pub fn stdin(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.0.stdin(cfg.into());
        self
    }

    pub fn stdout(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.0.stdout(cfg.into());
        self
    }

    pub fn stderr(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.0.stderr(cfg.into());
        self
    }

    pub fn kill_on_drop(&mut self, kill_on_drop: bool) -> &mut Self {
        self.0.kill_on_drop(kill_on_drop);
        self
    }

    pub fn spawn(&mut self) -> std::io::Result<Child> {
        self.0.spawn()
    }

    pub async fn output(&mut self) -> std::io::Result<std::process::Output> {
        self.0.output().await
    }

    pub async fn status(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.0.status().await
    }

    pub fn get_program(&self) -> &OsStr {
        self.0.get_program()
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use collections::HashMap;

    #[test]
    fn resolves_windows_command_shims() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let bin_directory = directory
            .path()
            .join("node_modules with spaces")
            .join(".bin");
        std::fs::create_dir_all(&bin_directory)?;
        let script = bin_directory.join("zed-test-server");
        std::fs::write(&script, "#!/bin/sh\nexit 1\n")?;
        std::fs::write(
            script.with_extension("cmd"),
            "@echo off\r\n@echo %~1\r\n@echo %ZED_TEST_SERVER_ENV%\r\n@cd\r\n",
        )?;
        let env = HashMap::from_iter([
            ("Path".into(), bin_directory.to_string_lossy().into_owned()),
            ("PathExt".into(), ".EXE;.CMD".into()),
            ("ZED_TEST_SERVER_ENV".into(), "server environment".into()),
        ]);

        for program in [
            std::path::PathBuf::from("zed-test-server"),
            script.strip_prefix(directory.path())?.to_path_buf(),
            script.clone(),
            script.with_extension("cmd"),
        ] {
            let mut command = new_command_with_env(&program, directory.path(), &env)?;
            assert_eq!(command.get_program(), script.with_extension("cmd"));
            let output = smol::block_on(command.arg("argument with spaces").output())?;
            assert!(output.status.success(), "{program:?}: {output:?}");
            let stdout = String::from_utf8(output.stdout)?;
            let lines = stdout.lines().collect::<Vec<_>>();
            assert_eq!(lines.first().copied(), Some("argument with spaces"));
            assert_eq!(lines.get(1).copied(), Some("server environment"));
            assert_eq!(
                lines.get(2).map(std::path::Path::new),
                Some(directory.path())
            );
        }
        Ok(())
    }

    #[test]
    fn resolves_windows_commands_in_path_order() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let first_directory = directory.path().join("first");
        let second_directory = directory.path().join("second");
        std::fs::create_dir_all(&first_directory)?;
        std::fs::create_dir_all(&second_directory)?;
        std::fs::write(first_directory.join("zed-test-server"), "#!/bin/sh\n")?;
        let first_command = first_directory.join("zed-test-server.cmd");
        std::fs::write(&first_command, "@exit /b 0\r\n")?;
        std::fs::copy(
            std::env::current_exe()?,
            second_directory.join("zed-test-server.exe"),
        )?;
        let env = HashMap::from_iter([
            (
                "PATH".into(),
                std::env::join_paths([&first_directory, &second_directory])?
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("PATHEXT".into(), ".EXE;.CMD".into()),
        ]);
        let command = new_command_with_env("zed-test-server", directory.path(), &env)?;
        assert_eq!(command.get_program(), first_command);

        let executable = first_directory.join("zed-test-native.exe");
        std::fs::copy(std::env::current_exe()?, &executable)?;
        let command = new_command_with_env(&executable, directory.path(), &env)?;
        assert_eq!(command.get_program(), executable);
        Ok(())
    }

    #[test]
    fn resolves_windows_commands_with_each_environment() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let bin_directory = directory.path().join("tools");
        std::fs::create_dir_all(&bin_directory)?;
        std::fs::write(bin_directory.join("zed-test-server.cmd"), "@echo cmd\r\n")?;
        std::fs::write(bin_directory.join("zed-test-server.bat"), "@echo bat\r\n")?;

        for (extensions, expected) in [(".CMD;.BAT", "cmd"), (".BAT;.CMD", "bat")] {
            let env = HashMap::from_iter([
                ("pAtH".into(), "tools".into()),
                ("pAtHeXt".into(), extensions.into()),
            ]);
            let mut command = new_command_with_env("zed-test-server", directory.path(), &env)?;
            let output = smol::block_on(command.output())?;
            assert!(output.status.success(), "{output:?}");
            assert_eq!(String::from_utf8(output.stdout)?.trim(), expected);
        }
        Ok(())
    }

    #[test]
    fn preserves_extensionless_windows_executables() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let executable = directory.path().join("zed-test-native");
        std::fs::copy(std::env::current_exe()?, &executable)?;
        std::fs::write(executable.with_extension("cmd"), "@exit /b 1\r\n")?;
        let env = HashMap::from_iter([
            (
                "PATH".into(),
                directory.path().to_string_lossy().into_owned(),
            ),
            ("PATHEXT".into(), ".EXE;.CMD".into()),
        ]);
        let command = new_command_with_env(&executable, directory.path(), &env)?;
        assert_eq!(command.get_program(), executable);
        Ok(())
    }

    #[test]
    fn does_not_infer_extensions_when_pathext_is_empty() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let script = directory.path().join("zed-test-server.cmd");
        std::fs::write(&script, "@exit /b 0\r\n")?;
        let env = HashMap::from_iter([
            (
                "PATH".into(),
                directory.path().to_string_lossy().into_owned(),
            ),
            ("PATHEXT".into(), String::new()),
        ]);
        assert!(new_command_with_env("zed-test-server", directory.path(), &env).is_err());
        let command = new_command_with_env(&script, directory.path(), &env)?;
        assert_eq!(command.get_program(), script);
        Ok(())
    }

    #[test]
    fn missing_windows_command_returns_error() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let env = HashMap::from_iter([(
            "PATH".into(),
            directory.path().to_string_lossy().into_owned(),
        )]);
        assert!(new_command_with_env("zed-missing-server", directory.path(), &env).is_err());
        Ok(())
    }
}
