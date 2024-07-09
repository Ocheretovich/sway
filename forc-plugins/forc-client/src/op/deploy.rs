use crate::{
    cmd,
    constants::TX_SUBMIT_TIMEOUT_MS,
    util::{
        node_url::get_node_url,
        pkg::{
            build_loader_contract, build_proxy_contract, built_pkgs, split_into_chunks,
            update_proxy_address_in_manifest,
        },
        tx::{
            bech32_from_secret, check_and_create_wallet_at_default_path, first_user_account,
            prompt_forc_wallet_password, select_manual_secret_key, select_secret_key,
            update_proxy_contract_target, WalletSelectionMode,
        },
    },
};
use anyhow::{bail, Context, Result};
use colored::Colorize;
use forc_pkg::manifest::GenericManifestFile;
use forc_pkg::{self as pkg, PackageManifestFile};
use forc_tracing::println_warning;
use forc_util::default_output_directory;
use forc_wallet::utils::default_wallet_path;
use fuel_core_client::client::types::TransactionStatus;
use fuel_core_client::client::FuelClient;
use fuel_crypto::fuel_types::ChainId;
use fuel_tx::Salt;
use fuel_vm::prelude::*;
use fuels::types::{transaction::TxPolicies, transaction_builders::CreateTransactionBuilder};
use fuels_accounts::{provider::Provider, wallet::WalletUnlocked, Account};
use fuels_core::types::bech32::Bech32Address;
use futures::FutureExt;
use pkg::{manifest::build_profile::ExperimentalFlags, BuildOpts, BuildProfile, BuiltPackage};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    str::FromStr,
};
use sway_core::language::parsed::TreeType;
use sway_core::BuildTarget;
use tracing::info;

const MAX_CONTRACT_SIZE: usize = 480;

#[derive(Debug, PartialEq, Eq, Clone, PartialOrd, Ord)]
pub struct DeployedContract {
    pub id: fuel_tx::ContractId,
    pub proxy: Option<fuel_tx::ContractId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentArtifact {
    transaction_id: String,
    salt: String,
    network_endpoint: String,
    chain_id: ChainId,
    contract_id: String,
    deployment_size: usize,
    deployed_block_height: u32,
}

impl DeploymentArtifact {
    pub fn to_file(
        &self,
        output_dir: &Path,
        pkg_name: &str,
        contract_id: ContractId,
    ) -> Result<()> {
        if !output_dir.exists() {
            std::fs::create_dir_all(output_dir)?;
        }

        let deployment_artifact_json = format!("{pkg_name}-deployment-0x{contract_id}");
        let deployments_path = output_dir
            .join(deployment_artifact_json)
            .with_extension("json");
        let deployments_file = std::fs::File::create(deployments_path)?;
        serde_json::to_writer_pretty(&deployments_file, &self)?;
        Ok(())
    }
}

type ContractSaltMap = BTreeMap<String, Salt>;

/// Takes the contract member salt inputs passed via the --salt option, validates them against
/// the manifests and returns a ContractSaltMap (BTreeMap of contract names to salts).
fn validate_and_parse_salts<'a>(
    salt_args: &[String],
    manifests: impl Iterator<Item = &'a PackageManifestFile>,
) -> Result<ContractSaltMap> {
    let mut contract_salt_map = BTreeMap::default();

    // Parse all the salt arguments first, and exit if there are errors in this step.
    for salt_arg in salt_args {
        if let Some((given_contract_name, salt)) = salt_arg.split_once(':') {
            let salt = salt
                .parse::<Salt>()
                .map_err(|e| anyhow::anyhow!(e))
                .unwrap();

            if let Some(old) = contract_salt_map.insert(given_contract_name.to_string(), salt) {
                bail!("2 salts provided for contract '{given_contract_name}':\n  {old}\n  {salt}");
            };
        } else {
            bail!("Invalid salt provided - salt must be in the form <CONTRACT_NAME>:<SALT> when deploying a workspace");
        }
    }

    for manifest in manifests {
        for (dep_name, contract_dep) in manifest.contract_deps() {
            let dep_pkg_name = contract_dep.dependency.package().unwrap_or(dep_name);
            if let Some(declared_salt) = contract_salt_map.get(dep_pkg_name) {
                bail!(
                    "Redeclaration of salt using the option '--salt' while a salt exists for contract '{}' \
                    under the contract dependencies of the Forc.toml manifest for '{}'\n\
                    Existing salt: '0x{}',\nYou declared: '0x{}'\n",
                    dep_pkg_name,
                    manifest.project_name(),
                    contract_dep.salt,
                    declared_salt,
                    );
            }
        }
    }

    Ok(contract_salt_map)
}

