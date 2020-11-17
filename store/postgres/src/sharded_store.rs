use diesel::{connection::SimpleConnection, Connection};
use std::str::FromStr;
use std::{collections::BTreeMap, collections::HashMap, sync::Arc};

use graph::{
    data::subgraph::schema::MetadataType,
    prelude::{
        web3::types::{Address, H256},
        ApiSchema, BlockNumber, DeploymentState, DynTryFuture, Entity, EntityKey,
        EntityModification, EntityQuery, Error, EthereumBlockPointer, EthereumCallCache, Logger,
        MetadataOperation, NodeId, QueryExecutionError, QueryStore, Schema, StopwatchMetrics,
        Store as StoreTrait, StoreError, StoreEvent, StoreEventStreamBox, SubgraphDeploymentEntity,
        SubgraphDeploymentId, SubgraphDeploymentStore, SubgraphEntityPair, SubgraphName,
        SubgraphVersionSwitchingMode, PRIMARY_SHARD,
    },
};

use crate::store::{ReplicaId, Store};
use crate::{deployment, primary, primary::Site};

/// Multiplex store operations on subgraphs and deployments between a primary
/// and any number of additional storage shards. See [this document](../../docs/sharded.md)
/// for details on how storage is split up
pub struct ShardedStore {
    primary: Arc<Store>,
    stores: HashMap<String, Arc<Store>>,
}

impl ShardedStore {
    #[allow(dead_code)]
    pub fn new(stores: HashMap<String, Arc<Store>>) -> Self {
        assert_eq!(
            1,
            stores.len(),
            "The sharded store can only handle one shard for now"
        );
        let primary = stores
            .get(PRIMARY_SHARD)
            .expect("we always have a primary store")
            .clone();
        Self { primary, stores }
    }

    // Only needed for tests
    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    pub(crate) fn clear_storage_cache(&self) {
        for store in self.stores.values() {
            store.storage_cache.lock().unwrap().clear();
        }
    }

    fn site(&self, id: &SubgraphDeploymentId) -> Result<Arc<Site>, StoreError> {
        let conn = self.primary_conn()?;
        let site = conn
            .find_site(id)?
            .ok_or_else(|| StoreError::DeploymentNotFound(id.to_string()))?;

        // We'll eventually cache this, for now create a new Arc
        Ok(Arc::new(site))
    }

    fn store(&self, id: &SubgraphDeploymentId) -> Result<(&Arc<Store>, Arc<Site>), StoreError> {
        let site = self.site(id)?;
        let store = self
            .stores
            .get(&site.shard)
            .ok_or(StoreError::UnknownShard(site.shard.clone()))?;
        Ok((store, site))
    }

    fn create_deployment_internal(
        &self,
        name: SubgraphName,
        schema: &Schema,
        deployment: SubgraphDeploymentEntity,
        node_id: NodeId,
        mode: SubgraphVersionSwitchingMode,
        // replace == true is only used in tests; for non-test code, it must
        // be 'false'
        replace: bool,
    ) -> Result<(), StoreError> {
        #[cfg(not(debug_assertions))]
        assert!(!replace);

        // We only allow one shard (the primary) for now, so it is fine
        // to forward this to the primary store
        let shard = PRIMARY_SHARD.to_string();

        let deployment_store = self
            .stores
            .get(&shard)
            .ok_or_else(|| StoreError::UnknownShard(shard.clone()))?;
        let pconn = self.primary_conn()?;

        // TODO: Check this for behavior on failure
        let site = pconn.allocate_site(shard.clone(), &schema.id)?;

        let graft_site = deployment
            .graft_base
            .as_ref()
            .map(|base| pconn.find_existing_site(&base))
            .transpose()?;
        if let Some(ref graft_site) = graft_site {
            if &graft_site.shard != &shard {
                return Err(StoreError::ConstraintViolation(format!("Can not graft across shards. {} is in shard {}, and the base {} is in shard {}", site.deployment, site.shard, graft_site.deployment, graft_site.shard)));
            }
        }
        // We can only use this for the metadata subgraph, since the subgraph
        // we are creating does not exist in the database yet
        let meta_site = Site::meta(shard);

        let econn = deployment_store.get_entity_conn(&meta_site, ReplicaId::Main)?;

        let mut event = econn.transaction(|| -> Result<_, StoreError> {
            let exists = deployment::exists(&econn.conn, &site.deployment)?;
            let event = if replace || !exists {
                let ops = deployment.create_operations(&schema.id);
                deployment_store.apply_metadata_operations_with_conn(&econn, ops)?
            } else {
                StoreEvent::new(vec![])
            };

            if !exists {
                econn.create_schema(site.namespace.clone(), schema, graft_site)?;
            }

            Ok(event)
        })?;

        let exists_and_synced = |id: &SubgraphDeploymentId| {
            let (store, _) = self.store(id)?;
            let conn = store.get_conn()?;
            deployment::exists_and_synced(&conn, id.as_str())
        };

        pconn.transaction(|| -> Result<_, StoreError> {
            // Create subgraph, subgraph version, and assignment
            let changes = pconn.create_subgraph_version(
                name,
                &schema.id,
                node_id,
                mode,
                exists_and_synced,
            )?;
            event.changes.extend(changes);
            pconn.send_store_event(&event)?;
            Ok(())
        })
    }

