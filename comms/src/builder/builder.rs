//  Copyright 2019 The Tari Project
//
//  Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
//  following conditions are met:
//
//  1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
//  disclaimer.
//
//  2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
//  following disclaimer in the documentation and/or other materials provided with the distribution.
//
//  3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
//  products derived from this software without specific prior written permission.
//
//  THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
//  INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
//  DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//  SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
//  SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
//  WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
//  USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use super::types::Factory;
use crate::{
    builder::CommsRoutes,
    connection::{ConnectionError, DealerProxyError, InprocAddress, ZmqContext},
    connection_manager::{ConnectionManager, PeerConnectionConfig},
    control_service::{ControlService, ControlServiceConfig, ControlServiceError, ControlServiceHandle},
    dispatcher::DispatchableKey,
    domain_connector::ConnectorError,
    inbound_message_service::{
        comms_msg_handlers::construct_comms_msg_dispatcher,
        error::InboundMessageServiceError,
        inbound_message_broker::{BrokerError, InboundMessageBroker},
        inbound_message_service::InboundMessageService,
    },
    outbound_message_service::{
        outbound_message_pool::OutboundMessagePoolConfig,
        outbound_message_service::OutboundMessageService,
        OutboundError,
        OutboundMessagePool,
    },
    peer_manager::{NodeIdentity, PeerManager, PeerManagerError},
    types::{CommsDataStore, CommsPublicKey},
    DomainConnector,
};
use derive_error::Error;
use log::*;
use serde::{de::DeserializeOwned, Serialize};
use std::{sync::Arc, thread::JoinHandle};

const LOG_TARGET: &'static str = "comms::builder";

#[derive(Debug, Error)]
pub enum CommsBuilderError {
    PeerManagerError(PeerManagerError),
    InboundMessageServiceError(ConnectionError),
    #[error(no_from)]
    OutboundMessageServiceError(OutboundError),
    #[error(no_from)]
    OutboundMessagePoolError(OutboundError),
    /// Node identity not set. Call `with_node_identity(node_identity)` on [CommsBuilder]
    NodeIdentityNotSet,
    #[error(no_from)]
    DealerProxyError(DealerProxyError),
    /// Comms routes have not been defined. Call `with_routes` on [CommsBuilder]
    RoutesNotDefined,
}

trait CommsBuilable {
    type PublicKey;
    type DispatcherFactory;
}

/// ## CommsBuilder
///
/// This builder give the Comms crate user everything they need to
/// get a p2p messaging layer up and running.
///
/// ```edition2018
/// use tari_comms::builder::{CommsBuilder, CommsRoutes};
/// use tari_comms::dispatcher::HandlerError;
/// use tari_comms::message::DomainMessageContext;
/// use tari_comms::control_service::ControlServiceConfig;
/// use tari_comms::peer_manager::NodeIdentity;
/// use std::sync::Arc;
/// use rand::OsRng;
///
/// // This should be loaded up from storage
/// let my_node_identity = NodeIdentity::random(&mut OsRng::new().unwrap(), "127.0.0.1:9000".parse().unwrap()).unwrap();
///
/// fn my_handler(_: DomainMessageContext) -> Result<(), HandlerError> {
///     println!("Your handler is called!");
///     Ok(())
/// }
///
/// let services = CommsBuilder::new()
///    .with_routes(CommsRoutes::<u8>::new())
///    // This enables the control service - allowing another peer to connect to this node
///    .configure_control_service(ControlServiceConfig::default())
///    .with_node_identity(my_node_identity)
///    .build()
///    .unwrap();
///
/// let handle = services.start().unwrap();
/// // Call shutdown when program shuts down
/// handle.shutdown();
/// ```
pub struct CommsBuilder<MType>
where MType: Clone
{
    zmq_context: ZmqContext,
    // Factories
    peer_storage_factory: Option<Box<Factory<CommsDataStore>>>,

    // Configs
    routes: Option<CommsRoutes<MType>>,
    control_service_config: Option<ControlServiceConfig<MType>>,
    omp_config: Option<OutboundMessagePoolConfig>,
    node_identity: Option<NodeIdentity<CommsPublicKey>>,
    peer_conn_config: Option<PeerConnectionConfig>,
}

