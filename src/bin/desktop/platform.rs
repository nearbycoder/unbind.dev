use anyhow::Result;

pub const APP_ID: &str = "dev.unbind.Unbind";

#[cfg(target_os = "linux")]
pub fn enable_links() -> Result<String> {
    use std::{env, fs, path::PathBuf, process::Command};

    use anyhow::{Context, ensure};

    let home = PathBuf::from(env::var_os("HOME").context("HOME is unavailable")?);
    let xdg = |key: &str, fallback: &str| {
        env::var_os(key)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(fallback))
    };
    let data = xdg("XDG_DATA_HOME", ".local/share");
    let applications = data.join("applications");
    let icons = data.join("icons/hicolor/scalable/apps");
    fs::create_dir_all(&applications)?;
    fs::create_dir_all(&icons)?;
    fs::create_dir_all(xdg("XDG_CONFIG_HOME", ".config"))?;

    let binary = env::current_exe()?;
    let path = binary
        .to_str()
        .context("The Unbind path is not valid UTF-8")?;
    ensure!(
        !path.contains(['\r', '\n']),
        "The Unbind path contains a newline"
    );
    let escaped = path
        .replace('\\', "\\\\\\\\")
        .replace('"', "\\\\\\\"")
        .replace('$', "\\\\\\$")
        .replace('`', "\\\\\\`")
        .replace('%', "%%");

    let filename = format!("{APP_ID}.desktop");
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=Unbind\nIcon={APP_ID}\nExec=\"{escaped}\" %u\nTerminal=false\nCategories=Network;\nMimeType=x-scheme-handler/unbind;\n"
    );
    fs::write(
        icons.join(format!("{APP_ID}.svg")),
        include_bytes!("../../../assets/icon.svg"),
    )?;
    fs::write(applications.join(&filename), entry)?;
    let result = Command::new("xdg-mime")
        .args(["default", &filename, "x-scheme-handler/unbind"])
        .output()
        .context("Find xdg-mime to register desktop invites")?;
    ensure!(
        result.status.success(),
        "Could not register Unbind as the invite handler"
    );
    Ok("Invite links now open in the Unbind desktop app.".to_owned())
}

#[cfg(windows)]
pub fn enable_links() -> Result<String> {
    use std::{env, fs, os::windows::process::CommandExt, path::PathBuf, process::Command};

    use anyhow::{Context, ensure};

    const SCHEME: &str = r"HKCU\Software\Classes\unbind";
    let dir = PathBuf::from(env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?)
        .join("unbind");
    fs::create_dir_all(&dir)?;
    let icon = dir.join("desktop.ico");
    fs::write(&icon, include_bytes!("../../../assets/icon.ico"))?;
    let binary = env::current_exe()?;
    let default_icon = format!("\"{}\",0", icon.display());
    let open_command = format!("\"{}\" \"%1\"", binary.display());
    let register = |key: &str, field: &[&str], value: &str| -> Result<()> {
        let result = Command::new("reg.exe")
            .creation_flags(0x08000000)
            .args(["add", key])
            .args(field)
            .args(["/d", value, "/f"])
            .output()
            .context("Start Windows URL protocol registration")?;
        ensure!(
            result.status.success(),
            "Windows could not register the Unbind invite handler"
        );
        Ok(())
    };
    register(SCHEME, &["/ve"], "URL:Unbind")?;
    register(SCHEME, &["/v", "URL Protocol", "/t", "REG_SZ"], "")?;
    register(&format!(r"{SCHEME}\DefaultIcon"), &["/ve"], &default_icon)?;
    register(
        &format!(r"{SCHEME}\shell\open\command"),
        &["/ve"],
        &open_command,
    )?;
    Ok("Invite links now open in the Unbind desktop app.".to_owned())
}

#[cfg(target_os = "macos")]
pub fn enable_links() -> Result<String> {
    use anyhow::{Context, ensure};

    let executable = std::env::current_exe()?;
    let bundle = executable
        .ancestors()
        .nth(3)
        .filter(|path| path.extension().is_some_and(|extension| extension == "app"))
        .context(
            "Move the packaged Unbind.app to Applications and open it to enable invite links",
        )?;
    let output = std::process::Command::new(
        "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
    ).arg("-f").arg(bundle).output().context("Register the Unbind app bundle with macOS")?;
    ensure!(
        output.status.success(),
        "macOS could not register the Unbind app bundle"
    );
    Ok("The Unbind app is registered to open desktop invite links.".to_owned())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn enable_links() -> Result<String> {
    anyhow::bail!("Desktop link registration is unavailable on this platform")
}
