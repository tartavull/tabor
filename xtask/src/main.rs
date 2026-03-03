use std::env;
use std::error::Error;
use std::fs;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Clone, Copy, Debug, Default)]
struct BuildOptions {
    release: bool,
    passkey: bool,
    universal: bool,
}

impl BuildOptions {
    fn profile_release(self) -> bool {
        self.release || self.universal
    }
}

enum CliCommand {
    App { options: BuildOptions },
    Run { options: BuildOptions, args: Vec<String> },
    Install { options: BuildOptions, launch: bool },
    RunRaw { options: BuildOptions, args: Vec<String> },
}

fn main() -> Result<(), Box<dyn Error>> {
    let command = parse_cli()?;
    let root = workspace_root();

    match command {
        CliCommand::App { options } => {
            let app_dir = build_app_bundle(&root, options)?;
            println!("Built {}", app_dir.display());
        },
        CliCommand::Run { options, args } => {
            let app_dir = build_app_bundle(&root, options)?;
            launch_app_bundle(&app_dir, &args)?;
        },
        CliCommand::Install { options, launch } => {
            let install_path = install_app_bundle(&root, options)?;
            println!("Installed {}", install_path.display());
            if launch {
                launch_app_bundle(&install_path, &[])?;
            }
        },
        CliCommand::RunRaw { options, args } => {
            if options.universal {
                return Err("`run-raw` does not support `--universal`".into());
            }
            let status = run_raw_binary(&root, options, &args)?;
            std::process::exit(status.code().unwrap_or(1));
        },
    }

    Ok(())
}

fn parse_cli() -> Result<CliCommand, Box<dyn Error>> {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() {
        print_help();
        return Err("missing command".into());
    }

    let command = &args[0];
    if command == "-h" || command == "--help" || command == "help" {
        print_help();
        std::process::exit(0);
    }

    let tail = &args[1..];
    match command.as_str() {
        "app" => {
            let (options, forwarded) = parse_build_options(tail, false)?;
            if !forwarded.is_empty() {
                return Err("unexpected arguments for `app`".into());
            }
            Ok(CliCommand::App { options })
        },
        "run" | "run-app" => {
            let (options, forwarded) = parse_build_options(tail, true)?;
            Ok(CliCommand::Run { options, args: forwarded })
        },
        "install" => {
            let (options, launch) = parse_install_options(tail)?;
            Ok(CliCommand::Install { options, launch })
        },
        "run-raw" => {
            let (options, forwarded) = parse_build_options(tail, true)?;
            Ok(CliCommand::RunRaw { options, args: forwarded })
        },
        other => Err(format!("unknown command `{other}`").into()),
    }
}

fn parse_build_options(
    args: &[String],
    allow_forward_unknown_flags: bool,
) -> Result<(BuildOptions, Vec<String>), Box<dyn Error>> {
    let mut options = BuildOptions::default();
    let mut index = 0;

    while index < args.len() {
        let arg = &args[index];

        if arg == "--" {
            index += 1;
            break;
        }

        match arg.as_str() {
            "--release" => {
                options.release = true;
                index += 1;
            },
            "--passkey" => {
                options.passkey = true;
                index += 1;
            },
            "--universal" => {
                options.universal = true;
                index += 1;
            },
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            },
            _ if arg.starts_with('-') => {
                if allow_forward_unknown_flags {
                    break;
                }
                return Err(format!("unknown option `{arg}`").into());
            },
            _ => break,
        }
    }

    validate_build_options(options)?;

    Ok((options, args[index..].to_vec()))
}

fn parse_install_options(args: &[String]) -> Result<(BuildOptions, bool), Box<dyn Error>> {
    let mut options = BuildOptions::default();
    let mut launch = false;

    for arg in args {
        match arg.as_str() {
            "--release" => options.release = true,
            "--passkey" => options.passkey = true,
            "--universal" => options.universal = true,
            "--launch" => launch = true,
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            },
            _ if arg.starts_with('-') => {
                return Err(format!("unknown option `{arg}`").into());
            },
            _ => return Err(format!("unexpected argument `{arg}` for `install`").into()),
        }
    }

    validate_build_options(options)?;

    Ok((options, launch))
}

fn validate_build_options(options: BuildOptions) -> Result<(), Box<dyn Error>> {
    if options.universal && !cfg!(target_os = "macos") {
        return Err("`--universal` is supported only on macOS".into());
    }

    if options.universal && !options.profile_release() {
        return Err("internal option validation failed".into());
    }

    Ok(())
}

