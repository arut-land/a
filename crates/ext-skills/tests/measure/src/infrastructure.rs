fn main() {
    #[cfg(all(unix, feature = "signal-maintained"))]
    let pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let operations = if cfg!(any(
        feature = "signal-custom",
        feature = "signal-maintained"
    )) {
        1_000_000
    } else {
        10_000
    };
    let start = std::time::Instant::now();
    for n in 0..operations {
        #[cfg(feature = "temp-custom")]
        {
            let path = std::env::temp_dir().join(format!("a-probe-{}-{n}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            std::hint::black_box(&path);
            std::fs::remove_dir(path).unwrap();
        }
        #[cfg(feature = "temp-maintained")]
        {
            let dir = tempfile::tempdir().unwrap();
            std::hint::black_box(dir.path());
        }
        #[cfg(feature = "signal-custom")]
        {
            let lease = sys::Signals::catch(&[sys::Signal::Resize]).unwrap();
            std::hint::black_box(&lease);
        }
        #[cfg(all(unix, feature = "signal-maintained"))]
        {
            let id = signal_hook::flag::register(
                signal_hook::consts::SIGWINCH,
                std::sync::Arc::clone(&pending),
            )
            .unwrap();
            signal_hook::low_level::unregister(id);
        }
        std::hint::black_box(n);
    }
    println!(
        "{}",
        serde_json::json!({"operations": operations, "ns_per_operation": start.elapsed().as_nanos() / operations})
    );
}
