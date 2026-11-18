use std::{
    env,
    io::{self, IsTerminal, Write},
    path::Path,
    process::Command,
};

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use std::{fs, path::PathBuf};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qrcode::{QrCode, render::unicode};
use reqwest::Url;

pub fn print_qr(link: &str) -> Result<()> {
    let qr = QrCode::new(link)?.render::<unicode::Dense1x2>().build();
    eprintln!("{qr}");
    Ok(())
}

pub fn unpack(uri: &str) -> Result<String> {
    let uri = Url::parse(uri).context("Invalid Unbind app link")?;
    ensure!(
        uri.scheme() == "unbind"
            && uri.host_str() == Some("connect")
            && matches!(uri.path(), "" | "/")
            && uri.query().is_none()
            && uri.username().is_empty()
            && uri.password().is_none()
            && uri.port().is_none(),
        "Expected an unbind://connect app link"
    );
    let fragment = uri
        .fragment()
        .filter(|value| value.len() <= 2048)
        .context("The app link is missing its invite")?;
    let invite = String::from_utf8(URL_SAFE_NO_PAD.decode(fragment)?)?;
    crate::signaling::Invite::from_link(&invite)?;
    Ok(invite)
}

pub fn confirm(uri: &str) -> Result<String> {
    let link = unpack(uri)?;
    let invite = crate::signaling::Invite::from_link(&link)?;
    ensure!(
        io::stdin().is_terminal(),
        "Open the invite in an interactive terminal to confirm"
    );
    eprint!(
        "Connect to the Unbind invite from {}? [y/N] ",
        invite.origin
    );
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    ensure!(
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        "Connection cancelled"
    );
    Ok(link)
}

pub fn register() -> Result<()> {
    let binary = env::current_exe().context("Could not locate the installed Unbind executable")?;
    register_platform(&binary)?;
    println!("Registered the unbind:// link handler. Opening an invite will ask for confirmation.");
    Ok(())
}

fn run(command: &mut Command, action: &str) -> Result<()> {
    let status = command.status().with_context(|| action.to_owned())?;
    ensure!(status.success(), "{action} failed ({status})");
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn home() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is undefined")
}

#[cfg(target_os = "linux")]
fn register_platform(binary: &Path) -> Result<()> {
    let data = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or(home()?.join(".local/share"));
    let directory = data.join("applications");
    fs::create_dir_all(&directory)?;
    let config = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or(home()?.join(".config"));
    fs::create_dir_all(config)?;
    let path = binary
        .to_str()
        .context("The executable path is not valid UTF-8")?;
    ensure!(
        !path.contains(['\n', '\r']),
        "The executable path contains a newline"
    );
    let escaped = path
        .replace('\\', "\\\\\\\\")
        .replace('"', "\\\\\"")
        .replace('$', "\\\\$")
        .replace('`', "\\\\`")
        .replace('%', "%%");
    let icons = data.join("icons/hicolor/scalable/apps");
    fs::create_dir_all(&icons)?;
    fs::write(
        icons.join("dev.unbind.Link.svg"),
        include_bytes!("../../../assets/icon.svg"),
    )?;
    fs::write(
        directory.join("dev.unbind.Link.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=Unbind\nIcon=dev.unbind.Link\nExec=\"{escaped}\" open-link %u\nTerminal=true\nNoDisplay=true\nMimeType=x-scheme-handler/unbind;\n"
        ),
    )?;
    run(
        Command::new("xdg-mime").args([
            "default",
            "dev.unbind.Link.desktop",
            "x-scheme-handler/unbind",
        ]),
        "Registering the desktop URL scheme",
    )
}

#[cfg(windows)]
fn register_platform(binary: &Path) -> Result<()> {
    const SCHEME: &str = r"HKCU\Software\Classes\unbind";
    let directory =
        PathBuf::from(env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?)
            .join("unbind");
    fs::create_dir_all(&directory)?;
    let icon = directory.join("unbind.ico");
    fs::write(&icon, include_bytes!("../../../assets/icon.ico"))?;
    let icon_value = format!("\"{}\",0", icon.display());
    run(
        Command::new("reg").args(["add", SCHEME, "/ve", "/d", "URL:Unbind", "/f"]),
        "Creating the Unbind URL scheme",
    )?;
    run(
        Command::new("reg").args([
            "add",
            SCHEME,
            "/v",
            "URL Protocol",
            "/t",
            "REG_SZ",
            "/d",
            "",
            "/f",
        ]),
        "Registering Unbind as a URL protocol",
    )?;
    run(
        Command::new("reg").args([
            "add",
            r"HKCU\Software\Classes\unbind\DefaultIcon",
            "/ve",
            "/d",
            &icon_value,
            "/f",
        ]),
        "Registering the Unbind icon",
    )?;
    let handler = format!("\"{}\" open-link \"%1\"", binary.display());
    run(
        Command::new("reg")
            .args([
                "add",
                r"HKCU\Software\Classes\unbind\shell\open\command",
                "/ve",
                "/d",
            ])
            .arg(handler)
            .arg("/f"),
        "Registering the Unbind command",
    )
}

