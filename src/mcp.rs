#[cfg(feature = "distributed-transport")]
use crate::distributed_cli::mcp::{ConfiguredController, Controllers};
use horde_rs::scenario::Scenario;
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams,
        ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, ServerCapabilities,
        ServerConfig, Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, env::VarError, error::Error};

const VALIDATE: &str = "validate_scenario";
const SUMMARIZE: &str = "summarize_report";
const REFERENCES: &[(&str, &str, &str)] = &[
    (
        "horde://reference/schemas/scenario-v1.schema.json",
        "scenario-v1.schema.json",
        include_str!("../schemas/scenario-v1.schema.json"),
    ),
    (
        "horde://reference/examples/journey.json",
        "journey.json",
        include_str!("../examples/journey.json"),
    ),
    (
        "horde://reference/examples/login.json",
        "login.json",
        include_str!("../examples/login.json"),
    ),
    (
        "horde://reference/examples/federated-login.json",
        "federated-login.json",
        include_str!("../examples/federated-login.json"),
    ),
];

#[derive(clap::Args)]
pub(crate) struct McpArgs {
    /// Expose this local controller configuration as a named inventory. Repeat for more inventories.
    #[cfg(feature = "distributed-transport")]
    #[arg(long, value_name = "NAME=CONFIG_PATH")]
    controller: Vec<ConfiguredController>,
}

pub(crate) async fn serve(args: McpArgs) -> Result<(), Box<dyn Error + Send + Sync>> {
    #[cfg(not(feature = "distributed-transport"))]
    let McpArgs {} = args;
    let server = Horde {
        #[cfg(feature = "distributed-transport")]
        controllers: Controllers::new(args.controller)?,
    };
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

struct Horde {
    #[cfg(feature = "distributed-transport")]
    controllers: Controllers,
}

impl ServerHandler for Horde {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_resources()
                .enable_tools()
                .build(),
        )
        .with_server_info(Implementation::new("horde", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "Use the scenario schema and examples in resources to draft a Horde JSON scenario. \
             Validate the draft offline with validate_scenario. Use dummy environment values for \
             validation and keep real credentials in the environment of the separately launched CLI.",
        )
    }

    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let resources = REFERENCES
            .iter()
            .map(|&(uri, name, _)| Resource::new(uri, name).with_mime_type("application/json"))
            .collect();
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let &(uri, _, text) = REFERENCES
            .iter()
            .find(|&&(uri, _, _)| uri == request.uri)
            .ok_or_else(|| ErrorData::resource_not_found("Unknown Horde resource", None))?;
        let content = ResourceContents::text(text, uri).with_mime_type("application/json");
        Ok(ReadResourceResult::new(vec![content]).into())
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = vec![
            read_only_tool(
                VALIDATE,
                "Validate a Horde v1 scenario offline using the CLI scenario validator.",
                json!({
                    "type": "object",
                    "required": ["scenario_json"],
                    "additionalProperties": false,
                    "properties": {
                        "scenario_json": {"type": "string", "description": "Complete JSON scenario document."},
                        "sample_env": {
                            "type": "object",
                            "description": "Dummy values for referenced environment variable names. The process environment is never used.",
                            "additionalProperties": {"type": "string"}
                        }
                    }
                }),
            ),
            read_only_tool(
                SUMMARIZE,
                "Summarize a version 1 Horde JSON report with failure rate, throughput, latency and threshold results.",
                json!({
                    "type": "object",
                    "required": ["report"],
                    "additionalProperties": false,
                    "properties": {
                        "report": {"type": "object", "description": "Parsed output from a Horde run with --json."}
                    }
                }),
            ),
        ];
        #[cfg(feature = "distributed-transport")]
        let tools = tools.into_iter().chain(self.controllers.tools()).collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let result = match request.name.as_ref() {
            VALIDATE => serde_json::from_value(arguments)
                .map_err(|_| "Expected scenario_json and optional string-valued sample_env.")
                .map(validate_scenario),
            SUMMARIZE => serde_json::from_value::<ReportArguments>(arguments)
                .map_err(|_| "Expected a report object.")
                .and_then(|arguments| summarize_report(&arguments.report))
                .map(CallToolResult::structured),
            name => {
                #[cfg(feature = "distributed-transport")]
                if let Some(result) = self.controllers.call(name, arguments).await {
                    return Ok(match result {
                        Ok(value) => CallToolResult::structured(value),
                        Err(error) => {
                            CallToolResult::error(vec![ContentBlock::text(error.to_string())])
                        }
                    }
                    .into());
                }
                return Err(ErrorData::invalid_params(
                    format!("Unknown Horde tool {name}"),
                    None,
                ));
            }
        };
        Ok(result
            .unwrap_or_else(|message| CallToolResult::error(vec![ContentBlock::text(message)]))
            .into())
    }
}

