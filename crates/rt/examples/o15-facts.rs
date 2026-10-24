fn main() {
    let facts = sys::resources::Resources::for_path(&std::env::temp_dir()).unwrap();
    let config = rt::Config::for_resources(facts);
    println!(
        "{{\"effective_cpus\":{},\"standard_cpus\":{},\"page_bytes\":{},\"storage\":\"{:?}\",\"async_workers\":{},\"cpu_workers\":{},\"storage_workers\":{}}}",
        facts.parallelism,
        std::thread::available_parallelism().map_or(1, usize::from),
        facts.page_size,
        facts.storage,
        config.workers,
        config.cpu.workers,
        config.storage.workers
    );
}
