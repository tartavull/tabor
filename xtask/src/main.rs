use std::env;
use std::error::Error;
#[cfg(target_os = "macos")]
use std::ffi::{CString, c_char, c_void};
use std::fs;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DistributionChannel {
    #[default]
    Direct,
    MacAppStore,
}

impl DistributionChannel {
    fn is_mac_app_store(self) -> bool {
        matches!(self, Self::MacAppStore)
    }

    fn plist_value(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::MacAppStore => "mac_app_store",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct BuildOptions {
    release: bool,
    passkey: bool,
    universal: bool,
    distribution: DistributionChannel,
}

impl BuildOptions {
    fn profile_release(self) -> bool {
        self.release || self.universal
    }
}

#[derive(Debug)]
enum CliCommand {
    App { options: BuildOptions },
    Run { options: BuildOptions, args: Vec<String> },
    Install { options: BuildOptions, launch: bool },
    Package { options: BuildOptions },
    RunRaw { options: BuildOptions, args: Vec<String> },
}

fn main() -> Result<(), Box<dyn Error>> {
    let command = parse_cli()?;
    let root = workspace_root();

    match command {
        CliCommand::App { options } => {
            if options.distribution.is_mac_app_store() {
                let app_path = build_app_bundle(&root, options)?;
                println!("Built {}", app_path.display());
            } else {
                let install_path = install_app_bundle(&root, options)?;
                println!("Installed {}", install_path.display());
            }
        },
        CliCommand::Run { options, args } => {
            let install_path = install_app_bundle(&root, options)?;
            launch_app_bundle(&install_path, &args)?;
        },
        CliCommand::Install { options, launch } => {
            let install_path = install_app_bundle(&root, options)?;
            println!("Installed {}", install_path.display());
            if launch {
                launch_app_bundle(&install_path, &[])?;
            }
        },
        CliCommand::Package { options } => {
            let package_path = package_mac_app_store_bundle(&root, options)?;
            println!("Packaged {}", package_path.display());
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
    parse_cli_args(&args)
}

fn parse_cli_args(args: &[String]) -> Result<CliCommand, Box<dyn Error>> {
    if args.is_empty() {
        print_help();
        return Err("missing command".into());
    }

    let command_name = &args[0];
    if command_name == "-h" || command_name == "--help" || command_name == "help" {
        print_help();
        std::process::exit(0);
    }

    let tail = &args[1..];
    let command = match command_name.as_str() {
        "app" => {
            let (options, forwarded) = parse_build_options(tail, false)?;
            if !forwarded.is_empty() {
                return Err("unexpected arguments for `app`".into());
            }
            CliCommand::App { options }
        },
        "run" | "run-app" => {
            let (options, forwarded) = parse_build_options(tail, true)?;
            CliCommand::Run { options, args: forwarded }
        },
        "install" => {
            let (options, launch) = parse_install_options(tail)?;
            CliCommand::Install { options, launch }
        },
        "package" => {
            let (options, forwarded) = parse_build_options(tail, false)?;
            if !forwarded.is_empty() {
                return Err("unexpected arguments for `package`".into());
            }
            CliCommand::Package { options }
        },
        "run-raw" => {
            let (options, forwarded) = parse_build_options(tail, true)?;
            CliCommand::RunRaw { options, args: forwarded }
        },
        other => return Err(format!("unknown command `{other}`").into()),
    };

    validate_command_options(&command)?;

    Ok(command)
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
            "--mac-app-store" => {
                options.distribution = DistributionChannel::MacAppStore;
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
            "--mac-app-store" => options.distribution = DistributionChannel::MacAppStore,
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

    if options.passkey && options.distribution.is_mac_app_store() {
        return Err(
            "`--passkey` is not yet supported with `--mac-app-store`; ship the non-passkey Mac App Store build first"
                .into(),
        );
    }

    if options.universal && !options.profile_release() {
        return Err("internal option validation failed".into());
    }

    Ok(())
}

fn validate_command_options(command: &CliCommand) -> Result<(), Box<dyn Error>> {
    match command {
        CliCommand::Run { options, .. } if options.distribution.is_mac_app_store() => {
            Err("`--mac-app-store` is not supported with `run`; build the staged app with `cargo xtask app --mac-app-store --release` instead".into())
        },
        CliCommand::Install { options, .. } if options.distribution.is_mac_app_store() => {
            Err("`--mac-app-store` is not supported with `install`; build the staged app with `cargo xtask app --mac-app-store --release` instead".into())
        },
        CliCommand::RunRaw { options, .. } if options.distribution.is_mac_app_store() => {
            Err("`--mac-app-store` is not supported with `run-raw`".into())
        },
        CliCommand::Package { options } if !options.distribution.is_mac_app_store() => {
            Err("`package` requires `--mac-app-store`".into())
        },
        _ => Ok(()),
    }
}

fn print_help() {
    eprintln!("{}", help_text());
}

fn help_text() -> &'static str {
    "\
Usage:
  cargo xtask app [--release] [--passkey] [--universal] [--mac-app-store]
  cargo xtask run [--release] [--passkey] [--universal] [-- <tabor args>]
  cargo xtask install [--release] [--passkey] [--universal] [--launch]
  cargo xtask package [--release] [--universal] --mac-app-store
  cargo xtask run-raw [--release] [--passkey] [-- <tabor args>]

Commands:
  app      Build/package and install Tabor.app to /Applications and set it as the default PDF opener, or stage a Mac App Store app
  run      Build/package/install Tabor.app to /Applications, set it as the default PDF opener, then launch
  install  Build/package and install Tabor.app to /Applications and set it as the default PDF opener
  package  Build and sign a Mac App Store `.pkg` submission artifact
  run-raw  Build and run the raw tabor binary directly (disabled on macOS)

Flags:
  --release         Build release profile (default: debug)
  --passkey         Build with passkey-webauthn feature and passkey entitlements
  --universal       Build universal release binary (x86_64 + aarch64) before packaging
  --mac-app-store   Use the Mac App Store distribution lane
  --launch          With `install`, launch installed Tabor.app after copying

Notes:
  `run-app` is supported as a compatibility alias for `run`.
  `--mac-app-store` is supported only with `app` and `package`.

Environment:
  TABOR_CODESIGN_ENTITLEMENTS
  TABOR_CODESIGN_HELPER_ENTITLEMENTS
  TABOR_CODESIGN_PROVISIONING_PROFILE
  TABOR_CODESIGN_IDENTITY
  TABOR_CODESIGN_DISTRIBUTION
  TABOR_CODESIGN_REQUIRE_PROVISIONING_PROFILE
  TABOR_CODESIGN_TEAM_ID / TABOR_CODESIGN_TEAM_NAME
  TABOR_REQUIRE_TEAM_CODESIGN (must remain 1 on macOS)
  TABOR_MAC_APP_STORE_CODESIGN_IDENTITY
  TABOR_MAC_APP_STORE_INSTALLER_IDENTITY
  TABOR_CEF_PATH / CEF_PATH
"
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
    if cfg!(target_os = "macos") {
        return Err(
            "`cargo xtask run-raw` is disabled on macOS because it bypasses signed Tabor.app launches"
                .into(),
        );
    }

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

    let app_dir = match options.distribution {
        DistributionChannel::Direct => staging_app_bundle_path()?,
        DistributionChannel::MacAppStore => {
            mac_app_store_app_bundle_path(root, options.profile_release())
        },
    };
    build_app_bundle_at(root, options, &app_dir)?;
    Ok(app_dir)
}

fn build_app_bundle_at(
    root: &Path,
    options: BuildOptions,
    app_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    build_tabor_binary(root, options)?;

    let app_template_info = root.join("extra").join("osx").join("Tabor.Info.plist");
    let app_template_icon = root.join("extra").join("osx").join("tabor.icns");
    let app_contents = app_dir.join("Contents");
    let app_binary = app_contents.join("MacOS").join("tabor");
    let app_resources = app_contents.join("Resources");
    let app_info_plist = app_contents.join("Info.plist");
    let app_icon = app_resources.join("tabor.icns");
    let app_entitlements = app_resources.join("Tabor.entitlements");

    if let Some(output_root) = app_dir.parent() {
        fs::create_dir_all(output_root)?;
    }

    if app_dir.exists() {
        make_tree_user_writable(app_dir)?;
        fs::remove_dir_all(app_dir)?;
    }

    fs::create_dir_all(app_dir.join("Contents").join("MacOS"))?;
    fs::create_dir_all(&app_resources)?;
    fs::copy(&app_template_info, &app_info_plist)?;
    set_plist_string_value(
        &app_info_plist,
        "TABORDistributionChannel",
        options.distribution.plist_value(),
    )?;
    fs::copy(&app_template_icon, &app_icon)?;

    let built_binary = tabor_binary_path(root, options.profile_release());
    fs::copy(&built_binary, &app_binary)?;
    set_executable(&app_binary)?;

    let explicit_codesign_entitlements =
        env::var_os("TABOR_CODESIGN_ENTITLEMENTS").map(PathBuf::from);
    let default_entitlements = root.join("extra").join("osx").join("Tabor.entitlements");
    let passkey_entitlements = root.join("extra").join("osx").join("Tabor.passkey.entitlements");
    let mac_app_store_entitlements = root.join("extra").join("osx").join("Tabor.mas.entitlements");
    let mac_app_store_helper_entitlements =
        root.join("extra").join("osx").join("Tabor.mas.inherit.entitlements");

    let app_resource_entitlements =
        explicit_codesign_entitlements.clone().unwrap_or_else(|| match options.distribution {
            DistributionChannel::Direct => {
                if options.passkey {
                    passkey_entitlements.clone()
                } else {
                    default_entitlements
                }
            },
            DistributionChannel::MacAppStore => mac_app_store_entitlements.clone(),
        });
    fs::copy(&app_resource_entitlements, &app_entitlements)?;

    run_script(root, "scripts/bundle-macos-deps.sh", app_dir)?;
    run_script(root, "scripts/create-macos-cef-helpers.sh", app_dir)?;

    make_tree_user_writable(app_dir)?;

    let mut sign = Command::new(root.join("scripts").join("sign-macos-app.sh"));
    sign.current_dir(root).arg(app_dir);

    if let Some(path) = explicit_codesign_entitlements.or(match options.distribution {
        DistributionChannel::Direct => {
            if options.passkey {
                Some(passkey_entitlements)
            } else {
                None
            }
        },
        DistributionChannel::MacAppStore => Some(mac_app_store_entitlements),
    }) {
        sign.env("TABOR_CODESIGN_ENTITLEMENTS", path);
    }
    if options.distribution.is_mac_app_store() {
        sign.env("TABOR_CODESIGN_DISTRIBUTION", "mac_app_store");
        sign.env("TABOR_CODESIGN_REQUIRE_PROVISIONING_PROFILE", "1");
        sign.env("TABOR_CODESIGN_HELPER_ENTITLEMENTS", mac_app_store_helper_entitlements);

        if let Some(identity) = env::var_os("TABOR_MAC_APP_STORE_CODESIGN_IDENTITY") {
            sign.env("TABOR_CODESIGN_IDENTITY", identity);
        }
    }

    run_checked(&mut sign, "codesign app bundle")?;

    Ok(())
}

fn install_app_bundle(root: &Path, options: BuildOptions) -> Result<PathBuf, Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Err("`cargo xtask install` is supported only on macOS".into());
    }

    let app_dir = build_app_bundle(root, options)?;
    let staging_root = app_dir.parent().ok_or("invalid staging path for Tabor.app")?.to_path_buf();
    let install_path = canonical_install_path()?;

    if install_path.exists() {
        make_tree_user_writable(&install_path)?;
        fs::remove_dir_all(&install_path)?;
    }

    copy_with_ditto(&app_dir, &install_path)?;
    verify_macos_app_bundle_signature(&install_path)?;

    if staging_root.exists() {
        make_tree_user_writable(&staging_root)?;
        fs::remove_dir_all(&staging_root)?;
    }

    remove_legacy_target_app_bundles(root)?;
    ensure_default_pdf_handler(&install_path, options.distribution)?;

    Ok(install_path)
}

fn package_mac_app_store_bundle(
    root: &Path,
    options: BuildOptions,
) -> Result<PathBuf, Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Err("`cargo xtask package` is supported only on macOS".into());
    }

