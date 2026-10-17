//! The daemon: owns every session, accepts clients, runs the agent.

use std::io::{self, BufReader};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use protocol::{
    BUILD, Body, ClientMsg, DaemonMsg, Entry, PROTO, SessionId, SessionSummary, Target, read_frame,
    write_frame,
};
use store::{Header, Log, Store};
use sys::Home;

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

struct Session {
    id: SessionId,
    workspace: PathBuf,
    state: Mutex<State>,
}

struct State {
    entries: Vec<Entry>,
    log: Log,
    clients: Vec<Sender<DaemonMsg>>,
}

impl Session {
    fn attach(&self, client: &Sender<DaemonMsg>, since: u64) {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = client.send(DaemonMsg::Attached {
            session: self.id,
            workspace: self.workspace.clone(),
        });
        for e in st.entries.iter().filter(|e| e.seq > since) {
            let _ = client.send(DaemonMsg::Entry(e.clone()));
        }
        let seq = st.entries.last().map_or(0, |e| e.seq);
        let _ = client.send(DaemonMsg::Synced { seq });
        st.clients.push(client.clone());
    }

    fn append(&self, body: Body) -> io::Result<()> {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = Entry {
            seq: st.entries.len() as u64 + 1,
            epoch: 0,
            body,
        };
        st.log.append(&entry)?;
        st.clients
            .retain(|c| c.send(DaemonMsg::Entry(entry.clone())).is_ok());
        st.entries.push(entry);
        Ok(())
    }

    fn summary(&self) -> SessionSummary {
        let st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        SessionSummary {
            id: self.id,
            workspace: self.workspace.clone(),
            entries: st.entries.len() as u64,
            clients: u32::try_from(st.clients.len()).unwrap_or(u32::MAX),
        }
    }
}

struct Daemon {
    store: Store,
    sessions: Mutex<Vec<Arc<Session>>>,
}

impl Daemon {
    fn load(home: &Home) -> io::Result<Self> {
        let store = Store::open(home.sessions_dir())?;
        let sessions = store
            .load_all()?
            .into_iter()
            .map(|l| {
                Arc::new(Session {
                    id: l.header.id,
                    workspace: l.header.workspace,
                    state: Mutex::new(State {
                        entries: l.entries,
                        log: l.log,
                        clients: Vec::new(),
                    }),
                })
            })
            .collect();
        Ok(Self {
            store,
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
        let session = Arc::new(Session {
            id: header.id,
            workspace: header.workspace,
            state: Mutex::new(State {
                entries: Vec::new(),
                log,
                clients: Vec::new(),
            }),
        });
        sessions.push(session.clone());
        Ok(session)
    }

    fn list(&self) -> Vec<SessionSummary> {
        let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.iter().map(|s| s.summary()).collect()
    }
}

/// The echo agent: replies with the user's text.
fn respond(session: &Session, text: String) -> io::Result<()> {
    session.append(Body::User { text: text.clone() })?;
    session.append(Body::Assistant { text })
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
                Some(s) => respond(s, text)?,
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
