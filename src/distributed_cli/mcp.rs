use super::{
    BoxError, ControllerConfig, DEFAULT_DEADLINE_SECONDS, DEFAULT_PROFILE, DEFAULT_PROFILE_LIMITS,
    build_controller, inventory::Inventory, parse_grunt, preflight_report, resolve_grunts,
};
use horde_rs::{
    Cancellation,
    distributed::{
        Budget,
        controller::{Controller, Grunt, plan_budgets},
        transport::valid_profile_name,
    },
};
use rmcp::model::{Tool, ToolAnnotations};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};
use tokio_rustls::rustls::pki_types::ServerName;

const LIST_INVENTORIES: &str = "list_inventories";
const LIST_GRUNTS: &str = "list_grunts";
const ADD_GRUNT: &str = "add_grunt";
const REMOVE_GRUNT: &str = "remove_grunt";
const PREFLIGHT: &str = "preflight_grunts";
const PREPARE: &str = "prepare_distributed_test";

#[derive(Clone)]
pub(crate) struct ConfiguredController {
    name: String,
    path: PathBuf,
}

impl FromStr for ConfiguredController {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (name, path) = value
            .split_once('=')
            .filter(|(name, path)| !name.trim().is_empty() && !path.is_empty())
            .ok_or("expected NAME=CONFIG_PATH")?;
        Ok(Self {
            name: name.to_owned(),
            path: PathBuf::from(path),
        })
    }
}

pub(crate) struct Controllers {
    paths: BTreeMap<String, PathBuf>,
}

impl Controllers {
    pub(crate) fn new(configs: Vec<ConfiguredController>) -> Result<Self, BoxError> {
        let mut paths = BTreeMap::new();
        for config in configs {
            match paths.entry(config.name) {
                Entry::Occupied(entry) => {
                    return Err(format!("duplicated MCP inventory name {:?}", entry.key()).into());
                }
                Entry::Vacant(entry) => {
                    entry.insert(std::path::absolute(config.path)?);
                }
            }
        }
        Ok(Self { paths })
    }

    fn path(&self, name: &str) -> Result<&Path, BoxError> {
        self.paths
            .get(name)
            .map(PathBuf::as_path)
            .ok_or_else(|| format!("unknown configured inventory {name:?}").into())
    }

    pub(crate) fn tools(&self) -> Vec<Tool> {
        if self.paths.is_empty() {
            return Vec::new();
        }
        let read_only = || ToolAnnotations::new().read_only(true).open_world(false);
        let inventory = json!({"type": "string", "description": "Configured inventory name from list_inventories."});
        let address = json!({"type": "string", "description": "Literal IPv4 or bracketed IPv6 address with port."});
        let positive_integer = json!({"type": "integer", "minimum": 1});
        vec![
            tool(
                LIST_INVENTORIES,
                "List the named Grunt inventories configured at server startup.",
                json!({}),
                &[],
                read_only(),
            ),
            tool(
                LIST_GRUNTS,
                "Read the entries in a configured Grunt inventory.",
                json!({"inventory": inventory}),
                &["inventory"],
                read_only(),
            ),
            tool(
                ADD_GRUNT,
                "Register a Grunt in a configured inventory. Supply a socket address and, when needed, the TLS identity.",
                json!({"inventory": inventory, "address": address, "tls_identity": {
                    "type": "string", "description": "Expected certificate DNS name or IP. Omit to use the controller default."
                }}),
                &["inventory", "address"],
                ToolAnnotations::new()
                    .read_only(false)
                    .destructive(false)
                    .idempotent(true)
                    .open_world(false),
            ),
            tool(
                REMOVE_GRUNT,
                "Remove a Grunt by socket address from a configured inventory.",
                json!({"inventory": inventory, "address": address}),
                &["inventory", "address"],
                ToolAnnotations::new()
                    .read_only(false)
                    .destructive(true)
                    .open_world(false),
            ),
            tool(
                PREFLIGHT,
                "Check the authenticated control hello for each Grunt in a configured inventory.",
                json!({"inventory": inventory}),
                &["inventory"],
                read_only().open_world(true),
            ),
            tool(
                PREPARE,
                "Preview a request budget or an installed scenario's iteration budget. Return controller arguments for manual execution.",
                json!({"inventory": inventory, "requests": positive_integer,
                    "scenario": {"type":"string","pattern":"^[a-zA-Z0-9_-]{1,64}$","description":"Name of the scenario installed on the Grunts."},
                    "iterations": positive_integer,
                    "concurrency": positive_integer, "rate_per_second": positive_integer,
                    "deadline_seconds": {"type":"integer","minimum":1,"maximum":3600,"default":DEFAULT_DEADLINE_SECONDS}}),
                &["inventory", "concurrency", "rate_per_second"],
                read_only(),
            ),
        ]
    }