impl<MType> CommsBuilder<MType>
where
    MType: DispatchableKey,
    MType: Serialize + DeserializeOwned,
    MType: Clone,
{
    pub fn new() -> Self {
        let zmq_context = ZmqContext::new();

        Self {
            zmq_context,
            control_service_config: None,
            peer_conn_config: None,
            omp_config: None,
            peer_storage_factory: None,
            routes: None,
            node_identity: None,
        }
    }

    pub fn with_routes(mut self, routes: CommsRoutes<MType>) -> Self {
        self.routes = Some(routes);
        self
    }

    pub fn with_peer_storage<F>(mut self, factory: F) -> Self
    where
        F: Factory<CommsDataStore>,
        F: 'static,
    {
        self.peer_storage_factory = Some(Box::new(factory));
        self
    }

    pub fn configure_control_service(mut self, config: ControlServiceConfig<MType>) -> Self {
        self.control_service_config = Some(config);
        self
    }

    pub fn configure_outbound_message_pool(mut self, config: OutboundMessagePoolConfig) -> Self {
        self.omp_config = Some(config);
        self
    }

    pub fn with_node_identity(mut self, node_identity: NodeIdentity<CommsPublicKey>) -> Self {
        self.node_identity = Some(node_identity);
        self
    }

    pub fn configure_peer_connections(mut self, config: PeerConnectionConfig) -> Self {
        self.peer_conn_config = Some(config);
        self
    }

    fn make_peer_manager(&mut self) -> Result<Arc<PeerManager<CommsPublicKey, CommsDataStore>>, CommsBuilderError> {
        let storage = self.peer_storage_factory.take().map(|f| f.make());
        let peer_manager = PeerManager::new(storage).map_err(CommsBuilderError::PeerManagerError)?;
        Ok(Arc::new(peer_manager))
    }

    fn make_control_service(
        &mut self,
        node_identity: Arc<NodeIdentity<CommsPublicKey>>,
    ) -> Option<ControlService<MType>>
    {
        self.control_service_config
            .take()
            .map(|config| ControlService::new(self.zmq_context.clone(), node_identity, config))
    }

    fn make_connection_manager(
        &mut self,
        node_identity: Arc<NodeIdentity<CommsPublicKey>>,
        peer_manager: Arc<PeerManager<CommsPublicKey, CommsDataStore>>,
        config: PeerConnectionConfig,
    ) -> Arc<ConnectionManager>
    {
        Arc::new(ConnectionManager::new(
            self.zmq_context.clone(),
            node_identity,
            peer_manager,
            config,
        ))
    }

    fn make_peer_connection_config(&mut self) -> PeerConnectionConfig {
        let mut config = self.peer_conn_config.take().unwrap_or_default();
        // If the message_sink_address is not set (is default) set it to a random inproc address
        if config.message_sink_address.is_default() {
            config.message_sink_address = InprocAddress::random();
        }
        config
    }

    fn make_node_identity(&mut self) -> Result<Arc<NodeIdentity<CommsPublicKey>>, CommsBuilderError> {
        self.node_identity
            .take()
            .map(Arc::new)
            .ok_or(CommsBuilderError::NodeIdentityNotSet)
    }

    fn make_outbound_message_service(
        &self,
        node_identity: Arc<NodeIdentity<CommsPublicKey>>,
        message_sink_address: InprocAddress,
        peer_manager: Arc<PeerManager<CommsPublicKey, CommsDataStore>>,
    ) -> Result<Arc<OutboundMessageService>, CommsBuilderError>
    {
        OutboundMessageService::new(
            self.zmq_context.clone(),
            node_identity,
            message_sink_address,
            peer_manager,
        )
        .map(Arc::new)
        .map_err(CommsBuilderError::OutboundMessageServiceError)
    }

    fn make_outbound_message_pool(
        &mut self,
        message_sink_address: InprocAddress,
        peer_manager: Arc<PeerManager<CommsPublicKey, CommsDataStore>>,
        connection_manager: Arc<ConnectionManager>,
    ) -> Result<OutboundMessagePool, CommsBuilderError>
    {
        let config = self.omp_config.take().unwrap_or_default();

        OutboundMessagePool::new(
            config,
            self.zmq_context.clone(),
            // OMP can requeue back onto itself
            message_sink_address.clone(),
            message_sink_address.clone(),
            peer_manager,
            connection_manager,
        )
        .map_err(CommsBuilderError::OutboundMessagePoolError)
    }

    fn make_inbound_message_service(
        &mut self,
        node_identity: Arc<NodeIdentity<CommsPublicKey>>,
        message_sink_address: InprocAddress,
        inbound_message_broker: Arc<InboundMessageBroker<MType>>,
        oms: Arc<OutboundMessageService>,
        peer_manager: Arc<PeerManager<CommsPublicKey, CommsDataStore>>,
    ) -> Result<InboundMessageService<MType>, CommsBuilderError>
    {
        InboundMessageService::new(
            self.zmq_context.clone(),
            node_identity,
            message_sink_address,
            Arc::new(construct_comms_msg_dispatcher()),
            inbound_message_broker,
            oms,
            peer_manager,
        )
        .map_err(CommsBuilderError::InboundMessageServiceError)
    }

    fn make_inbound_message_broker(&mut self, routes: &CommsRoutes<MType>) -> Arc<InboundMessageBroker<MType>> {
        let broker = routes.inner().iter().fold(
            InboundMessageBroker::new(self.zmq_context.clone()),
            |broker, (message_type, address)| broker.route(message_type.clone(), address.clone()),
        );

        Arc::new(broker)
    }

    pub fn build(mut self) -> Result<CommsServiceContainer<MType>, CommsBuilderError> {
        let node_identity = self.make_node_identity()?;

        let peer_manager = self.make_peer_manager()?;

        let peer_conn_config = self.make_peer_connection_config();

        let control_service = self.make_control_service(node_identity.clone());

        let connection_manager =
            self.make_connection_manager(node_identity.clone(), peer_manager.clone(), peer_conn_config.clone());

        let outbound_message_sink_address = InprocAddress::random();
        let outbound_message_service = self.make_outbound_message_service(
            node_identity.clone(),
            outbound_message_sink_address.clone(),
            peer_manager.clone(),
        )?;

        let outbound_message_pool = self.make_outbound_message_pool(
            outbound_message_sink_address,
            peer_manager.clone(),
            connection_manager.clone(),
        )?;

        let routes = self.routes.take().ok_or(CommsBuilderError::RoutesNotDefined)?;

        let inbound_message_broker = self.make_inbound_message_broker(&routes);

        let inbound_message_service = self.make_inbound_message_service(
            node_identity,
            peer_conn_config.message_sink_address,
            inbound_message_broker.clone(),
            outbound_message_service.clone(),
            peer_manager.clone(),
        )?;

        Ok(CommsServiceContainer {
            zmq_context: self.zmq_context,
            routes,
            control_service,
            inbound_message_service,
            connection_manager,
            outbound_message_pool,
            outbound_message_service,
        })
    }
}

