//! MCP schema decoding is confined to this boundary; clients render typed fields.

use std::collections::{BTreeMap, BTreeSet};

use protocol::{
    Choice, Field, FieldKind, Json, Prompt, Question, QuestionAnswer, Response, Selection,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{CallInput, Failure, Mcp, ResultFact, ServerName, Transport};

#[derive(Serialize, Deserialize)]
pub(crate) struct Continuation {
    identity: [u8; 32],
    state: Option<String>,
    headers: BTreeMap<String, String>,
}

impl Mcp {
    fn binding(&self, name: &ServerName, cx: &ext::Cx) -> Result<[u8; 32], Failure> {
        let server = self
            .0
            .servers
            .iter()
            .find(|s| &s.name == name)
            .ok_or_else(|| Failure::Configuration("MCP server removed".into()))?;
        let transport = match &server.transport {
            Transport::Stdio(argv) => json!({"stdio":argv}),
            Transport::Http(url) => json!({"http":url}),
        };
        let index = self
            .0
            .servers
            .iter()
            .position(|s| &s.name == name)
            .expect("configured server");
        let scope = self.0.scopes[index];
        let cwd = match scope {
            crate::config::Scope::Session => cx.dir(),
            crate::config::Scope::Workspace => cx.workspace.to_path_buf(),
            crate::config::Scope::User => self.0.directory.clone().ok_or_else(|| {
                Failure::Configuration("shared declaration has no directory".into())
            })?,
        };
        let inherited = match &cx.env {
            Some(env) if !scope.shared() => &env.vars,
            _ => self.0.environment.get_or_init(crate::stdio::snapshot),
        };
        let binding = json!({"name":server.name,"description":server.description,"transport":transport,"env":server.env,"scope":match scope {crate::config::Scope::Session=>"session",crate::config::Scope::Workspace=>"workspace",crate::config::Scope::User=>"user"},"directory":self.0.directory,"registration":self.0.registrations.get(name).map(|r|(&r.id,&r.issuer)),"cwd":cwd,"environment":inherited});
        Ok(Sha256::digest(binding.to_string().as_bytes()).into())
    }

    pub(crate) async fn continue_call(
        &self,
        input: &CallInput,
        continuation: Continuation,
        answer: QuestionAnswer,
        cx: &ext::Cx,
    ) -> Result<ext::QuestionStep<ResultFact, Continuation>, String> {
        if self
            .binding(&input.server, cx)
            .map_err(|e| format!("{e:?}"))?
            != continuation.identity
        {
            return Ok(ext::QuestionStep::Done(ResultFact::Failed(
                Failure::Configuration("MCP server identity changed while awaiting input".into()),
            )));
        }
        if answer.responses.values().any(|r| {
            matches!(
                r,
                Response::Continue(
                    protocol::ContinueAnswer::Decline | protocol::ContinueAnswer::Cancel
                )
            )
        }) {
            return Ok(ext::QuestionStep::Done(ResultFact::Failed(
                Failure::Cancelled,
            )));
        }
        let responses: BTreeMap<_, _> = answer
            .responses
            .into_iter()
            .filter(|(_, response)| !matches!(response, Response::Continue(_)))
            .map(|(key, response)| {
                let value = match response {
                    Response::Submit(values) => {
                        let content: BTreeMap<_, _> = values
                            .into_iter()
                            .map(|(k, v)| v.parse::<Value>().map(|v| (k, v)))
                            .collect::<Result<_, _>>()
                            .map_err(|e| e.to_string())?;
                        json!({"action":"accept","content":content})
                    }
                    Response::Continue(_) => {
                        unreachable!("state-only control is not a server input")
                    }
                    Response::Accept(_) => json!({"action":"accept"}),
                    Response::Decline => json!({"action":"decline"}),
                    Response::Cancel => json!({"action":"cancel"}),
                };
                Ok((key, value))
            })
            .collect::<Result<_, String>>()?;
        let mut params =
            json!({"name":input.tool,"arguments":input.arguments,"inputResponses":responses});
        if let Some(state) = &continuation.state {
            params["requestState"] = json!(state);
        }
        self.round(input, params, continuation.headers, cx).await
    }

    pub(crate) async fn round(
        &self,
        input: &CallInput,
        params: Value,
        headers: BTreeMap<String, String>,
        cx: &ext::Cx,
    ) -> Result<ext::QuestionStep<ResultFact, Continuation>, String> {
        let mut forwarded = http::HeaderMap::new();
        for (name, value) in &headers {
            forwarded.insert(
                http::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| e.to_string())?,
                value
                    .parse()
                    .map_err(|e: http::header::InvalidHeaderValue| e.to_string())?,
            );
        }
        let reply = match self
            .request_headers(&input.server, "tools/call", params, forwarded, cx)
            .await
        {
            Ok(reply) => reply,
            Err(error) => return Ok(ext::QuestionStep::Done(ResultFact::Failed(error))),
        };
        if reply["resultType"] != "input_required" {
            return Ok(ext::QuestionStep::Done(ResultFact::Ok(reply)));
        }
        let (question, state) = match decode(&input.server, &reply) {
            Ok(question) => question,
            Err(error) => return Ok(ext::QuestionStep::Done(ResultFact::Failed(error))),
        };
        Ok(ext::QuestionStep::Ask {
            question,
            state: Continuation {
                identity: self
                    .binding(&input.server, cx)
                    .map_err(|e| format!("{e:?}"))?,
                state,
                headers,
            },
        })
    }
}