fn read_only_tool(name: &'static str, description: &'static str, input_schema: Value) -> Tool {
    Tool::new(
        name,
        description,
        input_schema
            .as_object()
            .expect("tool schema object")
            .clone(),
    )
    .with_annotations(ToolAnnotations::new().read_only(true).open_world(false))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidationArguments {
    scenario_json: String,
    #[serde(default)]
    sample_env: BTreeMap<String, String>,
}

fn validate_scenario(arguments: ValidationArguments) -> CallToolResult {
    let mut missing_sample = None;
    let validation = Scenario::from_json_with_env(arguments.scenario_json.as_bytes(), |name| {
        arguments.sample_env.get(name).cloned().ok_or_else(|| {
            missing_sample = Some(name.to_owned());
            VarError::NotPresent
        })
    });
    match validation {
        Ok(_) => CallToolResult::structured(json!({
            "valid": true,
            "validation": "offline",
        })),
        Err(error) => {
            let diagnostic = match missing_sample {
                Some(name) => format!("Provide a dummy value for {name} in sample_env."),
                None => error.to_string(),
            };
            CallToolResult::structured_error(json!({
                "valid": false,
                "validation": "offline",
                "diagnostic": diagnostic,
            }))
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportArguments {
    report: Value,
}

fn summarize_report(report: &Value) -> Result<Value, &'static str> {
    if report["schema_version"].as_u64() != Some(1) {
        return Err("Expected a Horde report with schema_version 1.");
    }
    let kind = report.get("type").and_then(Value::as_str);
    if kind == Some("interrupted") {
        return Ok(json!({
            "type": "interrupted",
            "partial": true,
            "reason": report["reason"],
            "thresholds": report["thresholds"],
        }));
    }
    let (kind, totals, rate, latency) = match (report.get("type"), kind) {
        (None, _) => (
            "requests",
            report,
            &report["requests_per_second"],
            &report["latency_ms"],
        ),
        (_, Some(kind @ ("scenario" | "distributed_scenario"))) => {
            let scenarios = &report["scenarios"];
            (
                kind,
                scenarios,
                &scenarios["completed_per_second"],
                &scenarios["duration_ms"],
            )
        }
        (_, Some("distributed")) => (
            "distributed",
            &report["metrics"],
            &report["requests_per_second"],
            &report["latency_ms"],
        ),
        _ => return Err("Expected a request, scenario, distributed or interrupted run report."),
    };
    let completed = totals["completed"].as_u64();
    let failed = totals["failed"].as_u64();
    let (Some(completed), Some(failed)) = (completed, failed) else {
        return Err("The report must include integer completed and failed counts.");
    };
    if failed > completed {
        return Err("The failed count cannot exceed the completed count.");
    }
    let mut summary = json!({
        "type": kind,
        "partial": report["partial"].as_bool().unwrap_or(false),
        "completed": completed,
        "failed": failed,
        "failure_rate_percent": (completed != 0).then(|| failed as f64 / completed as f64 * 100.0),
        "elapsed_seconds": report["elapsed_seconds"],
        "completed_per_second": rate,
        "latency_ms": {"p50": latency["p50"], "p95": latency["p95"], "p99": latency["p99"]},
        "latency_method": report["latency_method"],
        "thresholds": report["thresholds"],
    });
    if matches!(kind, "scenario" | "distributed_scenario") {
        summary["name"] = report["name"].clone();
        summary["target_starts_per_second"] = totals["target_starts_per_second"].clone();
        summary["started_per_second"] = totals["started_per_second"].clone();
        summary["steps"] = report["steps"].clone();
    }
    if matches!(kind, "distributed" | "distributed_scenario") {
        summary["agents"] = report["agents"].clone();
        summary["interruption"] = report["interruption"].clone();
        summary["metrics_scope"] = json!("confirmed_grunt_results");
    }
    Ok(summary)
}
