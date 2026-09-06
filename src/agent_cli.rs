use crate::local_client::{LocalAuth, LocalGatewayClient};
use reqwest::Method;
use serde_json::{json, Map, Value};
use std::error::Error;

#[derive(Default)]
struct AgentOptions {
    task: Option<String>,
    capabilities: Vec<String>,
    min_context_window: Option<i64>,
    prompt: Option<String>,
    client_id: Option<String>,
    request_id: Option<String>,
    max_tokens: Option<i64>,
}

pub async fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print_help();
        return Ok(());
    }

    let client = LocalGatewayClient::from_env()?;
    let value = match args[0].as_str() {
        "health" => client.health().await?,
        "models" => client.models().await?,
        "capabilities" => client.capabilities().await?,
        "resolve" => {
            let options = parse_options(&args[1..])?;
            client.resolve(false, &resolve_body(options)).await?
        }
        "diagnostics" => {
            let options = parse_options(&args[1..])?;
            client.resolve(true, &resolve_body(options)).await?
        }
        "responses" => {
            let (model, prompt, options) = parse_execution(args, "responses")?;
            client
                .responses(&execution_body("responses", model, prompt, options))
                .await?
        }
        "chat" => {
            let (model, prompt, options) = parse_execution(args, "chat")?;
            client
                .chat(&execution_body("chat", model, prompt, options))
                .await?
        }
        "messages" => {
            let (model, prompt, options) = parse_execution(args, "messages")?;
            client
                .messages(&execution_body("messages", model, prompt, options))
                .await?
        }
        "admin-models" => {
            client
                .request(
                    Method::GET,
                    "/_llmgateway/models",
                    None,
                    LocalAuth::Admin,
                    false,
                )
                .await?
        }
        "accounts" => {
            client
                .request(
                    Method::GET,
                    "/_llmgateway/accounts",
                    None,
                    LocalAuth::Admin,
                    false,
                )
                .await?
        }
        "groups" => {
            client
                .request(
                    Method::GET,
                    "/_llmgateway/model-groups",
                    None,
                    LocalAuth::Admin,
                    false,
                )
                .await?
        }
        "clients" => {
            client
                .request(
                    Method::GET,
                    "/_llmgateway/clients",
                    None,
                    LocalAuth::Admin,
                    false,
                )
                .await?
        }
        "executions" => {
            let options = parse_options(&args[1..])?;
            let path = options
                .request_id
                .map(|request_id| format!("/_llmgateway/executions/{request_id}"))
                .unwrap_or_else(|| "/_llmgateway/executions".to_string());
            client
                .request(Method::GET, &path, None, LocalAuth::Admin, false)
                .await?
        }
        "explain" => {
            if args.len() < 2 {
                return Err("usage: llmgateway agent explain MODEL [--client-id ID] [--prompt TEXT]"
                    .into());
            }
            let model = args[1].clone();
            let options = parse_options(&args[2..])?;
            let mut body = Map::new();
            body.insert("model".into(), Value::String(model));
            if let Some(client_id) = options.client_id {
                body.insert("client_id".into(), Value::String(client_id));
            }
            if let Some(prompt) = options.prompt {
                body.insert(
                    "body".into(),
                    json!({"messages":[{"role":"user","content":prompt}]}),
                );
            }
            client
                .request(
                    Method::POST,
                    "/_llmgateway/routes/explain",
                    Some(&Value::Object(body)),
                    LocalAuth::Admin,
                    false,
                )
                .await?
        }
        other => {
            return Err(format!(
                "unknown agent command '{other}'. Run 'llmgateway agent --help'."
            )
            .into())
        }
    };

    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn parse_execution(
    args: &[String],
    command: &str,
) -> Result<(String, String, AgentOptions), Box<dyn Error>> {
    if args.len() < 3 {
        return Err(format!(
            "usage: llmgateway agent {command} MODEL PROMPT [--task TASK] [--capability NAME] [--min-context-window N]"
        )
        .into());
    }
    let model = args[1].clone();
    let prompt = args[2].clone();
    let options = parse_options(&args[3..])?;
    Ok((model, prompt, options))
}