fn invalid(message: impl Into<String>) -> Failure {
    Failure::Protocol(message.into())
}

fn decode(server: &ServerName, value: &Value) -> Result<(Question, Option<String>), Failure> {
    let state = value
        .get("requestState")
        .map(|s| {
            s.as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid("requestState must be a string"))
        })
        .transpose()?;
    let mut prompts = BTreeMap::new();
    if let Some(requests) = value.get("inputRequests") {
        for (key, request) in requests
            .as_object()
            .ok_or_else(|| invalid("inputRequests must be an object"))?
        {
            if request["method"] != "elicitation/create" {
                return Err(invalid("unsupported MCP input method"));
            }
            let params = &request["params"];
            let message = params["message"]
                .as_str()
                .ok_or_else(|| invalid("elicitation has no message"))?
                .to_owned();
            let prompt = match text(params, "mode")?.as_deref().unwrap_or("form") {
                "form" => Prompt::Form {
                    message,
                    fields: fields(&params["requestedSchema"])?,
                },
                "url" => {
                    let url = params["url"]
                        .as_str()
                        .ok_or_else(|| invalid("elicitation has no URL"))?;
                    let uri: http::Uri = url
                        .parse()
                        .map_err(|_| invalid("invalid elicitation URL"))?;
                    if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none() {
                        return Err(invalid("elicitation URL must use HTTP or HTTPS"));
                    }
                    Prompt::Url {
                        message,
                        url: url.into(),
                    }
                }
                _ => return Err(invalid("unknown elicitation mode")),
            };
            prompts.insert(key.clone(), prompt);
        }
    }
    if prompts.is_empty() {
        if state.is_none() {
            return Err(invalid("input_required has neither requests nor state"));
        }
        prompts.insert(
            "$continue".into(),
            Prompt::Continue {
                message: "The server needs another round. Continue when ready.".into(),
            },
        );
    }
    Ok((
        Question {
            source: server.to_string(),
            prompts,
        },
        state,
    ))
}

fn fields(schema: &Value) -> Result<Vec<Field>, Failure> {
    keys(
        schema,
        &[
            "type",
            "properties",
            "required",
            "title",
            "description",
            "$schema",
        ],
    )?;
    text(schema, "$schema")?;
    if schema["type"] != "object" {
        return Err(invalid("elicitation schema must be an object"));
    }
    let required = schema
        .get("required")
        .map(|v| {
            serde_json::from_value::<BTreeSet<String>>(v.clone())
                .map_err(|_| invalid("invalid required fields"))
        })
        .transpose()?
        .unwrap_or_default();
    let properties = schema["properties"]
        .as_object()
        .ok_or_else(|| invalid("elicitation schema has no properties"))?;
    if required.iter().any(|name| !properties.contains_key(name)) {
        return Err(invalid("required field is not declared"));
    }
    properties
        .iter()
        .map(|(name, schema)| field(name, schema, required.contains(name)))
        .collect()
}

