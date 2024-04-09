// Copyright (C) 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

#![allow(clippy::too_many_arguments)]

use super::get_bin_path;
use crate::{
    local::{kill_network, run_network, LocalNetworkOptions},
    print_banner, status_report, VerbosityLevel,
};
use color_eyre::{eyre::eyre, Help, Report, Result};
use sn_logging::LogFormat;
use sn_peers_acquisition::PeersArgs;
use sn_releases::{ReleaseType, SafeReleaseRepoActions};
use sn_service_management::{
    control::ServiceController, get_local_node_registry_path, NodeRegistry,
};
use std::path::PathBuf;
use xor_name::XOR_NAME_LEN;

pub async fn join(
    build: bool,
    count: u16,
    sybil: Option<String>,
    faucet_path: Option<PathBuf>,
    faucet_version: Option<String>,
    interval: u64,
    node_path: Option<PathBuf>,
    node_version: Option<String>,
    log_format: Option<LogFormat>,
    owner: Option<String>,
    owner_prefix: Option<String>,
    peers_args: PeersArgs,
    skip_validation: bool,
    verbosity: VerbosityLevel,
) -> Result<(), Report> {
    if verbosity != VerbosityLevel::Minimal {
        print_banner("Joining Local Network");
    }
    info!("Joining local network");

    println!("====================================================");
    println!("               Joining Local Network                ");
    let sybil = if let Some(xorname_str) = sybil {
        let bytes = hex::decode(xorname_str)?;
        let mut arr = [0u8; XOR_NAME_LEN];
        arr.copy_from_slice(&bytes);
        let xorname = xor_name::XorName(arr);
        println!("** WITH SYBIL NODE/s TO ECLIPSE XorName: {xorname} **");
        Some(xorname)
    } else {
        None
    };
    println!("====================================================");

    let local_node_reg_path = &get_local_node_registry_path()?;
    let mut local_node_registry = NodeRegistry::load(local_node_reg_path)?;

    let release_repo = <dyn SafeReleaseRepoActions>::default_config();
    let faucet_path = get_bin_path(
        build,
        faucet_path,
        ReleaseType::Faucet,
        faucet_version,
        &*release_repo,
        verbosity,
    )
    .await?;
    let node_path = get_bin_path(
        build,
        node_path,
        ReleaseType::Safenode,
        node_version,
        &*release_repo,
        verbosity,
    )
    .await?;

    // If no peers are obtained we will attempt to join the existing local network, if one
    // is running.
    let peers = match peers_args.get_peers().await {
        Ok(peers) => Some(peers),
        Err(err) => match err {
            sn_peers_acquisition::error::Error::PeersNotObtained => {
                warn!("PeersNotObtained, peers is set to None");
                None
            }
            _ => {
                error!("Failed to obtain peers: {err:?}");
                return Err(err.into());
            }
        },
    };
    let options = LocalNetworkOptions {
        faucet_bin_path: faucet_path,
        interval,
        join: true,
        node_count: count,
        owner,
        owner_prefix,
        sybil,
        peers,
        safenode_bin_path: node_path,
        skip_validation,
        log_format,
    };
    run_network(options, &mut local_node_registry, &ServiceController {}).await?;
    Ok(())
}

pub fn kill(keep_directories: bool, verbosity: VerbosityLevel) -> Result<()> {
    let local_reg_path = &get_local_node_registry_path()?;
    let local_node_registry = NodeRegistry::load(local_reg_path)?;
    if local_node_registry.nodes.is_empty() {
        info!("No local network is currently running, cannot kill it");
        println!("No local network is currently running");
    } else {
        if verbosity != VerbosityLevel::Minimal {
            print_banner("Killing Local Network");
        }
        info!("Kill local network");
        kill_network(&local_node_registry, keep_directories)?;
        std::fs::remove_file(local_reg_path)?;
    }
    Ok(())
}

pub async fn run(
    build: bool,
    clean: bool,
    count: u16,
    faucet_path: Option<PathBuf>,
    faucet_version: Option<String>,
    interval: u64,
    node_path: Option<PathBuf>,
    node_version: Option<String>,
    log_format: Option<LogFormat>,
    owner: Option<String>,
    owner_prefix: Option<String>,
    skip_validation: bool,
    verbosity: VerbosityLevel,
) -> Result<(), Report> {
    // In the clean case, the node registry must be loaded *after* the existing network has
    // been killed, which clears it out.
    let local_node_reg_path = &get_local_node_registry_path()?;
    let mut local_node_registry: NodeRegistry = if clean {
        debug!("Clean set to true, removing client, node dir and killing the network.");
        let client_data_path = dirs_next::data_dir()
            .ok_or_else(|| eyre!("Could not obtain user's data directory"))?
            .join("safe")
            .join("client");
        if client_data_path.is_dir() {
            std::fs::remove_dir_all(client_data_path)?;
        }
        kill(false, verbosity)?;
        NodeRegistry::load(local_node_reg_path)?
    } else {
        let local_node_registry = NodeRegistry::load(local_node_reg_path)?;
        if !local_node_registry.nodes.is_empty() {
            error!("A local network is already running, cannot run a new one");
            return Err(eyre!("A local network is already running")
                .suggestion("Use the kill command to destroy the network then try again"));
        }
        local_node_registry
    };

    if verbosity != VerbosityLevel::Minimal {
        print_banner("Launching Local Network");
    }
    info!("Launching local network");

    let release_repo = <dyn SafeReleaseRepoActions>::default_config();
    let faucet_path = get_bin_path(
        build,
        faucet_path,
        ReleaseType::Faucet,
        faucet_version,
        &*release_repo,
        verbosity,
    )
    .await?;
    let node_path = get_bin_path(
        build,
        node_path,
        ReleaseType::Safenode,
        node_version,
        &*release_repo,
        verbosity,
    )
    .await?;

    let options = LocalNetworkOptions {
        faucet_bin_path: faucet_path,
        join: false,
        interval,
        node_count: count,
        owner,
        owner_prefix,
        sybil: None,
        peers: None,
        safenode_bin_path: node_path,
        skip_validation,
        log_format,
    };
    run_network(options, &mut local_node_registry, &ServiceController {}).await?;

    local_node_registry.save()?;
    Ok(())
}

pub async fn status(details: bool, fail: bool, json: bool) -> Result<()> {
    let mut local_node_registry = NodeRegistry::load(&get_local_node_registry_path()?)?;
    if !json {
        print_banner("Local Network");
    }
    status_report(
        &mut local_node_registry,
        &ServiceController {},
        details,
        json,
        fail,
    )
    .await?;
    local_node_registry.save()?;
    Ok(())
}
