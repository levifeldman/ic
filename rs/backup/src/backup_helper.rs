use crate::util::{block_on, sleep_secs};
use ic_recovery::command_helper::exec_cmd; // TODO: refactor this and next, out of ic_recovery
use ic_recovery::file_sync_helper::download_binary;
use ic_registry_client::client::{RegistryClient, RegistryClientImpl};
use ic_registry_client_helpers::node::NodeRegistry;
use ic_registry_client_helpers::subnet::SubnetRegistry;
use ic_types::{ReplicaVersion, SubnetId};
use rand::{seq::SliceRandom, thread_rng};
use slog::{error, info, warn, Logger};
use std::ffi::OsStr;
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

const RETRIES_RSYNC_HOST: u64 = 5;
const RETRIES_BINARY_DOWNLOAD: u64 = 3;

pub struct BackupHelper {
    pub replica_version: ReplicaVersion,
    pub subnet_id: SubnetId,
    pub nns_url: String,
    pub root_dir: PathBuf,
    pub ssh_credentials: String,
    pub registry_client: Arc<RegistryClientImpl>,
    pub log: Logger,
}

impl BackupHelper {
    fn binary_dir(&self) -> PathBuf {
        self.root_dir
            .join(format!("binaries/{}", self.replica_version))
    }

    fn binary_file(&self, executable: &str) -> PathBuf {
        self.binary_dir().join(executable)
    }

    fn spool_root_dir(&self) -> PathBuf {
        self.root_dir.join("spool")
    }

    fn spool_dir(&self) -> PathBuf {
        self.spool_root_dir().join(self.subnet_id.to_string())
    }

    fn local_store_dir(&self) -> PathBuf {
        self.root_dir.join("ic_registry_local_store")
    }

    fn data_dir(&self) -> PathBuf {
        self.root_dir.join(format!("data/{}", self.subnet_id))
    }

    fn ic_config_dir(&self) -> PathBuf {
        self.data_dir().join("config")
    }

    fn ic_config_file_local(&self) -> PathBuf {
        self.ic_config_dir().join("ic.json5")
    }

    fn state_dir(&self) -> PathBuf {
        self.data_dir().join("ic_state")
    }

    fn username(&self) -> String {
        "backup".to_string()
    }

    fn download_binaries(&self) {
        if !self.binary_dir().exists() {
            std::fs::create_dir_all(self.binary_dir()).expect("Failure creating a directory");
        }
        self.download_binary("ic-replay".to_string());
        self.download_binary("sandbox_launcher".to_string());
        self.download_binary("canister_sandbox".to_string());
    }

    fn download_binary(&self, binary_name: String) {
        if self.binary_file(&binary_name).exists() {
            return;
        }
        for _ in 0..RETRIES_BINARY_DOWNLOAD {
            let res = block_on(download_binary(
                &self.log,
                self.replica_version.clone(),
                binary_name.clone(),
                self.binary_dir(),
            ));
            if res.is_ok() {
                return;
            }
            warn!(
                self.log,
                "Error while downloading {}: {:?}", binary_name, res
            );
            sleep_secs(10);
        }
        // Without the binaries we can't replay...
        panic!(
            "Binary {} is required for the replica {}",
            binary_name, self.replica_version
        );
    }

    fn rsync_node_backup(&self, node_ip: IpAddr) {
        info!(self.log, "Sync backup data from the node: {}", node_ip);
        let remote_dir = format!(
            "{}@[{}]:/var/lib/ic/backup/{}/",
            self.username(),
            node_ip,
            self.subnet_id
        );
        for _ in 0..RETRIES_RSYNC_HOST {
            match self.rsync_cmd(
                remote_dir.clone(),
                &self.spool_dir().into_os_string(),
                &["-qa", "--append-verify"],
            ) {
                Ok(_) => return,
                Err(e) => warn!(
                    self.log,
                    "Problem syncing backup directory with host: {} : {}", node_ip, e
                ),
            }
            sleep_secs(60);
        }
        warn!(self.log, "Didn't sync at all with host: {}", node_ip);
    }