async fn deploy_new_proxy(
    pkg: &BuiltPackage,
    owner_account_address: &mut Bech32Address,
    impl_contract: &fuel_tx::ContractId,
    build_opts: &BuildOpts,
    command: &cmd::Deploy,
    salt: Salt,
    wallet_mode: &WalletSelectionMode,
) -> Result<fuel_tx::ContractId> {
    info!("  {} proxy contract", "Creating".bold().green());
    let user_addr = if *owner_account_address != Bech32Address::default() {
        anyhow::Ok(owner_account_address.clone())
    } else {
        // Check if the wallet exists and if not create it at the default path.
        match wallet_mode {
            WalletSelectionMode::ForcWallet(password) => {
                let default_path = default_wallet_path();
                check_and_create_wallet_at_default_path(&default_path)?;
                let account = first_user_account(&default_wallet_path(), password)?;
                *owner_account_address = account.clone();
                Ok(account)
            }
            WalletSelectionMode::Manual => {
                let secret_key =
                    select_manual_secret_key(command.default_signer, command.signing_key)
                        .ok_or_else(|| {
                            anyhow::anyhow!("couldn't resolve the secret key for manual signing")
                        })?;
                bech32_from_secret(&secret_key)
            }
        }
    }?;
    let user_addr_hex: fuels_core::types::Address = user_addr.into();
    let user_addr = format!("0x{}", user_addr_hex);
    let pkg_name = pkg.descriptor.manifest_file.project_name();
    let contract_addr = format!("0x{}", impl_contract);
    let proxy_contract = build_proxy_contract(&user_addr, &contract_addr, pkg_name, build_opts)?;
    info!("   {} proxy contract", "Deploying".bold().green());
    let proxy = deploy_pkg(
        command,
        &pkg.descriptor.manifest_file,
        &proxy_contract,
        salt,
        wallet_mode,
    )
    .await?;
    Ok(proxy)
}

async fn deploy_chunked(
    command: &cmd::Deploy,
    compiled: &BuiltPackage,
    salt: Salt,
    wallet_mode: &WalletSelectionMode,
    provider: &Provider,
    pkg_name: &str,
) -> anyhow::Result<(ContractId, Vec<ContractId>)> {
    // TODO: remove this clone.
    let contract_chunks = split_into_chunks(compiled.bytecode.bytes.clone(), MAX_CONTRACT_SIZE);
    let mut deployed_contracts = vec![];
    for contract_chunk in contract_chunks {
        let deployed_contract = contract_chunk
            .deploy(provider, &salt, command, wallet_mode)
            .await?;
        deployed_contracts.push(deployed_contract);
    }
    let deployed_contract_ids: Vec<String> = deployed_contracts
        .iter()
        .map(|deployed_contract| format!("0x{}", deployed_contract.contract_id()))
        .collect();

    let deployed_contracts: Vec<_> = deployed_contracts
        .iter()
        .map(|deployed_contract| deployed_contract.contract_id().clone())
        .collect();

    let program_abi = match &compiled.program_abi {
        sway_core::asm_generation::ProgramABI::Fuel(abi) => abi,
        _ => bail!("contract chunking is only supported with fuelVM"),
    };

    let loader_contract = build_loader_contract(
        program_abi,
        &deployed_contract_ids,
        deployed_contracts.len(),
        pkg_name,
        &build_opts_from_cmd(command),
    )?;

    let deployed_id = deploy_pkg(
        command,
        &loader_contract.descriptor.manifest_file,
        &loader_contract,
        salt,
        wallet_mode,
    )
    .await?;

    Ok((deployed_id, deployed_contracts))
}

