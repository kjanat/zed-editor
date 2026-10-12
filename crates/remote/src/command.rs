use anyhow::{Context as _, Result, ensure};
use collections::HashMap;
use serde::{Deserialize, Serialize};
use std::{io::Read, time::Duration};
#[cfg(unix)]
use std::{
    io::{ErrorKind, Write as _},
    path::Path,
    time::Instant,
};
#[cfg(unix)]
use util::ResultExt as _;
use util::shell::ShellKind;

#[derive(Serialize, Deserialize)]
pub struct RemoteCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub working_dir: Option<String>,
}

impl RemoteCommand {
    pub fn retain_valid_env(&mut self) {
        self.env.retain(|name, _| {
            let valid = !name.is_empty() && !name.contains(['=', '\0']);
            if !valid {
                log::warn!("Skipping environment variable with invalid name");
            }
            valid
        });
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut buffer = vec![0; size_of::<u32>()];
        serde_json::to_writer(&mut buffer, self)?;
        let len = buffer.len() - size_of::<u32>();
        ensure!(len <= MAX_COMMAND_SIZE, "Remote command exceeds 16 MiB");
        buffer[..size_of::<u32>()].copy_from_slice(&(len as u32).to_le_bytes());
        Ok(buffer)
    }

    pub fn read(reader: &mut impl Read) -> Result<Self> {
        let mut len = [0; size_of::<u32>()];
        reader
            .read_exact(&mut len)
            .context("Failed to read remote command length")?;
        let len = u32::from_le_bytes(len) as usize;
        ensure!(len <= MAX_COMMAND_SIZE, "Remote command exceeds 16 MiB");
        let mut buffer = vec![0; len];
        reader
            .read_exact(&mut buffer)
            .context("Failed to read remote command")?;
        serde_json::from_slice(&buffer).map_err(|_| anyhow::anyhow!("Invalid remote command"))
    }
}

pub const STAGED_COMMAND_DIRECTORY: &str = "commands";

pub fn new_staged_command_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn is_valid_staged_command_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}

pub fn staged_launcher_command(launcher: &str, id: &str) -> Result<String> {
    ensure!(
        is_valid_staged_command_id(id),
        "Invalid staged remote command id"
    );
    Ok(format!("{launcher} --descriptor {id}"))
}

pub const STAGED_COMMAND_WAIT: Duration = Duration::from_secs(10);

#[cfg(unix)]
pub fn write_staged_command(directory: &Path, id: &str, frame: &[u8]) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};

    ensure!(
        is_valid_staged_command_id(id),
        "Invalid staged remote command id"
    );
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .context("Failed to create staged command directory")?;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
        .context("Failed to restrict staged command directory")?;

    let partial = directory.join(format!("{id}.partial"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)
        .context("Failed to create staged command")?;
    let written = file.write_all(frame).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written.and_then(|()| std::fs::rename(&partial, directory.join(id))) {
        std::fs::remove_file(&partial).log_err();
        return Err(error).context("Failed to write staged command");
    }
    Ok(())
}

