use anyhow::{Context as _, Result};
use semver::Version;
use smol::fs;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

pub(super) async fn prepare_node_adapter(
    runtime: &Path,
    name: &str,
    version: &Version,
) -> Result<(PathBuf, PathBuf)> {
    let metadata = fs::metadata(runtime).await?;
    let mut fingerprint = DefaultHasher::new();
    runtime.hash(&mut fingerprint);
    version.hash(&mut fingerprint);
    metadata.len().hash(&mut fingerprint);
    metadata.modified()?.hash(&mut fingerprint);
    let scratch_dir = paths::data_dir()
        .join(name)
        .join(format!("{version}-{:016x}", fingerprint.finish()));
    fs::create_dir_all(&scratch_dir).await?;
    let node = scratch_dir.join(if cfg!(windows) { "node.exe" } else { "node" });
    create_node_adapter(runtime, &node).await?;
    Ok((node, scratch_dir))
}

async fn create_node_adapter(runtime: &Path, node: &Path) -> Result<()> {
    match fs::symlink_metadata(node).await {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    #[cfg(unix)]
    let result = fs::unix::symlink(runtime, node).await;
    #[cfg(windows)]
    let result = {
        match fs::hard_link(runtime, node).await {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            Err(_) => {}
        }
        copy_node_adapter(runtime, node).await
    };
    #[cfg(not(any(unix, windows)))]
    let result = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Node.js executable adapters are unavailable on this platform",
    ));

    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).context("creating Node.js executable adapter"),
    }
}

#[cfg(windows)]
async fn copy_node_adapter(runtime: &Path, node: &Path) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_COPY: AtomicU64 = AtomicU64::new(0);
    let temporary = node.with_extension(format!(
        "{}-{}.tmp",
        std::process::id(),
        NEXT_COPY.fetch_add(1, Ordering::Relaxed)
    ));
    let result = async {
        fs::copy(runtime, &temporary).await?;
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
