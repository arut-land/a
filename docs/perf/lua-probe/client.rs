#![cfg(unix)]
#[path = "blackbox/common/mod.rs"]
mod common;
use common::Env;
use protocol::{Body, Target};
use std::time::Instant;

#[test]
#[ignore]
fn measure() {
    let env = Env::new("lua-probe");
    if std::env::var_os("LUA_PROBE_LOADED").is_some() {
        std::fs::create_dir_all(env.home.root().join("ext")).unwrap();
        std::fs::write(env.home.root().join("ext/probe.lua"), "function handler(input) return 'null' end\nfunction tool(input) return input end\n").unwrap();
    }
    env.start();
    assert!(env.cmd(&["daemon", "stop"]).status().unwrap().success());
    let start = Instant::now();
    env.start();
    let cold = start.elapsed();
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    let idle = std::process::Command::new("ps").args(["-o", "rss=", "-p", &env.pid().unwrap().to_string()]).output().unwrap();
    eprintln!("idle_rss_kib={}", String::from_utf8(idle.stdout).unwrap().trim());
    let mut handlers = Vec::new();
    let mut tools = Vec::new();
    for _ in 0..200 {
        let start = Instant::now();
        conn.input("hello");
        conn.until(|e| matches!(e.body, Body::Assistant { .. }));
        handlers.push(start.elapsed().as_nanos());
        if std::env::var_os("LUA_PROBE_LOADED").is_some() {
            let start = Instant::now();
            conn.input("/probe hello");
            conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
            tools.push(start.elapsed().as_nanos());
        }
    }
    handlers.sort(); tools.sort();
    let rss = || {
        let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &env.pid().unwrap().to_string()]).output().unwrap();
        String::from_utf8(out.stdout).unwrap().trim().parse::<i64>().unwrap()
    };
    let before = rss();
    for _ in 0..30 { let mut conn = env.connect(); conn.attach(Target::New, &env.ws); }
    eprintln!("cold_us={} handler_p50_ns={} tool_p50_ns={} rss_kib={} session_kib={}", cold.as_micros(), handlers[100], tools.get(100).copied().unwrap_or(0), before, (rss() - before) / 30);
}