    // Only for tests to simplify their handling of test fixtures, so that
    // tests can reset the block pointer of a subgraph by recreating it
    #[cfg(debug_assertions)]
    pub fn create_deployment_replace(
        &self,
        name: SubgraphName,
        schema: &Schema,
        deployment: SubgraphDeploymentEntity,
        node_id: NodeId,
        mode: SubgraphVersionSwitchingMode,
    ) -> Result<(), StoreError> {
        self.create_deployment_internal(name, schema, deployment, node_id, mode, true)
    }

    pub(crate) fn send_store_event(&self, event: &StoreEvent) -> Result<(), StoreError> {
        let conn = self.primary_conn()?;
        conn.send_store_event(event)
    }

    fn primary_conn(&self) -> Result<primary::Connection, StoreError> {
        let conn = self.primary.get_conn()?;
        Ok(primary::Connection::new(conn))
    }

    /// Delete all entities. This function exists solely for integration tests
    /// and should never be called from any other code. Unfortunately, Rust makes
    /// it very hard to export items just for testing
    #[cfg(debug_assertions)]
    pub fn delete_all_entities_for_test_use_only(&self) -> Result<(), StoreError> {
        let pconn = self.primary_conn()?;
        let schemas = pconn.sites()?;

        // Delete all subgraph schemas
        for schema in schemas {
            let (store, _) = self.store(&schema.deployment)?;
            let conn = store.get_conn()?;
            deployment::drop_entities(&conn, &schema.namespace)?;
        }

        // Delete metadata entities in each shard
        // Generated by running 'layout -g delete subgraphs.graphql'
        let query = "
        delete from subgraphs.ethereum_block_handler_filter_entity;
        delete from subgraphs.ethereum_contract_source;
        delete from subgraphs.dynamic_ethereum_contract_data_source;
        delete from subgraphs.ethereum_contract_abi;
        delete from subgraphs.subgraph;
        delete from subgraphs.subgraph_deployment;
        delete from subgraphs.ethereum_block_handler_entity;
        delete from subgraphs.subgraph_deployment_assignment;
        delete from subgraphs.ethereum_contract_mapping;
        delete from subgraphs.subgraph_version;
        delete from subgraphs.subgraph_manifest;
        delete from subgraphs.ethereum_call_handler_entity;
        delete from subgraphs.ethereum_contract_data_source;
        delete from subgraphs.ethereum_contract_data_source_template;
        delete from subgraphs.ethereum_contract_data_source_template_source;
        delete from subgraphs.ethereum_contract_event_handler;
    ";
        for store in self.stores.values() {
            let conn = store.get_conn()?;
            conn.batch_execute(query)?;
        }
        self.clear_storage_cache();
        Ok(())
    }
}

impl StoreTrait for ShardedStore {
    fn block_ptr(
        &self,
        id: SubgraphDeploymentId,
    ) -> Result<Option<EthereumBlockPointer>, failure::Error> {
        let (store, site) = self.store(&id)?;
        store.block_ptr(site.as_ref())
    }