/// Builds and deploys contract(s). If the given path corresponds to a workspace, all deployable members
/// will be built and deployed.
///
/// Upon success, returns the ID of each deployed contract in order of deployment.
///
/// When deploying a single contract, only that contract's ID is returned.
pub async fn deploy(command: cmd::Deploy) -> Result<Vec<DeployedContract>> {
    if command.unsigned {
        println_warning("--unsigned flag is deprecated, please prefer using --default-signer. Assuming `--default-signer` is passed. This means your transaction will be signed by an account that is funded by fuel-core by default for testing purposes.");
    }

    let mut deployed_contracts = Vec::new();
    let curr_dir = if let Some(ref path) = command.pkg.path {
        PathBuf::from(path)
    } else {
        std::env::current_dir()?
    };

    let build_opts = build_opts_from_cmd(&command);
    let built_pkgs = built_pkgs(&curr_dir, &build_opts)?;

    if built_pkgs.is_empty() {
        println_warning("No deployable contracts found in the current directory.");
        return Ok(deployed_contracts);
    }

    let contract_salt_map = if let Some(salt_input) = &command.salt {
        // If we're building 1 package, we just parse the salt as a string, ie. 0x00...
        // If we're building >1 package, we must parse the salt as a pair of strings, ie. contract_name:0x00...
        if built_pkgs.len() > 1 {
            let map = validate_and_parse_salts(
                salt_input,
                built_pkgs.iter().map(|b| &b.descriptor.manifest_file),
            )?;

            Some(map)
        } else {
            if salt_input.len() > 1 {
                bail!("More than 1 salt was specified when deploying a single contract");
            }

            // OK to index into salt_input and built_pkgs_with_manifest here,
            // since both are known to be len 1.
            let salt = salt_input[0]
                .parse::<Salt>()
                .map_err(|e| anyhow::anyhow!(e))
                .unwrap();
            let mut contract_salt_map = ContractSaltMap::default();
            contract_salt_map.insert(
                built_pkgs[0]
                    .descriptor
                    .manifest_file
                    .project_name()
                    .to_string(),
                salt,
            );
            Some(contract_salt_map)
        }
    } else {
        None
    };

    info!("  {} deployment", "Starting".bold().green());
    let wallet_mode = if command.default_signer || command.signing_key.is_some() {
        WalletSelectionMode::Manual
    } else {
        let password = prompt_forc_wallet_password(&default_wallet_path())?;
        WalletSelectionMode::ForcWallet(password)
    };

    let mut owner_account_address = Bech32Address::default();
    for pkg in built_pkgs {
        if pkg
            .descriptor
            .manifest_file
            .check_program_type(&[TreeType::Contract])
            .is_ok()
        {
            let salt = match (&contract_salt_map, command.default_salt) {
                (Some(map), false) => {
                    if let Some(salt) = map.get(pkg.descriptor.manifest_file.project_name()) {
                        *salt
                    } else {
                        Default::default()
                    }
                }
                (None, true) => Default::default(),
                (None, false) => rand::random(),
                (Some(_), true) => {
                    bail!("Both `--salt` and `--default-salt` were specified: must choose one")
                }
            };
            let node_url = get_node_url(&command.node, &pkg.descriptor.manifest_file.network)?;
            info!(
                "  {} contract: {}",
                "Deploying".bold().green(),
                &pkg.descriptor.name
            );
            let bytecode_size = pkg.bytecode.bytes.len();
            let (deployed_contract_id, chunk_ids) = if bytecode_size > MAX_CONTRACT_SIZE {
                // Deploy chunked
                let node_url = get_node_url(&command.node, &pkg.descriptor.manifest_file.network)?;
                let provider = Provider::connect(node_url).await?;
                deploy_chunked(
                    &command,
                    &pkg,
                    salt,
                    &wallet_mode,
                    &provider,
                    &pkg.descriptor.name,
                )
                .await?
            } else {
                // Deploy directly
                let deployed_contract_id = deploy_pkg(
                    &command,
                    &pkg.descriptor.manifest_file,
                    &pkg,
                    salt,
                    &wallet_mode,
                )
                .await?;
                (deployed_contract_id, vec![])
            };
            println!("Contract chunks deployed: {:#?}", chunk_ids);
            let proxy = &pkg.descriptor.manifest_file.proxy();
            let proxy_id = if let Some(proxy) = proxy {
                if proxy.enabled {
                    if let Some(proxy_addr) = &proxy.address {
                        // Make a call into the contract to update impl contract address to 'deployed_contract'.

                        // Create a contract instance for the proxy contract using default proxy contract abi and
                        // specified address.
                        info!("  {} proxy contract", "Updating".bold().green());
                        let provider = Provider::connect(node_url.clone()).await?;
                        // TODO: once https://github.com/FuelLabs/sway/issues/6071 is closed, this will return just a result
                        // and we won't need to handle the manual prompt based signature case.
                        let signing_key = select_secret_key(
                            &wallet_mode,
                            command.default_signer,
                            command.signing_key,
                            &provider,
                        )
                        .await?;

                        let signing_key = signing_key.ok_or_else(

                            || anyhow::anyhow!("proxy contract deployments are not supported with manual prompt based signing")
                        )?;
                        let proxy_contract =
                            ContractId::from_str(proxy_addr).map_err(|e| anyhow::anyhow!(e))?;

                        update_proxy_contract_target(
                            provider,
                            signing_key,
                            proxy_contract,
                            deployed_contract_id,
                        )
                        .await?;
                        Some(proxy_contract)
                    } else {
                        // Deploy a new proxy contract.
                        let deployed_proxy_contract = deploy_new_proxy(
                            &pkg,
                            &mut owner_account_address,
                            &deployed_contract_id,
                            &build_opts,
                            &command,
                            salt,
                            &wallet_mode,
                        )
                        .await?;

                        // Update manifest file such that the proxy address field points to the new proxy contract.
                        update_proxy_address_in_manifest(
                            &format!("0x{}", deployed_proxy_contract),
                            &pkg.descriptor.manifest_file,
                        )?;
                        Some(deployed_proxy_contract)
                    }
                } else {
                    None
                }
            } else {
                None
            };
            let deployed_contract = DeployedContract {
                id: deployed_contract_id,
                proxy: proxy_id,
            };
            deployed_contracts.push(deployed_contract);
        }
    }
    Ok(deployed_contracts)
}

