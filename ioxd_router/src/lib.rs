use async_trait::async_trait;
use clap_blocks::write_buffer::WriteBufferConfig;
use data_types::{NamespaceName, PartitionTemplate, TemplatePart};
use hashbrown::HashMap;
use hyper::{Body, Request, Response};
use iox_catalog::interface::Catalog;
use ioxd_common::{
    add_service,
    http::error::{HttpApiError, HttpApiErrorSource},
    rpc::RpcBuilderInput,
    serve_builder,
    server_type::{CommonServerState, RpcError, ServerType},
    setup_builder,
};
use metric::Registry;
use mutable_batch::MutableBatch;
use object_store::DynObjectStore;
use observability_deps::tracing::info;
use router::{
    dml_handlers::{
        DmlHandler, DmlHandlerChainExt, FanOutAdaptor, InstrumentationDecorator, Partitioner,
        RetentionValidator, SchemaValidator, ShardedWriteBuffer, WriteSummaryAdapter,
    },
    namespace_cache::{
        metrics::InstrumentedCache, MemoryNamespaceCache, NamespaceCache, ShardedCache,
    },
    namespace_resolver::{NamespaceAutocreation, NamespaceResolver, NamespaceSchemaResolver},
    server::{
        grpc::{sharder::ShardService, GrpcDelegate},
        http::HttpDelegate,
        RouterServer,
    },
    shard::Shard,
};
use sharder::{JumpHash, Sharder};
use std::{
    collections::BTreeSet,
    fmt::{Debug, Display},
    sync::Arc,
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use trace::TraceCollector;
use write_summary::WriteSummary;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to initialise write buffer connection: {0}")]
    WriteBuffer(#[from] write_buffer::core::WriteBufferError),

    #[error("Catalog error: {0}")]
    Catalog(#[from] iox_catalog::interface::Error),

    #[error("Catalog DSN error: {0}")]
    CatalogDsn(#[from] clap_blocks::catalog_dsn::Error),

    #[error("No shards found in Catalog")]
    Sharder,

    #[error("No topic named '{topic_name}' found in the catalog")]
    TopicCatalogLookup { topic_name: String },

    #[error("Failed to init shard grpc service: {0}")]
    ShardServiceInit(iox_catalog::interface::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub struct RouterServerType<D, N, S> {
    server: RouterServer<D, N, S>,
    shutdown: CancellationToken,
    trace_collector: Option<Arc<dyn TraceCollector>>,
}

impl<D, N, S> RouterServerType<D, N, S> {
    pub fn new(server: RouterServer<D, N, S>, common_state: &CommonServerState) -> Self {
        Self {
            server,
            shutdown: CancellationToken::new(),
            trace_collector: common_state.trace_collector(),
        }
    }
}

impl<D, N, S> std::fmt::Debug for RouterServerType<D, N, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Router")
    }
}

#[async_trait]
impl<D, N, S> ServerType for RouterServerType<D, N, S>
where
    D: DmlHandler<WriteInput = HashMap<String, MutableBatch>, WriteOutput = WriteSummary> + 'static,
    S: Sharder<(), Item = Arc<Shard>> + Clone + 'static,
    N: NamespaceResolver + 'static,
{
    /// Return the [`metric::Registry`] used by the router.
    fn metric_registry(&self) -> Arc<Registry> {
        self.server.metric_registry()
    }

    /// Returns the trace collector for router traces.
    fn trace_collector(&self) -> Option<Arc<dyn TraceCollector>> {
        self.trace_collector.as_ref().map(Arc::clone)
    }

    /// Dispatches `req` to the router [`HttpDelegate`] delegate.
    ///
    /// [`HttpDelegate`]: router::server::http::HttpDelegate
    async fn route_http_request(
        &self,
        req: Request<Body>,
    ) -> Result<Response<Body>, Box<dyn HttpApiErrorSource>> {
        self.server
            .http()
            .route(req)
            .await
            .map_err(IoxHttpErrorAdaptor)
            .map_err(|e| Box::new(e) as _)
    }

    /// Registers the services exposed by the router [`GrpcDelegate`] delegate.
    ///
    /// [`GrpcDelegate`]: router::server::grpc::GrpcDelegate
    async fn server_grpc(self: Arc<Self>, builder_input: RpcBuilderInput) -> Result<(), RpcError> {
        let builder = setup_builder!(builder_input, self);
        add_service!(builder, self.server.grpc().schema_service());
        add_service!(builder, self.server.grpc().catalog_service());
        add_service!(builder, self.server.grpc().object_store_service());
        add_service!(builder, self.server.grpc().shard_service());
        add_service!(builder, self.server.grpc().namespace_service());
        serve_builder!(builder);

        Ok(())
    }

    async fn join(self: Arc<Self>) {
        self.shutdown.cancelled().await;
    }

    fn shutdown(&self) {
        self.shutdown.cancel();
    }
}

/// This adaptor converts the `router` http error type into a type that
/// satisfies the requirements of ioxd's runner framework, keeping the
/// two decoupled.
#[derive(Debug)]
pub struct IoxHttpErrorAdaptor(router::server::http::Error);

impl Display for IoxHttpErrorAdaptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl std::error::Error for IoxHttpErrorAdaptor {}

impl HttpApiErrorSource for IoxHttpErrorAdaptor {
    fn to_http_api_error(&self) -> HttpApiError {
        HttpApiError::new(self.0.as_status_code(), self.to_string())
    }
}

/// Instantiate a router server
pub async fn create_router_server_type(
    common_state: &CommonServerState,
    metrics: Arc<metric::Registry>,
    catalog: Arc<dyn Catalog>,
    object_store: Arc<DynObjectStore>,
    write_buffer_config: &WriteBufferConfig,
    query_pool_name: &str,
    request_limit: usize,
) -> Result<Arc<dyn ServerType>> {
    // Initialise the sharded write buffer and instrument it with DML handler
    // metrics.
    let (write_buffer, sharder) = init_write_buffer(
        write_buffer_config,
        Arc::clone(&metrics),
        common_state.trace_collector(),
    )
    .await?;
    let write_buffer =
        InstrumentationDecorator::new("sharded_write_buffer", &metrics, write_buffer);

    // Initialise an instrumented namespace cache to be shared with the schema
    // validator, and namespace auto-creator that reports cache hit/miss/update
    // metrics.
    let ns_cache = Arc::new(InstrumentedCache::new(
        Arc::new(ShardedCache::new(
            std::iter::repeat_with(|| Arc::new(MemoryNamespaceCache::default())).take(10),
        )),
        &metrics,
    ));

    pre_warm_schema_cache(&ns_cache, &*catalog)
        .await
        .expect("namespace cache pre-warming failed");

    // Initialise and instrument the schema validator
    let schema_validator =
        SchemaValidator::new(Arc::clone(&catalog), Arc::clone(&ns_cache), &metrics);
    let schema_validator =
        InstrumentationDecorator::new("schema_validator", &metrics, schema_validator);

    // Add a retention validator into handler stack to reject data outside the retention period
    let retention_validator = RetentionValidator::new(Arc::clone(&catalog), Arc::clone(&ns_cache));
    let retention_validator =
        InstrumentationDecorator::new("retention_validator", &metrics, retention_validator);

    // Add a write partitioner into the handler stack that splits by the date
    // portion of the write's timestamp.
    let partitioner = Partitioner::new(PartitionTemplate {
        parts: vec![TemplatePart::TimeFormat("%Y-%m-%d".to_owned())],
    });
    let partitioner = InstrumentationDecorator::new("partitioner", &metrics, partitioner);

    // Initialise the Namespace ID lookup + cache
    let namespace_resolver =
        NamespaceSchemaResolver::new(Arc::clone(&catalog), Arc::clone(&ns_cache));

    ////////////////////////////////////////////////////////////////////////////
    //
    // THIS CODE IS FOR TESTING ONLY.
    //
    // The source of truth for the topics & query pools will be read from
    // the DB, rather than CLI args for a prod deployment.
    //
    ////////////////////////////////////////////////////////////////////////////
    //
    // Look up the topic ID needed to populate namespace creation
    // requests.
    //
    // This code / auto-creation is for architecture testing purposes only - a
    // prod deployment would expect namespaces to be explicitly created and this
    // layer would be removed.
    let schema_catalog = Arc::clone(&catalog);
    let mut txn = catalog.start_transaction().await?;
    let topic_id = txn
        .topics()
        .get_by_name(write_buffer_config.topic())
        .await?
        .map(|v| v.id)
        .unwrap_or_else(|| panic!("no topic named {} in catalog", write_buffer_config.topic()));
    let query_id = txn
        .query_pools()
        .create_or_get(query_pool_name)
        .await
        .map(|v| v.id)
        .unwrap_or_else(|e| {
            panic!(
                "failed to upsert query pool {} in catalog: {}",
                write_buffer_config.topic(),
                e
            )
        });
    txn.commit().await?;

    let namespace_resolver = NamespaceAutocreation::new(
        namespace_resolver,
        Arc::clone(&ns_cache),
        Arc::clone(&catalog),
        topic_id,
        query_id,
        None,
    );
    //
    ////////////////////////////////////////////////////////////////////////////

    let parallel_write = WriteSummaryAdapter::new(FanOutAdaptor::new(write_buffer));

    // Build the chain of DML handlers that forms the request processing
    // pipeline, starting with the namespace creator (for testing purposes) and
    // write partitioner that yields a set of partitioned batches.
    let handler_stack = retention_validator
        .and_then(schema_validator)
        .and_then(partitioner)
        // Once writes have been partitioned, they are processed in parallel.
        //
        // This block initialises a fan-out adaptor that parallelises partitioned
        // writes into the handler chain it decorates (schema validation, and then
        // into the sharded write buffer), and instruments the parallelised
        // operation.
        .and_then(InstrumentationDecorator::new(
            "parallel_write",
            &metrics,
            parallel_write,
        ));

    // Record the overall request handling latency
    let handler_stack = InstrumentationDecorator::new("request", &metrics, handler_stack);

    // Initialise the shard-mapping gRPC service.
    let shard_service = init_shard_service(sharder, write_buffer_config, catalog).await?;

    // Initialise the API delegates
    let http = HttpDelegate::new(
        common_state.run_config().max_http_request_size,
        request_limit,
        namespace_resolver,
        handler_stack,
        &metrics,
    );
    let grpc = GrpcDelegate::new(
        topic_id,
        query_id,
        schema_catalog,
        object_store,
        shard_service,
    );

    let router_server = RouterServer::new(http, grpc, metrics, common_state.trace_collector());
    let server_type = Arc::new(RouterServerType::new(router_server, common_state));
    Ok(server_type)
}

/// Initialise the [`ShardedWriteBuffer`] with one shard per Kafka partition,
/// using [`JumpHash`] to shard operations by their destination namespace &
/// table name.
///
/// Returns both the DML handler and the sharder it uses.
async fn init_write_buffer(
    write_buffer_config: &WriteBufferConfig,
    metrics: Arc<metric::Registry>,
    trace_collector: Option<Arc<dyn TraceCollector>>,
) -> Result<(
    ShardedWriteBuffer<Arc<JumpHash<Arc<Shard>>>>,
    Arc<JumpHash<Arc<Shard>>>,
)> {
    let write_buffer = Arc::new(
        write_buffer_config
            .writing(Arc::clone(&metrics), None, trace_collector)
            .await?,
    );

    // Construct the (ordered) set of shards.
    //
    // The sort order must be deterministic in order for all nodes to shard to
    // the same shard indexes, therefore we type assert the returned set is of the
    // ordered variety.
    let shards: BTreeSet<_> = write_buffer.shard_indexes();
    //          ^ don't change this to an unordered set

    info!(
        topic = write_buffer_config.topic(),
        shards = shards.len(),
        "connected to write buffer topic",
    );

    if shards.is_empty() {
        return Err(Error::Sharder);
    }

    // Initialise the sharder that maps (table, namespace, payload) to shards.
    let sharder = Arc::new(JumpHash::new(
        shards
            .into_iter()
            .map(|shard_index| Shard::new(shard_index, Arc::clone(&write_buffer), &metrics))
            .map(Arc::new),
    ));

    Ok((ShardedWriteBuffer::new(Arc::clone(&sharder)), sharder))
}

async fn init_shard_service<S>(
    sharder: S,
    write_buffer_config: &WriteBufferConfig,
    catalog: Arc<dyn Catalog>,
) -> Result<ShardService<S>>
where
    S: Send + Sync,
{
    // Get the TopicMetadata from the catalog for the configured topic.
    let topic = catalog
        .repositories()
        .await
        .topics()
        .get_by_name(write_buffer_config.topic())
        .await?
        .ok_or_else(|| Error::TopicCatalogLookup {
            topic_name: write_buffer_config.topic().to_string(),
        })?;

    // Initialise the sharder
    ShardService::new(sharder, topic, catalog)
        .await
        .map_err(Error::ShardServiceInit)
}

/// Pre-populate `cache` with the all existing schemas in `catalog`.
async fn pre_warm_schema_cache<T>(
    cache: &T,
    catalog: &dyn Catalog,
) -> Result<(), iox_catalog::interface::Error>
where
    T: NamespaceCache,
{
    iox_catalog::interface::list_schemas(catalog)
        .await?
        .for_each(|(ns, schema)| {
            let name = NamespaceName::try_from(ns.name)
                .expect("cannot convert existing namespace string to a `NamespaceName` instance");

            cache.put_schema(name, schema);
        });

    Ok(())
}

#[cfg(test)]
mod tests {
    use data_types::ColumnType;
    use iox_catalog::mem::MemCatalog;

    use super::*;

    #[tokio::test]
    async fn test_pre_warm_cache() {
        let catalog = Arc::new(MemCatalog::new(Default::default()));

        let mut repos = catalog.repositories().await;
        let topic = repos.topics().create_or_get("foo").await.unwrap();
        let pool = repos.query_pools().create_or_get("foo").await.unwrap();
        let namespace = repos
            .namespaces()
            .create("test_ns", None, topic.id, pool.id)
            .await
            .unwrap();

        let table = repos
            .tables()
            .create_or_get("name", namespace.id)
            .await
            .unwrap();
        let _column = repos
            .columns()
            .create_or_get("name", table.id, ColumnType::U64)
            .await
            .unwrap();

        drop(repos); // Or it'll deadlock.

        let cache = Arc::new(MemoryNamespaceCache::default());
        pre_warm_schema_cache(&cache, &*catalog)
            .await
            .expect("pre-warming failed");

        let name = NamespaceName::new("test_ns").unwrap();
        let got = cache.get_schema(&name).expect("should contain a schema");

        assert!(got.tables.get("name").is_some());
    }
}
