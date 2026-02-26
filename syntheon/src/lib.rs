use std::env;
use std::fs::{self, File, OpenOptions};
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::read::GzDecoder;
use fs2::FileExt;
use tar::Archive;

const CACHE_LOCK_FILE: &str = ".cache.lock";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheFingerprint {
    value: String,
}

impl CacheFingerprint {
    pub fn builder() -> CacheFingerprintBuilder {
        CacheFingerprintBuilder::default()
    }

    pub fn from_components<I, S>(components: I) -> Option<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut builder = Self::builder();
        for component in components {
            builder = builder.component(component);
        }
        builder.build()
    }

    pub fn as_str(&self) -> &str {
        self.value.as_str()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CacheFingerprintBuilder {
    parts: Vec<String>,
}

impl CacheFingerprintBuilder {
    pub fn component(mut self, value: impl AsRef<str>) -> Self {
        let normalized = sanitize_component(value.as_ref());
        if !normalized.is_empty() {
            self.parts.push(normalized);
        }
        self
    }

    pub fn kv(self, key: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        let key = sanitize_component(key.as_ref());
        let value = sanitize_component(value.as_ref());
        if key.is_empty() || value.is_empty() {
            return self;
        }
        self.component(format!("{key}-{value}"))
    }

    pub fn flag(self, key: impl AsRef<str>, enabled: bool) -> Self {
        self.kv(key, if enabled { "1" } else { "0" })
    }

    pub fn build(self) -> Option<CacheFingerprint> {
        if self.parts.is_empty() {
            None
        } else {
            Some(CacheFingerprint {
                value: self.parts.join("__"),
            })
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheDirBuilder {
    component: String,
    fingerprint: Option<CacheFingerprint>,
}

impl CacheDirBuilder {
    pub fn with_fingerprint(mut self, fingerprint: CacheFingerprint) -> Self {
        self.fingerprint = Some(fingerprint);
        self
    }

    pub fn with_fingerprint_opt(mut self, fingerprint: Option<CacheFingerprint>) -> Self {
        self.fingerprint = fingerprint;
        self
    }

    pub fn path(&self) -> PathBuf {
        let mut path = cache_root().join(self.component.as_str());
        if let Some(fingerprint) = &self.fingerprint {
            path = path.join(format!("fp-{}", fingerprint.as_str()));
        }
        path
    }

    pub fn lock(self) -> LockedCacheDir {
        LockedCacheDir::acquire(self.path())
    }
}

pub struct LockedCacheDir {
    path: PathBuf,
    lock_file: File,
}

impl LockedCacheDir {
    pub fn acquire(path: PathBuf) -> Self {
        fs::create_dir_all(&path).unwrap_or_else(|err| {
            panic!("failed to create cache directory {}: {err}", path.display())
        });
        let lock_path = path.join(CACHE_LOCK_FILE);
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap_or_else(|err| {
                panic!("failed to open cache lock {}: {err}", lock_path.display())
            });
        lock_file.lock_exclusive().unwrap_or_else(|err| {
            panic!("failed to lock cache directory {}: {err}", path.display())
        });
        Self { path, lock_file }
    }

    pub fn path(&self) -> &Path {
        self.path.as_path()
    }
}

impl AsRef<Path> for LockedCacheDir {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

impl Deref for LockedCacheDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.path()
    }
}

impl Drop for LockedCacheDir {
    fn drop(&mut self) {
        let _ = self.lock_file.unlock();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MakeRunner {
    jobs: usize,
}

impl MakeRunner {
    pub fn from_env() -> Self {
        Self::with_jobs(parallel_jobs())
    }

    pub fn with_jobs(jobs: usize) -> Self {
        Self { jobs: jobs.max(1) }
    }

    pub fn jobs(&self) -> usize {
        self.jobs
    }

    pub fn command(&self, dir: &Path) -> Command {
        let mut cmd = Command::new("make");
        apply_parallel(&mut cmd, self.jobs);
        cmd.current_dir(dir);
        cmd
    }

    pub fn command_for_target(&self, dir: &Path, target: &str) -> Command {
        let mut cmd = self.command(dir);
        cmd.arg(target);
        cmd
    }

    pub fn run(&self, dir: &Path, err: &str) {
        let mut cmd = self.command(dir);
        run(&mut cmd, err);
    }

    pub fn run_target(&self, dir: &Path, target: &str, err: &str) {
        let mut cmd = self.command_for_target(dir, target);
        run(&mut cmd, err);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CmakeVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl CmakeVersion {
    const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    fn at_least(self, other: CmakeVersion) -> bool {
        (self.major, self.minor, self.patch) >= (other.major, other.minor, other.patch)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CmakeCapabilities {
    pub build_parallel: bool,
    pub install_parallel: bool,
}

pub struct CmakeRunner {
    build_dir: PathBuf,
    jobs: usize,
    capabilities: CmakeCapabilities,
}

impl CmakeRunner {
    pub fn new(build_dir: impl AsRef<Path>) -> Self {
        Self {
            build_dir: build_dir.as_ref().to_path_buf(),
            jobs: parallel_jobs(),
            capabilities: cmake_capabilities(),
        }
    }

    pub fn with_jobs(mut self, jobs: usize) -> Self {
        self.jobs = jobs.max(1);
        self
    }

    pub fn jobs(&self) -> usize {
        self.jobs
    }

    pub fn capabilities(&self) -> CmakeCapabilities {
        self.capabilities
    }

    pub fn build_target(&self, target: &str, config: Option<&str>, err: &str) {
        let mut cmd = Command::new("cmake");
        cmd.arg("--build")
            .arg(&self.build_dir)
            .arg("--target")
            .arg(target);
        if let Some(config) = config {
            cmd.arg("--config").arg(config);
        }
        apply_cmake_parallel(&mut cmd, self.jobs, self.capabilities.build_parallel, false);
        run(&mut cmd, err);
    }

    pub fn install(&self, config: Option<&str>, err: &str) {
        let mut cmd = Command::new("cmake");
        cmd.arg("--install").arg(&self.build_dir);
        if let Some(config) = config {
            cmd.arg("--config").arg(config);
        }
        apply_cmake_parallel(
            &mut cmd,
            self.jobs,
            self.capabilities.install_parallel,
            true,
        );
        run(&mut cmd, err);
    }
}

pub fn manifest_dir() -> PathBuf {
    PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be provided"))
}

pub fn vendor_dir() -> PathBuf {
    manifest_dir().join("vendor")
}

pub fn out_dir() -> PathBuf {
    PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must be provided"))
}

pub fn target_triple() -> String {
    env::var("TARGET").expect("TARGET must be provided by cargo for build scripts")
}

pub fn cache_dir(component: impl AsRef<str>) -> CacheDirBuilder {
    let component = sanitize_component(component.as_ref());
    if component.is_empty() {
        panic!("cache_dir component must contain at least one alphanumeric character");
    }
    CacheDirBuilder {
        component,
        fingerprint: None,
    }
}

pub fn sanitize_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

pub fn target_root() -> PathBuf {
    if let Ok(dir) = env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }

    let out_dir = out_dir();
    if let Some(target_dir) = out_dir
        .ancestors()
        .find(|p| p.file_name().is_some_and(|name| name == "target"))
    {
        return target_dir.to_path_buf();
    }

    manifest_dir().join("target")
}

pub fn cache_root() -> PathBuf {
    let pkg = sanitize_component(
        &env::var("CARGO_PKG_NAME").expect("CARGO_PKG_NAME must be provided by cargo"),
    );
    let target_triple = sanitize_component(&target_triple());
    target_root()
        .join("build-deps")
        .join(pkg)
        .join(target_triple)
}

pub fn extract_tar_gz(archive_path: &Path, out_dir: &Path) {
    let file = File::open(archive_path)
        .unwrap_or_else(|e| panic!("failed to open {}: {e}", archive_path.display()));
    let gz = GzDecoder::new(file);
    let mut archive = Archive::new(gz);
    archive.unpack(out_dir).unwrap_or_else(|e| {
        panic!(
            "failed to extract archive {} into {}: {e}",
            archive_path.display(),
            out_dir.display()
        )
    });
}

pub fn run(cmd: &mut Command, err: &str) {
    let status = cmd.status().unwrap_or_else(|e| panic!("{err}: {e}"));
    if !status.success() {
        panic!("{err}: status {status}");
    }
}

pub fn cmake_version() -> CmakeVersion {
    let output = Command::new("cmake")
        .arg("--version")
        .output()
        .unwrap_or_else(|e| panic!("failed to query cmake version: {e}"));
    if !output.status.success() {
        panic!("cmake --version exited with {}", output.status);
    }
    let stdout = String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("cmake --version output not valid UTF-8: {e}"));
    let version_line = stdout
        .lines()
        .find(|line| line.starts_with("cmake version"))
        .unwrap_or_else(|| panic!("unexpected cmake --version output: {stdout}"));
    let mut tokens = version_line.split_whitespace();
    tokens.next();
    tokens.next();
    let raw_version = tokens
        .next()
        .unwrap_or_else(|| panic!("failed to locate cmake version in {version_line}"));
    parse_cmake_version(raw_version)
}

pub fn cmake_capabilities() -> CmakeCapabilities {
    let version = cmake_version();
    CmakeCapabilities {
        build_parallel: version.at_least(CmakeVersion::new(3, 12, 0)),
        install_parallel: version.at_least(CmakeVersion::new(3, 31, 0)),
    }
}

pub fn parallel_jobs() -> usize {
    env::var("NUM_JOBS")
        .ok()
        .and_then(|s| s.parse::<NonZeroUsize>().ok())
        .map(NonZeroUsize::get)
        .or_else(|| {
            std::thread::available_parallelism()
                .ok()
                .map(NonZeroUsize::get)
        })
        .unwrap_or(1)
}

pub fn apply_parallel(cmd: &mut Command, jobs: usize) {
    if jobs > 1 {
        cmd.arg(format!("-j{jobs}"));
    }
}

pub fn apply_cmake_parallel(
    cmd: &mut Command,
    jobs: usize,
    supports_parallel: bool,
    install: bool,
) {
    if jobs <= 1 {
        return;
    }
    let jobs_str = jobs.to_string();
    if supports_parallel {
        cmd.arg("--parallel").arg(&jobs_str);
    }
    if install {
        cmd.env("CMAKE_INSTALL_PARALLEL_LEVEL", jobs_str);
    } else {
        cmd.env("CMAKE_BUILD_PARALLEL_LEVEL", jobs_str);
    }
}

pub fn wants_native_cpu_flags() -> bool {
    let Ok(flags) = env::var("CARGO_ENCODED_RUSTFLAGS") else {
        return false;
    };

    let mut last_target_cpu = None;
    let mut saw_dash_c = false;
    for token in flags.split('\u{1f}') {
        if saw_dash_c {
            if let Some(cpu) = token.strip_prefix("target-cpu=") {
                last_target_cpu = Some(cpu);
            }
            saw_dash_c = false;
        }

        if token == "-C" {
            saw_dash_c = true;
            continue;
        }

        if let Some(cpu) = token.strip_prefix("-Ctarget-cpu=") {
            last_target_cpu = Some(cpu);
            continue;
        }
        if let Some(cpu) = token.strip_prefix("target-cpu=") {
            last_target_cpu = Some(cpu);
        }
    }

    last_target_cpu == Some("native")
}

pub fn clang_system_include_dirs() -> Vec<PathBuf> {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        return Vec::new();
    }

    let mut dirs = Vec::new();
    for flag in ["-print-file-name=include", "-print-file-name=include-fixed"] {
        let path = gcc_include_path(flag);
        if path.exists() {
            dirs.push(path);
        }
    }
    if dirs.is_empty() {
        panic!(
            "failed to locate system include directories via gcc; install clang headers or a \
             working gcc toolchain"
        );
    }
    dirs
}

pub fn gcc_include_path(flag: &str) -> PathBuf {
    let output = Command::new("gcc")
        .arg(flag)
        .output()
        .unwrap_or_else(|e| panic!("failed to invoke gcc {flag}: {e}"));
    if !output.status.success() {
        panic!("gcc {flag} exited with {}", output.status);
    }
    let path = String::from_utf8_lossy(&output.stdout);
    let trimmed = path.trim();
    if trimmed.is_empty() {
        panic!("gcc {flag} returned an empty include path");
    }
    PathBuf::from(trimmed)
}

pub fn macos_sdk_root() -> Option<PathBuf> {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return None;
    }

    if let Ok(sdkroot) = env::var("SDKROOT") {
        let sdkroot = PathBuf::from(sdkroot);
        if sdkroot.exists() {
            return Some(sdkroot);
        }
    }
    let output = Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sdkroot = String::from_utf8(output.stdout).ok()?;
    let sdkroot = sdkroot.trim();
    if sdkroot.is_empty() {
        return None;
    }
    let sdkroot = PathBuf::from(sdkroot);
    sdkroot.exists().then_some(sdkroot)
}

pub fn clang_system_include_args() -> Vec<String> {
    let mut out = Vec::new();
    for dir in clang_system_include_dirs() {
        out.push("-isystem".to_string());
        out.push(dir.display().to_string());
    }
    out
}

pub fn clang_macos_sysroot_args() -> Vec<String> {
    let Some(sdkroot) = macos_sdk_root() else {
        return Vec::new();
    };
    vec!["-isysroot".to_string(), sdkroot.display().to_string()]
}

fn parse_cmake_version(raw: &str) -> CmakeVersion {
    let mut parts = raw.split('.');
    let major = parse_version_component(parts.next(), raw, "major");
    let minor = parse_version_component(parts.next(), raw, "minor");
    let patch = parts
        .next()
        .map(|part| parse_version_component(Some(part), raw, "patch"))
        .unwrap_or(0);
    CmakeVersion::new(major, minor, patch)
}

fn parse_version_component(part: Option<&str>, raw: &str, label: &str) -> u32 {
    let raw_part = part.unwrap_or_else(|| {
        panic!("cmake version missing {label} component in {raw}");
    });
    let digits: String = raw_part
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        panic!("cmake version missing {label} digits in {raw}");
    }
    digits
        .parse::<u32>()
        .unwrap_or_else(|e| panic!("failed to parse cmake version {label} from {raw}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn fingerprint_builder_is_stable_and_sanitized() {
        let fingerprint = CacheFingerprint::builder()
            .kv("coeff", "gmp")
            .flag("pic", true)
            .component("v 1")
            .build()
            .expect("fingerprint should not be empty");
        assert_eq!(fingerprint.as_str(), "coeff-gmp__pic-1__v-1");
    }

    #[test]
    fn empty_fingerprint_builder_returns_none() {
        assert!(CacheFingerprint::builder().build().is_none());
    }

    #[test]
    fn cmake_version_parser_handles_patch_suffix() {
        let version = parse_cmake_version("3.31.6-rc1");
        assert_eq!(version, CmakeVersion::new(3, 31, 6));
    }

    #[test]
    fn locked_cache_dir_serializes_access() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "syntheon-lock-test-{}-{unique}",
            std::process::id()
        ));

        let (tx, rx) = mpsc::channel();
        let holder_path = path.clone();
        let holder = thread::spawn(move || {
            let _lock = LockedCacheDir::acquire(holder_path);
            tx.send(()).expect("holder failed to signal");
            thread::sleep(Duration::from_millis(250));
        });

        rx.recv().expect("holder did not signal lock acquisition");
        let waiter_path = path.clone();
        let started = Instant::now();
        let waiter = thread::spawn(move || {
            let _lock = LockedCacheDir::acquire(waiter_path);
        });

        waiter.join().expect("waiter thread panicked");
        let elapsed = started.elapsed();
        holder.join().expect("holder thread panicked");
        fs::remove_dir_all(&path).expect("failed to clean lock test directory");

        assert!(
            elapsed >= Duration::from_millis(150),
            "lock should block waiter; observed wait {elapsed:?}"
        );
    }
}
