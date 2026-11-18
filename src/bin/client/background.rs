use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use tokio::time::{Instant, sleep};

use crate::{
    management::{self, Action, create_private_file, private_dir},
    profiles,
    signaling::random_id,
};

pub async fn start(config: &Path, profile: &str, at_login: bool) -> Result<()> {
    profiles::load(config, profile)?;
    if find_profile(profile).await?.is_some() {
        bail!("The '{profile}' profile is already running");
    }
    let config = fs::canonicalize(config).context("Resolve profile configuration")?;
    let program = std::env::current_exe()?;
    let state = management::state_dir()?;
    let arguments = [
        "--config".into(),
        absolute_text(&config)?,
        "--state-dir".into(),
        absolute_text(&state)?,
        "share".into(),
        profile.into(),
    ];
    let mut child = if at_login {
        set_login(profile, &program, &arguments, true)?;
        None
    } else {
        let logs = state.join("logs");
        private_dir(&logs)?;
        let log = logs.join(format!("{profile}-{}.log", random_id()?));
        let output = create_private_file(&log)?;
        let mut command = Command::new(&program);
        command
            .args(&arguments)
            .stdin(Stdio::null())
            .stderr(Stdio::from(output.try_clone()?))
            .stdout(Stdio::from(output));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            command.creation_flags(DETACHED_PROCESS);
        }
        eprintln!("Background log at {}", log.display());
        Some(command.spawn().context("Start background Unbind share")?)
    };

    let deadline = Instant::now() + Duration::from_secs(16);
    let running = loop {
        if let Some((record, _)) = find_profile(profile).await? {
            break record;
        }
        if let Some(child) = child.as_mut()
            && let Some(status) = child.try_wait()?
        {
            bail!("The background share exited with {status}");
        }
        if Instant::now() >= deadline {
            if let Some(child) = child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            bail!("The background share did not become ready");
        }
        sleep(Duration::from_millis(150)).await;
    };
    let invitation = management::request(&running, Action::GetInitialInvite).await?;
    eprintln!(
        "Background share {}. Stop it with unbind stop {}.",
        running.id, running.id
    );
    println!(
        "{}",
        invitation["link"].as_str().context("Missing invite link")?
    );
    Ok(())
}

async fn find_profile(profile: &str) -> Result<Option<(management::Record, Value)>> {
    Ok(management::active()
        .await?
        .into_iter()
        .find(|(_, status)| status["profile"] == profile))
}

pub async fn stop(target: &str, at_login: bool) -> Result<()> {
    if at_login {
        ensure!(profiles::valid_name(target), "Invalid profile name");
        set_login(target, Path::new(""), &[], false)?;
    }
    let active = management::active().await?;
    let mut found = false;
    for (record, status) in active {
        if record.id == target || status["profile"] == target {
            management::request(&record, Action::Stop).await?;
            println!("Stopped {}", record.id);
            found = true;
        }
    }
    ensure!(
        found || at_login,
        "No Unbind session '{target}'. Run unbind status to list sessions."
    );
    Ok(())
}

fn absolute_text(path: &Path) -> Result<String> {
    Ok(path
        .to_str()
        .context("Unbind background paths must be valid UTF-8")?
        .to_owned())
}

