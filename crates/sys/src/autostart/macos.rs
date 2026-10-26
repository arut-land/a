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

/// The existing registration owns controls, regardless of which client asks.
fn owner(
    home: &Home,
    mut registered: impl FnMut(&str) -> io::Result<bool>,
) -> io::Result<Option<String>> {
    if plist(home)?.try_exists()? {
        return Ok(Some(label(home)));
    }
    // Overrides survive unregistration; only an existing service owns controls.
    Ok(registered(APP_LABEL)?.then(|| APP_LABEL.to_owned()))
}

fn registered(label: &str) -> io::Result<bool> {
    let output = Command::new("launchctl")
        .arg("print")
        .arg(format!("{}/{label}", domain()))
        .output()?;
    Ok(output.status.success())
}

#[derive(Clone, Copy, Debug)]
enum Switch {
    Enable,
    Disable,
}

fn switch(
    label: &str,
    action: Switch,
    mut run: impl FnMut(&mut Command) -> io::Result<()>,
) -> io::Result<()> {
    // enable/disable only changes future launches. bootout and SMAppService's
    // unregister also terminate the current process, which is a separate action.
    run(Command::new("launchctl")
        .arg(match action {
            Switch::Enable => "enable",
            Switch::Disable => "disable",
        })
        .arg(format!("{}/{label}", domain())))
}

pub(super) fn enable(home: &Home) -> io::Result<()> {
    let owner = if let Some(owner) = owner(home, registered)? {
        owner
    } else {
        update(&plist(home)?, contents(home)?.as_bytes())?;
        label(home)
    };
    switch(&owner, Switch::Enable, run)
}

pub(super) fn enable_bundled() -> io::Result<()> {
    switch(APP_LABEL, Switch::Enable, run)
}

pub(super) fn disable(home: &Home) -> io::Result<()> {
    if let Some(owner) = owner(home, registered)? {
        switch(&owner, Switch::Disable, run)?;
    }
    Ok(())
}

pub(super) fn registration(home: &Home) -> io::Result<super::Registration> {
    registration_with(home, &disabled()?, registered)
}

fn registration_with(
    home: &Home,
    disabled: &str,
    registered: impl FnMut(&str) -> io::Result<bool>,
) -> io::Result<super::Registration> {
    let Some(owner) = owner(home, registered)? else {
        return Ok(super::Registration::Unregistered);
    };
    Ok(if switched_off(disabled, &owner) {
        super::Registration::Disabled
    } else {
        super::Registration::Enabled
    })
}

pub(super) fn refresh(home: &Home) -> io::Result<bool> {
    let path = plist(home)?;
    if path.try_exists()? {
        update(&path, contents(home)?.as_bytes())?;
    }
    Ok(registration(home)? == super::Registration::Enabled)
}

/// launchd's override list for this user's domain, which the Login Items
/// switch (Background Task Management) writes.
fn disabled() -> io::Result<String> {
    let output = Command::new("launchctl")
        .arg("print-disabled")
        .arg(domain())
        .stdin(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether `label` is listed off: `"label" => disabled` (or `=> true` from
/// releases that print booleans).
fn switched_off(listing: &str, label: &str) -> bool {
    override_state(listing, label).unwrap_or(false)
}

fn override_state(listing: &str, label: &str) -> Option<bool> {
    let quoted = format!("\"{label}\"");
    listing.lines().find_map(|line| {
        match line
            .trim()
            .strip_prefix(quoted.as_str())?
            .trim_start()
            .strip_prefix("=>")?
            .trim()
        {
            "disabled" | "true" => Some(true),
            "enabled" | "false" => Some(false),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_switches_never_unload_the_running_daemon() {
        let mut actions = Vec::new();
        for action in [Switch::Disable, Switch::Enable] {
            switch(APP_LABEL, action, |command| {
                actions.push(
                    command
                        .get_args()
                        .map(|arg| arg.to_string_lossy().into_owned())
                        .collect::<Vec<_>>(),
                );
                Ok(())
            })
            .unwrap();
        }
        assert_eq!(
            actions,
            [
                vec!["disable".to_owned(), format!("{}/{APP_LABEL}", domain())],
                vec!["enable".to_owned(), format!("{}/{APP_LABEL}", domain())]
            ]
        );
    }

    #[test]
    fn overrides_cannot_create_a_bundled_registration() {
        let home = Home::from(std::env::temp_dir().join("a-autostart-bundle-owner"));
        for listing in [
            "\"dev.a.daemon\" => disabled",
            "\"dev.a.daemon\" => enabled",
        ] {
            assert_eq!(
                registration_with(&home, listing, |_| Ok(false)).unwrap(),
                super::super::Registration::Unregistered,
            );
        }
        assert_eq!(
            registration_with(&home, "\"dev.a.daemon\" => disabled", |label| {
                Ok(label == APP_LABEL)
            })
            .unwrap(),
            super::super::Registration::Disabled,
        );
        assert_eq!(
            registration_with(&home, "", |label| Ok(label == APP_LABEL)).unwrap(),
            super::super::Registration::Enabled,
        );
    }

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
