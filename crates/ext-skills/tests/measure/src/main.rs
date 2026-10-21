#![allow(dead_code)]
#[cfg(feature = "custom")]
#[path = "../../../src/parse/yaml.rs"]
mod yaml;

#[derive(serde::Deserialize, serde::Serialize)]
struct Header {
    name: String,
    description: String,
}
fn main() {
    for path in std::env::args().skip(1) {
        let source = std::fs::read_to_string(path).unwrap();
        let text = source
            .strip_prefix("---\n")
            .unwrap()
            .rsplit_once("---")
            .unwrap()
            .0;
        #[cfg(feature = "custom")]
        let result = yaml::header(text)
            .map(|h| Header {
                name: h.name,
                description: h.description,
            })
            .map_err(|e| e.to_string());
        #[cfg(feature = "serde")]
        let result = serde_yaml_ng::from_str::<Header>(text).map_err(|e| e.to_string());
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
        match result {
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
