#![cfg(feature = "mcp")]

use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
};

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    id: u64,
}

impl Client {
    async fn start(arguments: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_horde"))
            .arg("mcp")
            .args(arguments)
            .env("HORDE_MCP_TEST_PASSWORD", "process-only-value")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut client = Self {
            child,
            stdin,
            stdout,
            id: 0,
        };
        let initialized = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "horde-test", "version": "1"}
                }),
            )
            .await;
        assert_eq!(initialized["result"]["serverInfo"]["name"], "horde");
        client
            .stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        client
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let request = json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params});
        self.stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let line = self.stdout.next_line().await.unwrap().unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], self.id, "{response}");
        response
    }

    async fn finish(self) {
        let Self {
            mut child,
            stdin,
            mut stdout,
            ..
        } = self;
        drop(stdin);
        assert!(child.wait().await.unwrap().success());
        assert!(stdout.next_line().await.unwrap().is_none());
    }
}

#[tokio::test]
async fn stdio_exposes_references_and_offline_tools() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut client = Client::start(&[]).await;
        macro_rules! request {
            ($method:expr, $params:expr) => {client.request($method, $params).await};
        }

        let listed = request!("resources/list", json!({}));
        assert_eq!(listed["result"]["resources"].as_array().unwrap().len(), 4);
        let login = request!("resources/read", json!({"uri":"horde://reference/examples/login.json"}));
        let login = login["result"]["contents"][0]["text"].as_str().unwrap();
        assert_eq!(login, include_str!("../examples/login.json"));
        let unknown = request!("resources/read", json!({"uri":"file:///Cargo.toml"}));
        assert_eq!(unknown["error"]["code"], -32002);

        let tools = request!("tools/list", json!({}));
        let names: Vec<_> = tools["result"]["tools"]
            .as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["validate_scenario", "summarize_report"]);
        let mut login: Value = serde_json::from_str(login).unwrap();
        login["variables"]["password"] = json!({"env": "HORDE_MCP_TEST_PASSWORD"});
        let missing = request!("tools/call", json!({
            "name": "validate_scenario", "arguments": {"scenario_json": login.to_string()}
        }));
        assert_eq!(missing["result"]["isError"], true);
        assert!(missing["result"]["structuredContent"]["diagnostic"]
            .as_str().unwrap().contains("sample_env"));
        let valid = request!("tools/call", json!({
            "name": "validate_scenario",
            "arguments": {
                "scenario_json": login.to_string(),
                "sample_env": {"HORDE_MCP_TEST_PASSWORD": "dummy"}
            }
        }));
        assert_eq!(valid["result"]["structuredContent"]["valid"], true);
        login["steps"][1]["name"] = login["steps"][0]["name"].clone();
        let invalid = request!("tools/call", json!({
            "name": "validate_scenario",
            "arguments": {
                "scenario_json": login.to_string(),
                "sample_env": {"HORDE_MCP_TEST_PASSWORD": "dummy"}
            }
        }));
        assert_eq!(invalid["result"]["isError"], true);
        assert!(invalid["result"]["structuredContent"]["diagnostic"]
            .as_str().unwrap().contains("unique"));

        let summary = request!("tools/call", json!({
            "name": "summarize_report", "arguments": {"report": {
                "schema_version": 1, "type": "scenario", "name": "login", "elapsed_seconds": 1,
                "scenarios": {"completed": 4, "failed": 1, "completed_per_second": 4, "duration_ms": {"p95": 24}},
                "steps": [], "thresholds": {"failure_rate_percent": {"passed": false}}
            }}
        }));
        let summary = &summary["result"]["structuredContent"];
        assert_eq!(summary["failure_rate_percent"], 25.0);
        assert_eq!(summary["latency_ms"]["p95"], 24);
        assert_eq!(summary["thresholds"]["failure_rate_percent"]["passed"], false);
        client.finish().await;
    })
    .await
    .expect("MCP stdio session timed out");
}

