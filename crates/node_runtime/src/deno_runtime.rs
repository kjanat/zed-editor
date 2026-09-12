use anyhow::{Context as _, Result, bail, ensure};
use http_client::Url;
use semver::Version;
use smol::fs;
use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    env,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::Output,
};

use super::{
    NODE_CA_CERTS_ENV_VAR, NodeRuntimeTrait, NpmCommand, SystemNodeRuntime, build_npm_command_args,
    npm_command_env, proxy_argument, read_package_installed_version,
};

const MIN_DENO_VERSION: Version = Version::new(2, 9, 0);
const NPM_PACKAGE: &str = "npm:npm@10.9.4/npm";

#[derive(Clone, Debug)]
pub(super) struct DenoRuntime {
    deno: PathBuf,
    node: PathBuf,
    scratch_dir: PathBuf,
}

impl DenoRuntime {
    pub(super) async fn detect() -> Result<Self> {
        let deno =
            fs::canonicalize(which::which("deno").context("Deno was not found on PATH")?).await?;
        let output = util::command::new_command(&deno)
            .arg("--version")
            .output()
            .await
            .with_context(|| format!("checking Deno version at {}", deno.display()))?;
        ensure!(
            output.status.success(),
            "Deno --version failed at {}:\nstdout: {}\nstderr: {}",
            deno.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let version = parse_deno_version(&output.stdout)
            .with_context(|| format!("invalid version from Deno at {}", deno.display()))?;
        ensure!(
            version >= MIN_DENO_VERSION,
            "Deno {MIN_DENO_VERSION} or newer is required for Node.js compatibility; found {version}"
        );

        let metadata = fs::metadata(&deno).await?;
        let mut fingerprint = DefaultHasher::new();
        deno.hash(&mut fingerprint);
        version.hash(&mut fingerprint);
        metadata.len().hash(&mut fingerprint);
        metadata.modified()?.hash(&mut fingerprint);
        let scratch_dir = paths::data_dir()
            .join("deno")
            .join(format!("{version}-{:016x}", fingerprint.finish()));
        fs::create_dir_all(&scratch_dir).await?;
        let node = scratch_dir.join(if cfg!(windows) { "node.exe" } else { "node" });

        // Deno 2.9 translates Node CLI arguments when invoked through a file named `node`.
        // Keep the real executable so callers using Node flags and child processes share that mode.
        create_node_adapter(&deno, &node).await?;
        let output = util::command::new_command(&node)
            .args([
                "-p",
                "JSON.stringify({deno: process.versions.deno, node: process.versions.node})",
            ])
            .output()
            .await
            .context("checking Deno Node.js adapter")?;
        ensure!(
            output.status.success(),
            "Deno Node.js adapter failed at {}:\nstdout: {}\nstderr: {}",
            node.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        validate_adapter_versions(&output.stdout, &version)?;
        Ok(Self {
            deno,
            node,
            scratch_dir,
        })
    }
}

fn parse_deno_version(output: &[u8]) -> Result<Version> {
    let output = std::str::from_utf8(output)?;
    let version = output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("deno "))
        .and_then(|line| line.split_whitespace().next())
        .context("invalid Deno version output")?;
    Ok(Version::parse(version)?)
}

fn validate_adapter_versions(output: &[u8], expected: &Version) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct Versions {
        deno: Version,
        node: Version,
    }
    let versions: Versions = serde_json::from_slice(output)
        .context("Deno does not support the Node.js executable adapter")?;
    ensure!(
        versions.deno == *expected,
        "Deno executable adapter is stale"
    );
    ensure!(
        versions.node >= SystemNodeRuntime::MIN_VERSION,
        "Deno must support Node.js {} or newer; found {}",
        SystemNodeRuntime::MIN_VERSION,
        versions.node
    );
    Ok(())
}

async fn create_node_adapter(deno: &Path, node: &Path) -> Result<()> {
    match fs::symlink_metadata(node).await {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    #[cfg(unix)]
    let result = fs::unix::symlink(deno, node).await;
    #[cfg(windows)]
    let result = {
        match fs::hard_link(deno, node).await {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            Err(_) => {}
        }
        copy_node_adapter(deno, node).await
    };
    #[cfg(not(any(unix, windows)))]
    let result = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Deno Node.js executable adapters are unavailable on this platform",
    ));

    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).context("creating Deno Node.js executable adapter"),
    }
}

#[cfg(windows)]
async fn copy_node_adapter(deno: &Path, node: &Path) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_COPY: AtomicU64 = AtomicU64::new(0);
    let temporary = node.with_extension(format!(
        "{}-{}.tmp",
        std::process::id(),
        NEXT_COPY.fetch_add(1, Ordering::Relaxed)
    ));
    let result = async {
        fs::copy(deno, &temporary).await?;
        fs::rename(&temporary, node).await
    }
    .await;
    if result.is_err() {
        match fs::remove_file(&temporary).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    result
}

fn deno_npm_environment(node: &Path, proxy: Option<&Url>) -> HashMap<String, String> {
    let mut environment = npm_command_env(node);
    configure_deno_download_environment(&mut environment, proxy);
    for name in ["DENO_DIR", "DENO_TLS_CA_STORE", "DENO_CERT", "NO_PROXY"] {
        if let Ok(value) = env::var(name) {
            environment.insert(name.into(), value);
        }
    }
    environment
}

