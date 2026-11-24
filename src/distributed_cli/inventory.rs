use super::{BoxError, ControllerConfig, GruntSpec, parse_grunt, read_json};
use clap::{Args, Subcommand};
use horde_rs::distributed::transport::is_private_control_ip;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
};
use tokio_rustls::rustls::pki_types::ServerName;

#[derive(Args)]
pub(crate) struct InventoryArgs {
    /// Manage the inventory referenced by this controller configuration.
    #[arg(
        long,
        required_unless_present = "inventory",
        conflicts_with = "inventory"
    )]
    config: Option<PathBuf>,
    /// Manage this JSON inventory directly.
    #[arg(long)]
    inventory: Option<PathBuf>,
    #[command(subcommand)]
    operation: Operation,
}

#[derive(Subcommand)]
enum Operation {
    /// Register a peer. Creates the inventory file when needed.
    Add {
        #[arg(value_name = "ADDRESS[=TLS_IDENTITY]", value_parser = parse_grunt)]
        grunt: GruntSpec,
    },
    /// Remove a peer by its socket address.
    Remove { address: SocketAddr },
    /// Print the current JSON inventory.
    List,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Inventory {
    grunts: Vec<Entry>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    address: SocketAddr,
    #[serde(skip_serializing_if = "Option::is_none")]
    tls_identity: Option<String>,
}

impl Inventory {
    pub(super) fn read(path: &Path) -> Result<Self, BoxError> {
        read_json(path, "Grunt inventory")
    }

    pub(super) fn specs(&self) -> Result<Vec<GruntSpec>, BoxError> {
        self.grunts
            .iter()
            .map(|entry| {
                let identity = entry
                    .tls_identity
                    .clone()
                    .map(ServerName::try_from)
                    .transpose()
                    .map_err(|error| {
                        format!("invalid TLS identity for {}: {error}", entry.address)
                    })?;
                Ok(GruntSpec {
                    address: entry.address,
                    identity,
                })
            })
            .collect()
    }

    pub(super) fn edit<T>(
        path: &Path,
        edit: impl FnOnce(&mut Self) -> Result<(bool, T), BoxError>,
    ) -> Result<T, BoxError> {
        let path = match fs::canonicalize(path) {
            Ok(path) => path,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && path
                        .symlink_metadata()
                        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                fs::canonicalize(parent)?
                    .join(path.file_name().ok_or("missing inventory filename")?)
            }
            Err(error) => return Err(error.into()),
        };
        let mut lock_name = path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_name)?;
        // Keep the lock file in place so successive writers always lock the same inode.
        lock.lock()?;
        let mut inventory = if path.try_exists()? {
            Inventory::read(&path)?
        } else {
            Inventory::default()
        };

        let (changed, result) = edit(&mut inventory)?;
        if changed {
            replace_inventory(&path, &inventory)?;
        }
        Ok(result)
    }

    pub(super) fn add(&mut self, grunt: GruntSpec) -> Result<bool, BoxError> {
        let address = grunt.address;
        let ip = address.ip().to_canonical();
        if address.port() == 0 || !(ip.is_loopback() || is_private_control_ip(ip)) {
            return Err(
                "Grunt address requires a nonzero port and a loopback or private IP".into(),
            );
        }
        let tls_identity = grunt.identity.map(|name| name.to_str().into_owned());
        if let Some(entry) = self
            .grunts
            .iter()
            .find(|entry| same_address(entry.address, address))
        {
            if entry.tls_identity != tls_identity {
                return Err(format!("{address} already has a different TLS identity").into());
            }
            return Ok(false);
        }
        self.grunts.push(Entry {
            address,
            tls_identity,
        });
        Ok(true)
    }

    pub(super) fn remove(&mut self, address: SocketAddr) -> Result<(), BoxError> {
        let Some(index) = self
            .grunts
            .iter()
            .position(|entry| same_address(entry.address, address))
        else {
            return Err(format!("{address} is absent from the Grunt inventory").into());
        };
        self.grunts.remove(index);
        Ok(())
    }
}

impl InventoryArgs {
    pub(super) fn run(self) -> Result<ExitCode, BoxError> {
        let path = match self.config {
            Some(path) => {
                let config: ControllerConfig = read_json(&path, "controller configuration")?;
                config.inventory_path(&path)
            }
            None => self.inventory.ok_or("missing inventory path")?,
        };
        match self.operation {
            Operation::List => println!(
                "{}",
                serde_json::to_string_pretty(&Inventory::read(&path)?)?
            ),
            Operation::Add { grunt } => {
                Inventory::edit(&path, |inventory| Ok((inventory.add(grunt)?, ())))?
            }
            Operation::Remove { address } => Inventory::edit(&path, |inventory| {
                inventory.remove(address)?;
                Ok((true, ()))
            })?,
        }
        Ok(ExitCode::SUCCESS)
    }
}

fn same_address(left: SocketAddr, right: SocketAddr) -> bool {
    left.port() == right.port() && left.ip().to_canonical() == right.ip().to_canonical()
}

fn replace_inventory(path: &Path, inventory: &Inventory) -> Result<(), BoxError> {
    let mut nonce = [0; 8];
    getrandom::fill(&mut nonce).map_err(|_| "could not generate inventory temporary filename")?;
    let temp = path.with_file_name(format!(
        ".horde-inventory-{:016x}.tmp",
        u64::from_ne_bytes(nonce)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&temp)?;
    let result = (|| -> Result<(), BoxError> {
        write_inventory(file, inventory, path)?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_inventory(mut file: File, inventory: &Inventory, path: &Path) -> Result<(), BoxError> {
    match path.metadata() {
        Ok(metadata) => file.set_permissions(metadata.permissions())?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    serde_json::to_writer_pretty(&mut file, inventory)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}
