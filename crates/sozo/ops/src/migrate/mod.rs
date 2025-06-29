//! The migration module contains the logic for migrating the world.
//!
//! A migration is a sequence of steps that are executed in a specific order,
//! based on the [`WorldDiff`] that is computed from the local and remote world.
//!
//! Migrating a world can be sequenced as follows:
//!
//! 1. First the namespaces are synced.
//! 2. Then, all the resources (Contract, Models, Events) are synced, which can consist of:
//!    - Declaring the classes.
//!    - Registering the resources.
//!    - Upgrading the resources.
//! 3. Once resources are synced, the permissions are synced. Permissions can be in different
//!    states:
//!    - For newly registered resources, the permissions are applied.
//!    - For existing resources, the permissions are compared to the onchain state and the necessary
//!      changes are applied.
//! 4. All contracts that are not initialized are initialized, since permissions are applied,
//!    initialization of contracts can mutate resources.

use std::collections::HashMap;

use anyhow::anyhow;
use cainome::cairo_serde::{ByteArray, ClassHash, ContractAddress};
use dojo_utils::{Declarer, Deployer, Invoker, LabeledClass, TransactionResult, TxnConfig};
use dojo_world::config::calldata_decoder::decode_calldata;
use dojo_world::config::{metadata_config, ProfileConfig, ResourceConfig, WorldMetadata};
use dojo_world::constants::WORLD;
use dojo_world::contracts::abigen::world::ResourceMetadata;
use dojo_world::contracts::WorldContract;
use dojo_world::diff::{
    ExternalContractClassDiff, ExternalContractDiff, Manifest, ResourceDiff, WorldDiff, WorldStatus,
};
use dojo_world::local::ResourceLocal;
use dojo_world::metadata::MetadataStorage;
use dojo_world::remote::ResourceRemote;
use dojo_world::services::UploadService;
use dojo_world::{utils, ResourceType};
use starknet::accounts::{ConnectedAccount, SingleOwnerAccount};
use starknet::core::types::Call;
use starknet::providers::{AnyProvider, Provider};
use starknet::signers::LocalWallet;
use starknet_crypto::Felt;
use tracing::trace;

use crate::migration_ui::MigrationUi;

pub mod error;
pub use error::MigrationError;

#[derive(Debug)]
pub struct Migration<A>
where
    A: ConnectedAccount + Sync + Send,
{
    diff: WorldDiff,
    world: WorldContract<A>,
    txn_config: TxnConfig,
    profile_config: ProfileConfig,
    // This is only to retrieve the declarers or make custom calls.
    // Ideally, we want this rpc url to be exposed from the world.account.provider().
    rpc_url: String,
    guest: bool,
}

#[derive(Debug)]
pub struct MigrationResult {
    pub has_changes: bool,
    pub manifest: Manifest,
}

