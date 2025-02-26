use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    routing::{get, post},
    Router,
};
use clap::{Parser, ValueEnum};
use http::Request;
use ic_agent::{
    agent::http_transport::reqwest_transport::ReqwestHttpReplicaV2Transport,
    identity::AnonymousIdentity, Agent,
};
use ic_base_types::CanisterId;
use ic_icrc_rosetta::{
    common::constants::{BLOCK_SYNC_WAIT_SECS, MAX_BLOCK_SYNC_WAIT_SECS},
    common::storage::{storage_client::StorageClient, types::MetadataEntry},
    construction_api::endpoints::*,
    data_api::endpoints::*,
    ledger_blocks_synchronization::blocks_synchronizer::start_synching_blocks,
    AppState, Metadata,
};
use icrc_ledger_agent::{CallMode, Icrc1Agent};
use lazy_static::lazy_static;
use std::{net::TcpListener, sync::Arc};
use std::{path::PathBuf, process};
use tower_http::classify::{ServerErrorsAsFailures, SharedClassifier};
use tower_http::trace::TraceLayer;
use tower_request_id::{RequestId, RequestIdLayer};
use tracing::{debug, error, error_span, info, Level, Span};
use url::Url;

lazy_static! {
    static ref MAINNET_DEFAULT_URL: &'static str = "https://ic0.app";
    static ref TESTNET_DEFAULT_URL: &'static str = "https://exchanges.testnet.dfinity.network";
    static ref MAXIMUM_BLOCKS_PER_REQUEST: u64 = 2000;
}

#[derive(Clone, Debug, ValueEnum)]
enum StoreType {
    InMemory,
    File,
}

#[derive(Clone, Debug, ValueEnum)]
enum NetworkType {
    Mainnet,
    Testnet,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    ledger_id: CanisterId,

    #[arg(long)]
    icrc1_symbol: Option<String>,

    #[arg(long)]
    icrc1_decimals: Option<u8>,

    /// The port to which Rosetta will bind.
    /// If not set then it will be 0.
    #[arg(short, long)]
    port: Option<u16>,

    /// The file where the port to which Rosetta will bind
    /// will be written.
    #[arg(short = 'P', long)]
    port_file: Option<PathBuf>,

    /// The type of the store to use.
    #[arg(short, long, value_enum, default_value_t = StoreType::File)]
    store_type: StoreType,

    /// The file to use for the store if [store_type] is file.
    #[arg(short = 'f', long, default_value = "db.sqlite")]
    store_file: PathBuf,

    /// The network type that rosetta connects to.
    #[arg(short = 'n', long, value_enum)]
    network_type: NetworkType,

    /// URL of the IC to connect to.
    /// Default Mainnet URL is: https://ic0.app,
    /// Default Testnet URL is: https://exchanges.testnet.dfinity.network
    #[arg(long, short = 'u')]
    network_url: Option<String>,

    #[arg(short = 'L', long, default_value_t = Level::INFO)]
    log_level: Level,

    /// Set this option to only do one full sync of the ledger and then exit rosetta
    #[arg(long = "exit-on-sync")]
    exit_on_sync: bool,

    /// Set this option to only run the rosetta server, no block synchronization will be performed and no transactions can be submitted in this mode.
    #[arg(long)]
    offline: bool,
}

impl Args {
    /// Return the port to which Rosetta should bind to.
    fn get_port(&self) -> u16 {
        match (&self.port, &self.port_file) {
            (None, None) => 8080,
            (None, Some(_)) => 0,
            (Some(port), _) => *port,
        }
    }
    fn is_mainnet(&self) -> bool {
        match self.network_type {
            NetworkType::Mainnet => true,
            NetworkType::Testnet => false,
        }
    }

    fn effective_network_url(&self) -> String {
        self.network_url.clone().unwrap_or_else(|| {
            if self.is_mainnet() {
                (*MAINNET_DEFAULT_URL).to_string()
            } else {
                (*TESTNET_DEFAULT_URL).to_string()
            }
        })
    }