#[cfg(feature = "distributed-transport")]
mod distributed {
    use super::*;
    use std::{
        fs,
        io::ErrorKind,
        net::TcpListener,
        path::{Path, PathBuf},
    };

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let mut nonce = [0; 8];
            getrandom::fill(&mut nonce).unwrap();
            let path =
                std::env::temp_dir().join(format!("horde-mcp-{:016x}", u64::from_ne_bytes(nonce)));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    async fn call(client: &mut Client, name: &str, arguments: Value) -> Value {
        client
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await["result"]
            .clone()
    }

    async fn successful_call(client: &mut Client, name: &str, arguments: Value) -> Value {
        let result = call(client, name, arguments).await;
        assert_eq!(result["isError"], false, "{result}");
        result["structuredContent"].clone()
    }

    #[tokio::test]
    async fn manages_inventory_checks_control_hello_and_prepares_offline() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let scratch = Scratch::new();
            let config = scratch.0.join("controller.json");
            let inventory = scratch.0.join("grunts.json");
            let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grunt-tls");
            fs::write(
                &config,
                json!({
                    "inventory": "grunts.json",
                    "tls": {
                        "ca": [fixtures.join("ca.der")],
                        "cert": fixtures.join("controller.der"),
                        "key": fixtures.join("controller-key.der")
                    }
                })
                .to_string(),
            )
            .unwrap();

