use super::*;

#[test]
fn a_restart_keeps_new_records_separate_from_a_torn_tail() {
    let dir = std::env::temp_dir().join(format!("a-review-log-tail-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("daemon.jsonl");
    fs::write(&path, b"{\"unix_ms\":123,\"fact\":").unwrap();
    let mut writer = Rotating {
        path: path.clone(),
        max: MAX,
        keep: KEEP,
        repair: None,
    };
    write_record(
        &mut |line| writer.write(line),
        Record::new(Fact::RegistryRejected),
    )
    .unwrap();
    let text = fs::read_to_string(&path).unwrap();
    fs::remove_dir_all(dir).unwrap();
    assert!(
        text.lines()
            .any(|line| serde_json::from_str::<serde_json::Value>(line)
                .is_ok_and(|record| record["fact"] == "registry_rejected")),
        "the first post-restart record is joined to the torn tail: {text}"
    );
}

#[test]
fn a_stalled_writer_cannot_hold_a_producer_or_grow_the_queue() {
    let logger = Logger::new();
    // This is the consumer stopped at the worst possible instruction.
    let held = logger.0.records.lock().unwrap();
    for _ in 0..10_000 {
        logger.emit(Fact::RegistryRejected);
    }
    assert_eq!(logger.0.lost.load(Ordering::Relaxed), 10_000);
    drop(held);
    for _ in 0..10_000 {
        logger.emit(Fact::RegistryRejected);
    }
    assert_eq!(logger.0.records.lock().unwrap().len(), CAPACITY);
    assert_eq!(
        logger.0.lost.load(Ordering::Relaxed),
        20_000 - CAPACITY as u64
    );
}

#[test]
fn rotation_bounds_every_file_even_when_records_arrive_in_a_burst() {
    let dir = std::env::temp_dir().join(format!("a-typed-log-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let mut writer = Rotating {
        path: dir.join("daemon.log"),
        max: 256,
        keep: 2,
        repair: None,
    };
    for _ in 0..1000 {
        let mut line = json::Json::encode(&Record::new(Fact::RegistryRejected))
            .unwrap()
            .raw()
            .as_bytes()
            .to_vec();
        line.push(b'\n');
        writer.write(&line).unwrap();
    }
    let files: Vec<_> = fs::read_dir(&dir).unwrap().map(Result::unwrap).collect();
    assert_eq!(files.len(), 3);
    for file in files {
        assert!(file.metadata().unwrap().len() <= 256);
        let text = fs::read_to_string(file.path()).unwrap();
        assert!(text.ends_with('\n'));
        for line in text.lines() {
            assert!(serde_json::from_str::<serde::de::IgnoredAny>(line).is_ok());
        }
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn error_content_cannot_change_the_typed_record() {
    let expected = json::Json::encode(&Fact::Fault {
        session: None,
        operation: Operation::Commit,
        kind: Fault(io::ErrorKind::Other),
    })
    .unwrap();
    // Deterministic property corpus, including control characters and JSON
    // delimiters. Only the kind crosses the logging API, never Display.
    let mut state = 17u32;
    for len in 0..512 {
        let input: String = (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                char::from_u32(state % 0x0011_0000).unwrap_or('\0')
            })
            .collect();
        let error = io::Error::other(input);
        let logged = json::Json::encode(&Fact::Fault {
            session: None,
            operation: Operation::Commit,
            kind: Fault(error.kind()),
        })
        .unwrap();
        assert_eq!(logged, expected);
    }
}

#[test]
fn failed_writes_leave_a_count_when_the_writer_recovers() {
    let rt: rt::Rt = Arc::new(rt::Threads::new(1));
    let logger = Logger::new();
    let kept = Arc::new(Mutex::new(Vec::new()));
    let output = Arc::clone(&kept);
    let (entered, waiting) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let mut fail = true;
    logger.emit(Fact::RegistryRejected);
    logger.run(&rt, move |line| {
        if std::mem::take(&mut fail) {
            entered.send(()).unwrap();
            released.recv().unwrap();
            return Err(io::ErrorKind::StorageFull.into());
        }
        output.lock().unwrap().extend_from_slice(line);
        Ok(())
    });
    waiting.recv().unwrap();
    logger.emit(Fact::RegistryRejected);
    release.send(()).unwrap();
    rt::block_on(logger.flush());
    logger.0.closed.store(true, Ordering::Release);
    logger.0.ready.notify();
    let bytes = kept.lock().unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("\"writes\":1"), "{text}");
}

#[test]
fn a_client_witness_keeps_the_observed_identity_and_is_consumed_once() {
    let dir = std::env::temp_dir().join(format!("a-log-witness-{}", std::process::id()));
    let home = sys::Home::from(dir.clone());
    fs::create_dir_all(home.run_dir()).unwrap();
    let daemon = sys::crash::Record::current("0.0.0+0000000000000001");
    witness(&home, &daemon, Observation::Unresponsive);
    let observed = take_witness(&home).unwrap();
    assert_eq!(
        (observed.pid, observed.started),
        (daemon.pid, daemon.started)
    );
    assert!(matches!(observed.reason, Observation::Unresponsive));
    assert!(take_witness(&home).is_none());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn startup_facts_survive_until_a_writer_is_installed() {
    emit(Fact::ConfigFallback {
        kind: Fault(io::ErrorKind::InvalidData),
    });
    let logger = CURRENT.get().unwrap();
    let kept = Arc::new(Mutex::new(Vec::new()));
    let output = Arc::clone(&kept);
    let rt: rt::Rt = Arc::new(rt::Threads::new(1));
    logger.run(&rt, move |line| {
        output.lock().unwrap().extend_from_slice(line);
        Ok(())
    });
    rt::block_on(logger.flush());
    logger.0.closed.store(true, Ordering::Release);
    logger.0.ready.notify();
    let bytes = kept.lock().unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("config_fallback"), "{text}");
}

#[test]
fn write_recovery_reports_loss_without_needing_another_batch() {
    let rt: rt::Rt = Arc::new(rt::Threads::new(1));
    let logger = Logger::new();
    let kept = Arc::new(Mutex::new(Vec::new()));
    let output = Arc::clone(&kept);
    logger.emit(Fact::RegistryRejected);
    logger.emit(Fact::RegistryRejected);
    let mut fail = true;
    logger.run(&rt, move |line| {
        if std::mem::take(&mut fail) {
            return Err(io::ErrorKind::StorageFull.into());
        }
        output.lock().unwrap().extend_from_slice(line);
        Ok(())
    });
    rt::block_on(logger.flush());
    logger.0.closed.store(true, Ordering::Release);
    logger.0.ready.notify();
    let bytes = kept.lock().unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("registry_rejected"), "the disk recovered");
    assert!(text.contains("\"writes\":1"), "missing loss count: {text}");
}

#[test]
fn a_consumer_parked_during_wake_registration_cannot_hold_admission() {
    use std::future::Future;
    use std::pin::pin;
    use std::sync::mpsc;
    use std::task::{Context, Wake, Waker};

    struct ParkOnDrop {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl Wake for ParkOnDrop {
        fn wake(self: Arc<Self>) {}
    }
    impl Drop for ParkOnDrop {
        fn drop(&mut self) {
            self.entered.send(()).unwrap();
            self.release.get_mut().unwrap().recv().unwrap();
        }
    }
    let logger = Logger::new();
    let (entered, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    {
        let waker = Waker::from(Arc::new(ParkOnDrop {
            entered,
            release: Mutex::new(released),
        }));
        assert!(
            pin!(logger.0.ready.wait())
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
    }
    let consumer = logger.clone();
    let consumer = std::thread::spawn(move || {
        let _ = pin!(consumer.0.ready.wait())
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
    });
    waiting.recv().unwrap();
    // The old waker's destructor now parks the consumer inside registration.
    let producer = logger.clone();
    let (done, completed) = mpsc::channel();
    let producer = std::thread::spawn(move || {
        producer.emit(Fact::RegistryRejected);
        done.send(()).unwrap();
    });
    let admitted = completed.recv_timeout(Duration::from_secs(2));
    release.send(()).unwrap();
    consumer.join().unwrap();
    producer.join().unwrap();
    admitted.expect("admission must finish before the consumer is released");
    assert_eq!(logger.0.records.lock().unwrap().len(), 1);
}