#[derive(Debug, Error)]
pub enum CommsServicesError {
    ControlServiceError(ControlServiceError),
    ConnectionManagerError(ConnectionError),
    /// Comms services shut down uncleanly
    UncleanShutdown,
    /// The message type was not registered
    MessageTypeNotRegistered,
    ConnectorError(ConnectorError),
    InboundMessageBrokerError(BrokerError),
}

pub struct CommsServiceContainer<MType>
where
    MType: Serialize + DeserializeOwned,
    MType: DispatchableKey,
    MType: Clone,
{
    zmq_context: ZmqContext,
    routes: CommsRoutes<MType>,
    connection_manager: Arc<ConnectionManager>,
    control_service: Option<ControlService<MType>>,
    inbound_message_service: InboundMessageService<MType>,
    outbound_message_pool: OutboundMessagePool,
    outbound_message_service: Arc<OutboundMessageService>,
}

impl<MType> CommsServiceContainer<MType>
where
    MType: Serialize + DeserializeOwned,
    MType: DispatchableKey,
    MType: Clone,
{
    pub fn start(self) -> Result<CommsServices<MType>, CommsServicesError> {
        let mut control_service_handle = None;
        if let Some(control_service) = self.control_service {
            control_service_handle = Some(
                control_service
                    .serve(self.connection_manager.clone())
                    .map_err(CommsServicesError::ControlServiceError)?,
            );
        }

        let ims_handle = self.inbound_message_service.start();
        self.outbound_message_pool.start();

        Ok(CommsServices {
            // Transfer ownership to CommsServices
            zmq_context: self.zmq_context,
            outbound_message_service: self.outbound_message_service,
            routes: self.routes,
            connection_manager: self.connection_manager,

            // Add handles for started services
            control_service_handle,
            ims_handle,
        })
    }
}