    if !options.distribution.is_mac_app_store() {
        return Err("`cargo xtask package` requires `--mac-app-store`".into());
    }

    let app_dir = build_app_bundle(root, options)?;
    verify_mac_app_store_bundle(&app_dir)?;

    let package_path = mac_app_store_package_path(root, options.profile_release());
    if let Some(output_root) = package_path.parent() {
        fs::create_dir_all(output_root)?;
    }
    if package_path.exists() {
        fs::remove_file(&package_path)?;
    }

    let installer_identity = env::var("TABOR_MAC_APP_STORE_INSTALLER_IDENTITY").map_err(
        |_| "TABOR_MAC_APP_STORE_INSTALLER_IDENTITY is required for mac_app_store packaging",
    )?;

    let mut productbuild = Command::new("productbuild");
    productbuild
        .current_dir(root)
        .args(["--component"])
        .arg(&app_dir)
        .arg("/Applications")
        .args(["--sign", &installer_identity])
        .arg(&package_path);
    run_checked(&mut productbuild, "productbuild mac app store package")?;
    verify_mac_app_store_package_signature(&package_path)?;

    Ok(package_path)
}

fn canonical_install_path() -> Result<PathBuf, Box<dyn Error>> {
    let system_apps = PathBuf::from("/Applications");
    if !dir_is_writable(&system_apps) {
        return Err("/Applications is not writable; rerun with permissions that can replace /Applications/Tabor.app"
            .into());
    }

    Ok(system_apps.join("Tabor.app"))
}