    fn supports_proof_of_indexing<'a>(
        self: Arc<Self>,
        id: &'a SubgraphDeploymentId,
    ) -> DynTryFuture<'a, bool> {
        let (store, site) = self.store(&id).unwrap();
        store.clone().supports_proof_of_indexing(site)
    }

    fn get_proof_of_indexing<'a>(
        self: Arc<Self>,
        id: &'a SubgraphDeploymentId,
        indexer: &'a Option<Address>,
        block_hash: H256,
    ) -> DynTryFuture<'a, Option<[u8; 32]>> {
        let (store, site) = self.store(&id).unwrap();
        store
            .clone()
            .get_proof_of_indexing(site, indexer, block_hash)
    }

    fn get(&self, key: EntityKey) -> Result<Option<Entity>, QueryExecutionError> {
        let (store, site) = self.store(&key.subgraph_id)?;
        store.get(site.as_ref(), key)
    }

    fn get_many(
        &self,
        id: &SubgraphDeploymentId,
        ids_for_type: BTreeMap<&str, Vec<&str>>,
    ) -> Result<BTreeMap<String, Vec<Entity>>, StoreError> {
        let (store, site) = self.store(&id)?;
        store.get_many(site.as_ref(), ids_for_type)
    }

    fn find(&self, query: EntityQuery) -> Result<Vec<Entity>, QueryExecutionError> {
        let (store, site) = self.store(&query.subgraph_id)?;
        store.find(site.as_ref(), query)
    }

    fn find_one(&self, query: EntityQuery) -> Result<Option<Entity>, QueryExecutionError> {
        let (store, site) = self.store(&query.subgraph_id)?;
        store.find_one(site.as_ref(), query)
    }

    fn find_ens_name(&self, hash: &str) -> Result<Option<String>, QueryExecutionError> {
        self.primary.find_ens_name(hash)
    }

    fn transact_block_operations(
        &self,
        id: SubgraphDeploymentId,
        block_ptr_to: EthereumBlockPointer,
        mods: Vec<EntityModification>,
        stopwatch: StopwatchMetrics,
    ) -> Result<(), StoreError> {
        assert!(
            mods.in_shard(&id),
            "can only transact operations within one shard"
        );
        let (store, site) = self.store(&id)?;
        let event =
            store.transact_block_operations(site.as_ref(), block_ptr_to, mods, stopwatch)?;
        self.send_store_event(&event)
    }

    fn apply_metadata_operations(
        &self,
        target_deployment: &SubgraphDeploymentId,
        operations: Vec<MetadataOperation>,
    ) -> Result<(), StoreError> {
        assert!(
            operations.in_shard(target_deployment),
            "can only apply metadata operations for SubgraphDeployment and its subobjects"
        );

        let (store, site) = self.store(&target_deployment)?;
        let event = store.apply_metadata_operations(site.as_ref(), operations)?;
        self.send_store_event(&event)
    }

    fn revert_block_operations(
        &self,
        id: SubgraphDeploymentId,
        block_ptr_from: EthereumBlockPointer,
        block_ptr_to: EthereumBlockPointer,
    ) -> Result<(), StoreError> {
        let (store, site) = self.store(&id)?;
        let event = store.revert_block_operations(site.as_ref(), block_ptr_from, block_ptr_to)?;
        self.send_store_event(&event)
    }

    fn subscribe(&self, entities: Vec<SubgraphEntityPair>) -> StoreEventStreamBox {
        // Subscriptions always go through the primary
        self.primary.subscribe(entities)
    }

    fn deployment_state_from_name(
        &self,
        name: SubgraphName,
    ) -> Result<DeploymentState, StoreError> {
        let conn = self.primary_conn()?;
        let id = conn.transaction(|| conn.current_deployment_for_subgraph(name))?;
        self.deployment_state_from_id(id)
    }

    fn deployment_state_from_id(
        &self,
        id: SubgraphDeploymentId,
    ) -> Result<DeploymentState, StoreError> {
        let (store, _) = self.store(&id)?;
        store.deployment_state_from_id(id)
    }

    fn start_subgraph_deployment(
        &self,
        logger: &Logger,
        id: &SubgraphDeploymentId,
    ) -> Result<(), StoreError> {
        let (store, site) = self.store(id)?;

        let econn = store.get_entity_conn(&site, ReplicaId::Main)?;
        let pconn = self.primary_conn()?;
        let graft_base = match deployment::graft_pending(&econn.conn, id)? {
            Some((base_id, base_ptr)) => {
                let site = pconn.find_existing_site(&base_id)?;
                Some((site, base_ptr))
            }
            None => None,
        };
        econn.transaction(|| {
            deployment::unfail(&econn.conn, &site.deployment)?;
            econn.start_subgraph(logger, graft_base)
        })
    }

    fn block_number(
        &self,
        id: &SubgraphDeploymentId,
        block_hash: H256,
    ) -> Result<Option<BlockNumber>, StoreError> {
        let (store, _) = self.store(&id)?;
        store.block_number(id, block_hash)
    }

    fn query_store(
        self: Arc<Self>,
        id: &SubgraphDeploymentId,
        for_subscription: bool,
    ) -> Result<Arc<dyn QueryStore + Send + Sync>, StoreError> {
        assert!(
            !id.is_meta(),
            "a query store can only be retrieved for a concrete subgraph"
        );
        let (store, site) = self.store(&id)?;
        store.clone().query_store(site, for_subscription)
    }

    fn deployment_synced(&self, id: &SubgraphDeploymentId) -> Result<(), Error> {
        let pconn = self.primary_conn()?;
        let (dstore, _) = self.store(id)?;
        let dconn = dstore.get_conn()?;
        let event = pconn.transaction(|| -> Result<_, Error> {
            let changes = pconn.promote_deployment(id)?;
            Ok(StoreEvent::new(changes))
        })?;
        dconn.transaction(|| deployment::set_synced(&dconn, id))?;
        Ok(pconn.send_store_event(&event)?)
    }

    fn create_subgraph_deployment(
        &self,
        name: SubgraphName,
        schema: &Schema,
        deployment: SubgraphDeploymentEntity,
        node_id: NodeId,
        _network: String,
        mode: SubgraphVersionSwitchingMode,
    ) -> Result<(), StoreError> {
        self.create_deployment_internal(name, schema, deployment, node_id, mode, false)
    }

    fn create_subgraph(&self, name: SubgraphName) -> Result<String, StoreError> {
        let pconn = self.primary_conn()?;
        pconn.transaction(|| pconn.create_subgraph(&name))
    }

    fn remove_subgraph(&self, name: SubgraphName) -> Result<(), StoreError> {
        let pconn = self.primary_conn()?;
        pconn.transaction(|| -> Result<_, StoreError> {
            let changes = pconn.remove_subgraph(name)?;
            pconn.send_store_event(&StoreEvent::new(changes))
        })
    }

    fn reassign_subgraph(
        &self,
        id: &SubgraphDeploymentId,
        node_id: &NodeId,
    ) -> Result<(), StoreError> {
        let pconn = self.primary_conn()?;
        pconn.transaction(|| -> Result<_, StoreError> {
            let changes = pconn.reassign_subgraph(id, node_id)?;
            pconn.send_store_event(&StoreEvent::new(changes))
        })
    }
}

