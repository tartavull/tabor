use std::fs;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

const DOWNLOAD_DISABLED: &str =
    "automatic CEF download is disabled; provide the repo-pinned CEF runtime through CEF_PATH";

pub fn default_version(version: &str) -> String {
    version
        .split('+')
        .nth(1)
        .map(|value| value.to_string())
        .unwrap_or_else(|| version.to_string())
}

pub fn check_archive_json(version: &str, location: &str) -> Result<()> {
    let package_release = default_version(version);
    let expected = include_str!("../../../cef-version.txt").trim();
    let expected_release = expected.split_once('+').map_or(expected, |(release, _)| release);
    if package_release != expected_release {
        return Err(Error(format!(
            "CEF package version {package_release} does not match repo pin {expected}"
        )));
    }

    let version_header = Path::new(location).join("include/cef_version.h");
    let contents = fs::read_to_string(&version_header).map_err(|error| {
        Error(format!(
            "failed to read CEF version header {}: {error}",
            version_header.display()
        ))
    })?;
    let actual = contents
        .lines()
        .find_map(|line| line.strip_prefix("#define CEF_VERSION \"")?.strip_suffix('"'))
        .ok_or_else(|| {
            Error(format!(
                "CEF version header {} does not define CEF_VERSION",
                version_header.display()
            ))
        })?;
    if actual != expected {
        return Err(Error(format!(
            "CEF version mismatch at {}: found {actual}, expected {expected}",
            Path::new(location).display()
        )));
    }

    Ok(())
}

pub fn default_download_url() -> String {
    String::from("https://cef-builds.spotifycdn.com")
}

pub struct CefIndex;

impl CefIndex {
    pub fn download() -> Result<Self> {
        Err(Error(String::from(DOWNLOAD_DISABLED)))
    }

    pub fn download_from(_url: &str) -> Result<Self> {
        Err(Error(String::from(DOWNLOAD_DISABLED)))
    }

    pub fn platform(&self, _target: &str) -> Result<&CefPlatform> {
        Err(Error(String::from(DOWNLOAD_DISABLED)))
    }
}

pub struct CefPlatform;

impl CefPlatform {
    pub fn version(&self, _cef_version: &str) -> Result<&CefVersion> {
        Err(Error(String::from(DOWNLOAD_DISABLED)))
    }
}

pub struct CefVersion;

impl CefVersion {
    pub fn download_archive(&self, _out_dir: &Path, _minimal: bool) -> Result<PathBuf> {
        Err(Error(String::from(DOWNLOAD_DISABLED)))
    }

    pub fn download_archive_from<P>(
        &self,
        _url: &str,
        _location: P,
        _show_progress: bool,
    ) -> Result<PathBuf>
    where
        P: AsRef<Path>,
    {
        Err(Error(String::from(DOWNLOAD_DISABLED)))
    }

    pub fn write_archive_json(&self, _location: PathBuf) -> Result<()> {
        Ok(())
    }
}

pub fn extract_target_archive(
    _target: &str,
    _archive: &Path,
    _out_dir: &Path,
    _minimal: bool,
) -> Result<PathBuf> {
    Err(Error(String::from(DOWNLOAD_DISABLED)))
}

pub struct OsAndArch {
    pub os: &'static str,
    pub arch: &'static str,
}

impl fmt::Display for OsAndArch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cef_{}_{}", self.os, self.arch)
    }
}

impl TryFrom<&str> for OsAndArch {
    type Error = Error;

    fn try_from(target: &str) -> Result<Self> {
        match target {
            "aarch64-apple-darwin" => Ok(OsAndArch {
                os: "macos",
                arch: "aarch64",
            }),
            "x86_64-apple-darwin" => Ok(OsAndArch {
                os: "macos",
                arch: "x86_64",
            }),
            "x86_64-pc-windows-msvc" => Ok(OsAndArch {
                os: "windows",
                arch: "x86_64",
            }),
            "aarch64-pc-windows-msvc" => Ok(OsAndArch {
                os: "windows",
                arch: "aarch64",
            }),
            "i686-pc-windows-msvc" => Ok(OsAndArch {
                os: "windows",
                arch: "x86",
            }),
            "x86_64-unknown-linux-gnu" => Ok(OsAndArch {
                os: "linux",
                arch: "x86_64",
            }),
            "aarch64-unknown-linux-gnu" => Ok(OsAndArch {
                os: "linux",
                arch: "aarch64",
            }),
            "arm-unknown-linux-gnueabi" => Ok(OsAndArch {
                os: "linux",
                arch: "arm",
            }),
            other => Err(Error(format!("Unsupported target triplet: {other}"))),
        }
    }
}
