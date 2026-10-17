//! The daemon: owns every session, accepts clients, runs the agents.

mod session;

use std::io::{self, BufReader};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use agent::Toolset;
use protocol::{
    BUILD, ClientMsg, DaemonMsg, PROTO, SessionId, SessionSummary, Target, read_frame, write_frame,
};
use session::Session;
use store::{Header, Store};
use sys::Home;

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

struct Daemon {
    store: Store,
    tools: Arc<Toolset>,
    sessions: Mutex<Vec<Arc<Session>>>,
}

impl Daemon {
    fn load(home: &Home) -> io::Result<Self> {
        let store = Store::open(home.sessions_dir())?;
        let tools = Arc::new(Toolset::builtin());
        let sessions = store
            .load_all()?
            .into_iter()
            .map(|l| {
                Session::start(
                    l.header.id,
                    l.header.workspace,
                    l.entries,
                    l.log,
                    tools.clone(),
                )
            })
            .collect();
        Ok(Self {
            store,
            tools,
            sessions: Mutex::new(sessions),
        })
    }

    fn resolve(&self, target: &Target, workspace: PathBuf) -> io::Result<Arc<Session>> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let found = match target {
            Target::New => None,
            Target::Id(id) => sessions.iter().find(|s| s.id == *id).cloned(),
            Target::Latest { any_workspace } => sessions
                .iter()
                .rev()
                .find(|s| *any_workspace || s.workspace == workspace)
                .cloned(),
        };
        if let Some(s) = found {
            return Ok(s);
        }
        if !matches!(target, Target::New) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no matching session",
            ));
        }
        let header = Header {
            id: SessionId::new(),
            workspace,
        };
        let log = self.store.create(&header)?;
        let session = Session::start(
            header.id,
            header.workspace,
            Vec::new(),
            log,
            self.tools.clone(),
        );
        sessions.push(session.clone());
        Ok(session)
    }

    fn list(&self) -> Vec<SessionSummary> {
        let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.iter().map(|s| s.summary()).collect()
    }
}

#[cfg(unix)]
pub fn serve(listener: &UnixListener, home: &Home) -> io::Result<()> {
    let daemon = Arc::new(Daemon::load(home)?);
    for conn in listener.incoming() {
        let conn = conn?;
        let daemon = daemon.clone();
        thread::spawn(move || {
            if let Err(e) = handle(&daemon, conn) {
                eprintln!("connection: {e}");
            }
        });
    }
    Ok(())
}

#[cfg(unix)]
fn handle(daemon: &Daemon, conn: UnixStream) -> io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let (tx, rx) = mpsc::channel::<DaemonMsg>();
    let mut writer = conn;
    thread::spawn(move || {
        for msg in rx {
            if write_frame(&mut writer, &msg).is_err() {
                break;
            }
        }
    });

    match read_frame(&mut reader)? {
        Some(ClientMsg::Hello { proto, .. }) if proto == PROTO => {
            let _ = tx.send(DaemonMsg::Welcome {
                proto: PROTO,
                build: BUILD.to_owned(),
            });
        }
        _ => {
            let _ = tx.send(DaemonMsg::Mismatch { supported: PROTO });
            return Ok(());
        }
    }

    let mut attached: Option<Arc<Session>> = None;
    while let Some(msg) = read_frame(&mut reader)? {
        match msg {
            ClientMsg::Attach {
                target,
                workspace,
                since,
            } => match daemon.resolve(&target, workspace) {
                Ok(s) => {
                    s.attach(&tx, since);
                    attached = Some(s);
                }
                Err(e) => {
                    let _ = tx.send(DaemonMsg::Error {
                        message: e.to_string(),
                    });
                }
            },
            ClientMsg::Input { text } => match &attached {
                Some(s) => s.submit(text),
                None => {
                    let _ = tx.send(DaemonMsg::Error {
                        message: "not attached".into(),
                    });
                }
            },
            ClientMsg::List => {
                let _ = tx.send(DaemonMsg::Sessions {
                    sessions: daemon.list(),
                });
            }
            ClientMsg::Detach => break,
            ClientMsg::Hello { .. } => {}
        }
    }
    Ok(())
}