            let target = TcpListener::bind("127.0.0.1:0").unwrap();
            target.set_nonblocking(true).unwrap();
            let unresponsive = TcpListener::bind("127.0.0.1:0").unwrap();
            unresponsive.set_nonblocking(true).unwrap();
            let second = unresponsive.local_addr().unwrap().to_string();
            let mut grunt = Command::new(env!("CARGO_BIN_EXE_horde"))
                .args([
                    "grunt",
                    "--listen",
                    "127.0.0.1:0",
                    "--target",
                    &format!("http://{}/", target.local_addr().unwrap()),
                ])
                .arg("--ca")
                .arg(fixtures.join("ca.der"))
                .arg("--cert")
                .arg(fixtures.join("server.der"))
                .arg("--key")
                .arg(fixtures.join("server-key.der"))
                .arg("--controller-cert")
                .arg(fixtures.join("controller.der"))
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let first = BufReader::new(grunt.stdout.take().unwrap())
                .lines()
                .next_line()
                .await
                .unwrap()
                .unwrap();
            let mut client =
                Client::start(&["--controller", &format!("local={}", config.display())]).await;
            let tools = client.request("tools/list", json!({})).await;
            let names = tools["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert!(names.contains(&"preflight_grunts"));
            assert!(names.contains(&"prepare_distributed_test"));
            let listed = successful_call(&mut client, "list_inventories", json!({})).await;
            assert_eq!(listed["inventories"], json!(["local"]));
            assert_eq!(
                call(&mut client, "list_grunts", json!({"inventory":"unknown"})).await["isError"],
                true
            );

            let add = json!({"inventory": "local", "address": first, "tls_identity": "localhost"});
            assert_eq!(
                successful_call(&mut client, "add_grunt", add.clone()).await["changed"],
                true
            );
            assert_eq!(
                successful_call(&mut client, "add_grunt", add).await["changed"],
                false
            );
            let invalid = call(
                &mut client,
                "add_grunt",
                json!({"inventory":"local", "address":"10.0.0.1:7071"}),
            )
            .await;
            assert_eq!(invalid["isError"], true);
            let listed =
                successful_call(&mut client, "list_grunts", json!({"inventory":"local"})).await;
            assert_eq!(
                listed["grunts"],
                json!([{"address": first, "tls_identity":"localhost"}])
            );
            successful_call(
                &mut client,
                "add_grunt",
                json!({"inventory":"local", "address":second}),
            )
            .await;

            let prepared = successful_call(
                &mut client,
                "prepare_distributed_test",
                json!({
                    "inventory":"local", "requests":5, "concurrency":2, "rate_per_second":3
                }),
            )
            .await;
            assert_eq!(prepared["assignments"][0]["budget"]["requests"], 3);
            assert_eq!(prepared["assignments"][1]["budget"]["requests"], 2);
            assert_eq!(prepared["assignments"][1]["budget"]["rate_per_second"], 1);
            assert_eq!(prepared["argv"][1], "controller");
            assert_eq!(prepared["argv"][3], json!(config));
            let scenario = successful_call(
                &mut client,
                "prepare_distributed_test",
                json!({
                    "inventory": "local", "scenario": "login", "iterations": 5,
                    "concurrency": 2, "rate_per_second": 3
                }),
            )
            .await;
            assert_eq!(scenario["budget_unit"], "iterations");
            assert_eq!(scenario["assignments"][0]["budget"]["requests"], 3);
            assert_eq!(
                &scenario["argv"].as_array().unwrap()[4..8],
                ["--scenario", "login", "--iterations", "5"]
            );
            let one_grunt = successful_call(
                &mut client,
                "prepare_distributed_test",
                json!({
                    "inventory":"local", "requests":5, "concurrency":1, "rate_per_second":3
                }),
            )
            .await;
            assert_eq!(one_grunt["unused_grunts"], json!([second]));
            let too_large = call(
                &mut client,
                "prepare_distributed_test",
                json!({
                    "inventory":"local", "requests":20_001, "concurrency":2, "rate_per_second":3
                }),
            )
            .await;
            assert_eq!(too_large["isError"], true);
            assert_eq!(
                unresponsive.accept().unwrap_err().kind(),
                ErrorKind::WouldBlock
            );

            let preflight = successful_call(
                &mut client,
                "preflight_grunts",
                json!({"inventory":"local"}),
            )
            .await;
            assert_eq!(preflight["ready"], false);
            assert_eq!(preflight["grunts"][0]["status"], "ready");
            assert_eq!(preflight["grunts"][1]["status"], "failed");
            assert!(preflight["grunts"][1]["error"].is_string());
            assert_eq!(target.accept().unwrap_err().kind(), ErrorKind::WouldBlock);

            successful_call(
                &mut client,
                "remove_grunt",
                json!({"inventory":"local", "address":second}),
            )
            .await;
            let saved: Value = serde_json::from_slice(&fs::read(inventory).unwrap()).unwrap();
            assert_eq!(saved["grunts"], listed["grunts"]);
            let summary = successful_call(&mut client, "summarize_report", json!({"report":{
                "schema_version":1, "type":"distributed", "partial":true,
                "metrics":{"completed":3, "failed":0}, "agents":[
                    {"address":first, "status":"complete"}, {"address":second, "status":"missing"}],
                "thresholds":{"failure_rate":"not_evaluable", "p95":"not_evaluable"}
            }})).await;
            assert_eq!(summary["agents"][1]["status"], "missing");
            assert_eq!(summary["thresholds"]["p95"], "not_evaluable");
            assert_eq!(summary["metrics_scope"], "confirmed_grunt_results");
            let scenario_summary = successful_call(
                &mut client,
                "summarize_report",
                json!({"report":{
                    "schema_version": 1, "type": "distributed_scenario", "partial": true,
                    "name": "login", "scenarios": {"completed": 2, "failed": 1},
                    "steps": [{"name": "sign in", "completed": 2, "failed": 1}],
                    "agents": [{"address": first, "status": "complete"}],
                    "thresholds": {"p95": "not_evaluable"}
                }}),
            )
            .await;
            assert_eq!(scenario_summary["type"], "distributed_scenario");
            assert_eq!(scenario_summary["failure_rate_percent"], 50.0);
            assert_eq!(scenario_summary["steps"][0]["name"], "sign in");
            assert_eq!(scenario_summary["agents"][0]["status"], "complete");
            client.finish().await;
            grunt.kill().await.unwrap();
        })
        .await
        .expect("distributed MCP stdio session timed out");
    }
}