    pub(crate) async fn call(
        &self,
        name: &str,
        arguments: Value,
    ) -> Option<Result<Value, BoxError>> {
        if self.paths.is_empty() {
            return None;
        }
        Some(match name {
            LIST_INVENTORIES if arguments.as_object().is_some_and(|map| map.is_empty()) => {
                Ok(json!({"inventories": self.paths.keys().collect::<Vec<_>>() }))
            }
            LIST_INVENTORIES => Err("list_inventories takes no arguments".into()),
            LIST_GRUNTS => self.list(arguments),
            ADD_GRUNT => self.add(arguments).await,
            REMOVE_GRUNT => self.remove(arguments).await,
            PREFLIGHT => self.preflight(arguments).await,
            PREPARE => self.prepare(arguments),
            _ => return None,
        })
    }

    fn list(&self, arguments: Value) -> Result<Value, BoxError> {
        let args: InventoryArgument = serde_json::from_value(arguments)?;
        let path = self.path(&args.inventory)?;
        let config = ControllerConfig::read(path)?;
        let mut result = serde_json::to_value(Inventory::read(&config.inventory_path(path))?)?;
        result["inventory"] = json!(args.inventory);
        Ok(result)
    }

    async fn add(&self, arguments: Value) -> Result<Value, BoxError> {
        let args: GruntArgument = serde_json::from_value(arguments)?;
        let address = args.address.to_string();
        let spec = match &args.tls_identity {
            Some(identity) => parse_grunt(&format!("{address}={identity}"))?,
            None => parse_grunt(&address)?,
        };
        let path = self.path(&args.inventory)?;
        let config = ControllerConfig::read(path)?;
        let inventory_path = config.inventory_path(path);
        let changed = tokio::task::spawn_blocking(move || {
            Inventory::edit(&inventory_path, |inventory| {
                let changed = inventory.add(spec)?;
                inventory_grunts(&config, inventory)?;
                Ok((changed, changed))
            })
        })
        .await??;
        Ok(json!({
            "inventory": args.inventory, "changed": changed, "address": args.address,
            "tls_identity": args.tls_identity,
        }))
    }

    async fn remove(&self, arguments: Value) -> Result<Value, BoxError> {
        let args: RemoveArgument = serde_json::from_value(arguments)?;
        let path = self.path(&args.inventory)?;
        let inventory_path = ControllerConfig::read(path)?.inventory_path(path);
        tokio::task::spawn_blocking(move || {
            Inventory::edit(&inventory_path, |inventory| {
                inventory.remove(args.address)?;
                Ok((true, ()))
            })
        })
        .await??;
        Ok(json!({"inventory": args.inventory, "changed": true, "address": args.address}))
    }

    async fn preflight(&self, arguments: Value) -> Result<Value, BoxError> {
        let args: InventoryArgument = serde_json::from_value(arguments)?;
        let path = self.path(&args.inventory)?;
        let config = ControllerConfig::read(path)?;
        let grunts = inventory_grunts(&config, &Inventory::read(&config.inventory_path(path))?)?;
        let controller = build_controller(&config.tls, config.private_network)?;
        let results = controller.preflight(&grunts, Cancellation::new()).await?;
        let (_, mut report) = preflight_report(&grunts, results, None);
        report["inventory"] = json!(args.inventory);
        Ok(report)
    }

