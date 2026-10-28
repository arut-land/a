//! Scoped helpers through the real engine and native process provider.

use super::*;
use std::sync::Arc;

fn spec(workspace: &std::path::Path) -> ext::LaunchSpec {
    ext::LaunchSpec {
        process: ext::SpawnSpec {
            argv: vec![std::env::var("SCOPED_FIXTURE").unwrap()],
            env: std::collections::BTreeMap::new(),
            cwd: workspace.to_owned(),
        },
        output: ext::ProcessOutput::Stdio,
    }
}

#[test]
#[ignore = "release measurement controlled by the external AB/BA runner"]
fn process_scope_measurement() {
    let count: usize = std::env::var("SCOPED_COUNT").unwrap().parse().unwrap();
    let (root, home) = home("scope-measurement");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let sessions: Vec<_> = (0..count)
        .map(|_| daemon.attach_to(&Target::New, || root.clone()).unwrap())
        .collect();
    let scoped = ext::Scoped::new(Arc::clone(
        daemon.queried.get_arc::<ext::Lifetimes>().unwrap(),
    ));
    let started = Instant::now();
    let mut children = Vec::new();
    let mut latencies = Vec::new();
    for session in &sessions {
        let started = Instant::now();
        let cx = session.session().process_cx();
        let launcher = cx
            .service::<dyn ext::Processes>()
            .unwrap()
            .launcher(&cx)
            .unwrap();
        let launch = spec(&root);
        let child = rt::block_on(scoped.acquire(
            ext::InstanceKey {
                identity: "fixture",
                scope: ext::Scope::Workspace(root.clone()),
            },
            move |cancel| {
                Box::pin(async move {
                    let child = launcher.spawn(cancel, launch).await?;
                    let mut ready = [0];
                    child.output.as_ref().unwrap().read(&mut ready).await?;
                    Ok(child)
                })
            },
        ))
        .unwrap();
        cx.service::<ext::SessionLifetimes>()
            .unwrap()
            .retain(&child)
            .unwrap();
        latencies.push(started.elapsed().as_secs_f64() * 1000.0);
        children.push(child);
    }
    println!(
        "SCOPED {}",
        json!({"pid":std::process::id(),"children":children.iter().map(|p| p.id()).collect::<std::collections::BTreeSet<_>>(), "latencies_ms":latencies,"total_ms":started.elapsed().as_secs_f64()*1000.0})
    );
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    let controls: Vec<_> = children.iter().map(|p| Arc::clone(&p.process)).collect();
    drop(children);
    drop(sessions);
    daemon.hibernate(Duration::ZERO);
    rt::block_on(async {
        for child in controls {
            child.ended().await;
        }
    });
    if std::env::var_os("SCOPED_IDLE").is_some() {
        println!("RETIRED {}", std::process::id());
        line.clear();
        std::io::stdin().read_line(&mut line).unwrap();
    }
    rt::block_on(daemon.shutdown());
    drop(daemon);
    std::fs::remove_dir_all(root).unwrap();
}