pub struct CommsServices<MType> {
    zmq_context: ZmqContext,
    outbound_message_service: Arc<OutboundMessageService>,
    routes: CommsRoutes<MType>,
    control_service_handle: Option<ControlServiceHandle>,
    #[allow(dead_code)]
    ims_handle: JoinHandle<Result<(), InboundMessageServiceError>>,
    connection_manager: Arc<ConnectionManager>,
}

impl<MType> CommsServices<MType>
where
    MType: DispatchableKey,
    MType: Clone,
{
    pub fn get_outbound_message_service(&self) -> Arc<OutboundMessageService> {
        self.outbound_message_service.clone()
    }

    pub fn create_connector<'de>(&self, message_type: &MType) -> Result<DomainConnector<'de>, CommsServicesError> {
        let addr = self
            .routes
            .get_address(&message_type)
            .ok_or(CommsServicesError::MessageTypeNotRegistered)?;

        DomainConnector::listen(&self.zmq_context, &addr).map_err(CommsServicesError::ConnectorError)
    }

    pub fn shutdown(self) -> Result<(), CommsServicesError> {
        info!(target: LOG_TARGET, "Comms is shutting down");
        let mut shutdown_results = Vec::new();
        // Shutdown control service
        if let Some(control_service_shutdown_result) = self.control_service_handle.map(|hnd| hnd.shutdown()) {
            shutdown_results.push(control_service_shutdown_result.map_err(CommsServicesError::ControlServiceError));
        }

        // TODO: Shutdown other services

        // Lastly, Shutdown connection manager
        match Arc::try_unwrap(self.connection_manager) {
            Ok(conn_manager) => {
                for result in conn_manager.shutdown() {
                    shutdown_results.push(result.map_err(CommsServicesError::ConnectionManagerError));
                }
            },
            Err(_) => error!(
                target: LOG_TARGET,
                "Unable to cleanly shutdown connection manager because references are still held by other threads"
            ),
        }

        Self::check_clean_shutdown(shutdown_results)
    }

    fn check_clean_shutdown(results: Vec<Result<(), CommsServicesError>>) -> Result<(), CommsServicesError> {
        let mut has_error = false;
        for result in results {
            if let Err(err) = result {
                error!(target: LOG_TARGET, "Error occurred when shutting down {:?}", err);
                has_error = true;
            }
        }

        if has_error {
            Err(CommsServicesError::UncleanShutdown)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn new_no_control_service() {
        let comms_services = CommsBuilder::new()
            .with_routes(CommsRoutes::new().register("hello".to_owned()))
            .with_node_identity(NodeIdentity::random_for_test(None))
            .build()
            .unwrap();

        assert!(comms_services.control_service.is_none());
    }

    #[test]
    fn new_with_control_service() {
        let comms_services = CommsBuilder::new()
            .with_routes(CommsRoutes::new().register("hello".to_owned()))
            .with_node_identity(NodeIdentity::random_for_test(None))
            .configure_control_service(ControlServiceConfig::default())
            .build()
            .unwrap();

        assert!(comms_services.control_service.is_some());
    }
}