fn print_help() {
    eprintln!(
        "\
Usage:
  cargo xtask app [--release] [--passkey] [--universal]
  cargo xtask run [--release] [--passkey] [--universal] [-- <tabor args>]
  cargo xtask install [--release] [--passkey] [--universal] [--launch]
  cargo xtask run-raw [--release] [--passkey] [-- <tabor args>]

Commands:
  app      Build and package Tabor.app (bundle deps, helper apps, codesign)
  run      Build/package Tabor.app, then launch via `open -n ... --args`
  install  Build/package and install Tabor.app to /Applications or ~/Applications
  run-raw  Debug-only: build and run the raw tabor binary directly

Flags:
  --release    Build release profile (default: debug)
  --passkey    Build with passkey-webauthn feature and passkey entitlements
  --universal  Build universal release binary (x86_64 + aarch64) before packaging
  --launch     With `install`, launch installed Tabor.app after copying

Notes:
  `run-app` is supported as a compatibility alias for `run`.

Environment:
  TABOR_CODESIGN_ENTITLEMENTS
  TABOR_CODESIGN_PROVISIONING_PROFILE
  TABOR_CODESIGN_IDENTITY
  TABOR_CEF_PATH / CEF_PATH
"
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask should live under workspace root")
        .to_path_buf()
}

fn run_raw_binary(
    root: &Path,
    options: BuildOptions,
    args: &[String],
) -> Result<ExitStatus, Box<dyn Error>> {
    build_tabor_binary(root, options)?;

    let binary = tabor_binary_path(root, options.profile_release());
    let mut command = Command::new(binary);
    command.current_dir(root).args(args);

    let status = command.status()?;
    Ok(status)
}

fn build_app_bundle(root: &Path, options: BuildOptions) -> Result<PathBuf, Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Err("`cargo xtask app` is supported only on macOS".into());
    }

    build_tabor_binary(root, options)?;

    let app_template = root.join("extra").join("osx").join("Tabor.app");
    let app_dir = app_bundle_path(root, options.profile_release());
    let app_binary = app_dir.join("Contents").join("MacOS").join("tabor");
    let app_resources = app_dir.join("Contents").join("Resources");
    let app_entitlements = app_resources.join("Tabor.entitlements");

    if app_dir.exists() {
        make_tree_user_writable(&app_dir)?;
        fs::remove_dir_all(&app_dir)?;
    }

    copy_with_ditto(&app_template, &app_dir)?;
    fs::create_dir_all(app_dir.join("Contents").join("MacOS"))?;
    fs::create_dir_all(&app_resources)?;

    let built_binary = tabor_binary_path(root, options.profile_release());
    fs::copy(&built_binary, &app_binary)?;
    set_executable(&app_binary)?;

    let explicit_codesign_entitlements =
        env::var_os("TABOR_CODESIGN_ENTITLEMENTS").map(PathBuf::from);
    let default_entitlements = root.join("extra").join("osx").join("Tabor.entitlements");
    let passkey_entitlements = root.join("extra").join("osx").join("Tabor.passkey.entitlements");

    let app_resource_entitlements = explicit_codesign_entitlements.clone().unwrap_or_else(|| {
        if options.passkey { passkey_entitlements.clone() } else { default_entitlements }
    });
    fs::copy(&app_resource_entitlements, &app_entitlements)?;

    run_script(root, "scripts/bundle-macos-deps.sh", &app_dir)?;
    run_script(root, "scripts/create-macos-cef-helpers.sh", &app_dir)?;

    make_tree_user_writable(&app_dir)?;

    let mut sign = Command::new(root.join("scripts").join("sign-macos-app.sh"));
    sign.current_dir(root).arg(&app_dir);

    if let Some(path) = explicit_codesign_entitlements
        .or_else(|| if options.passkey { Some(passkey_entitlements) } else { None })
    {
        sign.env("TABOR_CODESIGN_ENTITLEMENTS", path);
    }

    run_checked(&mut sign, "codesign app bundle")?;

    Ok(app_dir)
}