fn run(command: &mut Command, description: &str) -> Result<()> {
    let result = command.output().with_context(|| description.to_owned())?;
    ensure!(
        result.status.success(),
        "{description}: {}",
        String::from_utf8_lossy(&result.stderr).trim()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_login(name: &str, program: &Path, args: &[String], enabled: bool) -> Result<()> {
    fn argument(value: &str) -> String {
        format!(
            "\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('%', "%%")
                .replace('\n', "\\n")
        )
    }
    let config = if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(path)
    } else {
        PathBuf::from(std::env::var_os("HOME").context("HOME is unavailable")?).join(".config")
    };
    let unit = format!("unbind-{name}.service");
    let directory = config.join("systemd/user");
    let path = directory.join(&unit);
    let command = || {
        let mut command = Command::new("systemctl");
        command.arg("--user");
        command
    };
    if !enabled {
        if path.is_file() {
            run(
                command().args(["disable", "--now", &unit]),
                "Disable the Unbind login service",
            )?;
            fs::remove_file(path)?;
            run(
                command().arg("daemon-reload"),
                "Reload the user service manager",
            )?;
        }
        return Ok(());
    }

    fs::create_dir_all(directory)?;
    let executable = argument(&absolute_text(program)?);
    let arguments: Vec<_> = args.iter().map(|arg| argument(arg)).collect();
    let text = format!(
        "[Unit]\nDescription=Unbind share {name}\n\n[Service]\nExecStart=:{executable} {}\nRestart=on-failure\nRestartSec=4\nStandardOutput=null\n\n[Install]\nWantedBy=default.target\n",
        arguments.join(" ")
    );
    create_private_file(&path)?.write_all(text.as_bytes())?;
    let setup = run(
        command().arg("daemon-reload"),
        "Reload the user service manager",
    )
    .and_then(|()| {
        run(
            command().args(["enable", "--now", &unit]),
            "Start the Unbind login service",
        )
    });
    if setup.is_err() {
        let _ = command().args(["disable", "--now", &unit]).output();
        let _ = fs::remove_file(&path);
        let _ = command().arg("daemon-reload").output();
    }
    setup
}

#[cfg(target_os = "macos")]
fn set_login(name: &str, program: &Path, args: &[String], enabled: bool) -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is unavailable")?);
    let directory = home.join("Library/LaunchAgents");
    let label = format!("dev.unbind.{name}");
    let path = directory.join(format!("{label}.plist"));
    let uid = Command::new("id").arg("-u").output()?;
    ensure!(
        uid.status.success(),
        "Could not determine the logged-in user"
    );
    let domain = format!("gui/{}", String::from_utf8_lossy(&uid.stdout).trim());
    if !enabled {
        if path.is_file() {
            run(
                Command::new("launchctl")
                    .arg("bootout")
                    .arg(&domain)
                    .arg(&path),
                "Stop the Unbind Login Agent",
            )?;
            fs::remove_file(path)?;
        }
        return Ok(());
    }

    fs::create_dir_all(&directory)?;
    let escape = |value: &str| {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let mut text = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>RunAtLoad</key><true/><key>ProgramArguments</key><array>"
    );
    for value in std::iter::once(absolute_text(program)?).chain(args.iter().cloned()) {
        text.push_str(&format!("<string>{}</string>", escape(&value)));
    }
    text.push_str("</array></dict></plist>\n");
    create_private_file(&path)?.write_all(text.as_bytes())?;
    let setup = run(
        Command::new("launchctl")
            .arg("bootstrap")
            .arg(&domain)
            .arg(&path),
        "Start the Unbind Login Agent",
    );
    if setup.is_err() {
        let _ = fs::remove_file(path);
    }
    setup
}

#[cfg(windows)]
fn set_login(name: &str, program: &Path, args: &[String], enabled: bool) -> Result<()> {
    let key = "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    let label = format!("Unbind-{name}");
    if !enabled {
        let status = Command::new("reg")
            .args(["query", key, "/v", &label])
            .output()?;
        if status.status.success() {
            run(
                Command::new("reg").args(["delete", key, "/v", &label, "/f"]),
                "Unregister the Unbind login service",
            )?;
        }
        return Ok(());
    }

    let executable = absolute_text(program)?;
    let mut command_line = format!("\"{executable}\"");
    for arg in args {
        command_line.push_str(&format!(" \"{arg}\""));
    }
    run(
        Command::new("reg").args([
            "add",
            key,
            "/v",
            &label,
            "/t",
            "REG_SZ",
            "/d",
            &command_line,
            "/f",
        ]),
        "Register the Unbind login service",
    )?;
    use std::os::windows::process::CommandExt;
    if let Err(error) = Command::new(program)
        .args(args)
        .creation_flags(0x0000_0008)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        let _ = Command::new("reg")
            .args(["delete", key, "/v", &label, "/f"])
            .output();
        return Err(error).context("Start the Unbind login service");
    }
    Ok(())
}