    fn rsync_config(&self, node_ip: IpAddr) {
        info!(self.log, "Sync ic.json5 from the node: {}", node_ip);
        let remote_dir = format!(
            "{}@[{}]:/run/ic-node/config/ic.json5",
            self.username(),
            node_ip
        );
        for _ in 0..RETRIES_RSYNC_HOST {
            match self.rsync_cmd(
                remote_dir.clone(),
                &self.ic_config_file_local().into_os_string(),
                &["-q"],
            ) {
                Ok(_) => return,
                Err(e) => warn!(
                    self.log,
                    "Problem syncing config from host: {} : {}", node_ip, e
                ),
            }
            sleep_secs(60);
        }
        warn!(self.log, "Didn't sync any config from host: {}", node_ip);
    }

    fn rsync_cmd(
        &self,
        remote_dir: String,
        local_dir: &OsStr,
        arguments: &[&str],
    ) -> Result<(), String> {
        let mut cmd = Command::new("rsync");
        cmd.arg("-e");
        cmd.arg(format!(
            "ssh -o StrictHostKeyChecking=no -i {}",
            self.ssh_credentials
        ));
        cmd.arg("--timeout=600");
        cmd.args(arguments);
        cmd.arg("--min-size=1").arg(remote_dir).arg(local_dir);
        info!(self.log, "Will execute: {:?}", cmd);
        if let Err(e) = exec_cmd(&mut cmd) {
            // TODO: probably tolerate: rsync warning: some files vanished before they could be transferred (code 24)
            Err(format!("Error: {}", e))
        } else {
            Ok(())
        }
    }

    pub fn sync(&self, nodes: Vec<IpAddr>) {
        if !self.spool_dir().exists() {
            std::fs::create_dir_all(self.spool_dir()).expect("Failure creating a directory");
        }
        if !self.ic_config_dir().exists() {
            std::fs::create_dir_all(self.ic_config_dir()).expect("Failure creating a directory");
        }

        let mut shuf_nodes = nodes;
        shuf_nodes.shuffle(&mut thread_rng());
        for n in shuf_nodes.clone() {
            self.rsync_config(n);
        }
        for n in shuf_nodes {
            self.rsync_node_backup(n);
        }
    }

    fn get_replica_version(&self) -> Result<ReplicaVersion, String> {
        let subnet_id = self.subnet_id;
        block_on(async {
            if let Err(err) = self.registry_client.try_polling_latest_version(200) {
                return Err(format!("couldn't poll the registry: {:?}", err));
            };
            let version = self.registry_client.get_latest_version();
            match self.registry_client.get_replica_version(subnet_id, version) {
                Ok(Some(replica_version)) => Ok(replica_version),
                other => Err(format!(
                    "can't fetch replica version from the registry for subnet_id={}: {:?}",
                    subnet_id, other
                )),
            }
        })
    }

