// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use api::v1::meta::Role;
use catalog::remote::RemoteCatalogManager;
use client::Client;
use common_base::Plugins;
use common_grpc::channel_manager::ChannelManager;
use common_meta::peer::Peer;
use common_meta::DatanodeId;
use common_runtime::Builder as RuntimeBuilder;
use common_test_util::temp_dir::create_temp_dir;
use datanode::datanode::{DatanodeOptions, ObjectStoreConfig};
use datanode::instance::Instance as DatanodeInstance;
use frontend::datanode::DatanodeClients;
use frontend::instance::Instance as FrontendInstance;
use meta_client::client::MetaClientBuilder;
use meta_srv::metasrv::{MetaSrv, MetaSrvOptions};
use meta_srv::mocks::MockInfo;
use meta_srv::service::store::kv::KvStoreRef;
use meta_srv::service::store::memory::MemStore;
use servers::grpc::GrpcServer;
use servers::query_handler::grpc::ServerGrpcQueryHandlerAdaptor;
use servers::Mode;
use tonic::transport::Server;
use tower::service_fn;

use crate::test_util::{
    create_datanode_opts, create_tmp_dir_and_datanode_opts, StorageGuard, StorageType, WalGuard,
};

pub struct GreptimeDbCluster {
    pub storage_guards: Vec<StorageGuard>,
    _wal_guards: Vec<WalGuard>,

    pub datanode_instances: HashMap<DatanodeId, Arc<DatanodeInstance>>,
    pub kv_store: KvStoreRef,
    pub meta_srv: MetaSrv,
    pub frontend: Arc<FrontendInstance>,
}

pub struct GreptimeDbClusterBuilder {
    cluster_name: String,
    kv_store: KvStoreRef,
    store_config: Option<ObjectStoreConfig>,
    datanodes: Option<u32>,
}

impl GreptimeDbClusterBuilder {
    pub fn new(cluster_name: &str) -> Self {
        Self {
            cluster_name: cluster_name.to_string(),
            kv_store: Arc::new(MemStore::default()),
            store_config: None,
            datanodes: None,
        }
    }

    pub fn with_store_config(mut self, store_config: ObjectStoreConfig) -> Self {
        self.store_config = Some(store_config);
        self
    }

    pub fn with_datanodes(mut self, datanodes: u32) -> Self {
        self.datanodes = Some(datanodes);
        self
    }

    pub async fn build(self) -> GreptimeDbCluster {
        let datanodes = self.datanodes.unwrap_or(4);

        let meta_srv = self.build_metasrv().await;

        let (datanode_instances, storage_guards, wal_guards) =
            self.build_datanodes(meta_srv.clone(), datanodes).await;

        let datanode_clients = build_datanode_clients(&datanode_instances, datanodes).await;

        self.wait_datanodes_alive(datanodes).await;

        let frontend = self
            .build_frontend(meta_srv.clone(), datanode_clients)
            .await;

        GreptimeDbCluster {
            storage_guards,
            _wal_guards: wal_guards,
            datanode_instances,
            kv_store: self.kv_store.clone(),
            meta_srv: meta_srv.meta_srv,
            frontend,
        }
    }

    async fn build_metasrv(&self) -> MockInfo {
        meta_srv::mocks::mock(MetaSrvOptions::default(), self.kv_store.clone(), None).await
    }

    async fn build_datanodes(
        &self,
        meta_srv: MockInfo,
        datanodes: u32,
    ) -> (
        HashMap<DatanodeId, Arc<DatanodeInstance>>,
        Vec<StorageGuard>,
        Vec<WalGuard>,
    ) {
        let mut instances = HashMap::with_capacity(datanodes as usize);
        let mut storage_guards = Vec::with_capacity(datanodes as usize);
        let mut wal_guards = Vec::with_capacity(datanodes as usize);

        for i in 0..datanodes {
            let datanode_id = i as u64 + 1;

            let mut opts = if let Some(store_config) = &self.store_config {
                let wal_tmp_dir = create_temp_dir(&format!("gt_wal_{}", &self.cluster_name));
                let wal_dir = wal_tmp_dir.path().to_str().unwrap().to_string();
                wal_guards.push(WalGuard(wal_tmp_dir));

                create_datanode_opts(store_config.clone(), wal_dir)
            } else {
                let (opts, guard) = create_tmp_dir_and_datanode_opts(
                    StorageType::File,
                    &format!("{}-dn-{}", self.cluster_name, datanode_id),
                );

                storage_guards.push(guard.storage_guard);
                wal_guards.push(guard.wal_guard);

                opts
            };
            opts.node_id = Some(datanode_id);
            opts.mode = Mode::Distributed;

            let dn_instance = self.create_datanode(&opts, meta_srv.clone()).await;

            instances.insert(datanode_id, dn_instance.clone());
        }
        (instances, storage_guards, wal_guards)
    }

