use crate::{
    ExtensionLibraryKind, ExtensionManifest, GrammarManifestEntry, parse_wasm_extension_version,
};
use ::fs::Fs;
use anyhow::{Context as _, Result};
use futures::{
    FutureExt, StreamExt,
    channel::oneshot::{self, Sender},
    io::{self, AsyncReadExt as _},
    lock::Mutex,
};
use gpui::BackgroundExecutor;
use heck::ToSnakeCase;
use http_client::{self, AsyncBody, HttpClient};
use language::LanguageConfig;
use path::PathExt;
use semver::Version;
use serde::Deserialize;
use std::{
    env, fs, mem,
    num::NonZeroUsize,
    ops::Not,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use util::ResultExt;
use wasm_encoder::{ComponentSectionId, Encode as _, RawSection, Section as _};
use wasmparser::Parser;

/// Currently, we compile with Rust's `wasm32-wasip2` target, which works with WASI `preview2` and the component model.
const RUST_TARGET: &str = "wasm32-wasip2";

/// Compiling Tree-sitter parsers from C to WebAssembly requires a compiler,
/// a WASI sysroot, and compiler runtime libraries, provided by `wasi-sdk`.
const WASI_SDK_VERSION: &str = "34";
const WASI_SDK_URL: &str = "https://github.com/WebAssembly/wasi-sdk/releases/download/";
const WASI_SDK_PLATFORM: Option<&str> = cfg_select! {
    all(target_os = "macos", target_arch = "x86_64") => Some("x86_64-macos"),
    all(target_os = "macos", target_arch = "aarch64") => Some("arm64-macos"),
    all(target_os = "linux", target_arch = "x86_64") => Some("x86_64-linux"),
    all(target_os = "linux", target_arch = "aarch64") => Some("arm64-linux"),
    all(target_os = "freebsd", target_arch = "x86_64") => Some("x86_64-linux"),
    all(target_os = "freebsd", target_arch = "aarch64") => Some("arm64-linux"),
    all(target_os = "windows", target_arch = "x86_64") => Some("x86_64-windows"),
    all(target_os = "windows", target_arch = "aarch64") => Some("arm64-windows"),
    _ => None
};

pub struct ExtensionBuilder {
    cache_dir: PathBuf,
    executor: BackgroundExecutor,
    pub http: Arc<dyn HttpClient>,
    build_lock: Mutex<()>,
    #[cfg(any(test, feature = "test-support"))]
    precompile_hook: parking_lot::Mutex<Option<PrecompileHook>>,
}

#[cfg(any(test, feature = "test-support"))]
type PrecompileHook =
    Arc<dyn Fn(&Path) -> futures::future::BoxFuture<'static, Result<()>> + Send + Sync>;

struct BuildSession {
    executor: BackgroundExecutor,
    // A failed sibling or timeout drops command futures before teardown. Keep
    // their processes and pipes here so cancellation cannot lose the exit barrier.
    children: parking_lot::Mutex<Vec<Arc<BuildProcess>>>,
}

struct BuildProcess {
    child: Mutex<util::process::ContainedChild>,
    stdout: Mutex<Box<dyn io::AsyncRead + Send + Unpin>>,
    stderr: Mutex<Box<dyn io::AsyncRead + Send + Unpin>>,
}

impl BuildProcess {
    async fn terminate(&self) -> Result<()> {
        self.child.lock().await.kill()?;
        let (status, stdout, stderr) = futures::join!(
            async { self.child.lock().await.terminate_and_wait().await },
            async { io::copy(&mut *self.stdout.lock().await, &mut io::sink()).await },
            async { io::copy(&mut *self.stderr.lock().await, &mut io::sink()).await },
        );
        status?;
        stdout.context("draining terminated build stdout")?;
        stderr.context("draining terminated build stderr")?;
        Ok(())
    }
}

impl BuildSession {
    fn new(executor: BackgroundExecutor) -> Self {
        Self {
            executor,
            children: Default::default(),
        }
    }

    async fn output(&self, command: std::process::Command) -> Result<std::process::Output> {
        let mut child = util::process::Child::spawn_contained(
            self.executor.scheduler_executor(),
            command,
            Stdio::null(),
            Stdio::piped(),
            Stdio::piped(),
        )?;
        let stdout = child.stdout.take().context("missing command stdout")?;
        let stderr = child.stderr.take().context("missing command stderr")?;
        let process = Arc::new(BuildProcess {
            child: Mutex::new(child),
            stdout: Mutex::new(Box::new(stdout)),
            stderr: Mutex::new(Box::new(stderr)),
        });
        self.children.lock().push(process.clone());
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();
        let (status, _, _) = futures::try_join!(
            async { process.child.lock().await.wait().await },
            async {
                process
                    .stdout
                    .lock()
                    .await
                    .read_to_end(&mut stdout_bytes)
                    .await
                    .context("reading build stdout")
            },
            async {
                process
                    .stderr
                    .lock()
                    .await
                    .read_to_end(&mut stderr_bytes)
                    .await
                    .context("reading build stderr")
            },
        )?;
        process.terminate().await?;
        self.children
            .lock()
            .retain(|entry| !Arc::ptr_eq(entry, &process));
        Ok(std::process::Output {
            status,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        })
    }

    async fn terminate(&self) -> Result<()> {
        let children = self.children.lock().clone();
        let mut errors = Vec::new();
        // Signal every sibling before waiting for one child to exit.
        for child in &children {
            if let Err(error) = child.child.lock().await.kill() {
                errors.push(format!("{error:#}"));
            }
        }
        for child in children {
            if let Err(error) = child.terminate().await {
                errors.push(format!("{error:#}"));
            }
        }
        anyhow::ensure!(
            errors.is_empty(),
            "terminating build processes: {}",
            errors.join("; ")
        );
        self.children.lock().clear();
        Ok(())
    }
}

impl Drop for BuildSession {
    fn drop(&mut self) {
        for child in self.children.get_mut() {
            if let Some(mut child) = child.child.try_lock() {
                child.kill().log_err();
            }
        }
    }
}

pub enum CompilationConcurrency {
    Unbounded,
    Bounded(NonZeroUsize),
}

const DEFAULT_COMPILATION_CONCURRENCY: NonZeroUsize = NonZeroUsize::new(3).unwrap();

pub struct CompileExtensionOptions {
    pub release: bool,
    pub max_concurrency: CompilationConcurrency,
}

impl CompileExtensionOptions {
    pub const fn dev() -> Self {
        Self {
            release: false,
            max_concurrency: CompilationConcurrency::Bounded(DEFAULT_COMPILATION_CONCURRENCY),
        }
    }
}

#[derive(Deserialize)]
struct CargoToml {
    package: CargoTomlPackage,
}

#[derive(Deserialize)]
struct CargoTomlPackage {
    name: String,
}

impl ExtensionBuilder {
    pub fn new(
        http_client: Arc<dyn HttpClient>,
        cache_dir: PathBuf,
        executor: BackgroundExecutor,
    ) -> Self {
        Self {
            cache_dir,
            executor,
            http: http_client,
            build_lock: Mutex::new(()),
            #[cfg(any(test, feature = "test-support"))]
            precompile_hook: parking_lot::Mutex::new(None),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_precompile_hook(
        &self,
        hook: impl Fn(&Path) -> futures::future::BoxFuture<'static, Result<()>> + Send + Sync + 'static,
    ) {
        *self.precompile_hook.lock() = Some(Arc::new(hook));
    }

    pub async fn compile_extension(
        &self,
        extension_dir: &Path,
        extension_manifest: &mut ExtensionManifest,
        options: CompileExtensionOptions,
        fs: Arc<dyn Fs>,
    ) -> Result<()> {
        self.compile_extension_session(extension_dir, extension_manifest, options, fs, None)
            .await
    }

    pub async fn compile_extension_with_timeout(
        &self,
        extension_dir: &Path,
        extension_manifest: &mut ExtensionManifest,
        options: CompileExtensionOptions,
        fs: Arc<dyn Fs>,
        executor: BackgroundExecutor,
        timeout: Duration,
    ) -> Result<()> {
        self.compile_extension_session(
            extension_dir,
            extension_manifest,
            options,
            fs,
            Some((executor, timeout)),
        )
        .await
    }

    async fn compile_extension_session(
        &self,
        extension_dir: &Path,
        extension_manifest: &mut ExtensionManifest,
        options: CompileExtensionOptions,
        fs: Arc<dyn Fs>,
        deadline: Option<(BackgroundExecutor, Duration)>,
    ) -> Result<()> {
        let timeout = async {
            match deadline {
                Some((executor, duration)) => executor.timer(duration).await,
                None => futures::future::pending().await,
            }
        }
        .fuse();
        futures::pin_mut!(timeout);
        let build_lock = self.build_lock.lock().fuse();
        futures::pin_mut!(build_lock);
        // Keep cache ownership outside the cancelled future until every child
        // exits; a second build must not reuse partially written SDK files yet.
        let _build_lock = futures::select_biased! {
            guard = build_lock => guard,
            () = timeout => anyhow::bail!("extension compilation timed out waiting for another build"),
        };
        let session = BuildSession::new(self.executor.clone());
        let result = {
            let compilation = self
                .compile_extension_inner(extension_dir, extension_manifest, options, fs, &session)
                .fuse();
            futures::pin_mut!(compilation);
            futures::select_biased! {
                result = compilation => result,
                () = timeout => Err(anyhow::anyhow!("extension compilation timed out")),
            }
        };
        let cleanup = session.terminate().await;
        match (result, cleanup) {
            (Err(error), Err(cleanup)) => {
                Err(error.context(format!("build process cleanup failed: {cleanup:#}")))
            }
            (Err(error), Ok(())) => Err(error),
            (Ok(()), cleanup) => cleanup,
        }
    }

    async fn compile_extension_inner(
        &self,
        extension_dir: &Path,
        extension_manifest: &mut ExtensionManifest,
        options: CompileExtensionOptions,
        fs: Arc<dyn Fs>,
        session: &BuildSession,
    ) -> Result<()> {
        let start = std::time::Instant::now();

        #[cfg(any(test, feature = "test-support"))]
        {
            let hook = self.precompile_hook.lock().clone();
            if let Some(hook) = hook {
                hook(extension_dir).await?;
            }
        }

        populate_defaults(extension_manifest, extension_dir, fs.clone()).await?;

        anyhow::ensure!(
            extension_dir.is_absolute(),
            "extension dir {extension_dir:?} is not an absolute path"
        );

        fs.create_dir(&self.cache_dir)
            .await
            .context("creating cache directory")?;

        let (tx, mut rx) = oneshot::channel();

        let clang_path = extension_manifest.grammars.is_empty().not().then(|| {
            std::iter::repeat_n(
                async {
                    self.install_wasi_sdk_if_needed(session)
                        .await
                        .log_err()
                        .map(Arc::new)
                }
                .shared(),
                extension_manifest.grammars.len(),
            )
        });

        let rust_compilation_task =
            (extension_manifest.lib.kind == Some(ExtensionLibraryKind::Rust)).then(|| {
                async {
                    log::info!("compiling Rust extension {extension_dir:?}");
                    self.compile_rust_extension(
                        extension_dir,
                        extension_manifest,
                        tx,
                        &options,
                        session,
                    )
                    .await
                    .context("compiling Rust extension")?;

                    log::info!("compiled Rust extension {extension_dir:?}");
                    Ok(())
                }
                .boxed()
            });

        let grammar_compilation_tasks = extension_manifest
            .grammars
            .iter()
            .zip(clang_path.into_iter().flatten())
            .map(|((grammar_name, grammar_metadata), clang_path_task)| {
                async move {
                    let snake_cased_grammar_name = grammar_name.to_snake_case();
                    anyhow::ensure!(
                        grammar_name.as_ref() == snake_cased_grammar_name.as_str(),
                        "grammar name '{grammar_name}' must be \
                            written in snake_case: {snake_cased_grammar_name}"
                    );

                    log::info!("compiling grammar {grammar_name} for extension {extension_dir:?}");

                    let clang_path = clang_path_task.await.context("resolving clang path")?;

                    self.compile_grammar(
                        extension_dir,
                        grammar_name.as_ref(),
                        grammar_metadata,
                        &clang_path,
                        session,
                    )
                    .await
                    .with_context(|| format!("compiling grammar '{grammar_name}'"))?;
                    log::info!("compiled grammar {grammar_name} for extension {extension_dir:?}");

                    Ok(())
                }
                .boxed()
            });

        let tasks = rust_compilation_task
            .into_iter()
            .chain(grammar_compilation_tasks)
            .collect::<Vec<_>>();

        match options.max_concurrency {
            CompilationConcurrency::Unbounded => {
                futures::future::try_join_all(tasks).await?;
            }
            CompilationConcurrency::Bounded(max_concurrency) => {
                let mut stream = futures::stream::iter(tasks).buffered(max_concurrency.get());

                while let Some(result) = stream.next().await {
                    result?;
                }
            }
        }

        if let Ok(version) = rx.try_recv() {
            extension_manifest.lib.version = version;
        }

        log::info!(
            "finished compiling extension {extension_dir:?} in {time:.2}s",
            time = start.elapsed().as_secs_f64(),
        );
        Ok(())
    }

    async fn compile_rust_extension(
        &self,
        extension_dir: &Path,
        manifest: &ExtensionManifest,
        wasm_extension_api_version_tx: Sender<Version>,
        options: &CompileExtensionOptions,
        session: &BuildSession,
    ) -> anyhow::Result<()> {
        self.install_rust_wasm_target_if_needed(session).await?;

        let cargo_toml_content = fs::read_to_string(extension_dir.join("Cargo.toml"))?;
        let cargo_toml: CargoToml = toml::from_str(&cargo_toml_content)?;

        log::info!("compiling Rust crate for extension {extension_dir:?}");
        let mut command = util::command::new_std_command("cargo");
        command
            .args(["build", "--target", RUST_TARGET])
            .args(options.release.then_some("--release"))
            .arg("--target-dir")
            .arg(extension_dir.join("target"))
            // WASI builds do not work with sccache and just stuck, so disable it.
            .env("RUSTC_WRAPPER", "")
            .current_dir(extension_dir);
        let output = session.output(command).await.context("running `cargo`")?;
        anyhow::ensure!(
            output.status.success(),
            "failed to build extension {}",
            String::from_utf8_lossy(&output.stderr)
        );

        log::info!("compiled Rust crate for extension {extension_dir:?}");

        let mut wasm_path = PathBuf::from(extension_dir);
        wasm_path.extend([
            "target",
            RUST_TARGET,
            if options.release { "release" } else { "debug" },
            &cargo_toml
                .package
                .name
                // The wasm32-wasip2 target normalizes `-` in package names to `_` in the resulting `.wasm` file.
                .replace('-', "_"),
        ]);
        wasm_path.set_extension("wasm");

        log::info!("encoding wasm component for extension {extension_dir:?}");

        let component_bytes =
            fs::read(&wasm_path).with_context(|| format!("reading output module {wasm_path:?}"))?;

        let component_bytes = self
            .strip_custom_sections(&component_bytes)
            .context("stripping debug sections from wasm component")?;

        let wasm_extension_api_version =
            parse_wasm_extension_version(&manifest.id, &component_bytes)
                .context("compiled wasm did not contain a valid zed extension api version")?;
        wasm_extension_api_version_tx
            .send(wasm_extension_api_version)
            .map_err(|_| anyhow::anyhow!("Failed to send API version"))?;

        let extension_file = extension_dir.join("extension.wasm");
        fs::write(extension_file.clone(), &component_bytes).context("writing extension.wasm")?;

        log::info!("extension {extension_dir:?} written to {extension_file:?}");

        Ok(())
    }

    async fn compile_grammar(
        &self,
        extension_dir: &Path,
        grammar_name: &str,
        grammar_metadata: &GrammarManifestEntry,
        clang_path: &Path,
        session: &BuildSession,
    ) -> Result<()> {
        let mut grammar_repo_dir = extension_dir.to_path_buf();
        grammar_repo_dir.extend(["grammars", grammar_name]);

        let mut grammar_wasm_path = grammar_repo_dir.clone();
        grammar_wasm_path.set_extension("wasm");

        log::info!("checking out {grammar_name} parser");
        self.checkout_repo(
            &grammar_repo_dir,
            &grammar_metadata.repository,
            &grammar_metadata.rev,
            session,
        )
        .await?;

        let base_grammar_path = grammar_metadata
            .path
            .as_ref()
            .map(|path| grammar_repo_dir.join(path))
            .unwrap_or(grammar_repo_dir);

        let src_path = base_grammar_path.join("src");
        let parser_path = src_path.join("parser.c");
        let scanner_path = src_path.join("scanner.c");

        // Skip recompiling if the WASM object is already newer than the source files
        if file_newer_than_deps(&grammar_wasm_path, &[&parser_path, &scanner_path]).unwrap_or(false)
        {
            log::info!(
                "skipping compilation of {grammar_name} parser because the existing compiled grammar is up to date"
            );
        } else {
            log::info!("compiling {grammar_name} parser");
            let mut command = util::command::new_std_command(clang_path);
            command
                .args(["-fPIC", "-shared", "-Os"])
                .arg(format!("-Wl,--export=tree_sitter_{grammar_name}"))
                .arg("-o")
                .arg(&grammar_wasm_path)
                .arg("-I")
                .arg(&src_path)
                .arg(&parser_path)
                .args(scanner_path.exists().then_some(scanner_path));
            let clang_output = session.output(command).await.context("running clang")?;

            anyhow::ensure!(
                clang_output.status.success(),
                "failed to compile {grammar_name} parser with clang: {}",
                String::from_utf8_lossy(&clang_output.stderr),
            );
        }

        Ok(())
    }

    async fn checkout_repo(
        &self,
        directory: &Path,
        url: &str,
        rev: &str,
        session: &BuildSession,
    ) -> Result<()> {
        let git_dir = directory.join(".git");

        if directory.exists() {
            let mut command = util::command::new_std_command("git");
            command
                .arg("--git-dir")
                .arg(&git_dir)
                .args(["remote", "get-url", "origin"])
                .env("GIT_CONFIG_GLOBAL", "/dev/null");
            let remotes_output = session.output(command).await?;
            let has_remote = remotes_output.status.success()
                && String::from_utf8_lossy(&remotes_output.stdout).trim() == url;
            anyhow::ensure!(
                has_remote,
                "grammar directory {directory:?} already exists, but is not a git clone of '{url}'"
            );
        } else {
            fs::create_dir_all(directory)
                .with_context(|| format!("creating grammar directory {directory:?}"))?;
            let mut command = util::command::new_std_command("git");
            command.arg("init").current_dir(directory);
            let init_output = session.output(command).await?;
            anyhow::ensure!(
                init_output.status.success(),
                "failed to run `git init` in directory {directory:?}"
            );

            let mut command = util::command::new_std_command("git");
            command
                .arg("--git-dir")
                .arg(&git_dir)
                .args(["remote", "add", "origin", url]);
            let remote_add_output = session
                .output(command)
                .await
                .context("executing `git remote add`")?;
            anyhow::ensure!(
                remote_add_output.status.success(),
                "failed to add remote {url} for git repository {git_dir:?}"
            );
        }

        let mut command = util::command::new_std_command("git");
        command
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["fetch", "--depth", "1", "origin", rev]);
        let fetch_output = session
            .output(command)
            .await
            .context("executing `git fetch`")?;

        let mut command = util::command::new_std_command("git");
        command
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["checkout", rev])
            .current_dir(directory);
        let checkout_output = session
            .output(command)
            .await
            .context("executing `git checkout`")?;
        if !checkout_output.status.success() {
            anyhow::ensure!(
                fetch_output.status.success(),
                "failed to fetch revision {rev} in directory {directory:?}"
            );
            anyhow::bail!(
                "failed to checkout revision {rev} in directory {directory:?}: {}",
                String::from_utf8_lossy(&checkout_output.stderr)
            );
        }

        Ok(())
    }

    async fn install_rust_wasm_target_if_needed(&self, session: &BuildSession) -> Result<()> {
        let mut command = util::command::new_std_command("rustc");
        command.args(["--print", "target-libdir", "--target", RUST_TARGET]);
        let rustc_output = session.output(command).await.context("running rustc")?;
        anyhow::ensure!(
            rustc_output.status.success(),
            "failed to retrieve the `{RUST_TARGET}` target libdir: {}",
            String::from_utf8_lossy(&rustc_output.stderr)
        );

        let target_libdir = PathBuf::from(String::from_utf8(rustc_output.stdout)?.trim());
        if target_libdir.exists() {
            return Ok(());
        }

        anyhow::ensure!(
            which::which("rustup").is_ok(),
            "the `{RUST_TARGET}` target is not installed, and `rustup` is not available to \
             install it. Add the target to your Rust toolchain (e.g. `targets = \
             [\"{RUST_TARGET}\"]` for Nix rust-overlay/fenix toolchains) or install it via \
             your package manager"
        );

        let mut command = util::command::new_std_command("rustup");
        command.args(["target", "add", RUST_TARGET]);
        let output = session
            .output(command)
            .await
            .context("running `rustup target add`")?;
        anyhow::ensure!(
            output.status.success(),
            "failed to install the `{RUST_TARGET}` target: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        Ok(())
    }

    async fn install_wasi_sdk_if_needed(&self, session: &BuildSession) -> Result<PathBuf> {
        if let Some(sdk_path) = env::var_os("WASI_SDK_PATH").filter(|path| !path.is_empty()) {
            let sdk_path = PathBuf::from(sdk_path);
            let clang_path = wasi_sdk_clang_path(&sdk_path);
            if fs::metadata(&clang_path).is_ok_and(|metadata| metadata.is_file()) {
                log::info!("using wasi-sdk from WASI_SDK_PATH: {sdk_path:?}");
                return Ok(clang_path);
            }
            log::warn!(
                "WASI_SDK_PATH is set to {sdk_path:?} but clang was not found at {clang_path:?}, falling back to download"
            );
        }

        let wasi_sdk_dir = self.cache_dir.join("wasi-sdk");
        let clang_path = wasi_sdk_clang_path(&wasi_sdk_dir);

        match self
            .update_wasi_sdk_if_needed(&wasi_sdk_dir, &clang_path, session)
            .await
        {
            Ok(()) => Ok(clang_path),
            Err(error) => {
                if fs::metadata(&clang_path).is_ok_and(|metadata| metadata.is_file()) {
                    let installed_version = installed_wasi_sdk_version(&wasi_sdk_dir)
                        .unwrap_or_else(|| "unknown".to_string());
                    log::warn!(
                        "failed to install wasi-sdk {WASI_SDK_VERSION}.0, using cached wasi-sdk {installed_version} from {wasi_sdk_dir:?}: {error:#}"
                    );
                    Ok(clang_path)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn update_wasi_sdk_if_needed(
        &self,
        wasi_sdk_dir: &Path,
        clang_path: &Path,
        session: &BuildSession,
    ) -> Result<()> {
        if installed_wasi_sdk_version(wasi_sdk_dir) == Some(format!("{WASI_SDK_VERSION}.0"))
            && fs::metadata(clang_path).is_ok_and(|metadata| metadata.is_file())
        {
            return Ok(());
        }

        let platform = WASI_SDK_PLATFORM.with_context(|| {
            format!("wasi-sdk is not available for platform {}", env::consts::OS)
        })?;
        let url = format!(
            "{WASI_SDK_URL}wasi-sdk-{WASI_SDK_VERSION}/wasi-sdk-{WASI_SDK_VERSION}.0-{platform}.tar.gz"
        );

        log::info!("downloading wasi-sdk to {wasi_sdk_dir:?}");

        let tar_out_dir = self.cache_dir.join("wasi-sdk-temp");

        fs::remove_dir_all(&tar_out_dir).ok();
        fs::create_dir_all(&tar_out_dir).context("creating extraction directory")?;

        let mut response = self.http.get(&url, AsyncBody::default(), true).await?;

        // Write the response to a temporary file
        let tar_gz_path = self.cache_dir.join("wasi-sdk.tar.gz");
        let tar_gz_file =
            fs::File::create(&tar_gz_path).context("creating temporary tar.gz file")?;
        let response_body = response.body_mut();

        let mut async_file = io::AllowStdIo::new(tar_gz_file);
        io::copy(response_body, &mut async_file)
            .await
            .context("streaming response to file")?;
        drop(async_file);

        log::info!("un-tarring wasi-sdk to {tar_out_dir:?}");

        // Shell out to tar to extract the archive
        let mut command = util::command::new_std_command("tar");
        command
            .arg("-xzf")
            .arg(&tar_gz_path)
            .arg("-C")
            .arg(&tar_out_dir);
        let tar_output = session.output(command).await.context("running tar")?;

        anyhow::ensure!(
            tar_output.status.success(),
            "failed to extract wasi-sdk archive: {}",
            String::from_utf8_lossy(&tar_output.stderr)
        );

        log::info!("finished downloading wasi-sdk");

        // Clean up the temporary tar.gz file
        fs::remove_file(&tar_gz_path).ok();

        let inner_dir = fs::read_dir(&tar_out_dir)?
            .next()
            .context("no content")?
            .context("reading contents of extracted wasi archive directory")?
            .path();
        match fs::remove_dir_all(wasi_sdk_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("removing outdated wasi-sdk"),
        }
        fs::rename(&inner_dir, wasi_sdk_dir).context("moving extracted wasi directory")?;
        fs::remove_dir_all(&tar_out_dir).ok();

        Ok(())
    }

    // This was adapted from:
    // https://github.com/bytecodealliance/wasm-tools/blob/e8809bb17fcf69aa8c85cd5e6db7cff5cf36b1de/src/bin/wasm-tools/strip.rs
    fn strip_custom_sections(&self, input: &Vec<u8>) -> Result<Vec<u8>> {
        use wasmparser::Payload::*;

        let strip_custom_section = |name: &str| {
            // Default strip everything but:
            // * the `name` section
            // * any `component-type` sections
            // * the `dylink.0` section
            // * our custom version section
            name != "name"
                && !name.starts_with("component-type:")
                && name != "dylink.0"
                && name != "zed:api-version"
        };

        let mut output = Vec::new();
        let mut stack = Vec::new();

        for payload in Parser::new(0).parse_all(input) {
            let payload = payload?;

            // Track nesting depth, so that we don't mess with inner producer sections:
            match payload {
                Version { encoding, .. } => {
                    output.extend_from_slice(match encoding {
                        wasmparser::Encoding::Component => &wasm_encoder::Component::HEADER,
                        wasmparser::Encoding::Module => &wasm_encoder::Module::HEADER,
                    });
                }
                ModuleSection { .. } | ComponentSection { .. } => {
                    stack.push(mem::take(&mut output));
                    continue;
                }
                End { .. } => {
                    let mut parent = match stack.pop() {
                        Some(c) => c,
                        None => break,
                    };
                    if output.starts_with(&wasm_encoder::Component::HEADER) {
                        parent.push(ComponentSectionId::Component as u8);
                        output.encode(&mut parent);
                    } else {
                        parent.push(ComponentSectionId::CoreModule as u8);
                        output.encode(&mut parent);
                    }
                    output = parent;
                }
                _ => {}
            }

            if let CustomSection(c) = &payload
                && strip_custom_section(c.name())
            {
                continue;
            }
            if let Some((id, range)) = payload.as_section() {
                RawSection {
                    id,
                    data: &input[range],
                }
                .append_to(&mut output);
            }
        }

        Ok(output)
    }
}

pub async fn populate_defaults(
    manifest: &mut ExtensionManifest,
    extension_path: &Path,
    fs: Arc<dyn Fs>,
) -> Result<()> {
    // For legacy extensions on the v0 schema (aka, using `extension.json`), clear out any existing
    // contents of the computed fields, since we don't care what the existing values are.
    if manifest.schema_version.is_v0() {
        manifest.languages.clear();
        manifest.grammars.clear();
        manifest.themes.clear();
    }

    let cargo_toml_path = extension_path.join("Cargo.toml");
    if cargo_toml_path.exists() {
        manifest.lib.kind = Some(ExtensionLibraryKind::Rust);
    }

    let languages_dir = extension_path.join("languages");
    if fs.is_dir(&languages_dir).await {
        let mut language_dir_entries = fs
            .read_dir(&languages_dir)
            .await
            .context("listing languages directory")?;

        while let Some(language_dir) = language_dir_entries.next().await {
            let language_dir = language_dir?;
            let config_path = language_dir.join(LanguageConfig::FILE_NAME);
            if fs.is_file(config_path.as_path()).await {
                let relative_language_dir = language_dir
                    .strip_prefix(extension_path)?
                    .to_rel_path_buf()?;
                if !manifest.languages.contains(&relative_language_dir) {
                    manifest.languages.push(relative_language_dir);
                }
            }
        }
    }

    let themes_dir = extension_path.join("themes");
    if fs.is_dir(&themes_dir).await {
        let mut theme_dir_entries = fs
            .read_dir(&themes_dir)
            .await
            .context("listing themes directory")?;

        while let Some(theme_path) = theme_dir_entries.next().await {
            let theme_path = theme_path?;
            if theme_path.extension() == Some("json".as_ref()) {
                let relative_theme_path =
                    theme_path.strip_prefix(extension_path)?.to_rel_path_buf()?;
                if !manifest.themes.contains(&relative_theme_path) {
                    manifest.themes.push(relative_theme_path);
                }
            }
        }
    }

    let icon_themes_dir = extension_path.join("icon_themes");
    if fs.is_dir(&icon_themes_dir).await {
        let mut icon_theme_dir_entries = fs
            .read_dir(&icon_themes_dir)
            .await
            .context("listing icon themes directory")?;

        while let Some(icon_theme_path) = icon_theme_dir_entries.next().await {
            let icon_theme_path = icon_theme_path?;
            if icon_theme_path.extension() == Some("json".as_ref()) {
                let relative_icon_theme_path = icon_theme_path
                    .strip_prefix(extension_path)?
                    .to_rel_path_buf()?;
                if !manifest.icon_themes.contains(&relative_icon_theme_path) {
                    manifest.icon_themes.push(relative_icon_theme_path);
                }
            }
        }
    };
    if manifest.snippets.is_none()
        && let snippets_json_path = extension_path.join("snippets.json")
        && fs.is_file(&snippets_json_path).await
    {
        manifest.snippets = Some("snippets.json".into());
    }

    // For legacy extensions on the v0 schema (aka, using `extension.json`), we want to populate the grammars in
    // the manifest using the contents of the `grammars` directory.
    if manifest.schema_version.is_v0() {
        let grammars_dir = extension_path.join("grammars");
        if fs.is_dir(&grammars_dir).await {
            let mut grammar_dir_entries = fs
                .read_dir(&grammars_dir)
                .await
                .context("listing grammars directory")?;

            while let Some(grammar_path) = grammar_dir_entries.next().await {
                let grammar_path = grammar_path?;
                if grammar_path.extension() == Some("toml".as_ref()) {
                    #[derive(Deserialize)]
                    struct GrammarConfigToml {
                        pub repository: String,
                        pub commit: String,
                        #[serde(default)]
                        pub path: Option<String>,
                    }

                    let grammar_config = fs.load(&grammar_path).await?;
                    let grammar_config: GrammarConfigToml = toml::from_str(&grammar_config)?;

                    let grammar_name = grammar_path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .context("no grammar name")?;
                    if !manifest.grammars.contains_key(grammar_name) {
                        manifest.grammars.insert(
                            grammar_name.into(),
                            GrammarManifestEntry {
                                repository: grammar_config.repository,
                                rev: grammar_config.commit,
                                path: grammar_config.path,
                            },
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

/// Returns `true` if the target exists and its last modified time is greater than that
/// of each dependency which exists (i.e., dependency paths which do not exist are ignored).
///
/// # Errors
///
/// Returns `Err` if any of the underlying file I/O operations fail.
fn file_newer_than_deps(target: &Path, dependencies: &[&Path]) -> Result<bool, std::io::Error> {
    if !target.try_exists()? {
        return Ok(false);
    }
    let target_modified = target.metadata()?.modified()?;
    for dependency in dependencies {
        if !dependency.try_exists()? {
            continue;
        }
        let dep_modified = dependency.metadata()?.modified()?;
        if target_modified < dep_modified {
            return Ok(false);
        }
    }
    Ok(true)
}

fn wasi_sdk_clang_path(wasi_sdk_dir: &Path) -> PathBuf {
    let mut clang_path = wasi_sdk_dir.to_path_buf();
    clang_path.extend(["bin", &format!("clang{}", env::consts::EXE_SUFFIX)]);
    clang_path
}

fn installed_wasi_sdk_version(wasi_sdk_dir: &Path) -> Option<String> {
    let version_path = wasi_sdk_dir.join("VERSION");
    let contents = match fs::read_to_string(&version_path) {
        Ok(contents) => contents,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!("failed to read wasi-sdk version from {version_path:?}: {error}");
            }
            return None;
        }
    };
    contents.lines().next().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        str::FromStr,
        thread::sleep,
        time::Duration,
    };

    use gpui::TestAppContext;
    use indoc::indoc;

    use crate::{
        ExtensionManifest, ExtensionSnippets,
        extension_builder::{file_newer_than_deps, installed_wasi_sdk_version, populate_defaults},
    };

    #[test]
    fn build_process_fixture() {
        let Ok(mode) = std::env::var("ZED_BUILD_PROCESS_FIXTURE") else {
            return;
        };
        if mode == "parent" || mode == "orphan" {
            let mut command = util::command::new_command(
                std::env::current_exe().expect("current test executable"),
            );
            command
                .args([
                    "--exact",
                    "extension_builder::tests::build_process_fixture",
                    "--nocapture",
                ])
                .env("ZED_BUILD_PROCESS_FIXTURE", "descendant")
                .stdin(util::command::Stdio::inherit())
                .stdout(util::command::Stdio::inherit())
                .stderr(util::command::Stdio::inherit());
            if mode == "parent" {
                futures::executor::block_on(command.status()).expect("wait for build descendant");
            } else {
                let mut descendant = command.spawn().expect("spawn build descendant");
                let readiness =
                    std::env::var_os("ZED_BUILD_PROCESS_READY").expect("descendant readiness path");
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                while !Path::new(&readiness).is_file() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "descendant never became ready"
                    );
                    assert!(
                        descendant
                            .try_status()
                            .expect("descendant status")
                            .is_none()
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                println!("parent output");
                eprintln!("parent diagnostic");
            }
        } else {
            let readiness =
                std::env::var_os("ZED_BUILD_PROCESS_READY").expect("descendant readiness path");
            std::fs::write(readiness, "ready").expect("write descendant readiness");
            loop {
                std::thread::park();
            }
        }
    }

    #[gpui::test]
    async fn test_build_session_cleans_descendant_pipes_after_natural_exit(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let directory = tempfile::tempdir().expect("readiness directory");
        let mut command =
            util::command::new_std_command(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "extension_builder::tests::build_process_fixture",
                "--nocapture",
            ])
            .env("ZED_BUILD_PROCESS_FIXTURE", "orphan")
            .env("ZED_BUILD_PROCESS_READY", directory.path().join("ready"));
        let session = super::BuildSession::new(cx.background_executor.clone());
        let result = session.output(command).await;
        session.terminate().await.expect("finish build cleanup");
        let output = result.expect("natural exit must complete without deadline");
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("parent output"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("parent diagnostic"));
        assert!(session.children.lock().is_empty());
    }

    #[gpui::test]
    async fn test_failed_build_sibling_keeps_process_owned_until_cleanup(cx: &mut TestAppContext) {
        use futures::FutureExt as _;
        cx.background_executor.allow_parking();
        let directory = tempfile::tempdir().expect("readiness directory");
        let readiness = directory.path().join("ready");
        let session = super::BuildSession::new(cx.background_executor.clone());
        let mut command =
            util::command::new_std_command(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "extension_builder::tests::build_process_fixture",
                "--nocapture",
            ])
            .env("ZED_BUILD_PROCESS_FIXTURE", "parent")
            .env("ZED_BUILD_PROCESS_READY", &readiness);
        let sibling_failure = async {
            // Let the native descendant inherit its pipes before a sibling fails.
            cx.background_executor
                .spawn(async move {
                    let deadline = std::time::Instant::now() + Duration::from_secs(30);
                    while !readiness.is_file() {
                        anyhow::ensure!(
                            std::time::Instant::now() < deadline,
                            "descendant never became ready"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await?;
            let missing = util::command::new_std_command("zed-missing-build-command");
            session.output(missing).await
        };
        let result = {
            let running = session.output(command).fuse();
            let failure = sibling_failure.fuse();
            futures::pin_mut!(running, failure);
            futures::try_join!(running, failure)
        };
        assert!(result.is_err());
        let processes = session.children.lock().clone();
        assert_eq!(processes.len(), 1);
        session
            .terminate()
            .await
            .expect("clean up failed build siblings");
        assert!(processes.iter().all(|process| {
            process
                .child
                .try_lock()
                .is_some_and(|child| child.exit_status().is_some())
        }));
        assert!(session.children.lock().is_empty());
        session.terminate().await.expect("repeat session cleanup");
    }

    #[gpui::test]
    async fn test_build_cleanup_reaps_child_when_pipe_drain_fails(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        struct BrokenPipe;
        impl futures::io::AsyncRead for BrokenPipe {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buffer: &mut [u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Ready(Err(std::io::Error::other("injected pipe failure")))
            }
        }
        let mut command =
            util::command::new_std_command(std::env::current_exe().expect("test executable"));
        command.args(["--exact", "extension_builder::tests::build_process_fixture"]);
        // Prevent inherited fixture mode when nextest itself was launched by a fixture.
        command.env_remove("ZED_BUILD_PROCESS_FIXTURE");
        let child = util::process::Child::spawn_contained(
            cx.background_executor.scheduler_executor(),
            command,
            std::process::Stdio::null(),
            std::process::Stdio::null(),
            std::process::Stdio::null(),
        )
        .expect("spawn build child");
        let process = super::BuildProcess {
            child: futures::lock::Mutex::new(child),
            stdout: futures::lock::Mutex::new(Box::new(BrokenPipe)),
            stderr: futures::lock::Mutex::new(Box::new(futures::io::empty())),
        };
        let error = process
            .terminate()
            .await
            .expect_err("pipe cleanup failure must propagate");
        assert!(format!("{error:#}").contains("injected pipe failure"));
        assert!(process.child.lock().await.exit_status().is_some());
    }

    #[gpui::test]
    async fn test_cancelled_build_session_reaps_siblings_and_descendants(cx: &mut TestAppContext) {
        use futures::{AsyncReadExt as _, FutureExt as _};
        cx.background_executor.allow_parking();
        let directory = tempfile::tempdir().expect("create readiness directory");
        let readiness = [
            directory.path().join("first"),
            directory.path().join("second"),
        ];
        let session = super::BuildSession::new(cx.background_executor.clone());
        let command = |readiness: &Path| {
            let mut command = util::command::new_std_command(
                std::env::current_exe().expect("current test executable"),
            );
            command
                .args([
                    "--exact",
                    "extension_builder::tests::build_process_fixture",
                    "--nocapture",
                ])
                .env("ZED_BUILD_PROCESS_FIXTURE", "parent")
                .env("ZED_BUILD_PROCESS_READY", readiness);
            command
        };
        let [first_readiness, second_readiness] = &readiness;
        let mut first = session.output(command(first_readiness)).boxed();
        let mut second = session.output(command(second_readiness)).boxed();
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        let processes = session.children.lock().clone();
        assert_eq!(processes.len(), 2);
        drop((first, second));

        cx.background_executor
            .spawn(async move {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while !readiness.iter().all(|path| path.is_file()) {
                    anyhow::ensure!(
                        std::time::Instant::now() < deadline,
                        "build descendants did not become ready"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
            .expect("wait for native descendants");

        session
            .terminate()
            .await
            .expect("terminate build process trees");
        assert!(session.children.lock().is_empty());
        for process in processes {
            let child = process.child.lock().await;
            assert!(child.exit_status().is_some());
            #[cfg(windows)]
            assert_eq!(
                child.has_running_processes().expect("query job"),
                Some(false)
            );
            let mut remaining = Vec::new();
            assert_eq!(
                process
                    .stdout
                    .lock()
                    .await
                    .read_to_end(&mut remaining)
                    .await
                    .expect("read stdout EOF"),
                0
            );
            assert_eq!(
                process
                    .stderr
                    .lock()
                    .await
                    .read_to_end(&mut remaining)
                    .await
                    .expect("read stderr EOF"),
                0
            );
        }
    }

    #[test]
    fn test_file_newer_than_deps() {
        // Don't use TempTree because we need to guarantee the order
        let tmpdir = tempfile::tempdir().unwrap();
        let target = tmpdir.path().join("target.wasm");
        let dep1 = tmpdir.path().join("parser.c");
        let dep2 = tmpdir.path().join("scanner.c");

        assert!(
            !file_newer_than_deps(&target, &[&dep1, &dep2]).unwrap(),
            "target doesn't exist"
        );
        std::fs::write(&target, "foo").unwrap(); // Create target
        assert!(
            file_newer_than_deps(&target, &[&dep1, &dep2]).unwrap(),
            "dependencies don't exist; target is newer"
        );
        sleep(Duration::from_secs(1));
        std::fs::write(&dep1, "foo").unwrap(); // Create dep1 (newer than target)
        // Dependency is newer
        assert!(
            !file_newer_than_deps(&target, &[&dep1, &dep2]).unwrap(),
            "a dependency is newer (target {:?}, dep1 {:?})",
            target.metadata().unwrap().modified().unwrap(),
            dep1.metadata().unwrap().modified().unwrap(),
        );
        sleep(Duration::from_secs(1));
        std::fs::write(&dep2, "foo").unwrap(); // Create dep2
        sleep(Duration::from_secs(1));
        std::fs::write(&target, "foobar").unwrap(); // Update target
        assert!(
            file_newer_than_deps(&target, &[&dep1, &dep2]).unwrap(),
            "target is newer than dependencies (target {:?}, dep2 {:?})",
            target.metadata().unwrap().modified().unwrap(),
            dep2.metadata().unwrap().modified().unwrap(),
        );
    }

    #[test]
    fn test_installed_wasi_sdk_version() {
        let tmpdir = tempfile::tempdir().unwrap();
        let version_path = tmpdir.path().join("VERSION");

        assert_eq!(installed_wasi_sdk_version(tmpdir.path()), None);

        std::fs::write(&version_path, "").unwrap();
        assert_eq!(installed_wasi_sdk_version(tmpdir.path()), None);

        std::fs::write(&version_path, "25.0\nwasi-libc: 574b88da4815\n").unwrap();
        assert_eq!(
            installed_wasi_sdk_version(tmpdir.path()),
            Some("25.0".to_string())
        );

        std::fs::write(&version_path, "34.0\r\nwasi-libc: 2e6fb9d8ee0c\r\n").unwrap();
        assert_eq!(
            installed_wasi_sdk_version(tmpdir.path()),
            Some("34.0".to_string())
        );
    }

    #[gpui::test]
    async fn test_snippet_location_is_kept(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let extension_path = Path::new("/extension");

        fs.insert_tree(
            extension_path,
            serde_json::json!({
                "extension.toml": indoc! {r#"
                    id = "test-manifest"
                    name = "Test Manifest"
                    version = "0.0.1"
                    schema_version = 1

                    snippets = "./snippets/snippets.json"
                    "#
                },
                "snippets.json": "",
            }),
        )
        .await;

        let mut manifest = ExtensionManifest::load(fs.clone(), extension_path)
            .await
            .unwrap();

        populate_defaults(&mut manifest, extension_path, fs.clone())
            .await
            .unwrap();

        assert_eq!(
            manifest.snippets,
            Some(ExtensionSnippets::Single(
                PathBuf::from_str("./snippets/snippets.json").unwrap()
            ))
        )
    }

    #[gpui::test]
    async fn test_automatic_snippet_location_is_relative(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let extension_path = Path::new("/extension");

        fs.insert_tree(
            extension_path,
            serde_json::json!({
                "extension.toml": indoc! {r#"
                    id = "test-manifest"
                    name = "Test Manifest"
                    version = "0.0.1"
                    schema_version = 1

                    "#
                },
                "snippets.json": "",
            }),
        )
        .await;

        let mut manifest = ExtensionManifest::load(fs.clone(), extension_path)
            .await
            .unwrap();

        populate_defaults(&mut manifest, extension_path, fs.clone())
            .await
            .unwrap();

        assert_eq!(
            manifest.snippets,
            Some(ExtensionSnippets::Single(
                PathBuf::from_str("snippets.json").unwrap()
            ))
        )
    }
}