fn fixture(workspace: &std::path::Path) -> ext::LaunchSpec {
    ext::LaunchSpec {
        process: ext::SpawnSpec {
            argv: vec![
                std::env::current_exe().unwrap().to_str().unwrap().into(),
                "--exact".into(),
                "tests::scoped_processes::fixture_child".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
            env: std::collections::BTreeMap::new(),
            cwd: workspace.to_owned(),
        },
        output: ext::ProcessOutput::Stdio,
    }
}

#[test]
#[ignore = "child process for lifetime tests"]
fn fixture_child() {
    use std::io::{Read, Write};
    if std::env::var_os("SCOPED_PARK").is_some() {
        loop {
            std::thread::park();
        }
    }
    let descendant = if std::env::var_os("SCOPED_DESCENDANT").is_some() {
        Some(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::scoped_processes::fixture_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env_clear()
                .env("SCOPED_PARK", "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        )
    } else {
        None
    };
    let mut output = std::io::stdout();
    writeln!(
        output,
        "\x1e{}",
        descendant.as_ref().map_or(0, std::process::Child::id)
    )
    .unwrap();
    output.flush().unwrap();
    let mut byte = [0];
    let _ = std::io::stdin().read(&mut byte);
    if let Some(mut child) = descendant {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

async fn readiness(child: &ext::OwnedProcess) -> u32 {
    let output = child.output.as_ref().unwrap();
    let mut byte = [0];
    loop {
        assert_eq!(
            output.read(&mut byte).await.unwrap(),
            1,
            "fixture exited before readiness"
        );
        if byte[0] == 30 {
            break;
        }
    }
    let mut pid = String::new();
    loop {
        assert_eq!(output.read(&mut byte).await.unwrap(), 1);
        if byte[0] == b'\n' {
            break;
        }
        pid.push(char::from(byte[0]));
    }
    pid.trim().parse().unwrap()
}

#[test]
fn simultaneous_demand_starts_one_process_and_cold_start_is_single_flight() {
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Waker};
    let (root, home) = home("scope-single-flight");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let sessions: Vec<_> = (0..128)
        .map(|_| daemon.attach_to(&Target::New, || root.clone()).unwrap())
        .collect();
    let cx = sessions[0].session().process_cx();
    let launcher = cx
        .service::<dyn ext::Processes>()
        .unwrap()
        .launcher(&cx)
        .unwrap();
    let scoped = ext::Scoped::new(Arc::clone(
        daemon.queried.get_arc::<ext::Lifetimes>().unwrap(),
    ));
    let key = ext::InstanceKey {
        identity: "fixture",
        scope: ext::Scope::User,
    };
    let opening = ext::Flag::default();
    let starts = Arc::new(AtomicUsize::new(0));
    let mut waiting = Vec::new();
    for _ in 0..128 {
        let launcher = launcher.clone();
        let launch = fixture(&root);
        let opening = opening.clone();
        let starts = starts.clone();
        let mut acquire = Box::pin(scoped.acquire(key.clone(), move |cancel| {
            Box::pin(async move {
                starts.fetch_add(1, Ordering::SeqCst);
                cancel
                    .until(opening.wait())
                    .await
                    .ok_or(std::io::ErrorKind::Interrupted)?;
                let child = launcher.spawn(cancel, launch).await?;
                readiness(&child).await;
                Ok(child)
            })
        }));
        assert!(
            acquire
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let _: &dyn Send = &acquire;
        waiting.push(acquire);
    }
    drop(waiting.remove(0));
    opening.set();
    let leases: Vec<_> = waiting
        .into_iter()
        .map(|f| rt::block_on(f).unwrap())
        .collect();
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert!(leases.iter().all(|child| child.id() == leases[0].id()));
    for (session, lease) in sessions.iter().skip(1).zip(&leases) {
        session
            .session()
            .process_cx()
            .service::<ext::SessionLifetimes>()
            .unwrap()
            .retain(lease)
            .unwrap();
    }
    let control = Arc::clone(&leases[0].process);
    let cancelled = leases[0].cancelled().clone();
    drop(leases);
    assert!(control.alive(), "session demand did not retain the process");
    drop((cx, sessions));
    daemon.hibernate(Duration::ZERO);
    assert!(cancelled.is_set(), "last lease did not retire immediately");
    rt::block_on(control.ended());
    assert!(sys::process_started(control.id()).is_none());
    drop(daemon);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn sharing_obeys_exact_identity_workspace_and_session_boundaries() {
    let (root, home) = home("scope-boundaries");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let a = daemon.attach_to(&Target::New, || root.clone()).unwrap();
    let b = daemon.attach_to(&Target::New, || root.clone()).unwrap();
    let other = tempfile::tempdir().unwrap();
    let c = daemon
        .attach_to(&Target::New, || other.path().to_owned())
        .unwrap();
    let scoped = ext::Scoped::new(Arc::clone(
        daemon.queried.get_arc::<ext::Lifetimes>().unwrap(),
    ));
    let mut leases = Vec::new();
    for scope in [
        ext::Scope::User,
        ext::Scope::Workspace(root.clone()),
        ext::Scope::Session(a.session().id),
    ] {
        for session in [&a, &b, &c] {
            let scope = match &scope {
                ext::Scope::User => ext::Scope::User,
                ext::Scope::Workspace(_) => {
                    ext::Scope::Workspace(session.session().workspace.clone())
                }
                ext::Scope::Session(_) => ext::Scope::Session(session.session().id),
            };
            let cx = session.session().process_cx();
            let launcher = cx
                .service::<dyn ext::Processes>()
                .unwrap()
                .launcher(&cx)
                .unwrap();
            let launch = fixture(&root);
            leases.push(
                rt::block_on(scoped.acquire(
                    ext::InstanceKey {
                        identity: "fixture",
                        scope,
                    },
                    move |cancel| {
                        Box::pin(async move {
                            let child = launcher.spawn(cancel, launch).await?;
                            readiness(&child).await;
                            Ok(child)
                        })
                    },
                ))
                .unwrap(),
            );
        }
    }
    assert_eq!(leases[0].id(), leases[1].id());
    assert_eq!(leases[1].id(), leases[2].id());
    assert_eq!(leases[3].id(), leases[4].id());
    assert_ne!(leases[4].id(), leases[5].id());
    assert_eq!(
        leases[6..]
            .iter()
            .map(|p| p.id())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
    // Equal argv never merges two separately trusted declarations.
    let cx = a.session().process_cx();
    let launcher = cx
        .service::<dyn ext::Processes>()
        .unwrap()
        .launcher(&cx)
        .unwrap();
    let launch = fixture(&root);
    let distinct = rt::block_on(scoped.acquire(
        ext::InstanceKey {
            identity: "other declaration",
            scope: ext::Scope::User,
        },
        move |cancel| Box::pin(async move { launcher.spawn(cancel, launch).await }),
    ))
    .unwrap();
    assert_ne!(distinct.id(), leases[0].id());
    rt::block_on(daemon.shutdown());
    assert!(
        leases
            .iter()
            .all(|p| sys::process_started(p.id()).is_none())
    );
    assert!(sys::process_started(distinct.id()).is_none());
    let rejected = rt::block_on(scoped.acquire(
        ext::InstanceKey {
            identity: "fixture",
            scope: ext::Scope::User,
        },
        |_| panic!("shutdown admitted startup"),
    ));
    assert!(rejected.is_err());
    drop((leases, distinct, cx, a, b, c, daemon));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn hibernating_one_session_releases_only_its_demand_and_never_pins_its_view() {
    let (root, home) = home("scope-hibernate");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let first = daemon.attach_to(&Target::New, || root.clone()).unwrap();
    let second = daemon.attach_to(&Target::New, || root.clone()).unwrap();
    let scoped = ext::Scoped::new(Arc::clone(
        daemon.queried.get_arc::<ext::Lifetimes>().unwrap(),
    ));
    let cx = first.session().process_cx();
    let first_lifetimes = cx.service_arc::<ext::SessionLifetimes>().unwrap();
    let launcher = cx
        .service::<dyn ext::Processes>()
        .unwrap()
        .launcher(&cx)
        .unwrap();
    let launch = fixture(&root);
    let child = rt::block_on(scoped.acquire(
        ext::InstanceKey {
            identity: "fixture",
            scope: ext::Scope::Workspace(root.clone()),
        },
        move |cancel| {
            Box::pin(async move {
                let child = launcher.spawn(cancel, launch).await?;
                readiness(&child).await;
                Ok(child)
            })
        },
    ))
    .unwrap();
    first_lifetimes.retain(&child).unwrap();
    let second_cx = second.session().process_cx();
    second_cx
        .service::<ext::SessionLifetimes>()
        .unwrap()
        .retain(&child)
        .unwrap();
    let control = Arc::clone(&child.process);
    let id = first.session().id;
    let weak = first.session().process_session_weak();
    drop((cx, second_cx, child, first));
    daemon.slot(id).unwrap().unload();
    daemon.wait("first actor ended", PATIENCE, || {
        daemon.slot(id).unwrap().ending().is_none()
    });
    let deadline = Instant::now() + PATIENCE;
    while weak.upgrade().is_some() {
        assert!(
            Instant::now() < deadline,
            "shared process pinned the session projection"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        control.alive(),
        "hibernating one session killed another's helper"
    );
    assert!(!second.session().has_work());
    assert!(
        first_lifetimes
            .retain(
                &rt::block_on(scoped.acquire(
                    ext::InstanceKey {
                        identity: "fixture",
                        scope: ext::Scope::Workspace(root.clone())
                    },
                    |_| panic!("live generation replaced")
                ))
                .unwrap()
            )
            .is_err()
    );
    let id = second.session().id;
    drop(second);
    daemon.slot(id).unwrap().unload();
    rt::block_on(control.ended());
    assert!(sys::process_started(control.id()).is_none());
    assert!(daemon.idle(), "helper kept idle daemon alive");
    drop(daemon);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn last_demand_and_shutdown_reap_the_process_and_its_descendants() {
    let (root, home) = home("scope-descendants");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let session = daemon.attach_to(&Target::New, || root.clone()).unwrap();
    let cx = session.session().process_cx();
    let scoped = ext::Scoped::new(Arc::clone(
        daemon.queried.get_arc::<ext::Lifetimes>().unwrap(),
    ));
    for stage in 0..2 {
        let launcher = cx
            .service::<dyn ext::Processes>()
            .unwrap()
            .launcher(&cx)
            .unwrap();
        let mut launch = fixture(&root);
        launch
            .process
            .env
            .insert("SCOPED_DESCENDANT".into(), "1".into());
        let descendant = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let pid = descendant.clone();
        let child = rt::block_on(scoped.acquire(
            ext::InstanceKey {
                identity: "fixture",
                scope: ext::Scope::User,
            },
            move |cancel| {
                Box::pin(async move {
                    let child = launcher.spawn(cancel, launch).await?;
                    pid.store(readiness(&child).await, std::sync::atomic::Ordering::SeqCst);
                    Ok(child)
                })
            },
        ))
        .unwrap();
        let descendant = descendant.load(std::sync::atomic::Ordering::SeqCst);
        assert!(sys::process_started(descendant).is_some());
        let control = Arc::clone(&child.process);
        if stage == 0 {
            drop(child);
        } else {
            rt::block_on(daemon.shutdown());
            drop(child);
        }
        rt::block_on(control.ended());
        assert!(sys::process_started(control.id()).is_none());
        assert!(
            sys::process_started(descendant).is_none(),
            "descendant survived completed retirement"
        );
    }
    drop((cx, session, daemon));
    std::fs::remove_dir_all(root).unwrap();
}

struct CleanupGate {
    child: ext::OwnedProcess,
    closing: ext::Flag,
    release: ext::Flag,
}

impl ext::ScopedResource for CleanupGate {
    fn ended(&self) -> ext::BoxFuture<'_, ()> {
        self.child.process.ended()
    }
    fn close(&self) -> ext::BoxFuture<'_, ()> {
        Box::pin(async move {
            self.closing.set();
            self.release.wait().await;
            ext::ScopedResource::close(&self.child).await;
        })
    }
}

#[test]
fn a_successor_waits_for_native_cleanup_even_when_old_requests_retain_leases() {
    use std::future::Future;
    use std::task::{Context, Waker};
    let (root, home) = home("scope-successor");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let session = daemon.attach_to(&Target::New, || root.clone()).unwrap();
    let cx = session.session().process_cx();
    let scoped = ext::Scoped::new(Arc::clone(
        daemon.queried.get_arc::<ext::Lifetimes>().unwrap(),
    ));
    let key = ext::InstanceKey {
        identity: "fixture",
        scope: ext::Scope::User,
    };
    let closing = ext::Flag::default();
    let release = ext::Flag::default();
    let launcher = cx
        .service::<dyn ext::Processes>()
        .unwrap()
        .launcher(&cx)
        .unwrap();
    let launch = fixture(&root);
    let child = rt::block_on(scoped.acquire(key.clone(), {
        let closing = closing.clone();
        let release = release.clone();
        move |cancel| {
            Box::pin(async move {
                let child = launcher.spawn(cancel, launch).await?;
                readiness(&child).await;
                Ok(CleanupGate {
                    child,
                    closing,
                    release,
                })
            })
        }
    }))
    .unwrap();
    child.retire();
    rt::block_on(closing.wait());
    let old = child.child.id();
    let launcher = cx
        .service::<dyn ext::Processes>()
        .unwrap()
        .launcher(&cx)
        .unwrap();
    let launch = fixture(&root);
    let mut next = Box::pin(scoped.acquire(key, move |cancel| {
        Box::pin(async move {
            assert!(
                sys::process_started(old).is_none(),
                "successor overlapped native cleanup"
            );
            let child = launcher.spawn(cancel, launch).await?;
            readiness(&child).await;
            let release = ext::Flag::default();
            release.set();
            Ok(CleanupGate {
                child,
                closing: ext::Flag::default(),
                release,
            })
        })
    }));
    assert!(
        next.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(child.child.alive(), "test did not hold native cleanup open");
    release.set();
    let next = rt::block_on(next).unwrap();
    assert_ne!(next.child.id(), old);
    rt::block_on(child.retired());
    rt::block_on(daemon.shutdown());
    drop((child, next, cx, session, daemon));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_crash_retires_its_generation_and_only_explicit_demand_restarts_it() {
    let (root, home) = home("scope-crash");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let session = daemon.attach_to(&Target::New, || root.clone()).unwrap();
    let cx = session.session().process_cx();
    let scoped = ext::Scoped::new(Arc::clone(
        daemon.queried.get_arc::<ext::Lifetimes>().unwrap(),
    ));
    let key = ext::InstanceKey {
        identity: "fixture",
        scope: ext::Scope::User,
    };
    let launcher = cx
        .service::<dyn ext::Processes>()
        .unwrap()
        .launcher(&cx)
        .unwrap();
    let launch = fixture(&root);
    let old = rt::block_on(scoped.acquire(key.clone(), move |cancel| {
        Box::pin(async move {
            let child = launcher.spawn(cancel, launch).await?;
            readiness(&child).await;
            Ok(child)
        })
    }))
    .unwrap();
    old.signal(ext::ProcessSignal::Kill).unwrap();
    rt::block_on(old.retired());
    assert!(old.cancelled().is_set());
    assert!(sys::process_started(old.id()).is_none());
    let launcher = cx
        .service::<dyn ext::Processes>()
        .unwrap()
        .launcher(&cx)
        .unwrap();
    let launch = fixture(&root);
    let next = rt::block_on(scoped.acquire(key, move |cancel| {
        Box::pin(async move {
            let child = launcher.spawn(cancel, launch).await?;
            readiness(&child).await;
            Ok(child)
        })
    }))
    .unwrap();
    assert_ne!(next.id(), old.id());
    rt::block_on(daemon.shutdown());
    drop((old, next, cx, session, daemon));
    std::fs::remove_dir_all(root).unwrap();
}
