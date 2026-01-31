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

pub fn default_version(version: &str) -> String {
    version
        .split('+')
        .nth(1)
        .map(|value| value.to_string())
        .unwrap_or_else(|| version.to_string())
}

pub fn check_archive_json(_version: &str, _location: &str) -> Result<()> {
    Ok(())
}

pub struct CefIndex;

impl CefIndex {
    pub fn download() -> Result<Self> {
        Err(Error(String::from("CEF download disabled")))
    }

    pub fn platform(&self, _target: &str) -> Result<&CefPlatform> {
        Err(Error(String::from("CEF download disabled")))
    }
}

pub struct CefPlatform;

impl CefPlatform {
    pub fn version(&self, _cef_version: &str) -> Result<&CefVersion> {
        Err(Error(String::from("CEF download disabled")))
    }
}

pub struct CefVersion;

impl CefVersion {
    pub fn download_archive(&self, _out_dir: &Path, _minimal: bool) -> Result<PathBuf> {
        Err(Error(String::from("CEF download disabled")))
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
    Err(Error(String::from("CEF download disabled")))
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
