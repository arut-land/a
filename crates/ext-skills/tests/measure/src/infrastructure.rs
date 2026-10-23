fn main() {
    let start = std::time::Instant::now();
    for n in 0..10000 {
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
            // SAFETY: the action is empty and cannot allocate, lock or panic.
            let id =
                unsafe { signal_hook::low_level::register(signal_hook::consts::SIGWINCH, || {}) }
                    .unwrap();
            signal_hook::low_level::unregister(id);
        }
        std::hint::black_box(n);
    }
    println!(
        "{}",
        serde_json::json!({"ns_per_operation": start.elapsed().as_nanos() / 10000})
    );
}