impl<A> Migration<A>
where
    A: ConnectedAccount + Sync + Send,
{
    /// Creates a new migration.
    pub fn new(
        diff: WorldDiff,
        world: WorldContract<A>,
        txn_config: TxnConfig,
        profile_config: ProfileConfig,
        rpc_url: String,
        guest: bool,
    ) -> Self {
        Self { diff, world, txn_config, profile_config, rpc_url, guest }
    }

    /// Migrates the world by syncing the namespaces, resources, permissions and initializing the
    /// contracts.
    ///
    /// TODO: find a more elegant way to pass an UI printer to the ops library than a hard coded
    /// spinner.
    pub async fn migrate(
        &self,
        ui: &mut MigrationUi,
    ) -> Result<MigrationResult, MigrationError<A::SignError>> {
        let world_has_changed = if !self.guest { self.ensure_world(ui).await? } else { false };

        let resources_have_changed =
            if !self.diff.is_synced() { self.sync_resources(ui).await? } else { false };

        let permissions_have_changed = self.sync_permissions(ui).await?;

        let contracts_have_changed = self.initialize_contracts(ui).await?;

        let external_contracts_have_changed = self.sync_external_contracts(ui).await?;

        Ok(MigrationResult {
            has_changes: world_has_changed
                || resources_have_changed
                || permissions_have_changed
                || external_contracts_have_changed
                || contracts_have_changed,
            manifest: Manifest::new(&self.diff),
        })
    }

    /// Upload resources metadata to IPFS and update the ResourceMetadata Dojo model.
    ///
    /// # Arguments
    ///
    /// # Returns
    pub async fn upload_metadata(
        &self,
        ui: &mut MigrationUi,
        service: &mut impl UploadService,
    ) -> anyhow::Result<()> {
        ui.update_text("Uploading metadata...");

        let mut invoker = Invoker::new(&self.world.account, self.txn_config);

        // world
        let current_hash = self.diff.world_info.metadata_hash;
        let new_metadata = WorldMetadata::from(self.diff.profile_config.world.clone());

        let res = new_metadata.upload_if_changed(service, current_hash).await?;

        if let Some((new_uri, new_hash)) = res {
            trace!(new_uri, new_hash = format!("{:#066x}", new_hash), "World metadata updated.");

            invoker.add_call(self.world.set_metadata_getcall(&ResourceMetadata {
                resource_id: WORLD,
                metadata_uri: ByteArray::from_string(&new_uri)?,
                metadata_hash: new_hash,
            }));
        }

        // contracts
        if let Some(configs) = &self.diff.profile_config.contracts {
            let calls = self.upload_metadata_from_resource_config(service, configs).await?;
            invoker.extend_calls(calls);
        }

        // libraries
        if let Some(configs) = &self.diff.profile_config.libraries {
            let calls = self.upload_metadata_from_resource_config(service, configs).await?;
            invoker.extend_calls(calls);
        }

        // models
        if let Some(configs) = &self.diff.profile_config.models {
            let calls = self.upload_metadata_from_resource_config(service, configs).await?;
            invoker.extend_calls(calls);
        }

        // events
        if let Some(configs) = &self.diff.profile_config.events {
            let calls = self.upload_metadata_from_resource_config(service, configs).await?;
            invoker.extend_calls(calls);
        }

        if self.do_multicall() {
            ui.update_text_boxed(format!("Uploading {} metadata...", invoker.calls.len()));
            invoker.multicall().await.map_err(|e| anyhow!(e.to_string()))?;
        } else {
            ui.update_text_boxed(format!(
                "Uploading {} metadata (sequentially)...",
                invoker.calls.len()
            ));
            invoker.invoke_all_sequentially().await.map_err(|e| anyhow!(e.to_string()))?;
        }

        Ok(())
    }

    async fn upload_metadata_from_resource_config(
        &self,
        service: &mut impl UploadService,
        config: &[ResourceConfig],
    ) -> anyhow::Result<Vec<Call>> {
        let mut calls = vec![];

        for item in config {
            let selector = dojo_types::naming::compute_selector_from_tag_or_name(&item.tag);

            let current_hash =
                self.diff.resources.get(&selector).map_or(Felt::ZERO, |r| r.metadata_hash());

            let new_metadata = metadata_config::ResourceMetadata::from(item.clone());

            let res = new_metadata.upload_if_changed(service, current_hash).await?;

            if let Some((new_uri, new_hash)) = res {
                trace!(
                    tag = item.tag,
                    new_uri,
                    new_hash = format!("{:#066x}", new_hash),
                    "Resource metadata updated."
                );

                calls.push(self.world.set_metadata_getcall(&ResourceMetadata {
                    resource_id: selector,
                    metadata_uri: ByteArray::from_string(&new_uri)?,
                    metadata_hash: new_hash,
                }));
            }
        }

        Ok(calls)
    }

    /// Returns whether multicall should be used. By default, it is enabled.
    fn do_multicall(&self) -> bool {
        self.profile_config.migration.as_ref().is_none_or(|m| !m.disable_multicall.unwrap_or(false))
    }

    /// For all contracts that are not initialized, initialize them by using the init call arguments
    /// found in the [`ProfileConfig`].
    ///
    /// Returns true if at least one contract has been initialized, false otherwise.
    async fn initialize_contracts(
        &self,
        ui: &mut MigrationUi,
    ) -> Result<bool, MigrationError<A::SignError>> {
        ui.update_text("Initializing contracts...");

        let mut invoker = Invoker::new(&self.world.account, self.txn_config);

        let init_call_args = if let Some(init_call_args) = &self.profile_config.init_call_args {
            init_call_args.clone()
        } else {
            HashMap::new()
        };

        // Ensure we can order the contracts to initialize, if specified.
        // Keeps the tag matched to the call to initialize.
        let ordered_init_tags = self
            .profile_config
            .migration
            .as_ref()
            .map_or(vec![], |m| m.order_inits.clone().unwrap_or_default());

        // Keeps map between the order index and the call to initialize.
        let mut ordered_init_calls = HashMap::new();

        for (selector, resource) in &self.diff.resources {
            if resource.resource_type() == ResourceType::Contract {
                let tag = resource.tag();

                // TODO: maybe we want a resource diff with a new variant. Where the migration
                // is skipped, but we still have the local resource.
                if self.profile_config.is_skipped(&tag) {
                    trace!(tag = resource.tag(), "Contract init skipping resource.");
                    continue;
                }

                let (do_init, init_call_args) = match resource {
                    ResourceDiff::Created(ResourceLocal::Contract(_)) => {
                        (true, init_call_args.get(&tag))
                    }
                    ResourceDiff::Updated(_, ResourceRemote::Contract(contract)) => {
                        (!contract.is_initialized, init_call_args.get(&tag))
                    }
                    ResourceDiff::Synced(_, ResourceRemote::Contract(contract)) => {
                        (!contract.is_initialized, init_call_args.get(&tag))
                    }
                    _ => (false, None),
                };

                if do_init {
                    // The injection of class hash and addresses is no longer supported since the
                    // world contains an internal DNS.
                    let args = if let Some(args) = init_call_args {
                        decode_calldata(args).map_err(|_| MigrationError::InitCallArgs)?
                    } else {
                        vec![]
                    };

                    trace!(tag, ?args, "Initializing contract.");

                    if let Some(order_index) = ordered_init_tags.iter().position(|t| *t == tag) {
                        ordered_init_calls
                            .insert(order_index, self.world.init_contract_getcall(selector, &args));
                    } else {
                        invoker.add_call(self.world.init_contract_getcall(selector, &args));
                    }
                }
            }
        }

        if !ordered_init_calls.is_empty() {
            let mut ordered_keys: Vec<_> = ordered_init_calls.keys().cloned().collect();
            ordered_keys.sort();

            let ordered_calls: Vec<_> = ordered_keys
                .into_iter()
                .map(|k| ordered_init_calls.get(&k).expect("Ordered call must exist.").clone())
                .collect();

            invoker.extends_ordered(ordered_calls);
        }

        let has_changed = !invoker.calls.is_empty();

        if !invoker.calls.is_empty() {
            if self.do_multicall() {
                let ui_text = format!("Initializing {} contracts...", invoker.calls.len());
                ui.update_text_boxed(ui_text);

                invoker.multicall().await?;
            } else {
                let ui_text =
                    format!("Initializing {} contracts (sequentially)...", invoker.calls.len());
                ui.update_text_boxed(ui_text);

                invoker.invoke_all_sequentially().await?;
            }
        }

        Ok(has_changed)
    }

    /// Syncs the permissions.
    ///
    /// This first version is naive, and only applies the local permissions to the resources, if the
    /// permission is not already set onchain.
    ///
    /// TODO: An other function must be added to sync the remote permissions to the local ones,
    /// and allow the user to reset the permissions onchain to the local ones.
    ///
    /// TODO: for error message, we need the name + namespace (or the tag for non-namespace
    /// resources). Change `DojoSelector` with a struct containing the local definition of an
    /// overlay resource, which can contain also writers.
    ///
    /// Returns true if at least one permission has changed, false otherwise.
    async fn sync_permissions(
        &self,
        ui: &mut MigrationUi,
    ) -> Result<bool, MigrationError<A::SignError>> {
        ui.update_text("Syncing permissions...");

        let mut invoker = Invoker::new(&self.world.account, self.txn_config);

        // Only takes the local permissions that are not already set onchain to apply them.
        for (selector, resource) in &self.diff.resources {
            if self.profile_config.is_skipped(&resource.tag()) {
                trace!(tag = resource.tag(), "Sync permissions skipping resource.");
                continue;
            }

            for pdiff in self.diff.get_writers(*selector).only_local() {
                trace!(
                    target = resource.tag(),
                    grantee_tag = pdiff.tag.unwrap_or_default(),
                    grantee_address = format!("{:#066x}", pdiff.address),
                    "Granting writer permission."
                );

                invoker.add_call(
                    self.world.grant_writer_getcall(selector, &ContractAddress(pdiff.address)),
                );
            }

            for pdiff in self.diff.get_owners(*selector).only_local() {
                trace!(
                    target = resource.tag(),
                    grantee_tag = pdiff.tag.unwrap_or_default(),
                    grantee_address = format!("{:#066x}", pdiff.address),
                    "Granting owner permission."
                );

                invoker.add_call(
                    self.world.grant_owner_getcall(selector, &ContractAddress(pdiff.address)),
                );
            }
        }

        let has_changed = !invoker.calls.is_empty();

        if self.do_multicall() {
            let ui_text = format!("Syncing {} permissions...", invoker.calls.len());
            ui.update_text_boxed(ui_text);

            invoker.multicall().await?;
        } else {
            let ui_text = format!("Syncing {} permissions (sequentially)...", invoker.calls.len());
            ui.update_text_boxed(ui_text);

            invoker.invoke_all_sequentially().await?;
        }

        Ok(has_changed)
    }

    /// Declare classes.
    async fn declare_classes(
        &self,
        ui: &mut MigrationUi,
        classes: HashMap<Felt, LabeledClass>,
    ) -> Result<(), MigrationError<A::SignError>> {
        // Declaration can be slow, and can be speed up by using multiple accounts.
        // Since migrator account from `self.world.account` is under the [`ConnectedAccount`] trait,
        // we can group it with the predeployed accounts which are concrete types.
        let accounts = self.get_accounts().await;
        let n_classes = classes.len();

        if accounts.is_empty() {
            trace!("Declaring classes with migrator account.");
            let mut declarer = Declarer::new(&self.world.account, self.txn_config);
            declarer.extend_classes(classes.into_values().collect());

            let ui_text = format!("Declaring {} classes...", n_classes);
            ui.update_text_boxed(ui_text);

            declarer.declare_all().await?;
        } else {
            trace!("Declaring classes with {} accounts.", accounts.len());
            let mut declarers = vec![];
            for account in accounts {
                declarers.push(Declarer::new(account, self.txn_config));
            }

            for (idx, (_, labeled_class)) in classes.into_iter().enumerate() {
                let declarer_idx = idx % declarers.len();
                declarers[declarer_idx].add_class(labeled_class.clone());
            }

            let ui_text =
                format!("Declaring {} classes with {} accounts...", n_classes, declarers.len());
            ui.update_text_boxed(ui_text);

            let declarers_futures =
                futures::future::join_all(declarers.into_iter().map(|d| d.declare_all())).await;

            for declarer_results in declarers_futures {
                if let Err(e) = declarer_results {
                    // The issue is that `e` is bound to concrete type `SingleOwnerAccount`.
                    // Thus, we can't return `e` directly.
                    // Might have a better solution by addind a new variant?
                    if e.to_string().contains("Class already declared") {
                        // If the class is already declared, it might be because it was already
                        // declared in a previous run or an other declarer.
                        continue;
                    }

                    return Err(MigrationError::DeclareClassError(e.to_string()));
                }
            }
        }

        Ok(())
    }

    /// Syncs the resources by declaring the classes and registering/upgrading the resources.
    ///
    /// Returns true if at least one resource has changed, false otherwise.
    async fn sync_resources(
        &self,
        ui: &mut MigrationUi,
    ) -> Result<bool, MigrationError<A::SignError>> {
        ui.update_text("Syncing resources...");

        let mut invoker = Invoker::new(&self.world.account, self.txn_config);

        // Namespaces must be synced first, since contracts, models and events are namespaced.
        self.namespaces_getcalls(&mut invoker).await?;

        let mut classes: HashMap<Felt, LabeledClass> = HashMap::new();
        let mut n_resources = 0;

        // Collects the calls and classes to be declared to sync the resources.
        for resource in self.diff.resources.values() {
            if self.profile_config.is_skipped(&resource.tag()) {
                trace!(tag = resource.tag(), "Sync skipping resource.");
                continue;
            }

            match resource.resource_type() {
                ResourceType::Contract => {
                    let (contract_calls, contract_classes) =
                        self.contracts_calls_classes(resource).await?;

                    if !contract_calls.is_empty() {
                        n_resources += 1;
                    }

                    invoker.extend_calls(contract_calls);
                    classes.extend(contract_classes);
                }
                ResourceType::Library => {
                    let (library_calls, library_classes) =
                        self.libraries_calls_classes(resource).await?;

                    if !library_calls.is_empty() {
                        n_resources += 1;
                    }

                    invoker.extend_calls(library_calls);
                    classes.extend(library_classes);
                }
                ResourceType::Model => {
                    let (model_calls, model_classes) = self.models_calls_classes(resource).await?;

                    if !model_calls.is_empty() {
                        n_resources += 1;
                    }

                    invoker.extend_calls(model_calls);
                    classes.extend(model_classes);
                }
                ResourceType::Event => {
                    let (event_calls, event_classes) = self.events_calls_classes(resource).await?;

                    if !event_calls.is_empty() {
                        n_resources += 1;
                    }

                    invoker.extend_calls(event_calls);
                    classes.extend(event_classes);
                }
                _ => continue,
            }
        }

        let has_classes = !classes.is_empty();
        let has_calls = !invoker.calls.is_empty();
        let has_changed = has_classes || has_calls;

        self.declare_classes(ui, classes).await?;

        if self.do_multicall() {
            let ui_text = format!("Registering {} resources...", n_resources);
            ui.update_text_boxed(ui_text);

            invoker.multicall().await?;
        } else {
            let ui_text = format!("Registering {} resources (sequentially)...", n_resources);
            ui.update_text_boxed(ui_text);

            invoker.invoke_all_sequentially().await?;
        }

        Ok(has_changed)
    }

    /// Syncs the external contracts by declaring their classes and deploying them with
    /// configured constructor data.
    ///
    /// Returns true if at least one external contract has changed, false otherwise.
    async fn sync_external_contracts(
        &self,
        ui: &mut MigrationUi,
    ) -> Result<bool, MigrationError<A::SignError>> {
        let ui_text =
            format!("Syncing {} external contracts...", self.diff.external_contracts.len());
        ui.update_text_boxed(ui_text);

        let mut invoker = Invoker::new(&self.world.account, self.txn_config);

        // declaring external contract classes
        let classes: HashMap<_, _> = self
            .diff
            .external_contract_classes
            .iter()
            .filter_map(|(_, c)| self.external_contract_classes(c))
            .collect();

        let ui_text = format!("Declaring {} external contract classes...", classes.len());
        ui.update_text_boxed(ui_text);

        self.declare_classes(ui, classes).await?;

        // then deploying new external contracts
        let deployer = Deployer::new(&self.world.account, self.txn_config);

        for contract in self.diff.external_contracts.values() {
            if let ExternalContractDiff::Created(contract) = contract {
                if let Some((_, call)) = deployer
                    .deploy_via_udc_getcall(
                        contract.class_hash,
                        contract.salt,
                        &contract.raw_constructor_data,
                        Felt::ZERO,
                    )
                    .await?
                {
                    invoker.add_call(call);
                }
            }
        }

        let has_changed = !invoker.calls.is_empty();

        if self.do_multicall() {
            let ui_text = format!("Deploying {} external contracts...", invoker.calls.len());
            ui.update_text_boxed(ui_text);

            invoker.multicall().await?;
        } else {
            let ui_text =
                format!("Deploying {} external contracts (sequentially)...", invoker.calls.len());
            ui.update_text_boxed(ui_text);

            invoker.invoke_all_sequentially().await?;
        }

        Ok(has_changed)
    }

    /// Returns the calls required to sync the namespaces.
    async fn namespaces_getcalls(
        &self,
        invoker: &mut Invoker<&A>,
    ) -> Result<(), MigrationError<A::SignError>> {
        for namespace_selector in &self.diff.namespaces {
            // TODO: abstract this expect by having a function exposed in the diff.
            let resource =
                self.diff.resources.get(namespace_selector).expect("Namespace not found in diff.");

            if let ResourceDiff::Created(ResourceLocal::Namespace(namespace)) = resource {
                trace!(name = namespace.name, "Registering namespace.");

                invoker.add_call(
                    self.world
                        .register_namespace_getcall(&ByteArray::from_string(&namespace.name)?),
                );
            }
        }

        Ok(())
    }

    /// Gathers the calls required to sync the contracts and classes to be declared.
    ///
    /// Currently, classes are cloned to be flattened, this is not ideal but the [`WorldDiff`]
    /// will be required later.
    /// If we could extract the info before syncing the resources, then we could avoid cloning the
    /// classes.
    ///
    /// Returns a tuple of calls and (casm_class_hash, class) to be declared.
    async fn contracts_calls_classes(
        &self,
        resource: &ResourceDiff,
    ) -> Result<(Vec<Call>, HashMap<Felt, LabeledClass>), MigrationError<A::SignError>> {
        let mut calls = vec![];
        let mut classes = HashMap::new();

        let namespace = resource.namespace();
        let ns_bytearray = ByteArray::from_string(&namespace)?;
        let tag = resource.tag();

        if let ResourceDiff::Created(ResourceLocal::Contract(contract)) = resource {
            trace!(
                namespace,
                name = contract.common.name,
                class_hash = format!("{:#066x}", contract.common.class_hash),
                "Registering contract."
            );

            let casm_class_hash = contract.common.casm_class_hash;
            let class = contract.common.class.clone().flatten()?;

            classes.insert(
                casm_class_hash,
                LabeledClass { label: tag.clone(), casm_class_hash, class },
            );

            calls.push(self.world.register_contract_getcall(
                &contract.dojo_selector(),
                &ns_bytearray,
                &ClassHash(contract.common.class_hash),
            ));
        }

        if let ResourceDiff::Updated(
            ResourceLocal::Contract(contract_local),
            ResourceRemote::Contract(_contract_remote),
        ) = resource
        {
            trace!(
                namespace,
                name = contract_local.common.name,
                class_hash = format!("{:#066x}", contract_local.common.class_hash),
                "Upgrading contract."
            );

            let casm_class_hash = contract_local.common.casm_class_hash;
            let class = contract_local.common.class.clone().flatten()?;

            classes.insert(
                casm_class_hash,
                LabeledClass { label: tag.clone(), casm_class_hash, class },
            );

            calls.push(self.world.upgrade_contract_getcall(
                &ns_bytearray,
                &ClassHash(contract_local.common.class_hash),
            ));
        }

        Ok((calls, classes))
    }

    /// Gathers the calls required to sync the libraries' classes to be declared.
    ///
    /// Returns a tuple of calls and (casm_class_hash, class) to be declared.
    async fn libraries_calls_classes(
        &self,
        resource: &ResourceDiff,
    ) -> Result<(Vec<Call>, HashMap<Felt, LabeledClass>), MigrationError<A::SignError>> {
        let mut calls = vec![];
        let mut classes = HashMap::new();

        let namespace = resource.namespace();
        let ns_bytearray = ByteArray::from_string(&namespace)?;
        let tag = resource.tag();

        if let ResourceDiff::Created(ResourceLocal::Library(library)) = resource {
            trace!(
                namespace,
                name = library.common.name,
                class_hash = format!("{:#066x}", library.common.class_hash),
                "Registering library."
            );

            let casm_class_hash = library.common.casm_class_hash;
            let class = library.common.class.clone().flatten()?;

            classes.insert(
                casm_class_hash,
                LabeledClass { label: tag.clone(), casm_class_hash, class },
            );

            let name = ByteArray::from_string(&library.common.name).unwrap();
            let version = ByteArray::from_string(&library.version).unwrap();
            calls.push(self.world.register_library_getcall(
                &ns_bytearray,
                &ClassHash(library.common.class_hash),
                &name,
                &version,
            ));
        }

        if let ResourceDiff::Updated(
            ResourceLocal::Library(_library_local),
            ResourceRemote::Library(_library_remote),
        ) = resource
        {
            panic!("libraries cannot be updated!")
        }

        Ok((calls, classes))
    }

    /// Returns the calls required to sync the models and gather the classes to be declared.
    ///
    /// Returns a tuple of calls and (casm_class_hash, class) to be declared.
    async fn models_calls_classes(
        &self,
        resource: &ResourceDiff,
    ) -> Result<(Vec<Call>, HashMap<Felt, LabeledClass>), MigrationError<A::SignError>> {
        let mut calls = vec![];
        let mut classes = HashMap::new();

        let namespace = resource.namespace();
        let ns_bytearray = ByteArray::from_string(&namespace)?;
        let tag = resource.tag();

        if let ResourceDiff::Created(ResourceLocal::Model(model)) = resource {
            trace!(
                namespace,
                name = model.common.name,
                class_hash = format!("{:#066x}", model.common.class_hash),
                "Registering model."
            );

            let casm_class_hash = model.common.casm_class_hash;
            let class = model.common.class.clone().flatten()?;

            classes.insert(
                casm_class_hash,
                LabeledClass { label: tag.clone(), casm_class_hash, class },
            );

            calls.push(
                self.world
                    .register_model_getcall(&ns_bytearray, &ClassHash(model.common.class_hash)),
            );
        }

        if let ResourceDiff::Updated(
            ResourceLocal::Model(model_local),
            ResourceRemote::Model(_model_remote),
        ) = resource
        {
            trace!(
                namespace,
                name = model_local.common.name,
                class_hash = format!("{:#066x}", model_local.common.class_hash),
                "Upgrading model."
            );

            let casm_class_hash = model_local.common.casm_class_hash;
            let class = model_local.common.class.clone().flatten()?;

            classes.insert(
                casm_class_hash,
                LabeledClass { label: tag.clone(), casm_class_hash, class },
            );

            calls.push(
                self.world.upgrade_model_getcall(
                    &ns_bytearray,
                    &ClassHash(model_local.common.class_hash),
                ),
            );
        }

        Ok((calls, classes))
    }

    /// Returns the calls required to sync the events and gather the classes to be declared.
    ///
    /// Returns a tuple of calls and (casm_class_hash, class) to be declared.
    async fn events_calls_classes(
        &self,
        resource: &ResourceDiff,
    ) -> Result<(Vec<Call>, HashMap<Felt, LabeledClass>), MigrationError<A::SignError>> {
        let mut calls = vec![];
        let mut classes = HashMap::new();

        let namespace = resource.namespace();
        let ns_bytearray = ByteArray::from_string(&namespace)?;
        let tag = resource.tag();

        if let ResourceDiff::Created(ResourceLocal::Event(event)) = resource {
            trace!(
                namespace,
                name = event.common.name,
                class_hash = format!("{:#066x}", event.common.class_hash),
                "Registering event."
            );

            let casm_class_hash = event.common.casm_class_hash;
            let class = event.common.class.clone().flatten()?;

            classes.insert(
                casm_class_hash,
                LabeledClass { label: tag.clone(), casm_class_hash, class },
            );

            calls.push(
                self.world
                    .register_event_getcall(&ns_bytearray, &ClassHash(event.common.class_hash)),
            );
        }

        if let ResourceDiff::Updated(
            ResourceLocal::Event(event_local),
            ResourceRemote::Event(_event_remote),
        ) = resource
        {
            trace!(
                namespace,
                name = event_local.common.name,
                class_hash = format!("{:#066x}", event_local.common.class_hash),
                "Upgrading event."
            );

            let casm_class_hash = event_local.common.casm_class_hash;
            let class = event_local.common.class.clone().flatten()?;

            classes.insert(
                casm_class_hash,
                LabeledClass { label: tag.clone(), casm_class_hash, class },
            );

            calls.push(
                self.world.upgrade_event_getcall(
                    &ns_bytearray,
                    &ClassHash(event_local.common.class_hash),
                ),
            );
        }

        Ok((calls, classes))
    }

    /// Get the external contract class info to be declared.
    ///
    /// Returns a tuple with the CASM class hash and class info.
    /// If the class is already declared, returns None.
    fn external_contract_classes(
        &self,
        contract_class: &ExternalContractClassDiff,
    ) -> Option<(Felt, LabeledClass)> {
        match contract_class {
            ExternalContractClassDiff::Created(c) => Some((
                c.casm_class_hash,
                LabeledClass {
                    label: c.contract_name.clone(),
                    casm_class_hash: c.casm_class_hash,
                    class: c.class.clone().flatten().unwrap(),
                },
            )),
            _ => None,
        }
    }

    /// Ensures the world is declared and deployed if necessary.
    ///
    /// Returns true if the world has to be deployed/updated, false otherwise.
    async fn ensure_world(
        &self,
        ui: &mut MigrationUi,
    ) -> Result<bool, MigrationError<A::SignError>> {
        match &self.diff.world_info.status {
            WorldStatus::Synced => return Ok(false),
            WorldStatus::NotDeployed => {
                ui.update_text("Deploying the world...");
                trace!("Deploying the first world.");

                let labeled_class = LabeledClass {
                    label: "world".to_string(),
                    casm_class_hash: self.diff.world_info.casm_class_hash,
                    class: self.diff.world_info.class.clone().flatten()?,
                };

                Declarer::declare(labeled_class, &self.world.account, &self.txn_config).await?;

                // We want to wait for the receipt to be able to print the
                // world block number.
                let mut txn_config = self.txn_config;
                txn_config.wait = true;
                txn_config.receipt = true;

                let deployer = Deployer::new(&self.world.account, txn_config);

                let res = deployer
                    .deploy_via_udc(
                        self.diff.world_info.class_hash,
                        utils::world_salt(&self.profile_config.world.seed)?,
                        &[self.diff.world_info.class_hash],
                        Felt::ZERO,
                    )
                    .await?;

                match res {
                    TransactionResult::HashReceipt(hash, receipt) => {
                        let block_msg = if let Some(n) = receipt.block.block_number() {
                            n.to_string()
                        } else {
                            // If we are in the pending block, we must get the latest block of the
                            // chain to display it to the user.
                            let provider = &self.world.account.provider();

                            format!(
                                "pending ({})",
                                provider.block_number().await.map_err(MigrationError::Provider)?
                            )
                        };

                        ui.stop_and_persist_boxed(
                            "🌍",
                            format!(
                                "World deployed at block {} with txn hash: {:#066x}",
                                block_msg, hash
                            ),
                        );

                        ui.restart("World deployed, continuing...");
                    }
                    _ => unreachable!(),
                }
            }
            WorldStatus::NewVersion => {
                trace!("Upgrading the world.");
                ui.update_text("Upgrading the world...");

                let labeled_class = LabeledClass {
                    label: "world".to_string(),
                    casm_class_hash: self.diff.world_info.casm_class_hash,
                    class: self.diff.world_info.class.clone().flatten()?,
                };

                Declarer::declare(labeled_class, &self.world.account, &self.txn_config).await?;

                let mut invoker = Invoker::new(&self.world.account, self.txn_config);

                invoker.add_call(
                    self.world.upgrade_getcall(&ClassHash(self.diff.world_info.class_hash)),
                );

                invoker.multicall().await?;
            }
        };

        Ok(true)
    }

    /// Returns the accounts to use for the migration.
    ///
    /// This is useful to use multiple accounts since the declare transaction is nonce-based,
    /// and can only be parallelized by using different accounts.
    ///
    /// Accounts can come from the profile config, otherwise we fallback to the predeployed
    /// accounts.
    async fn get_accounts(&self) -> Vec<SingleOwnerAccount<AnyProvider, LocalWallet>> {
        // TODO: if profile config have some migrators, use them instead.

        // If the RPC provider does not support the predeployed accounts, this will fail silently.
        dojo_utils::get_predeployed_accounts(&self.world.account, &self.rpc_url)
            .await
            .unwrap_or_default()
    }
}
