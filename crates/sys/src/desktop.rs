use std::io;
use std::process::{Command, Stdio};
use std::thread;

/// Opens a link without waiting for the browser to exit.
pub fn open_browser(link: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(windows)]
    let program = "explorer.exe";
    #[cfg(not(any(target_os = "macos", windows)))]
    let program = "xdg-open";
    let mut command = Command::new(program);
    command
        .arg(link)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    let mut child = {
        let null = std::fs::File::options()
            .read(true)
            .write(true)
            .open("/dev/null")?;
        crate::proc::unix::SpawnPlan::new(&command, [&null, &null, &null])
            .inherit_snapshot()
            .uncontained()
            .spawn()?
    };
    #[cfg(windows)]
    let mut child = command.spawn()?;
    // Some openers stay until the browser they started exits.
    thread::spawn(move || child.wait());
    Ok(())
}
