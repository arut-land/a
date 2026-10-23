#![allow(dead_code)]
#[cfg(feature = "saphyr")]
use saphyr::LoadableYamlNode;
#[cfg(feature = "custom")]
#[path = "baseline.rs"]
mod yaml;

#[cfg(feature = "event")]
#[path = "../../../src/parse/yaml.rs"]
mod yaml;

#[derive(serde::Deserialize, serde::Serialize)]
struct Header {
    name: String,
    description: String,
}
fn decode(text: &str) -> Result<Header, String> {
    #[cfg(any(feature = "custom", feature = "event"))]
    let result = yaml::header(text)
        .map(|h| Header {
            name: h.name,
            description: h.description,
        })
        .map_err(|e| e.to_string());
    #[cfg(feature = "serde")]
    let result = serde_yaml_ng::from_str::<Header>(text).map_err(|e| e.to_string());
    #[cfg(feature = "saphyr")]
    let result = saphyr::Yaml::load_from_str(text)
        .map_err(|e| e.to_string())
        .and_then(|docs| {
            let doc = &docs[0];
            Ok(Header {
                name: doc["name"].as_str().ok_or("name")?.to_owned(),
                description: doc["description"].as_str().ok_or("description")?.to_owned(),
            })
        });
    #[cfg(feature = "yaml")]
    let result = yaml_rust2::YamlLoader::load_from_str(text)
        .map_err(|e| e.to_string())
        .and_then(|docs| {
            let doc = &docs[0];
            Ok(Header {
                name: doc["name"].as_str().ok_or("name")?.to_owned(),
                description: doc["description"].as_str().ok_or("description")?.to_owned(),
            })
        });
    result
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let bench = args.first().is_some_and(|s| s == "--bench");
    let texts: Vec<_> = args
        .iter()
        .skip(usize::from(bench))
        .map(|path| {
            let source = std::fs::read_to_string(path).unwrap();
            source
                .strip_prefix("---\n")
                .unwrap()
                .rsplit_once("---")
                .unwrap()
                .0
                .to_owned()
        })
        .collect();
    if bench {
        let start = std::time::Instant::now();
        for _ in 0..10000 {
            for text in &texts {
                std::hint::black_box(decode(std::hint::black_box(text)).unwrap());
            }
        }
        println!(
            "{}",
            serde_json::json!({"ns_per_header": start.elapsed().as_nanos() / (10000 * texts.len() as u128)})
        );
        return;
    }
    for text in &texts {
        match decode(text) {
            Ok(mut h) => {
                h.description = h
                    .description
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("{}", serde_json::to_string(&h).unwrap());
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }
}