/// Deploy a single pkg given deploy command and the manifest file
pub async fn deploy_pkg(
    command: &cmd::Deploy,
    manifest: &PackageManifestFile,
    compiled: &BuiltPackage,
    salt: Salt,
    wallet_mode: &WalletSelectionMode,
) -> Result<fuel_tx::ContractId> {
    let node_url = get_node_url(&command.node, &manifest.network)?;
    let client = FuelClient::new(node_url.clone())?;
    let bytecode = &compiled.bytecode.bytes;

    let mut storage_slots =
        if let Some(storage_slot_override_file) = &command.override_storage_slots {
            let storage_slots_file = std::fs::read_to_string(storage_slot_override_file)?;
            let storage_slots: Vec<StorageSlot> = serde_json::from_str(&storage_slots_file)?;
            storage_slots
        } else {
            compiled.storage_slots.clone()
        };
    storage_slots.sort();
    let contract = Contract::from(bytecode.as_slice());
    let root = contract.root();
    let state_root = Contract::initial_state_root(storage_slots.iter());
    let contract_id = contract.id(&salt, &root, &state_root);

    let provider = Provider::connect(node_url.clone()).await?;
    let tx_policies = TxPolicies::default();

    let mut tb = CreateTransactionBuilder::prepare_contract_deployment(
        bytecode.clone(),
        contract_id,
        state_root,
        salt,
        storage_slots.clone(),
        tx_policies,
    );
    let signing_key = select_secret_key(
        wallet_mode,
        command.default_signer || command.unsigned,
        command.signing_key,
        &provider,
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("failed to select a signer for the transaction"))?;
    let wallet = WalletUnlocked::new_from_private_key(signing_key, Some(provider.clone()));

    wallet.add_witnesses(&mut tb)?;
    wallet.adjust_for_fee(&mut tb, 0).await?;
    let tx = tb.build(provider).await?;
    let tx = Transaction::from(tx);

    let chain_id = client.chain_info().await?.consensus_parameters.chain_id();

    let deployment_request = client.submit_and_await_commit(&tx).map(|res| match res {
        Ok(logs) => match logs {
            TransactionStatus::Submitted { .. } => {
                bail!("contract {} deployment timed out", &contract_id);
            }
            TransactionStatus::Success { block_height, .. } => {
                let pkg_name = manifest.project_name();
                info!("\n\n  {} {pkg_name}!", "Deployed".bold().green());
                info!("  {}: {node_url}", "Network".bold().green());
                info!("  {}: 0x{contract_id}", "Contract ID".bold().green());
                info!("  {}: {}\n", "Block".bold().green(), &block_height);

                // Create a deployment artifact.
                let deployment_size = bytecode.len();
                let deployment_artifact = DeploymentArtifact {
                    transaction_id: format!("0x{}", tx.id(&chain_id)),
                    salt: format!("0x{}", salt),
                    network_endpoint: node_url.to_string(),
                    chain_id,
                    contract_id: format!("0x{}", contract_id),
                    deployment_size,
                    deployed_block_height: *block_height,
                };

                let output_dir = command
                    .pkg
                    .output_directory
                    .as_ref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| default_output_directory(manifest.dir()))
                    .join("deployments");
                deployment_artifact.to_file(&output_dir, pkg_name, contract_id)?;

                Ok(contract_id)
            }
            e => {
                bail!(
                    "contract {} failed to deploy due to an error: {:?}",
                    &contract_id,
                    e
                )
            }
        },
        Err(e) => bail!("{e}"),
    });
    // submit contract deployment with a timeout
    let contract_id = tokio::time::timeout(
        Duration::from_millis(TX_SUBMIT_TIMEOUT_MS),
        deployment_request,
    )
    .await
    .with_context(|| {
        format!(
            "Timed out waiting for contract {} to deploy. The transaction may have been dropped.",
            &contract_id
        )
    })??;
    Ok(contract_id)
}

