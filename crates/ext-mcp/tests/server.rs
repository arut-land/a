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
            "tools/list" if marker.contains("review-large-list") => {
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
            "tools/list" if marker.contains("bounded-escaped") => {
                json!({"resultType":"complete","tools":[{"name":"echo","description":"é".repeat(2000),"inputSchema":{"type":"object","description":"\u{0000}\n\\".repeat(80_000)}}]})
            }
            "tools/list" if marker.contains("invalid-name") => {
                json!({"resultType":"complete","tools":[{"name":"bad\nname","inputSchema":{"type":"object"}}]})
            }
            "tools/list" => {
                json!({"resultType":"complete","tools":[{"name":"echo","description":"Echo arguments","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]})
            }
            "tools/call" => {
                let tool = request["params"]["name"].as_str().unwrap();
                writeln!(
                    OpenOptions::new().append(true).open(&marker).unwrap(),
                    "call {tool}"
                )
                .unwrap();
                match tool {
                    "progress-pause" => {
                        for message in ["initial\n", "latest\n"] {
                            send(
                                &json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":id,"message":message}}),
                            );
                        }
                        continue;
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
                    "crash" => std::process::exit(7),
                    "wait" => {
                        send(
                            &json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":id,"progress":1,"message":"waiting"}}),
                        );
                        std::fs::write(format!("{marker}.ready"), "ready").unwrap();
                        continue;
                    }
                    "bad" => {
                        println!("invalid json");
                        continue;
                    }
                    "wrong-id" => {
                        send(
                            &json!({"jsonrpc":"2.0","id":99999,"result":{"resultType":"complete"}}),
                        );
                        continue;
                    }
                    "remote" => {
                        send(
                            &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"bad arguments"}}),
                        );
                        continue;
                    }
                    "input" => json!({"resultType":"input_required","inputRequests":{}}),
                    _ => {
                        json!({"resultType":"complete","content":[{"type":"text","text":request["params"]["arguments"].to_string()}]})
                    }
                }
            }
            _ => panic!("unexpected method {method}"),
        };
        send(&json!({"jsonrpc":"2.0","id":id,"result":result}));
    }
}