/// Methods similar to those for SubgraphDeploymentStore
impl SubgraphDeploymentStore for ShardedStore {
    fn input_schema(&self, id: &SubgraphDeploymentId) -> Result<Arc<Schema>, Error> {
        let (store, _) = self.store(&id)?;
        let info = store.subgraph_info(id)?;
        Ok(info.input)
    }

    fn api_schema(&self, id: &SubgraphDeploymentId) -> Result<Arc<ApiSchema>, Error> {
        let (store, _) = self.store(&id)?;
        let info = store.subgraph_info(id)?;
        Ok(info.api)
    }

    fn network_name(&self, id: &SubgraphDeploymentId) -> Result<Option<String>, Error> {
        let (store, _) = self.store(&id)?;
        let info = store.subgraph_info(id)?;
        Ok(info.network)
    }
}

impl EthereumCallCache for ShardedStore {
    fn get_call(
        &self,
        contract_address: Address,
        encoded_call: &[u8],
        block: EthereumBlockPointer,
    ) -> Result<Option<Vec<u8>>, failure::Error> {
        self.primary.get_call(contract_address, encoded_call, block)
    }

    fn set_call(
        &self,
        contract_address: Address,
        encoded_call: &[u8],
        block: EthereumBlockPointer,
        return_value: &[u8],
    ) -> Result<(), failure::Error> {
        self.primary
            .set_call(contract_address, encoded_call, block, return_value)
    }
}

trait ShardData {
    // Return `true` if this object resides in the shard for the
    // data for the given deployment
    fn in_shard(&self, id: &SubgraphDeploymentId) -> bool;
}

impl ShardData for MetadataType {
    fn in_shard(&self, _: &SubgraphDeploymentId) -> bool {
        use MetadataType::*;

        match self {
            Subgraph | SubgraphDeploymentAssignment => false,
            SubgraphDeployment
            | SubgraphManifest
            | EthereumContractDataSource
            | DynamicEthereumContractDataSource
            | EthereumContractSource
            | EthereumContractMapping
            | EthereumContractAbi
            | EthereumBlockHandlerEntity
            | EthereumBlockHandlerFilterEntity
            | EthereumCallHandlerEntity
            | EthereumContractEventHandler
            | EthereumContractDataSourceTemplate
            | EthereumContractDataSourceTemplateSource
            | SubgraphError => true,
        }
    }
}

impl ShardData for MetadataOperation {
    fn in_shard(&self, id: &SubgraphDeploymentId) -> bool {
        use MetadataOperation::*;
        match self {
            Set { entity, .. } | Remove { entity, .. } | Update { entity, .. } => {
                entity.in_shard(id)
            }
        }
    }
}

impl<T> ShardData for Vec<T>
where
    T: ShardData,
{
    fn in_shard(&self, id: &SubgraphDeploymentId) -> bool {
        self.iter().all(|op| op.in_shard(id))
    }
}

impl ShardData for EntityModification {
    fn in_shard(&self, id: &SubgraphDeploymentId) -> bool {
        let key = self.entity_key();
        let mod_id = &key.subgraph_id;

        if mod_id.is_meta() {
            // We do not flag an unknown MetadataType as an error here since
            // there are some valid types of metadata, e.g. SubgraphVersion
            // that are not reflected in the enum. We are just careful and
            // assume they are not stored in the same shard as subgraph data
            MetadataType::from_str(&key.entity_type)
                .ok()
                .map(|typ| typ.in_shard(id))
                .unwrap_or(false)
        } else {
            mod_id == id
        }
    }
}