fn remove_legacy_target_app_bundles(root: &Path) -> Result<(), Box<dyn Error>> {
    for profile in ["debug", "release"] {
        let legacy_app = root.join("target").join(profile).join("osx").join("Tabor.app");
        if legacy_app.exists() {
            make_tree_user_writable(&legacy_app)?;
            fs::remove_dir_all(&legacy_app)?;
        }
    }

    Ok(())
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
    verify_macos_app_bundle_signature(app_dir)?;

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

fn verify_macos_app_bundle_signature(app_dir: &Path) -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }

    let mut verify = Command::new("codesign");
    verify.args(["--verify", "--deep", "--strict"]).arg(app_dir);
    run_checked(&mut verify, "verify app bundle signature")?;

    let mut inspect = Command::new("codesign");
    inspect.arg("-dvv").arg(app_dir);
    run_checked(&mut inspect, "inspect app bundle signature")
}

fn verify_mac_app_store_bundle(app_dir: &Path) -> Result<(), Box<dyn Error>> {
    verify_macos_app_bundle_signature(app_dir)?;

    let provisioning_profile = app_dir.join("Contents").join("embedded.provisionprofile");
    if !provisioning_profile.is_file() {
        return Err(format!(
            "expected embedded provisioning profile in {}",
            provisioning_profile.display()
        )
        .into());
    }

    let main_binary = app_dir.join("Contents").join("MacOS").join("tabor");
    verify_codesign_entitlement(&main_binary, "com.apple.security.app-sandbox")?;

    let helper_binary = app_dir
        .join("Contents")
        .join("Frameworks")
        .join("Tabor Helper (Renderer).app")
        .join("Contents")
        .join("MacOS")
        .join("Tabor Helper (Renderer)");
    if helper_binary.exists() {
        verify_codesign_entitlement(&helper_binary, "com.apple.security.inherit")?;
    }

    Ok(())
}