#[cfg(unix)]
pub fn read_staged_command(root: &Path, id: &str, wait: Duration) -> Result<RemoteCommand> {
    ensure!(
        is_valid_staged_command_id(id),
        "Invalid staged remote command id"
    );
    let deadline = Instant::now() + wait;
    loop {
        if let Some(mut file) = open_staged_command(root, id)? {
            return RemoteCommand::read(&mut file);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("Staged remote command not found");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn open_staged_command(root: &Path, id: &str) -> Result<Option<std::fs::File>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Failed to read staged command directory"),
    };
    for entry in entries {
        let path = entry
            .context("Failed to read staged command directory")?
            .path()
            .join(id);
        match std::fs::File::open(&path) {
            Ok(file) => return Ok(Some(file)),
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
            Err(error) => return Err(error).context("Failed to open staged command"),
        }
    }
    Ok(None)
}

pub fn stdio_launcher_command(shell_kind: ShellKind, remote_binary_path: &str) -> Result<String> {
    let helper = shell_kind
        .try_quote(remote_binary_path)
        .context("shell quoting")?;
    Ok(format!("exec {helper} exec"))
}

pub fn home_stdio_launcher_command(
    shell_kind: ShellKind,
    home_dir: Option<&str>,
    relative_binary_path: &str,
) -> Result<String> {
    match home_dir {
        Some(home_dir) => stdio_launcher_command(
            shell_kind,
            &format!("{}/{relative_binary_path}", home_dir.trim_end_matches('/')),
        ),
        None => Ok(format!(
            "exec {} exec",
            home_relative_path(shell_kind, Some(relative_binary_path))?
        )),
    }
}

pub fn home_relative_path(shell_kind: ShellKind, relative_path: Option<&str>) -> Result<String> {
    let quoted = relative_path
        .map(|relative_path| shell_kind.try_quote(relative_path).context("shell quoting"))
        .transpose()?;
    Ok(match (shell_kind, quoted) {
        (ShellKind::Nushell, Some(quoted)) => format!("($env.HOME | path join {quoted})"),
        (ShellKind::Nushell, None) => String::from("$env.HOME"),
        (_, Some(quoted)) => format!("\"$HOME\"/{quoted}"),
        (_, None) => String::from("\"$HOME\""),
    })
}

const MAX_COMMAND_SIZE: usize = 16 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_remote_command_frame() -> Result<()> {
        let json = br#"{"program":"agent","args":["","--acp"],"env":{"TOKEN":"'\"\n\r\t\\$()=secret\n"},"working_dir":"~/project with spaces"}"#;
        let mut input = (json.len() as u32).to_le_bytes().to_vec();
        input.extend_from_slice(json);
        let remaining = b"{\"jsonrpc\":\"2.0\",\"id\":1}\n\0\xff";
        input.extend_from_slice(remaining);
        let mut input = Cursor::new(input);
        let command = RemoteCommand::read(&mut input)?;
        assert_eq!(command.program, "agent");
        assert_eq!(command.args, ["", "--acp"]);
        assert_eq!(
            command.env,
            HashMap::from_iter([(
                String::from("TOKEN"),
                String::from("'\"\n\r\t\\$()=secret\n")
            )])
        );
        assert_eq!(
            command.working_dir.as_deref(),
            Some("~/project with spaces")
        );
        let mut output = Vec::new();
        input.read_to_end(&mut output)?;
        assert_eq!(output, remaining);
        let encoded = command.encode()?;
        assert_eq!(&encoded[4..], json);
        assert_eq!(&encoded[..4], &(json.len() as u32).to_le_bytes());
        Ok(())
    }

    #[test]
    fn test_remote_command_size_limit() -> Result<()> {
        let mut command = RemoteCommand {
            program: String::from("agent"),
            args: Vec::new(),
            env: HashMap::default(),
            working_dir: None,
        };
        let overhead = command.encode()?.len() - size_of::<u32>();
        command
            .program
            .push_str(&"a".repeat(MAX_COMMAND_SIZE - overhead));
        let encoded = command.encode()?;
        assert_eq!(encoded.len(), MAX_COMMAND_SIZE + size_of::<u32>());
        assert_eq!(
            RemoteCommand::read(&mut &encoded[..])?.program,
            command.program
        );
        command.program.push('a');
        assert_eq!(
            command.encode().unwrap_err().to_string(),
            "Remote command exceeds 16 MiB"
        );
        let oversized = (MAX_COMMAND_SIZE as u32 + 1).to_le_bytes();
        let error = RemoteCommand::read(&mut &oversized[..]).err().unwrap();
        assert_eq!(error.to_string(), "Remote command exceeds 16 MiB");
        Ok(())
    }

    #[test]
    fn test_retain_valid_env() {
        let mut command = RemoteCommand {
            program: String::from("agent"),
            args: Vec::new(),
            env: HashMap::from_iter(
                [
                    ("", "empty"),
                    ("NAME=VALUE", "equals"),
                    ("NAME\0NUL", "nul"),
                    ("LINE\nBREAK", "kept"),
                    ("TAB\tKEY", "kept"),
                    ("KEY-WITH-DASH", "kept"),
                    ("GH_TOKEN", "kept"),
                ]
                .map(|(name, value)| (name.to_owned(), value.to_owned())),
            ),
            working_dir: None,
        };
        command.retain_valid_env();
        assert_eq!(
            command.env,
            HashMap::from_iter([
                (String::from("LINE\nBREAK"), String::from("kept")),
                (String::from("TAB\tKEY"), String::from("kept")),
                (String::from("KEY-WITH-DASH"), String::from("kept")),
                (String::from("GH_TOKEN"), String::from("kept")),
            ])
        );
    }

    #[test]
    fn test_home_relative_path() -> Result<()> {
        assert_eq!(home_relative_path(ShellKind::Posix, None)?, "\"$HOME\"");
        assert_eq!(
            home_relative_path(ShellKind::Posix, Some("a b/c"))?,
            "\"$HOME\"/'a b/c'"
        );
        assert_eq!(
            home_relative_path(ShellKind::Tcsh, Some("bang!"))?,
            "\"$HOME\"/'bang'\\!"
        );
        assert_eq!(home_relative_path(ShellKind::Nushell, None)?, "$env.HOME");
        assert_eq!(
            home_relative_path(ShellKind::Nushell, Some("a b/O'Brien"))?,
            "($env.HOME | path join \"a b/O'Brien\")"
        );
        assert_eq!(
            home_stdio_launcher_command(ShellKind::Nushell, None, ".zed_server/server")?,
            "exec ($env.HOME | path join .zed_server/server) exec"
        );
        assert_eq!(
            home_stdio_launcher_command(ShellKind::Nushell, Some("/home/u/"), "x y")?,
            "exec '/home/u/x y' exec"
        );
        assert_eq!(
            home_stdio_launcher_command(ShellKind::Posix, None, "x y")?,
            "exec \"$HOME\"/'x y' exec"
        );
        Ok(())
    }

    #[test]
    fn test_staged_command_ids() -> Result<()> {
        let id = new_staged_command_id();
        assert!(is_valid_staged_command_id(&id), "{id}");
        for invalid in ["", "../etc/passwd", "id with space", "a/b", &"a".repeat(65)] {
            assert!(!is_valid_staged_command_id(invalid), "{invalid}");
        }
        assert_eq!(
            staged_launcher_command("exec '/home/u/x y' exec", &id)?,
            format!("exec '/home/u/x y' exec --descriptor {id}")
        );
        assert!(staged_launcher_command("exec helper exec", "; rm -rf ~").is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_staged_command_round_trip() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let temp_dir = tempfile::tempdir()?;
        let root = temp_dir.path().join("server_state/commands");
        let directory = root.join("4242");
        let command = RemoteCommand {
            program: String::from("agent"),
            args: vec![String::from("--acp")],
            env: HashMap::from_iter([(String::from("API_KEY"), String::from("agent-secret"))]),
            working_dir: Some(String::from("~/project")),
        };
        let id = new_staged_command_id();
        assert_eq!(
            read_staged_command(&root, &id, Duration::ZERO)
                .err()
                .map(|error| error.to_string()),
            Some(String::from("Staged remote command not found"))
        );
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("stray-file"), b"")?;
        write_staged_command(&directory, &id, &command.encode()?)?;
        let mode = |path: &Path| -> Result<u32> {
            Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
        };
        assert_eq!(mode(&directory)?, 0o700);
        assert_eq!(mode(&directory.join(&id))?, 0o600);
        assert!(!directory.join(format!("{id}.partial")).exists());

        for _ in 0..2 {
            let staged = read_staged_command(&root, &id, Duration::ZERO)?;
            assert_eq!(staged.program, command.program);
            assert_eq!(staged.args, command.args);
            assert_eq!(staged.env, command.env);
            assert_eq!(staged.working_dir, command.working_dir);
        }
        assert!(write_staged_command(&directory, "../escape", b"").is_err());
        assert!(read_staged_command(&root, "../escape", Duration::ZERO).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_read_staged_command_waits_for_the_file() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let root = temp_dir.path().join("commands");
        let directory = root.join("4242");
        let id = new_staged_command_id();
        let frame = RemoteCommand {
            program: String::from("late"),
            args: Vec::new(),
            env: HashMap::default(),
            working_dir: None,
        }
        .encode()?;
        let writer = std::thread::spawn({
            let id = id.clone();
            move || {
                std::thread::sleep(Duration::from_millis(50));
                write_staged_command(&directory, &id, &frame)
            }
        });
        let staged = read_staged_command(&root, &id, STAGED_COMMAND_WAIT)?;
        assert_eq!(staged.program, "late");
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("writer panicked"))??;
        Ok(())
    }

    #[test]
    fn test_remote_command_invalid_input() {
        for input in [&b""[..], &[1, 0, 0], &[1, 0, 0, 0]] {
            assert!(RemoteCommand::read(&mut &input[..]).is_err());
        }
        let json = br#"{"program":"agent","args":"secret"}"#;
        let mut input = (json.len() as u32).to_le_bytes().to_vec();
        input.extend_from_slice(json);
        let error = RemoteCommand::read(&mut &input[..]).err().unwrap();
        assert_eq!(error.to_string(), "Invalid remote command");
    }
}
