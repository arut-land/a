//! A deterministic stdio peer; it records effects separately from replies.
use std::fs::OpenOptions;
use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn send(value: &Value) {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, value).unwrap();
    writeln!(out).unwrap();
    out.flush().unwrap();
}

fn main() {
    let marker = std::env::args().nth(1).expect("marker file");
    if marker == "--descendant" {
        let marker = std::env::args().nth(2).expect("parent marker");
        std::fs::write(
            format!("{marker}.descendant-{}", std::process::id()),
            "ready",
        )
        .unwrap();
        loop {
            std::thread::park();
        }
    }
    let mut children = Vec::new();
    writeln!(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&marker)
            .unwrap(),
        "start {}",
        std::process::id()
    )
    .unwrap();
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let id = &request["id"];
        let method = request["method"].as_str().unwrap();
        if method == "notifications/cancelled" {
            continue;
        }
        assert_eq!(
            request["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
            "2026-07-28"
        );
        assert!(
            request["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"].is_object()
        );
        let result = match method {
            "server/discover" => {
                json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}}})
            }
            "tools/list" => tools(&request, &marker),
            "tools/call" => match call(&request, &marker, &mut children) {
                Some(result) => result,
                None => continue,
            },
            _ => panic!("unexpected method {method}"),
        };
        send(&json!({"jsonrpc":"2.0","id":id,"result":result}));
    }
    for mut child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn tools(request: &Value, marker: &str) -> Value {
    match () {
        () if marker.contains("review-large-list") => {
            let page: usize = request["params"]["cursor"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap();
            let mut result = json!({"resultType":"complete","tools":[{"name":format!("tool-{page}"),"description":"x".repeat(900_000),"inputSchema":{"type":"object"}}]});
            if page < 19 {
                result["nextCursor"] = json!((page + 1).to_string());
            }
            result
        }
        () if marker.contains("bounded-escaped") => {
            json!({"resultType":"complete","tools":[{"name":"echo","description":"é".repeat(2000),"inputSchema":{"type":"object","description":"\u{0000}\n\\".repeat(80_000)}}]})
        }
        () if marker.contains("invalid-name") => {
            json!({"resultType":"complete","tools":[{"name":"bad\nname","inputSchema":{"type":"object"}}]})
        }
        () => {
            json!({"resultType":"complete","tools":[{"name":"echo","description":"Echo arguments","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]})
        }
    }
}

fn tree(marker: &str, children: &mut Vec<std::process::Child>) {
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--descendant", marker])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    children.push(child);
    while !std::path::Path::new(&format!("{marker}.descendant-{pid}")).exists() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    writeln!(
        OpenOptions::new().append(true).open(marker).unwrap(),
        "descendant {pid}"
    )
    .unwrap();
}

fn call(request: &Value, marker: &str, children: &mut Vec<std::process::Child>) -> Option<Value> {
    let id = &request["id"];
    let tool = request["params"]["name"].as_str().unwrap();
    writeln!(
        OpenOptions::new().append(true).open(marker).unwrap(),
        "call {tool}"
    )
    .unwrap();
    let result = match tool {
        "tree" | "tree-wait" => {
            tree(marker, children);
            if tool == "tree-wait" {
                send(
                    &json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":id,"message":"tree ready"}}),
                );
                return None;
            }
            json!({"resultType":"complete","content":[]})
        }
        "progress-pause" => {
            for message in ["initial\n", "latest\n"] {
                send(
                    &json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":id,"message":message}}),
                );
            }
            return None;
        }
        "progress-burst" => {
            for i in 0..100 {
                send(
                    &json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":id,"message":format!("update {i}\n")}}),
                );
            }
            json!({"resultType":"complete","content":[]})
        }
        "flood" => {
            for _ in 0..40 {
                send(
                    &json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":id,"message":"x".repeat(65536)}}),
                );
            }
            json!({"resultType":"complete","content":[]})
        }
        "fragmented" | "adjacent" => {
            framed_reply(request);
            return None;
        }
        "crash" => std::process::exit(7),
        "wait" | "partial-wait" => {
            send(
                &json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":id,"progress":1,"message":"waiting"}}),
            );
            if tool == "partial-wait" {
                let mut out = std::io::stdout().lock();
                let bytes = serde_json::to_vec(
                    &json!({"jsonrpc":"2.0","id":id,"result":{"resultType":"complete"}}),
                )
                .unwrap();
                out.write_all(&bytes[..bytes.len() / 2]).unwrap();
                out.flush().unwrap();
            }
            std::fs::write(format!("{marker}.ready"), "ready").unwrap();
            return None;
        }
        "bad" => {
            println!("invalid json");
            return None;
        }
        "wrong-id" => {
            send(&json!({"jsonrpc":"2.0","id":99999,"result":{"resultType":"complete"}}));
            return None;
        }
        "remote" => {
            send(
                &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"bad arguments"}}),
            );
            return None;
        }
        "input" => json!({"resultType":"input_required","inputRequests":{}}),
        _ => {
            json!({"resultType":"complete","content":[{"type":"text","text":request["params"]["arguments"].to_string()}]})
        }
    };
    Some(result)
}

fn framed_reply(request: &Value) {
    let id = &request["id"];
    let tool = request["params"]["name"].as_str().unwrap();
    let result = json!({"resultType":"complete","content":[{"type":"text","text":request["params"]["arguments"]["text"]}]});
    let mut bytes = Vec::new();
    if tool == "adjacent" {
        for message in ["first\n", "last\n"] {
            serde_json::to_writer(&mut bytes, &json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":id,"message":message}})).unwrap();
            bytes.push(b'\n');
        }
    }
    serde_json::to_writer(
        &mut bytes,
        &json!({"jsonrpc":"2.0","id":id,"result":result}),
    )
    .unwrap();
    bytes.push(b'\n');
    let mut out = std::io::stdout().lock();
    for chunk in bytes.chunks(if tool == "fragmented" {
        137
    } else {
        bytes.len()
    }) {
        out.write_all(chunk).unwrap();
        out.flush().unwrap();
    }
}
