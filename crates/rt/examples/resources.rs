//! Hosted-only resource-policy and primitive comparison.
use std::{hint::black_box, time::Instant};

#[global_allocator]
static ALLOCATOR: &stats_alloc::StatsAlloc<std::alloc::System> = &stats_alloc::INSTRUMENTED_SYSTEM;

fn measure(name: &str, mut f: impl FnMut()) {
    let region = stats_alloc::Region::new(ALLOCATOR);
    let start = Instant::now();
    for _ in 0..100_000 {
        f();
    }
    println!(
        "{name} ns={} stats={:?}",
        start.elapsed().as_nanos() / 100_000,
        region.change()
    );
}
fn main() {
    println!(
        "facts {:?}",
        sys::resources::Resources::for_path(std::path::Path::new(".")).unwrap()
    );
    measure("custom-oneshot", || {
        let (tx, rx) = rt::oneshot::channel();
        tx.send(black_box(42u64));
        black_box(rt::block_on(rx));
    });
    measure("futures-oneshot", || {
        let (tx, rx) = futures_channel::oneshot::channel();
        tx.send(black_box(42u64)).unwrap();
        black_box(rt::block_on(rx).unwrap());
    });
    measure("oneshot", || {
        let (tx, rx) = oneshot::async_channel();
        tx.send(black_box(42u64)).unwrap();
        black_box(rt::block_on(rx).unwrap());
    });
    measure("custom-unbounded", || {
        let (tx, rx) = rt::channel::unbounded();
        tx.send(black_box(42u64)).unwrap();
        black_box(rx.try_recv());
    });
    measure("async-unbounded", || {
        let (tx, rx) = async_channel::unbounded();
        tx.try_send(black_box(42u64)).unwrap();
        black_box(rx.try_recv().unwrap());
    });
    measure("futures-unbounded", || {
        let (tx, mut rx) = futures_channel::mpsc::unbounded();
        tx.unbounded_send(black_box(42u64)).unwrap();
        black_box(rx.try_recv().unwrap());
    });
    for round in 0..5 {
        for ceiling in if round % 2 == 0 {
            [4, 8, usize::MAX]
        } else {
            [usize::MAX, 8, 4]
        } {
            let mut config = rt::Config::for_cores(32);
            config.cpu.workers = 31.min(ceiling);
            let runtime = rt::Threads::configured(config);
            let start = Instant::now();
            let jobs = (0..64)
                .map(|seed| {
                    rt::spawn(
                        &runtime,
                        rt::unblock_with(&runtime, rt::BlockingClass::Cpu, 1 << 20, move || {
                            let mut value = seed as u64;
                            let mut data = vec![0u64; 131_072];
                            for _ in 0..32 {
                                for slot in &mut data {
                                    value = black_box(
                                        value.wrapping_mul(6364136223846793005).wrapping_add(1),
                                    );
                                    *slot = value;
                                }
                            }
                            black_box(data);
                        }),
                    )
                })
                .collect::<Vec<_>>();
            for job in jobs {
                rt::block_on(job);
            }
            println!(
                "cpu ceiling={ceiling} round={round} elapsed_us={}",
                start.elapsed().as_micros()
            );
            runtime.shutdown();
        }
    }
}