#[cfg(target_os = "macos")]
fn register_platform(binary: &Path) -> Result<()> {
    let directory = home()?.join("Library/Application Support/Unbind");
    fs::create_dir_all(&directory)?;
    let app = directory.join("Unbind Link.app");
    let stage = tempfile::tempdir_in(&directory)?;
    let staging_app = stage.path().join("Unbind Link.app");
    let source = stage.path().join("handler.applescript");
    let path = binary
        .to_str()
        .context("The executable path is not valid UTF-8")?;
    ensure!(
        !path.contains(['\n', '\r']),
        "The executable path contains a newline"
    );
    let path = path.replace('\\', "\\\\").replace('"', "\\\"");
    fs::write(
        &source,
        format!(
            "on open location inviteUrl\nset launchCommand to (quoted form of \"{path}\") & \" open-link \" & (quoted form of inviteUrl)\ntell application \"Terminal\"\nactivate\ndo script launchCommand\nend tell\nend open location\n"
        ),
    )?;
    run(
        Command::new("osacompile")
            .arg("-o")
            .arg(&staging_app)
            .arg(&source),
        "Building the Unbind link handler",
    )?;
    let contents = staging_app.join("Contents");
    let resources = contents.join("Resources");
    let icon_catalog = resources.join("Assets.car");
    let plist = contents.join("Info.plist");
    fs::write(
        resources.join("Unbind.icns"),
        include_bytes!("../../../assets/icon.icns"),
    )?;
    let mut configure = Command::new("/usr/libexec/PlistBuddy");
    if icon_catalog.is_file() {
        // macOS prefers the stock applet asset catalog over an ICNS file.
        fs::remove_file(icon_catalog)?;
        if Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", "Print :CFBundleIconName"])
            .arg(&plist)
            .output()?
            .status
            .success()
        {
            configure.args(["-c", "Delete :CFBundleIconName"]);
        }
    }
    run(
        configure
            .args([
                "-c",
                "Set :CFBundleIdentifier dev.unbind.link-handler",
                "-c",
                "Set :CFBundleIconFile Unbind.icns",
                "-c",
                "Add :CFBundleURLTypes array",
                "-c",
                "Add :CFBundleURLTypes:0 dict",
                "-c",
                "Add :CFBundleURLTypes:0:CFBundleURLName string Unbind invitation",
                "-c",
                "Add :CFBundleURLTypes:0:CFBundleURLSchemes array",
                "-c",
                "Add :CFBundleURLTypes:0:CFBundleURLSchemes:0 string unbind",
            ])
            .arg(plist),
        "Configuring the Unbind link handler",
    )?;
    fs::write(contents.join(".unbind-link-handler"), "")?;
    run(
        Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-", "--preserve-metadata=entitlements"])
            .arg(&staging_app),
        "Signing the local Unbind link handler",
    )?;

    let previous = stage.path().join("previous.app");
    if app.exists() {
        ensure!(
            app.join("Contents/.unbind-link-handler").is_file(),
            "An existing application occupies {}",
            app.display()
        );
        fs::rename(&app, &previous)?;
    }
    if let Err(error) = fs::rename(staging_app, &app) {
        if previous.exists() {
            fs::rename(&previous, &app).context("Could not restore the previous handler")?;
        }
        return Err(error.into());
    }
    run(Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
        .arg("-f").arg(app), "Registering the Unbind URL scheme")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn register_platform(_: &Path) -> Result<()> {
    anyhow::bail!("Desktop link registration is unavailable on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_the_embedded_invite() {
        let link = crate::signaling::Invite::new("https://unbind.dev")
            .unwrap()
            .link()
            .to_string();
        let uri = format!("unbind://connect#{}", URL_SAFE_NO_PAD.encode(&link));
        assert_eq!(unpack(&uri).unwrap(), link);
        assert!(unpack("unbind://connect#YW55dGhpbmc").is_err());
        assert!(unpack(&uri.replacen("connect", "install", 1)).is_err());
    }
}