    async fn wait_datanodes_alive(&self, expected_datanodes: u32) {
        let kv_store = self.kv_store();
        for _ in 0..10 {
            let alive_datanodes = meta_srv::lease::alive_datanodes(1000, &kv_store, |_, _| true)
                .await
                .unwrap()
                .len() as u32;
            if alive_datanodes == expected_datanodes {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await
        }
        panic!("Some Datanodes are not alive in 10 seconds!")
    }

    async fn create_datanode(
        &self,
        opts: &DatanodeOptions,
        meta_srv: MockInfo,
    ) -> Arc<DatanodeInstance> {
        let instance = Arc::new(
            DatanodeInstance::with_mock_meta_server(opts, meta_srv)
                .await
                .unwrap(),
        );
        instance.start().await.unwrap();

        // create another catalog and schema for testing
        let _ = instance
            .catalog_manager()
            .as_any()
            .downcast_ref::<RemoteCatalogManager>()
            .unwrap()
            .create_catalog_and_schema("another_catalog", "another_schema")
            .await
            .unwrap();
        instance
    }

    async fn build_frontend(
        &self,
        meta_srv: MockInfo,
        datanode_clients: Arc<DatanodeClients>,
    ) -> Arc<FrontendInstance> {
        let mut meta_client = MetaClientBuilder::new(1000, 0, Role::Frontend)
            .enable_router()
            .enable_store()
            .channel_manager(meta_srv.channel_manager)
            .build();
        meta_client.start(&[&meta_srv.server_addr]).await.unwrap();
        let meta_client = Arc::new(meta_client);

        Arc::new(
            FrontendInstance::try_new_distributed_with(
                meta_client,
                datanode_clients,
                Arc::new(Plugins::default()),
            )
            .await
            .unwrap(),
        )
    }

    fn kv_store(&self) -> KvStoreRef {
        self.kv_store.clone()
    }
}

async fn build_datanode_clients(
    instances: &HashMap<DatanodeId, Arc<DatanodeInstance>>,
    datanodes: u32,
) -> Arc<DatanodeClients> {
    let clients = Arc::new(DatanodeClients::default());
    for i in 0..datanodes {
        let datanode_id = i as u64 + 1;
        let instance = instances.get(&datanode_id).cloned().unwrap();
        let (addr, client) = create_datanode_client(instance).await;
        clients
            .insert_client(Peer::new(datanode_id, addr), client)
            .await;
    }
    clients
}

async fn create_datanode_client(datanode_instance: Arc<DatanodeInstance>) -> (String, Client) {
    let (client, server) = tokio::io::duplex(1024);

    let runtime = Arc::new(
        RuntimeBuilder::default()
            .worker_threads(2)
            .thread_name("grpc-handlers")
            .build()
            .unwrap(),
    );

    // create a mock datanode grpc service, see example here:
    // https://github.com/hyperium/tonic/blob/master/examples/src/mock/mock.rs
    let grpc_server = GrpcServer::new(
        ServerGrpcQueryHandlerAdaptor::arc(datanode_instance),
        None,
        None,
        runtime,
    );
    tokio::spawn(async move {
        Server::builder()
            .add_service(grpc_server.create_flight_service())
            .add_service(grpc_server.create_database_service())
            .serve_with_incoming(futures::stream::iter(vec![Ok::<_, std::io::Error>(server)]))
            .await
    });

    // Move client to an option so we can _move_ the inner value
    // on the first attempt to connect. All other attempts will fail.
    let mut client = Some(client);
    // "127.0.0.1:3001" is just a placeholder, does not actually connect to it.
    let addr = "127.0.0.1:3001";
    let channel_manager = ChannelManager::new();
    channel_manager
        .reset_with_connector(
            addr,
            service_fn(move |_| {
                let client = client.take();

                async move {
                    if let Some(client) = client {
                        Ok(client)
                    } else {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "Client already taken",
                        ))
                    }
                }
            }),
        )
        .unwrap();
    (
        addr.to_string(),
        Client::with_manager_and_urls(channel_manager, vec![addr]),
    )
}