    pub fn collect_subnet_nodes(&self) -> Result<Vec<IpAddr>, String> {
        let subnet_id = self.subnet_id;
        let result = block_on(async {
            if let Err(err) = self.registry_client.try_polling_latest_version(200) {
                return Err(format!("couldn't poll the registry: {:?}", err));
            };
            let version = self.registry_client.get_latest_version();
            match self
                .registry_client
                .get_node_ids_on_subnet(subnet_id, version)
            {
                Ok(Some(node_ids)) => Ok(node_ids
                    .into_iter()
                    .filter_map(|node_id| {
                        self.registry_client
                            .get_transport_info(node_id, version)
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()),
                other => Err(format!(
                    "no node ids found in the registry for subnet_id={}: {:?}",
                    subnet_id, other
                )),
            }
        })?;
        result
            .into_iter()
            .filter_map(|node_record| {
                node_record.http.map(|http| {
                    http.ip_addr.parse().map_err(|err| {
                        format!("couldn't parse ip address from the registry: {:?}", err)
                    })
                })
            })
            .collect()
    }

    fn last_checkpoint(&self) -> u64 {
        if !self.state_dir().exists() {
            return 0u64;
        }
        match std::fs::read_dir(self.state_dir().join("checkpoints")) {
            Ok(file_list) => file_list
                .flatten()
                .map(|filename| {
                    filename
                        .path()
                        .file_name()
                        .unwrap_or_else(|| OsStr::new("0"))
                        .to_os_string()
                        .into_string()
                        .unwrap_or_else(|_| "0".to_string())
                })
                .map(|s| u64::from_str_radix(&s, 16).unwrap_or(0))
                .fold(0u64, |a, b| -> u64 { a.max(b) }),
            Err(_) => 0,
        }
    }

    // TODO: remove this function by improving ic-replay parameters
    fn copy_local_store(&self) {
        let subnet_registry = self.data_dir().join("ic_registry_local_store");
        let mut cmd = Command::new("rm");
        cmd.arg("-rf");
        cmd.arg(subnet_registry.clone());
        info!(self.log, "Will execute: {:?}", cmd);
        if let Err(e) = exec_cmd(&mut cmd) {
            error!(self.log, "Error: {}", e);
        }
        let mut cmd = Command::new("cp");
        cmd.arg("-rf");
        cmd.arg(self.local_store_dir());
        cmd.arg(subnet_registry);
        info!(self.log, "Will execute: {:?}", cmd);
        if let Err(e) = exec_cmd(&mut cmd) {
            error!(self.log, "Error: {}", e);
        }
    }

    pub fn replay(&mut self) {
        let start_height = self.last_checkpoint();
        if !self.state_dir().exists() {
            std::fs::create_dir_all(self.state_dir()).expect("Failure creating a directory");
        }

        self.copy_local_store();
        self.replay_current_version();

        if self.last_checkpoint() > start_height {
            info!(self.log, "Replay was successful!");
        } else {
            match self.get_replica_version() {
                Ok(latest_version) => {
                    // Is there a replica version upgrade?
                    if latest_version != self.replica_version {
                        info!(self.log, "Upgrade detected to: {}", latest_version);
                        self.replica_version = latest_version;
                        // try to replay with the new version instead
                        self.replay_current_version();
                        if self.last_checkpoint() > start_height {
                            info!(self.log, "Replay was successful with new version!");
                        } else {
                            warn!(self.log, "No progress with the new version!");
                        }
                    } else {
                        warn!(self.log, "Replay had no progress!");
                    }
                }
                Err(e) => {
                    error!(self.log, "Error fetching replica version:: {}", e);
                }
            }
        }
    }

    fn replay_current_version(&mut self) {
        let start_height = self.last_checkpoint();
        info!(
            self.log,
            "Replaying from height #{} of subnet {:?} with version {}",
            start_height,
            self.subnet_id,
            self.replica_version
        );
        self.download_binaries();

        let ic_admin = self.binary_file("ic-replay");
        let mut cmd = Command::new(ic_admin);
        cmd.arg("--data-root")
            .arg(&self.data_dir())
            .arg("--subnet-id")
            .arg(&self.subnet_id.to_string())
            .arg(&self.ic_config_file_local())
            .arg("restore-from-backup")
            .arg(&self.local_store_dir())
            .arg(&self.spool_root_dir())
            .arg(&self.replica_version.to_string())
            .arg(start_height.to_string())
            .stdout(Stdio::piped());
        info!(self.log, "Will execute: {:?}", cmd);
        match exec_cmd(&mut cmd) {
            Err(e) => {
                error!(self.log, "Error: {}", e.to_string());
            }
            Ok(Some(stdout)) => {
                info!(self.log, "Replay result:");
                info!(self.log, "{}", stdout);
            }
            Ok(None) => {
                error!(self.log, "No output from the replay process!")
            }
        }
        info!(self.log, "Last height: #{}!", self.last_checkpoint());
    }
}
