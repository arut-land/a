//! A fixed workload for comparing explicit async-worker configurations.
use rt::{BlockingClass, Config, Threads, block_on, spawn, unblock_with};
use std::time::Instant;

fn main() {
    for workers in [2, 4, 32] {
        let rt = Threads::configured(Config {
            workers,
            ..Config::for_cores(workers)
        });
        let began = Instant::now();
        let tasks: Vec<_> = (0..2048)
            .map(|_| {
                spawn(&rt, async {
                    for _ in 0..100 {
                        futures_lite::future::yield_now().await;
                    }
                })
            })
            .collect();
        for task in tasks {
            block_on(task);
        }
        let async_us = began.elapsed().as_micros();
        let began = Instant::now();
        let jobs: Vec<_> = (0..256_u64)
            .map(|seed| {
                let work = unblock_with(&rt, BlockingClass::Cpu, 4096, move || {
                    let mut n = seed;
                    for _ in 0..100_000 {
                        n = std::hint::black_box(
                            n.wrapping_mul(6364136223846793005).wrapping_add(1),
                        );
                    }
                    n
                });
                spawn(&rt, work)
            })
            .collect();
        for job in jobs {
            std::hint::black_box(block_on(job));
        }
        println!(
            "workers={workers} async_us={async_us} cpu_us={}",
            began.elapsed().as_micros()
        );
        rt.shutdown();
    }
}