fn verify_mac_app_store_package_signature(package_path: &Path) -> Result<(), Box<dyn Error>> {
    let mut check = Command::new("pkgutil");
    check.args(["--check-signature"]).arg(package_path);
    run_checked(&mut check, "inspect mac app store package signature")
}

const PDF_CONTENT_TYPE: &str = "com.adobe.pdf";
const LAUNCH_SERVICES_ROLE_VIEWER: u32 = 0x0000_0002;
const LAUNCH_SERVICES_RETRY_ATTEMPTS: usize = 4;
const LAUNCH_SERVICES_RETRY_DELAY: Duration = Duration::from_millis(250);
#[cfg(target_os = "macos")]
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

fn ensure_default_pdf_handler(
    app_dir: &Path,
    distribution: DistributionChannel,
) -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "macos") || distribution.is_mac_app_store() {
        return Ok(());
    }

    let bundle_id = read_bundle_identifier(app_dir)?;
    set_default_pdf_handler_for_bundle(&bundle_id)?;
    println!("Set {} as the default viewer for {}", bundle_id, PDF_CONTENT_TYPE);
    Ok(())
}

fn read_bundle_identifier(app_dir: &Path) -> Result<String, Box<dyn Error>> {
    let info_plist = app_dir.join("Contents").join("Info.plist");
    let output = Command::new("plutil")
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
        .arg(&info_plist)
        .output()?;
    if !output.status.success() {
        return Err(
            format!("unable to read CFBundleIdentifier from {}", info_plist.display()).into()
        );
    }

    let bundle_id = String::from_utf8(output.stdout)?.trim().to_string();
    if bundle_id.is_empty() {
        return Err(format!("empty CFBundleIdentifier in {}", info_plist.display()).into());
    }
    Ok(bundle_id)
}

