# Deploy a scenario to remote Grunts

Use the [example Ansible playbook](../examples/ansible/deploy-scenario.yml)
to copy a scenario onto Linux Grunts that already run under systemd. The
playbook expects an existing Grunt configuration and a systemd unit that
starts `horde grunt --config` with that file. Use SSH with permission to
update the configuration and restart the unit.

Use the same Horde revision on the controller and Grunts, with scenario
preflight support. See the [installation guide](installation.md).

## Match the inventory to the SSH hosts

Copy [the example Ansible inventory](../examples/ansible/inventory.yml) and
set the SSH hosts, user, config path and unit name. Put the machines in the
group for the Horde inventory you want to deploy to. For example, the
Ansible group `staging` should contain the machines listed by
`horde inventory --config staging-controller.json list`. Horde uses the
control socket and TLS identity from its inventory. Ansible uses its own
SSH connection settings, which can point to a different interface.

## Deploy the same JSON to every Grunt

From the repository root, deploy during a window without active tests.
Replace the source path with your own scenario to use this outside the
bundled login demo.

```sh
ansible-playbook -i staging-ansible.yml examples/ansible/deploy-scenario.yml \
  --limit staging \
  -e horde_scenario_name=login \
  -e "horde_scenario_source=$PWD/examples/login.json"
```

Each Grunt gets the same JSON bytes in `scenarios/login.json` next to its
config file. The playbook adds or updates the entry under `scenarios`,
keeps the other configuration fields and restarts the service one host at
a time. Each invocation restarts the Grunts so a retry also loads files
copied by a previous interrupted run. The playbook backs up existing files
before replacing them. To roll back a scenario update, run it again with
the previous JSON file and the same profile name.

Use environment variable references for credentials in the scenario and
provide their values in the Grunt service environment. Each step must
resolve to a literal loopback IP on its own Grunt. If the loopback port
differs by machine, use an environment variable for the base URL so the
deployed JSON stays identical.

## Verify the deployment

On the controller, use the config that references the Horde inventory.

```sh
horde preflight --config staging-controller.json --scenario login
```

The JSON report has `ready: true` when every Grunt in the inventory
authenticates and reports the same installed revision and step list. Each
peer that describes the scenario reports its SHA-256 as `scenario_revision`.
If revisions differ, the first Grunt that describes the scenario in
inventory order is the reference. Missing scenarios, connection errors
and mismatches set a failed peer status and exit code 1. No test traffic
is sent.

When preflight passes, launch the test with the same controller configuration.

```sh
horde controller --config staging-controller.json \
  --scenario login --iterations 5 --concurrency 2 --rate 4
```
