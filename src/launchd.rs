//! Running the daemon at login as a launchd user agent.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nix::unistd::getuid;

use crate::env::Env;

pub const LABEL: &str = "com.ae2rs.cendrillon";

/// Writes the agent's plist pointing at the running executable and (re)starts it.
pub fn install(env: &Env) -> Result<()> {
    let exe = std::env::current_exe()?
        .canonicalize()
        .context("locating the cendrillon executable")?;
    let plist_path = env.launch_agent_file(LABEL);
    fs::create_dir_all(plist_path.parent().context("plist path has no parent")?)?;
    fs::write(&plist_path, plist(&exe, &env.log_file()))
        .with_context(|| format!("writing {}", plist_path.display()))?;

    let domain = format!("gui/{}", getuid());
    // Replace a running instance, which may be an older binary.
    let _ = launchctl(&["bootout", &format!("{domain}/{LABEL}")]);
    // bootout returns before the old instance is gone; bootstrap fails until it is.
    let plist_arg = plist_path.to_string_lossy();
    let mut attempts = 0;
    while let Err(e) = launchctl(&["bootstrap", &domain, &plist_arg]) {
        attempts += 1;
        if attempts == 10 {
            return Err(e);
        }
        sleep(Duration::from_millis(500));
    }
    println!(
        "Installed {} running {}",
        plist_path.display(),
        exe.display()
    );
    println!("Logs: {}", env.log_file().display());
    Ok(())
}

/// Stops the agent and removes its plist.
pub fn uninstall(env: &Env) -> Result<()> {
    let plist_path = env.launch_agent_file(LABEL);
    let _ = launchctl(&["bootout", &format!("gui/{}/{LABEL}", getuid())]);
    match fs::remove_file(&plist_path) {
        Ok(()) => println!("Removed {}", plist_path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("Not installed."),
        Err(e) => return Err(e).with_context(|| format!("removing {}", plist_path.display())),
    }
    Ok(())
}

fn launchctl(args: &[&str]) -> Result<()> {
    let output = Command::new("/bin/launchctl").args(args).output()?;
    if !output.status.success() {
        bail!(
            "launchctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// A low-priority background agent that starts at login and is restarted if it dies.
fn plist(exe: &Path, log: &Path) -> String {
    let exe = xml_escape(&exe.to_string_lossy());
    let log = xml_escape(&log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>60</integer>
    <key>ProcessType</key>
    <string>Background</string>
    <key>LowPriorityIO</key>
    <true/>
    <key>Nice</key>
    <integer>10</integer>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.plist");
        let text = plist(
            Path::new("/Users/me/.cargo/bin/cendrillon"),
            Path::new("/Users/me/Library/Logs/a&b.log"),
        );
        fs::write(&path, &text).unwrap();
        let lint = Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&path)
            .output()
            .unwrap();
        assert!(lint.status.success(), "{lint:?}");
        assert!(text.contains("<string>/Users/me/Library/Logs/a&amp;b.log</string>"));
    }
}