fn build_opts_from_cmd(cmd: &cmd::Deploy) -> pkg::BuildOpts {
    pkg::BuildOpts {
        pkg: pkg::PkgOpts {
            path: cmd.pkg.path.clone(),
            offline: cmd.pkg.offline,
            terse: cmd.pkg.terse,
            locked: cmd.pkg.locked,
            output_directory: cmd.pkg.output_directory.clone(),
            json_abi_with_callpaths: cmd.pkg.json_abi_with_callpaths,
            ipfs_node: cmd.pkg.ipfs_node.clone().unwrap_or_default(),
        },
        print: pkg::PrintOpts {
            ast: cmd.print.ast,
            dca_graph: cmd.print.dca_graph.clone(),
            dca_graph_url_format: cmd.print.dca_graph_url_format.clone(),
            asm: cmd.print.asm(),
            bytecode: cmd.print.bytecode,
            bytecode_spans: false,
            ir: cmd.print.ir(),
            reverse_order: cmd.print.reverse_order,
        },
        time_phases: cmd.print.time_phases,
        metrics_outfile: cmd.print.metrics_outfile.clone(),
        minify: pkg::MinifyOpts {
            json_abi: cmd.minify.json_abi,
            json_storage_slots: cmd.minify.json_storage_slots,
        },
        build_profile: cmd.build_profile.clone(),
        release: cmd.build_profile == BuildProfile::RELEASE,
        error_on_warnings: false,
        binary_outfile: cmd.build_output.bin_file.clone(),
        debug_outfile: cmd.build_output.debug_file.clone(),
        build_target: BuildTarget::default(),
        tests: false,
        member_filter: pkg::MemberFilter::only_contracts(),
        experimental: ExperimentalFlags {
            new_encoding: !cmd.no_encoding_v1,
        },
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn setup_manifest_files() -> BTreeMap<String, PackageManifestFile> {
        let mut contract_to_manifest = BTreeMap::default();

        let manifests_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test")
            .join("data");

        for entry in manifests_dir.read_dir().unwrap() {
            let manifest =
                PackageManifestFile::from_file(entry.unwrap().path().join("Forc.toml")).unwrap();
            contract_to_manifest.insert(manifest.project_name().to_string(), manifest);
        }

        contract_to_manifest
    }

    #[test]
    fn test_parse_and_validate_salts_pass() {
        let mut manifests = setup_manifest_files();
        let mut expected = ContractSaltMap::new();
        let mut salt_strs = vec![];

        // Remove contracts with dependencies
        manifests.remove("contract_with_dep_with_salt_conflict");
        manifests.remove("contract_with_dep");

        for (index, manifest) in manifests.values().enumerate() {
            let salt = "0x0000000000000000000000000000000000000000000000000000000000000000";

            let salt_str = format!("{}:{salt}", manifest.project_name());
            salt_strs.push(salt_str.to_string());

            expected.insert(
                manifest.project_name().to_string(),
                salt.parse::<Salt>().unwrap(),
            );

            let got = validate_and_parse_salts(&salt_strs, manifests.values()).unwrap();
            assert_eq!(got.len(), index + 1);
            assert_eq!(got, expected);
        }
    }

    #[test]
    fn test_parse_and_validate_salts_duplicate_salt_input() {
        let manifests = setup_manifest_files();
        let first_name = manifests.first_key_value().unwrap().0;
        let salt: Salt = "0x0000000000000000000000000000000000000000000000000000000000000000"
            .parse()
            .unwrap();
        let salt_str = format!("{first_name}:{salt}");
        let err_message =
            format!("2 salts provided for contract '{first_name}':\n  {salt}\n  {salt}");

        assert_eq!(
            validate_and_parse_salts(&[salt_str.clone(), salt_str], manifests.values())
                .unwrap_err()
                .to_string(),
            err_message,
        );
    }

    #[test]
    fn test_parse_single_salt_multiple_manifests_malformed_input() {
        let manifests = setup_manifest_files();
        let salt_str =
            "contract_a=0x0000000000000000000000000000000000000000000000000000000000000000";
        let err_message =
            "Invalid salt provided - salt must be in the form <CONTRACT_NAME>:<SALT> when deploying a workspace";

        assert_eq!(
            validate_and_parse_salts(&[salt_str.to_string()], manifests.values())
                .unwrap_err()
                .to_string(),
            err_message,
        );
    }

    #[test]
    fn test_parse_multiple_salts_conflict() {
        let manifests = setup_manifest_files();
        let salt_str =
            "contract_with_dep:0x0000000000000000000000000000000000000000000000000000000000000001";
        let err_message =
            "Redeclaration of salt using the option '--salt' while a salt exists for contract 'contract_with_dep' \
            under the contract dependencies of the Forc.toml manifest for 'contract_with_dep_with_salt_conflict'\n\
            Existing salt: '0x0000000000000000000000000000000000000000000000000000000000000000',\n\
            You declared: '0x0000000000000000000000000000000000000000000000000000000000000001'\n";

        assert_eq!(
            validate_and_parse_salts(&[salt_str.to_string()], manifests.values())
                .unwrap_err()
                .to_string(),
            err_message,
        );
    }
}