fn set_default_pdf_handler_for_bundle(bundle_id: &str) -> Result<(), Box<dyn Error>> {
    let mut last_failure = String::from("unknown LaunchServices failure");
    for attempt in 1..=LAUNCH_SERVICES_RETRY_ATTEMPTS {
        let status = launch_services_set_default_role_handler(
            PDF_CONTENT_TYPE,
            LAUNCH_SERVICES_ROLE_VIEWER,
            bundle_id,
        )?;
        if status == 0 {
            match verify_default_pdf_handler_for_bundle(bundle_id) {
                Ok(()) => return Ok(()),
                Err(err) => {
                    last_failure = err.to_string();
                },
            }
        } else {
            last_failure = format!("LaunchServices set failed with status {status}");
        }
        if attempt < LAUNCH_SERVICES_RETRY_ATTEMPTS {
            thread::sleep(LAUNCH_SERVICES_RETRY_DELAY);
        }
    }

    Err(format!(
        "failed to set default PDF handler to `{bundle_id}` after {} attempts: {}",
        LAUNCH_SERVICES_RETRY_ATTEMPTS, last_failure
    )
    .into())
}

fn verify_default_pdf_handler_for_bundle(bundle_id: &str) -> Result<(), Box<dyn Error>> {
    let handler =
        launch_services_copy_default_role_handler(PDF_CONTENT_TYPE, LAUNCH_SERVICES_ROLE_VIEWER)?;
    match handler {
        Some(handler) if handler == bundle_id => Ok(()),
        Some(handler) => {
            Err(format!("default PDF handler mismatch: expected `{bundle_id}`, got `{handler}`")
                .into())
        },
        None => Err("LaunchServices returned no default PDF handler".into()),
    }
}

#[cfg(target_os = "macos")]
#[allow(non_camel_case_types)]
type CFIndex = isize;

#[cfg(target_os = "macos")]
type Boolean = u8;

#[cfg(target_os = "macos")]
enum __CFString {}

#[cfg(target_os = "macos")]
type CFStringRef = *const __CFString;

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFStringGetCString(
        the_string: CFStringRef,
        buffer: *mut c_char,
        buffer_size: CFIndex,
        encoding: u32,
    ) -> Boolean;
    fn CFStringGetLength(the_string: CFStringRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
    fn CFRelease(cf: *const c_void);
}