fn install_app_bundle(root: &Path, options: BuildOptions) -> Result<PathBuf, Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Err("`cargo xtask install` is supported only on macOS".into());
    }

    let app_dir = build_app_bundle(root, options)?;
    let install_path = preferred_install_path()?;

    if let Some(parent) = install_path.parent() {
        fs::create_dir_all(parent)?;
    }

    if install_path.exists() {
        make_tree_user_writable(&install_path)?;
        fs::remove_dir_all(&install_path)?;
    }

    copy_with_ditto(&app_dir, &install_path)?;

    Ok(install_path)
}

fn preferred_install_path() -> Result<PathBuf, Box<dyn Error>> {
    let system_apps = PathBuf::from("/Applications");
    if dir_is_writable(&system_apps) {
        return Ok(system_apps.join("Tabor.app"));
    }

    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    let user_apps = PathBuf::from(home).join("Applications");
    Ok(user_apps.join("Tabor.app"))
}

fn dir_is_writable(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }

    let probe = path.join(format!(".tabor-write-test-{}", std::process::id()));
    let created = OpenOptions::new().write(true).create_new(true).open(&probe).is_ok();
    if created {
        let _ = fs::remove_file(&probe);
    }
    created
}

fn launch_app_bundle(app_dir: &Path, args: &[String]) -> Result<(), Box<dyn Error>> {
    let mut command = Command::new("open");
    command.arg("-n").arg(app_dir);

    if !args.is_empty() {
        command.arg("--args").args(args);
    }

    run_checked(&mut command, "launch app bundle")
}

fn build_tabor_binary(root: &Path, options: BuildOptions) -> Result<(), Box<dyn Error>> {
    if options.universal {
        build_universal_tabor_binary(root, options.passkey)
    } else {
        let mut command = Command::new("cargo");
        command.current_dir(root).args(["build", "-p", "tabor", "--bin", "tabor"]);

        if options.profile_release() {
            command.arg("--release");
        }

        if options.passkey {
            command.args(["--features", "passkey-webauthn"]);
        }

        run_checked(&mut command, "build tabor binary")
    }
}

fn build_universal_tabor_binary(root: &Path, passkey: bool) -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Err("`--universal` is supported only on macOS".into());
    }

    for target in ["x86_64-apple-darwin", "aarch64-apple-darwin"] {
        let mut build = Command::new("cargo");
        build.current_dir(root).args([
            "build",
            "-p",
            "tabor",
            "--bin",
            "tabor",
            "--release",
            "--target",
            target,
        ]);
        if passkey {
            build.args(["--features", "passkey-webauthn"]);
        }
        run_checked(&mut build, &format!("build tabor binary for {target}"))?;
    }

    let x86 = root.join("target").join("x86_64-apple-darwin").join("release").join("tabor");
    let arm = root.join("target").join("aarch64-apple-darwin").join("release").join("tabor");
    let output = tabor_binary_path(root, true);

    let mut lipo = Command::new("lipo");
    lipo.args(["-create"]).arg(&x86).arg(&arm).args(["-output"]).arg(&output);
    run_checked(&mut lipo, "lipo universal tabor binary")
}

fn run_script(root: &Path, relative_script: &str, app_dir: &Path) -> Result<(), Box<dyn Error>> {
    let script = root.join(relative_script);
    let mut command = Command::new(script);
    command.current_dir(root).arg(app_dir);
    run_checked(&mut command, relative_script)
}

fn run_checked(command: &mut Command, context: &str) -> Result<(), Box<dyn Error>> {
    let status = command.status()?;
    if !status.success() {
        return Err(format!("{context} failed with status {status}").into());
    }
    Ok(())
}

fn copy_with_ditto(source: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    let mut command = Command::new("ditto");
    command.arg(source).arg(destination);
    run_checked(&mut command, "ditto copy")
}

fn app_bundle_path(root: &Path, release: bool) -> PathBuf {
    root.join("target").join(profile_dir(release)).join("osx").join("Tabor.app")
}

fn tabor_binary_path(root: &Path, release: bool) -> PathBuf {
    root.join("target").join(profile_dir(release)).join("tabor")
}

fn profile_dir(release: bool) -> &'static str {
    if release { "release" } else { "debug" }
}

fn make_tree_user_writable(path: &Path) -> Result<(), Box<dyn Error>> {
    let mut command = Command::new("chmod");
    command.arg("-R").arg("u+w").arg(path);
    run_checked(&mut command, "chmod app bundle writable")
}

fn set_executable(path: &Path) -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }

    Ok(())
}
