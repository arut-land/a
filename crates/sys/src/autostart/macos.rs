use super::{Command, Home, io, run, update};
use std::path::PathBuf;

pub(super) const LOGIN: super::Login = super::Login::Serve;

/// The installed app's bundled agent, registered through `SMAppService`.
const APP_LABEL: &str = "dev.a.daemon";

fn label(home: &Home) -> String {
    format!("{APP_LABEL}.{:016x}", crate::endpoint::home_identity(home))
}

fn domain() -> String {
    format!("gui/{}", rustix::process::geteuid().as_raw())
}

fn user_home() -> io::Result<PathBuf> {
    std::env::home_dir().ok_or_else(|| io::ErrorKind::NotFound.into())
}

fn plist(home: &Home) -> io::Result<PathBuf> {
    Ok(user_home()?
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", label(home))))
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// launchd loads every agent in `~/Library/LaunchAgents` at login and
/// `RunAtLoad` starts it then. No `KeepAlive`: a daemon that exits (stopped,
/// or drained for a newer build) is not brought back.
fn contents(home: &Home) -> io::Result<String> {
    let exe = home.runtime().current_exe();
    let exe = exe.to_str().ok_or(io::ErrorKind::InvalidInput)?;
    let user_home = user_home()?;
    let user_home = user_home.to_str().ok_or(io::ErrorKind::InvalidInput)?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{}</string>
<key>ProgramArguments</key><array><string>{}</string><string>__daemon</string><string>--login</string></array>
<key>EnvironmentVariables</key><dict><key>HOME</key><string>{}</string></dict>
<key>WorkingDirectory</key><string>/</string>
<key>RunAtLoad</key><true/>
<key>Umask</key><integer>63</integer>
<key>ExitTimeOut</key><integer>30</integer>
</dict></plist>
"#,
        xml(&label(home)),
        xml(exe),
        xml(user_home)
    ))
}

pub(super) fn enable(home: &Home) -> io::Result<()> {
    // Written, not bootstrapped: loading it now would start a second daemon
    // only for it to find this home's daemon serving.
    update(&plist(home)?, contents(home)?.as_bytes()).map(drop)
}

pub(super) fn disable(home: &Home) -> io::Result<()> {
    let path = plist(home)?;
    if !path.exists() {
        return Ok(());
    }
    // Loaded since the last login, unless this login enabled it.
    let _ =
        run(Command::new("launchctl")
            .arg("bootout")
            .arg(format!("{}/{}", domain(), label(home))));
    std::fs::remove_file(path)
}

pub(super) fn refresh(home: &Home) -> io::Result<bool> {
    let path = plist(home)?;
    let disabled = disabled()?;
    if path.exists() {
        update(&path, contents(home)?.as_bytes())?;
        return Ok(!switched_off(&disabled, &label(home)));
    }
    // The app's agent names its helper inside the bundle, so it never needs
    // rewriting; it is on while launchd has it loaded.
    Ok(!switched_off(&disabled, APP_LABEL)
        && run(Command::new("launchctl")
            .arg("print")
            .arg(format!("{}/{APP_LABEL}", domain())))
        .is_ok())
}

/// launchd's override list for this user's domain, which the Login Items
/// switch (Background Task Management) writes.
fn disabled() -> io::Result<String> {
    let output = Command::new("launchctl")
        .arg("print-disabled")
        .arg(domain())
        .stdin(std::process::Stdio::null())
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether `label` is listed off: `"label" => disabled` (or `=> true` from
/// releases that print booleans).
fn switched_off(listing: &str, label: &str) -> bool {
    let quoted = format!("\"{label}\"");
    listing.lines().any(|line| {
        let line = line.trim();
        line.strip_prefix(quoted.as_str())
            .and_then(|rest| rest.trim_start().strip_prefix("=>"))
            .is_some_and(|state| matches!(state.trim(), "disabled" | "true"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_names_cannot_cross_state_homes() {
        let root = std::env::temp_dir().join(format!("a-autostart-names-{}", std::process::id()));
        let one = Home::from(root.join("one"));
        let two = Home::from(root.join("two"));
        assert_ne!(label(&one), label(&two));
        assert_ne!(plist(&one).unwrap(), plist(&two).unwrap());
    }

    #[test]
    fn the_login_items_switch_turns_residency_off() {
        let listing = "disabled services = {\n\t\"dev.a.daemon.1\" => disabled\n\t\"dev.a.daemon\" => enabled\n\t\"other\" => true\n}\n";
        assert!(switched_off(listing, "dev.a.daemon.1"));
        assert!(!switched_off(listing, "dev.a.daemon"));
        assert!(switched_off(listing, "other"));
        assert!(!switched_off(listing, "dev.a.daemon.2"));
    }

    #[test]
    fn plist_values_cannot_become_markup() {
        assert_eq!(xml("<&\"'>"), "&lt;&amp;&quot;&apos;&gt;");
    }

    #[test]
    fn the_login_agent_starts_at_load_and_is_never_kept_alive() {
        let agent = contents(&Home::from(std::env::temp_dir().join("a-autostart-plist"))).unwrap();
        assert!(agent.contains("<string>__daemon</string><string>--login</string>"));
        assert!(agent.contains("<key>RunAtLoad</key><true/>"));
        assert!(!agent.contains("KeepAlive") && !agent.contains("Sockets"));
    }
}