fn field(name: &str, schema: &Value, required: bool) -> Result<Field, Failure> {
    let allowed: &[&str] = match schema["type"].as_str() {
        Some("string") if schema.get("enum").is_some() || schema.get("oneOf").is_some() => &[
            "type",
            "title",
            "description",
            "default",
            "enum",
            "enumNames",
            "oneOf",
        ],
        Some("string") => &[
            "type",
            "title",
            "description",
            "default",
            "minLength",
            "maxLength",
            "format",
        ],
        Some("number" | "integer") => &[
            "type",
            "title",
            "description",
            "default",
            "minimum",
            "maximum",
        ],
        Some("boolean") => &["type", "title", "description", "default"],
        Some("array") => &[
            "type",
            "title",
            "description",
            "default",
            "minItems",
            "maxItems",
            "items",
        ],
        _ => return Err(invalid("unsupported elicitation field type")),
    };
    keys(schema, allowed)?;
    for (min, max) in [("minLength", "maxLength"), ("minItems", "maxItems")] {
        if let (Some(min), Some(max)) = (unsigned(schema, min)?, unsigned(schema, max)?)
            && min > max
        {
            return Err(invalid("field minimum exceeds maximum"));
        }
    }
    if let (Some(min), Some(max)) = (number(schema, "minimum")?, number(schema, "maximum")?)
        && min.as_f64() > max.as_f64()
    {
        return Err(invalid("field minimum exceeds maximum"));
    }
    let kind = kind(schema)?;
    let field = Field {
        name: name.into(),
        title: text(schema, "title")?,
        description: text(schema, "description")?,
        required,
        default: schema
            .get("default")
            .map(Json::encode)
            .transpose()
            .map_err(|e| invalid(e.to_string()))?,
        kind,
    };
    if let Some(default) = &field.default {
        field.validate(default).map_err(invalid)?;
    }
    Ok(field)
}
fn kind(schema: &Value) -> Result<FieldKind, Failure> {
    Ok(match schema["type"].as_str() {
        Some("string") if schema.get("enum").is_some() || schema.get("oneOf").is_some() => {
            FieldKind::Choice {
                options: choices(schema, "oneOf")?,
                selection: Selection::One,
            }
        }
        Some("string") => {
            let format = text(schema, "format")?;
            if format
                .as_ref()
                .is_some_and(|f| !matches!(f.as_str(), "email" | "uri" | "date" | "date-time"))
            {
                return Err(invalid("unsupported string format"));
            }
            FieldKind::String {
                min: unsigned(schema, "minLength")?,
                max: unsigned(schema, "maxLength")?,
                format,
            }
        }
        Some("number" | "integer") => FieldKind::Number {
            integer: schema["type"] == "integer",
            min: number(schema, "minimum")?,
            max: number(schema, "maximum")?,
        },
        Some("boolean") => FieldKind::Boolean,
        Some("array") => {
            keys(&schema["items"], &["type", "enum", "anyOf"])?;
            FieldKind::Choice {
                options: choices(&schema["items"], "anyOf")?,
                selection: Selection::Many {
                    min: unsigned(schema, "minItems")?,
                    max: unsigned(schema, "maxItems")?,
                },
            }
        }
        _ => return Err(invalid("unsupported elicitation field type")),
    })
}

fn keys(value: &Value, allowed: &[&str]) -> Result<(), Failure> {
    if value
        .as_object()
        .is_none_or(|v| v.keys().any(|k| !allowed.contains(&k.as_str())))
    {
        return Err(invalid("unsupported elicitation schema keyword"));
    }
    Ok(())
}
fn text(value: &Value, key: &str) -> Result<Option<String>, Failure> {
    value
        .get(key)
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid(format!("{key} must be a string")))
        })
        .transpose()
}
fn unsigned(value: &Value, key: &str) -> Result<Option<u64>, Failure> {
    value
        .get(key)
        .map(|v| {
            v.as_u64()
                .ok_or_else(|| invalid(format!("{key} must be unsigned")))
        })
        .transpose()
}
fn number(value: &Value, key: &str) -> Result<Option<serde_json::Number>, Failure> {
    value
        .get(key)
        .map(|v| {
            v.as_number()
                .cloned()
                .ok_or_else(|| invalid(format!("{key} must be a number")))
        })
        .transpose()
}
fn choices(schema: &Value, titled: &str) -> Result<Vec<Choice>, Failure> {
    if schema.get("enum").is_some() && schema.get(titled).is_some() {
        return Err(invalid("choice has two definitions"));
    }
    if schema.get("type").is_some_and(|kind| kind != "string") {
        return Err(invalid("choice values must be strings"));
    }
    let labels = schema
        .get("enumNames")
        .map(|v| {
            serde_json::from_value::<Vec<String>>(v.clone())
                .map_err(|_| invalid("enumNames must contain strings"))
        })
        .transpose()?;
    if labels.as_ref().is_some_and(|t| {
        schema
            .get("enum")
            .and_then(Value::as_array)
            .is_none_or(|e| e.len() != t.len())
    }) {
        return Err(invalid("enumNames must match enum values"));
    }
    let choices = if let Some(values) = schema.get("enum") {
        values
            .as_array()
            .ok_or_else(|| invalid("enum must be an array"))?
            .iter()
            .enumerate()
            .map(|(index, v)| {
                v.as_str()
                    .map(|s| Choice {
                        value: s.into(),
                        title: labels
                            .as_ref()
                            .map_or_else(|| s.into(), |titles| titles[index].clone()),
                    })
                    .ok_or_else(|| invalid("enum value must be a string"))
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        schema[titled]
            .as_array()
            .ok_or_else(|| invalid("choice must have options"))?
            .iter()
            .map(|v| {
                keys(v, &["const", "title"])?;
                Ok(Choice {
                    value: text(v, "const")?.ok_or_else(|| invalid("choice has no value"))?,
                    title: text(v, "title")?.ok_or_else(|| invalid("choice has no title"))?,
                })
            })
            .collect::<Result<Vec<_>, Failure>>()?
    };
    if choices.is_empty()
        || choices
            .iter()
            .enumerate()
            .any(|(i, c)| choices[..i].iter().any(|old| old.value == c.value))
    {
        return Err(invalid("choices must be nonempty and distinct"));
    }
    Ok(choices)
}