    fn prepare(&self, arguments: Value) -> Result<Value, BoxError> {
        let args: PrepareArguments = serde_json::from_value(arguments)?;
        let (profile, unit, count, workload_args) =
            match (args.scenario.as_deref(), args.iterations, args.requests) {
                (None, None, Some(requests)) => (
                    DEFAULT_PROFILE,
                    "requests",
                    requests,
                    vec!["--requests".to_owned(), requests.to_string()],
                ),
                (Some(name), Some(iterations), None) if valid_profile_name(name) => (
                    name,
                    "iterations",
                    iterations,
                    vec![
                        "--scenario".to_owned(),
                        name.to_owned(),
                        "--iterations".to_owned(),
                        iterations.to_string(),
                    ],
                ),
                _ => return Err("provide requests or a valid scenario name with iterations".into()),
            };
        let path = self.path(&args.inventory)?;
        let config = ControllerConfig::read(path)?;
        let grunts = inventory_grunts(&config, &Inventory::read(&config.inventory_path(path))?)?;
        let deadline_seconds = args.deadline_seconds.unwrap_or(DEFAULT_DEADLINE_SECONDS);
        let budget = Budget {
            requests: Some(count),
            concurrency: args.concurrency,
            rate_per_second: args.rate_per_second,
            duration: None,
        };
        let shares = plan_budgets(budget, grunts.len(), Duration::from_secs(deadline_seconds))?;
        let mut assignments = Vec::with_capacity(shares.len());
        for (grunt, share) in grunts.iter().zip(&shares) {
            DEFAULT_PROFILE_LIMITS.validate(*share).map_err(|error| {
                format!("Grunt {}: {error} for the built-in profile", grunt.address)
            })?;
            assignments.push(json!({
                "address": grunt.address,
                "tls_identity": grunt.server_name.to_str(),
                "budget": share,
            }));
        }
        let unused = grunts
            .iter()
            .skip(shares.len())
            .map(|grunt| grunt.address)
            .collect::<Vec<_>>();
        let executable = std::env::current_exe()?;
        let executable = executable
            .to_str()
            .ok_or("Horde executable path is not UTF-8")?;
        let path = path
            .to_str()
            .ok_or("controller configuration path is not UTF-8")?;
        let argv = [executable, "controller", "--config", path]
            .map(str::to_owned)
            .into_iter()
            .chain(workload_args)
            .chain([
                "--concurrency".to_owned(),
                args.concurrency.to_string(),
                "--rate".to_owned(),
                args.rate_per_second.to_string(),
                "--deadline".to_owned(),
                deadline_seconds.to_string(),
            ])
            .collect::<Vec<_>>();
        Ok(json!({
            "inventory": args.inventory,
            "profile": profile,
            "budget_unit": unit,
            "budget": budget,
            "deadline_seconds": deadline_seconds,
            "assignments": assignments,
            "unused_grunts": unused,
            "connectivity_checked": false,
            "argv": argv,
        }))
    }
}

fn inventory_grunts(
    config: &ControllerConfig,
    inventory: &Inventory,
) -> Result<Vec<Grunt>, BoxError> {
    let default = config
        .server_name
        .clone()
        .map(ServerName::try_from)
        .transpose()?;
    let grunts = resolve_grunts(inventory.specs()?, default.as_ref(), config.private_network);
    Controller::validate_grunt_set(&grunts, config.private_network)?;
    Ok(grunts)
}

fn tool(
    name: &'static str,
    description: &'static str,
    properties: Value,
    required: &[&str],
    annotations: ToolAnnotations,
) -> Tool {
    let schema = json!({
        "type":"object",
        "properties":properties,
        "required":required,
        "additionalProperties":false,
    });
    Tool::new(
        name,
        description,
        schema.as_object().expect("tool schema object").clone(),
    )
    .with_annotations(annotations)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryArgument {
    inventory: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GruntArgument {
    inventory: String,
    address: SocketAddr,
    tls_identity: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveArgument {
    inventory: String,
    address: SocketAddr,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareArguments {
    inventory: String,
    requests: Option<u64>,
    scenario: Option<String>,
    iterations: Option<u64>,
    concurrency: u64,
    rate_per_second: u64,
    deadline_seconds: Option<u64>,
}