#[cfg(target_os = "macos")]
#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn LSSetDefaultRoleHandlerForContentType(
        in_content_type: CFStringRef,
        in_role: u32,
        in_handler_bundle_id: CFStringRef,
    ) -> i32;
    fn LSCopyDefaultRoleHandlerForContentType(
        in_content_type: CFStringRef,
        in_role: u32,
    ) -> CFStringRef;
}

#[cfg(target_os = "macos")]
struct ScopedCfString(CFStringRef);

#[cfg(target_os = "macos")]
impl ScopedCfString {
    fn new(value: &str) -> Result<Self, Box<dyn Error>> {
        let value = CString::new(value)?;
        let string = unsafe {
            CFStringCreateWithCString(std::ptr::null(), value.as_ptr(), K_CF_STRING_ENCODING_UTF8)
        };
        if string.is_null() {
            return Err(format!("unable to create CFString for `{value:?}`").into());
        }
        Ok(Self(string))
    }

    fn as_ref(&self) -> CFStringRef {
        self.0
    }
}

#[cfg(target_os = "macos")]
impl Drop for ScopedCfString {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.0.cast());
        }
    }
}

#[cfg(target_os = "macos")]
fn launch_services_set_default_role_handler(
    content_type: &str,
    role: u32,
    bundle_id: &str,
) -> Result<i32, Box<dyn Error>> {
    let content_type = ScopedCfString::new(content_type)?;
    let bundle_id = ScopedCfString::new(bundle_id)?;
    Ok(unsafe {
        LSSetDefaultRoleHandlerForContentType(content_type.as_ref(), role, bundle_id.as_ref())
    })
}

#[cfg(not(target_os = "macos"))]
fn launch_services_set_default_role_handler(
    _content_type: &str,
    _role: u32,
    _bundle_id: &str,
) -> Result<i32, Box<dyn Error>> {
    Ok(0)
}

#[cfg(target_os = "macos")]
fn launch_services_copy_default_role_handler(
    content_type: &str,
    role: u32,
) -> Result<Option<String>, Box<dyn Error>> {
    let content_type = ScopedCfString::new(content_type)?;
    let handler = unsafe { LSCopyDefaultRoleHandlerForContentType(content_type.as_ref(), role) };
    if handler.is_null() {
        return Ok(None);
    }

    let handler = ScopedCfString(handler);
    let length = unsafe { CFStringGetLength(handler.as_ref()) };
    let capacity = unsafe {
        CFStringGetMaximumSizeForEncoding(length, K_CF_STRING_ENCODING_UTF8).saturating_add(1)
    };
    let capacity_usize = usize::try_from(capacity)
        .map_err(|_| "invalid CFString buffer size for LaunchServices handler")?;
    let mut buffer = vec![0_u8; capacity_usize];
    let ok = unsafe {
        CFStringGetCString(
            handler.as_ref(),
            buffer.as_mut_ptr().cast(),
            capacity,
            K_CF_STRING_ENCODING_UTF8,
        )
    };
    if ok == 0 {
        return Err("unable to convert LaunchServices handler bundle id to UTF-8".into());
    }

    let c_str = std::ffi::CStr::from_bytes_until_nul(&buffer)
        .map_err(|_| "LaunchServices handler bundle id was not NUL-terminated")?;
    Ok(Some(c_str.to_str()?.to_string()))
}

#[cfg(not(target_os = "macos"))]
fn launch_services_copy_default_role_handler(
    _content_type: &str,
    _role: u32,
) -> Result<Option<String>, Box<dyn Error>> {
    Ok(None)
}