    fn are_metadata_args_set(&self) -> bool {
        self.icrc1_symbol.is_some() && self.icrc1_decimals.is_some()
    }
}

fn init_logs(log_level: Level) {
    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(false) // instead include file and lines in the next lines
        .with_file(true) // display source code file paths
        .with_line_number(true) // display source code line numbers
        .init();
}

type FnTraceLayer =
    TraceLayer<SharedClassifier<ServerErrorsAsFailures>, fn(&Request<Body>) -> Span>;

fn add_request_span() -> FnTraceLayer {
    // See tower-request-id crate and the example at
    // https://github.com/imbolc/tower-request-id/blob/fe372479a56bd540784b87812d4d78473e43c6d4/examples/logging.rs

    // Let's create a tracing span for each request
    TraceLayer::new_for_http().make_span_with(|request: &Request<Body>| {
        // We get the request id from the extensions
        let request_id = request
            .extensions()
            .get::<RequestId>()
            .map(ToString::to_string)
            .unwrap_or_else(|| "unknown".into());
        // And then we put it along with other information into the `request` span
        error_span!(
            "request",
            id = %request_id,
            method = %request.method(),
            uri = %request.uri(),
        )
    })
}

async fn load_metadata(
    args: &Args,
    icrc1_agent: &Icrc1Agent,
    storage: &StorageClient,
) -> anyhow::Result<Metadata> {
    if args.offline {
        let db_metadata_entries = storage.read_metadata()?;
        // If metadata is empty and the args are not set, bail out.
        if db_metadata_entries.is_empty() && !args.are_metadata_args_set() {
            bail!("Metadata must be initialized by starting Rosetta in online mode first or by providing ICRC-1 metadata arguments.");
        }

        // If metadata is set in args and not entries are found in the database,
        // return the metadata from the args.
        if args.are_metadata_args_set() && db_metadata_entries.is_empty() {
            return Ok(Metadata::from_args(
                args.icrc1_symbol.clone().unwrap(),
                args.icrc1_decimals.unwrap(),
            ));
        }

        // Populate a metadata object with the database entries.
        let db_metadata = Metadata::from_metadata_entries(&db_metadata_entries)?;
        // If the metadata args are not set, return using the db metadata.
        if !args.are_metadata_args_set() {
            return Ok(db_metadata);
        }

        // Extract the symbol and decimals from the arguments.
        let symbol = args
            .icrc1_symbol
            .clone()
            .context("ICRC-1 symbol should be provided in offline mode.")?;
        let decimals = args
            .icrc1_decimals
            .context("ICRC-1 decimals should be provided in offline mode.")?;

        // If the database entries is empty, return the metadata as no validation
        // can be done.
        if db_metadata_entries.is_empty() {
            return Ok(Metadata::from_args(symbol, decimals));
        }

        // Populate a metadata object with the database entries.
        let db_metadata = Metadata::from_metadata_entries(&db_metadata_entries)?;

        // If the symbols do not match, bail out.
        if db_metadata.symbol != symbol {
            bail!(
                "Provided symbol does not match symbol retrieved in online mode. Expected: {}",
                db_metadata.symbol
            );
        }

        // If the decimals do not match, bail out.
        if db_metadata.decimals != decimals {
            bail!(
                "Provided decimals does not match symbol retrieved in online mode. Expected: {}",
                db_metadata.decimals
            );
        }

        return Ok(db_metadata);
    }

    let ic_metadata_entries = icrc1_agent
        .metadata(CallMode::Update)
        .await
        .with_context(|| "Failed to get metadata")?
        .iter()
        .map(|(key, value)| MetadataEntry::from_metadata_value(key, value))
        .collect::<Result<Vec<MetadataEntry>>>()?;

    storage.write_metadata(ic_metadata_entries.clone())?;

    Metadata::from_metadata_entries(&ic_metadata_entries)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    init_logs(args.log_level);

    let storage = Arc::new(match args.store_type {
        StoreType::InMemory => StorageClient::new_in_memory()?,
        StoreType::File => StorageClient::new_persistent(&args.store_file)?,
    });

    let network_url = args.effective_network_url();

    let ic_agent = Agent::builder()
        .with_identity(AnonymousIdentity)
        .with_transport(ReqwestHttpReplicaV2Transport::create(
            Url::parse(&network_url)
                .context(format!("Failed to parse URL {}", network_url.clone()))?,
        )?)
        .build()?;

    // Only fetch root key if the network is not the mainnet
    if !args.is_mainnet() {
        debug!("Network type is not mainnet --> Trying to fetch root key");
        ic_agent.fetch_root_key().await?;
    }

    debug!("Rosetta connects to : {}", network_url);

    debug!(
        "Network status is : {:?}",
        ic_agent.status().await?.replica_health_status
    );

    let icrc1_agent = Arc::new(Icrc1Agent {
        agent: ic_agent,
        ledger_canister_id: args.ledger_id.into(),
    });

    if !args.offline {
        info!("Starting to sync blocks");
        start_synching_blocks(
            icrc1_agent.clone(),
            storage.clone(),
            *MAXIMUM_BLOCKS_PER_REQUEST,
        )
        .await?;
    }

    info!("Starting to update account balances");
    // Once the entire blockchain has been synched and no gaps remain, the account_balance table can be updated
    storage.update_account_balances()?;

    // If the option of exiting after the synchronization is completed is set we can exit rosetta
    if args.exit_on_sync {
        process::exit(0);
    }

    let metadata = load_metadata(&args, &icrc1_agent, &storage).await?;
    let shared_state = Arc::new(AppState {
        icrc1_agent: icrc1_agent.clone(),
        ledger_id: args.ledger_id,
        storage: storage.clone(),
        metadata,
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/network/list", post(network_list))
        .route("/network/options", post(network_options))
        .route("/network/status", post(network_status))
        .route("/block", post(block))
        .route("/account/balance", post(account_balance))
        .route("/block/transaction", post(block_transaction))
        .route("/mempool", post(mempool))
        .route("/mempool/transaction", post(mempool_transaction))
        .route("/construction/derive", post(construction_derive))
        .route("/construction/preprocess", post(construction_preprocess))
        .route("/construction/metadata", post(construction_metadata))
        .route("/construction/combine", post(construction_combine))
        .route("/construction/submit", post(construction_submit))
        .route("/construction/hash", post(construction_hash))
        // This layer creates a span for each http request and attaches
        // the request_id, HTTP Method and path to it.
        .layer(add_request_span())
        // This layer creates a new id for each request and puts it into the
        // request extensions. Note that it should be added after the
        // Trace layer.
        .layer(RequestIdLayer)
        .with_state(shared_state);

    let tcp_listener = TcpListener::bind(format!("0.0.0.0:{}", args.get_port()))?;

    if let Some(port_file) = args.port_file {
        std::fs::write(port_file, tcp_listener.local_addr()?.port().to_string())?;
    }

    if !args.offline {
        tokio::spawn(async move {
            let mut sync_wait_secs = BLOCK_SYNC_WAIT_SECS;
            loop {
                if let Err(e) = start_synching_blocks(
                    icrc1_agent.clone(),
                    storage.clone(),
                    *MAXIMUM_BLOCKS_PER_REQUEST,
                )
                .await
                {
                    error!("Error while syncing blocks: {}", e);
                    sync_wait_secs = std::cmp::min(sync_wait_secs * 2, MAX_BLOCK_SYNC_WAIT_SECS);
                    info!("Retrying in {} seconds.", sync_wait_secs);
                } else {
                    sync_wait_secs = BLOCK_SYNC_WAIT_SECS;
                }

                tokio::time::sleep(std::time::Duration::from_secs(sync_wait_secs)).await;
            }
        });
    }

    info!("Starting Rosetta server");

    axum::Server::from_tcp(tcp_listener)?
        .serve(app.into_make_service())
        .await
        .context("Unable to start the Rosetta server")
}