fn configure_deno_download_environment(
    environment: &mut HashMap<String, String>,
    proxy: Option<&Url>,
) {
    if let Some(proxy) = proxy_argument(proxy) {
        environment.insert("HTTP_PROXY".into(), proxy.clone());
        environment.insert("HTTPS_PROXY".into(), proxy);
    }
    if let Some(certificates) = environment.get(NODE_CA_CERTS_ENV_VAR).cloned() {
        environment.insert("DENO_CERT".into(), certificates);
    }
}

#[async_trait::async_trait]
impl NodeRuntimeTrait for DenoRuntime {
    fn boxed_clone(&self) -> Box<dyn NodeRuntimeTrait> {
        Box::new(self.clone())
    }

    fn binary_path(&self) -> Result<PathBuf> {
        Ok(self.node.clone())
    }

    async fn run_npm_subcommand(
        &self,
        directory: Option<&Path>,
        proxy: Option<&Url>,
        subcommand: &str,
        args: &[&str],
    ) -> Result<Output> {
        let npm = self.npm_command(directory, proxy, subcommand, args).await?;
        let mut command = util::command::new_command(npm.path);
        command.args(npm.args).envs(npm.env);
        if let Some(directory) = directory {
            command.current_dir(directory);
        }
        let output = command.output().await?;
        if !output.status.success() {
            bail!(
                "failed to execute npm {subcommand} through Deno:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(output)
    }

    async fn npm_command(
        &self,
        prefix_dir: Option<&Path>,
        proxy: Option<&Url>,
        subcommand: &str,
        args: &[&str],
    ) -> Result<NpmCommand> {
        let mut command_args = [
            "run",
            "-A",
            "--no-config",
            "--no-lock",
            "--node-modules-dir=none",
            NPM_PACKAGE,
        ]
        .map(String::from)
        .to_vec();
        command_args.extend(build_npm_command_args(
            None,
            prefix_dir,
            &self.scratch_dir.join("npm-cache"),
            None,
            None,
            proxy,
            subcommand,
            args,
        ));
        Ok(NpmCommand {
            path: self.deno.clone(),
            args: command_args,
            env: deno_npm_environment(&self.node, proxy),
        })
    }

    async fn npm_package_installed_version(
        &self,
        local_package_directory: &Path,
        name: &str,
    ) -> Result<Option<Version>> {
        read_package_installed_version(local_package_directory.join("node_modules"), name).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_bootstrap_preserves_ca_and_path() -> Result<()> {
        let mut environment = HashMap::from([
            (
                NODE_CA_CERTS_ENV_VAR.to_string(),
                "corporate-ca.pem".to_string(),
            ),
            ("PATH".to_string(), "adapter".to_string()),
        ]);
        let proxy = Url::parse("http://localhost:8080")?;
        configure_deno_download_environment(&mut environment, Some(&proxy));
        assert_eq!(
            environment.get("DENO_CERT").map(String::as_str),
            Some("corporate-ca.pem")
        );
        assert_eq!(
            environment.get(NODE_CA_CERTS_ENV_VAR).map(String::as_str),
            Some("corporate-ca.pem")
        );
        assert_eq!(environment.get("PATH").map(String::as_str), Some("adapter"));
        assert_eq!(
            environment.get("HTTP_PROXY"),
            environment.get("HTTPS_PROXY")
        );
        Ok(())
    }

    #[test]
    fn adapter_requires_deno_and_supported_node_versions() -> Result<()> {
        let version = Version::new(2, 9, 4);
        validate_adapter_versions(br#"{"deno":"2.9.4","node":"24.0.0"}"#, &version)?;
        assert!(validate_adapter_versions(br#"{"node":"24.0.0"}"#, &version).is_err());
        assert!(
            validate_adapter_versions(br#"{"deno":"2.9.4","node":"20.0.0"}"#, &version).is_err()
        );
        assert!(
            validate_adapter_versions(br#"{"deno":"2.9.3","node":"24.0.0"}"#, &version).is_err()
        );
        Ok(())
    }

    #[test]
    fn npm_arguments_keep_deno_options_before_npm_options() -> Result<()> {
        smol::block_on(async {
            let runtime = DenoRuntime {
                deno: PathBuf::from("deno"),
                node: PathBuf::from("adapter/node"),
                scratch_dir: PathBuf::from("runtime"),
            };
            let proxy = Url::parse("http://localhost:8080")?;
            let command = runtime
                .npm_command(
                    Some(Path::new("prefix")),
                    Some(&proxy),
                    "exec",
                    &["--", "tool"],
                )
                .await?;
            assert_eq!(command.path, runtime.deno);
            let path = command.env.get("PATH").context("npm PATH is missing")?;
            assert_eq!(
                env::split_paths(path).next().as_deref(),
                runtime.node.parent()
            );
            assert_eq!(
                command.args,
                vec![
                    "run".to_string(),
                    "-A".to_string(),
                    "--no-config".to_string(),
                    "--no-lock".to_string(),
                    "--node-modules-dir=none".to_string(),
                    NPM_PACKAGE.to_string(),
                    "--prefix".to_string(),
                    "prefix".to_string(),
                    "exec".to_string(),
                    format!(
                        "--cache={}",
                        runtime.scratch_dir.join("npm-cache").display()
                    ),
                    "--proxy".to_string(),
                    "http://127.0.0.1:8080/".to_string(),
                    "--".to_string(),
                    "tool".to_string(),
                ]
            );
            assert_eq!(
                command.env.get("HTTPS_PROXY").map(String::as_str),
                Some("http://127.0.0.1:8080/")
            );
            Ok(())
        })
    }
}