fn verify_codesign_entitlement(binary: &Path, entitlement_key: &str) -> Result<(), Box<dyn Error>> {
    let output =
        Command::new("codesign").args(["-d", "--entitlements", ":-"]).arg(binary).output()?;
    if !output.status.success() {
        return Err(format!("unable to inspect entitlements for {}", binary.display()).into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.contains(entitlement_key) && !stderr.contains(entitlement_key) {
        return Err(
            format!("missing entitlement `{entitlement_key}` on {}", binary.display()).into()
        );
    }

    Ok(())
}

fn staging_app_bundle_path() -> Result<PathBuf, Box<dyn Error>> {
    let epoch_nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(env::temp_dir()
        .join(format!("tabor-xtask-{}-{epoch_nanos}", std::process::id()))
        .join("Tabor.app"))
}

fn mac_app_store_output_root(root: &Path, release: bool) -> PathBuf {
    root.join("target").join(profile_dir(release)).join("mas")
}

fn mac_app_store_app_bundle_path(root: &Path, release: bool) -> PathBuf {
    mac_app_store_output_root(root, release).join("Tabor.app")
}

fn mac_app_store_package_path(root: &Path, release: bool) -> PathBuf {
    mac_app_store_output_root(root, release).join("Tabor.pkg")
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

fn set_plist_string_value(plist_path: &Path, key: &str, value: &str) -> Result<(), Box<dyn Error>> {
    let set_command = format!("Set :{key} {value}");
    let mut set = Command::new("/usr/libexec/PlistBuddy");
    set.args(["-c", &set_command]).arg(plist_path);
    let set_status = set.status()?;
    if set_status.success() {
        return Ok(());
    }

    let add_command = format!("Add :{key} string {value}");
    let mut add = Command::new("/usr/libexec/PlistBuddy");
    add.args(["-c", &add_command]).arg(plist_path);
    run_checked(&mut add, &format!("add plist key {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn parse_package_mac_app_store_release() {
        let command = parse_cli_args(&argv(&["package", "--mac-app-store", "--release"]))
            .expect("package command should parse");
        match command {
            CliCommand::Package { options } => {
                assert!(options.release);
                assert!(options.distribution.is_mac_app_store());
            },
            _ => panic!("expected package command"),
        }
    }

    #[test]
    fn package_requires_mac_app_store_distribution() {
        let err = parse_cli_args(&argv(&["package"]))
            .expect_err("package should reject direct distribution")
            .to_string();
        assert!(err.contains("`package` requires `--mac-app-store`"));
    }

    #[test]
    fn mac_app_store_rejects_passkey_mode() {
        let err = parse_cli_args(&argv(&["app", "--mac-app-store", "--passkey"]))
            .expect_err("mac app store should reject passkey mode")
            .to_string();
        assert!(err.contains("`--passkey` is not yet supported with `--mac-app-store`"));
    }

    #[test]
    fn run_rejects_mac_app_store_distribution() {
        let err = parse_cli_args(&argv(&["run", "--mac-app-store"]))
            .expect_err("run should reject mac_app_store distribution")
            .to_string();
        assert!(err.contains("`--mac-app-store` is not supported with `run`"));
    }

    #[test]
    fn install_rejects_mac_app_store_distribution() {
        let err = parse_cli_args(&argv(&["install", "--mac-app-store"]))
            .expect_err("install should reject mac_app_store distribution")
            .to_string();
        assert!(err.contains("`--mac-app-store` is not supported with `install`"));
    }

    #[test]
    fn help_text_mentions_default_pdf_opener() {
        let help = help_text();
        assert!(help.contains("default PDF opener"));
    }

    #[test]
    fn ensure_default_pdf_handler_is_noop_for_mac_app_store_distribution() {
        let result = ensure_default_pdf_handler(
            Path::new("/Applications/Tabor.app"),
            DistributionChannel::MacAppStore,
        );
        assert!(result.is_ok(), "mac app store installs should skip PDF default changes");
    }

    #[test]
    fn run_raw_rejects_mac_app_store_distribution() {
        let err = parse_cli_args(&argv(&["run-raw", "--mac-app-store"]))
            .expect_err("run-raw should reject mac_app_store distribution")
            .to_string();
        assert!(err.contains("`--mac-app-store` is not supported with `run-raw`"));
    }
}