fn parse_options(args: &[String]) -> Result<AgentOptions, Box<dyn Error>> {
    let mut options = AgentOptions::default();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        match flag {
            "--task" => {
                options.task = Some(take_value(args, &mut index, flag)?);
            }
            "--capability" => {
                options
                    .capabilities
                    .push(take_value(args, &mut index, flag)?);
            }
            "--min-context-window" => {
                let raw = take_value(args, &mut index, flag)?;
                let value = raw
                    .parse::<i64>()
                    .map_err(|_| "--min-context-window must be an integer")?;
                if value <= 0 {
                    return Err("--min-context-window must be greater than zero".into());
                }
                options.min_context_window = Some(value);
            }
            "--prompt" => {
                options.prompt = Some(take_value(args, &mut index, flag)?);
            }
            "--client-id" => {
                options.client_id = Some(take_value(args, &mut index, flag)?);
            }
            "--request-id" => {
                options.request_id = Some(take_value(args, &mut index, flag)?);
            }
            "--max-tokens" => {
                let raw = take_value(args, &mut index, flag)?;
                let value = raw
                    .parse::<i64>()
                    .map_err(|_| "--max-tokens must be an integer")?;
                if value <= 0 {
                    return Err("--max-tokens must be greater than zero".into());
                }
                options.max_tokens = Some(value);
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            unknown => return Err(format!("unknown option '{unknown}'").into()),
        }
        index += 1;
    }
    Ok(options)
}

fn take_value(
    args: &[String],
    index: &mut usize,
    flag: &str,
) -> Result<String, Box<dyn Error>> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value").into())
}

fn resolve_body(options: AgentOptions) -> Value {
    let mut body = Map::new();
    body.insert(
        "requirements".into(),
        json!({
            "capabilities": options.capabilities,
            "min_context_window": options.min_context_window
        }),
    );
    if let Some(task) = options.task {
        body.insert("task".into(), Value::String(task));
    }
    if let Some(prompt) = options.prompt {
        body.insert(
            "body".into(),
            json!({"messages":[{"role":"user","content":prompt}]}),
        );
    }
    Value::Object(body)
}

fn execution_body(
    protocol: &str,
    model: String,
    prompt: String,
    options: AgentOptions,
) -> Value {
    let mut body = match protocol {
        "responses" => json!({"model":model,"input":prompt}),
        "messages" => json!({
            "model":model,
            "max_tokens":options.max_tokens.unwrap_or(1024),
            "messages":[{"role":"user","content":prompt}]
        }),
        _ => json!({
            "model":model,
            "messages":[{"role":"user","content":prompt}]
        }),
    };

    let object = body
        .as_object_mut()
        .expect("native agent request body must be an object");
    if let Some(task) = options.task {
        object.insert("llmgateway_task".into(), Value::String(task));
    }
    if !options.capabilities.is_empty() || options.min_context_window.is_some() {
        object.insert(
            "llmgateway_requirements".into(),
            json!({
                "capabilities":options.capabilities,
                "min_context_window":options.min_context_window
            }),
        );
    }
    body
}

fn print_help() {
    println!(
        r#"llmgateway agent

READ + EXECUTE:
  llmgateway agent health
  llmgateway agent models
  llmgateway agent capabilities
  llmgateway agent resolve [--model is intentionally omitted; use gateway default] [--task TASK] [--capability NAME]... [--min-context-window N] [--prompt TEXT]
  llmgateway agent diagnostics [--task TASK] [--capability NAME]... [--min-context-window N] [--prompt TEXT]
  llmgateway agent responses MODEL PROMPT [routing options]
  llmgateway agent chat MODEL PROMPT [routing options]
  llmgateway agent messages MODEL PROMPT [--max-tokens N] [routing options]

ADMIN DIAGNOSTICS:
  llmgateway agent admin-models
  llmgateway agent accounts
  llmgateway agent groups
  llmgateway agent clients
  llmgateway agent executions [--request-id ID]
  llmgateway agent explain MODEL [--client-id ID] [--prompt TEXT]

Environment:
  LLMGATEWAY_BASE_URL          default http://127.0.0.1:7331
  LLMGATEWAY_CLIENT_API_KEY    scoped execution key
  LLMGATEWAY_API_KEY           admin/legacy key"#
    );
}

#[cfg(test)]
mod tests {
    use super::{execution_body, parse_options};
    use serde_json::json;

    #[test]
    fn execution_body_carries_routing_requirements() {
        let args = vec![
            "--task".to_string(),
            "coding".to_string(),
            "--capability".to_string(),
            "coding".to_string(),
            "--min-context-window".to_string(),
            "32000".to_string(),
        ];
        let options = parse_options(&args).unwrap();
        let body = execution_body(
            "chat",
            "llmgateway-auto".into(),
            "hello".into(),
            options,
        );
        assert_eq!(body["llmgateway_task"], "coding");
        assert_eq!(body["llmgateway_requirements"]["capabilities"], json!(["coding"]));
        assert_eq!(body["llmgateway_requirements"]["min_context_window"], 32000);
    }
}
