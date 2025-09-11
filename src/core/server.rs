use std::{
    error::Error,
    net::SocketAddr,
    sync::{Arc, LazyLock},
};

use alloy::{
    consensus::{SignableTransaction, TxEip1559, TxLegacy, transaction::RlpEcdsaDecodableTx},
    primitives::{Address, B256, keccak256},
};
use axum::{
    Router,
    extract::{OriginalUri, Request, State},
    http::StatusCode,
    response::Response,
    routing::post,
};
use bitcoin::hex::DisplayHex;
use brc20_prog::{
    Brc20ProgApiClient,
    types::{EthCall, U64ED},
};
use http::HeaderMap;
use jsonrpsee::http_client::HttpClient;
use reqwest::Client;
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};

use crate::core::{
    config::Config,
    database::TxDatabase,
    minter::Minter,
    rpc_utils::{
        JsonRpcRequest, is_batch_request, make_response, parse_batch, parse_request,
        remove_duplicate_input_and_data, wrap_error, wrap_json_rpc, wrap_json_rpc_error,
    },
    transaction::{convert_to_brc20_inscription, generate_dummy_signed_tx},
};

struct ServerState {
    minter: Minter,
    database: TxDatabase,
    brc20_client: HttpClient,
    chain_id: u64,
    evm_address: String,
}

impl ServerState {
    async fn new(config: &Config) -> Self {
        // Initialize the server here
        let minter = Minter::new(
            config.bitcoin_network.clone(),
            config.to_spend_fee_rate,
            &config.secret,
            &config.bitcoin_rpc_url,
            &config.bitcoin_rpc_user,
            &config.bitcoin_rpc_password,
        );
        let brc20_client = HttpClient::builder()
            .build(config.brc20_rpc_url.as_str())
            .expect("Failed to create BRC2.0 RPC client");

        let chain_id_str = brc20_client
            .eth_chain_id()
            .await
            .expect("Failed to get chain ID");
        let chain_id: u64 = serde_json::from_str::<U64ED>(format!("\"{}\"", chain_id_str).as_str())
            .expect("Failed to parse chain ID")
            .into();

        tracing::info!("Connected to BRC2.0 RPC server, chain ID: {}", chain_id_str);

        let database = TxDatabase::new(&config.db_url).await;
        database.create_tables().await;

        tracing::info!("Bitcoin address: {}", minter.sender);
        tracing::info!("Bitcoin network: {:?}", config.bitcoin_network);
        tracing::info!("EVM tx signer: {}", config.evm_address);

        ServerState {
            minter,
            brc20_client,
            chain_id,
            evm_address: config.evm_address.clone(),
            database,
        }
    }

    async fn clear_tables(&self) {
        self.database.clear_tables().await;
    }

    async fn get_nonce(&self, address: Address) -> Result<u64, Box<dyn Error>> {
        let database_nonce = self
            .database
            .get_transaction_count(&address.to_string())
            .await as u64;

        let brc20_nonce: u64 = match self
            .brc20_client
            .eth_get_transaction_count(address.into(), "latest".to_string())
            .await
        {
            Ok(response) => {
                let Ok(nonce) = serde_json::from_str::<U64ED>(format!("\"{}\"", response).as_str())
                else {
                    return Err(format!("Failed to parse nonce: {}", response).into());
                };
                nonce.into()
            }
            Err(_) => {
                return Err("Failed to get nonce from BRC2.0 client".into());
            }
        };

        Ok(std::cmp::max(database_nonce, brc20_nonce))
    }

    async fn estimate_fee(&self, eth_call: EthCall) -> Result<u64, Box<dyn Error>> {
        // First, estimate gas using the BRC2.0 RPC client
        let estimated_gas: u64 = match self
            .brc20_client
            .eth_estimate_gas(eth_call.clone(), None)
            .await
        {
            Ok(response) => {
                let Ok(limit) = serde_json::from_str::<U64ED>(format!("\"{}\"", response).as_str())
                else {
                    return Err(format!("Failed to parse gas estimate: {}", response).into());
                };
                limit.into()
            }
            Err(_) => {
                return Err("Failed to estimate gas for eth_call".into());
            }
        };
        // Create a dummy signed Ethereum transaction
        let Ok(dummy_tx) = generate_dummy_signed_tx(self.chain_id, eth_call).await else {
            return Err("Failed to generate dummy signed transaction".into());
        };
        // Finally, convert it to a BRC20 inscription and calculate the estimated fee in satoshis
        let inscription = convert_to_brc20_inscription(&dummy_tx, estimated_gas);
        let total_fee = match self.minter.estimate(&inscription).await {
            Ok(total_fee) => total_fee,
            Err(err) => {
                return Err(format!(
                    "Failed to create BRC20 inscription for fee estimation: {}",
                    err
                )
                .into());
            }
        };
        Ok(total_fee)
    }

    async fn mint(&self, signed_eth_tx: &str) -> Result<B256, Box<dyn Error>> {
        tracing::info!("Received signed EVM transaction: {}", signed_eth_tx);
        let Ok(tx_data) = &mut hex::decode(signed_eth_tx.trim_start_matches("0x")) else {
            return Err("Failed to decode signed transaction hex".into());
        };
        let Ok((tx, signature)) = TxLegacy::rlp_decode_with_signature(&mut tx_data.as_slice())
        else {
            if TxEip1559::rlp_decode_with_signature(&mut tx_data.as_slice()).is_ok() {
                return Err(
                    "EIP-1559 transactions are not supported in BRC2.0, use legacy transactions instead."
                        .into(),
                );
            } else {
                return Err("Failed to decode signed EVM transaction".into());
            }
        };

        if tx.chain_id != Some(self.chain_id) {
            return Err(format!(
                "Chain ID mismatch: expected {}, got {:?}",
                self.chain_id, tx.chain_id
            )
            .into());
        }

        let tx_hash = keccak256(tx.encoded_for_signing());
        let Ok(sender_address) = signature.recover_address_from_prehash(&tx_hash) else {
            return Err("Failed to recover sender address from signature".into());
        };

        if sender_address.0.to_string().to_ascii_lowercase()
            != self.evm_address.to_ascii_lowercase()
        {
            return Err(format!(
                "Sender address does not match EVM address: {}",
                sender_address
            )
            .into());
        }

        self.minter.check_fee_rate_warning().await;

        // Convert the signed Ethereum transaction to a BRC20 inscription
        let inscription = convert_to_brc20_inscription(&signed_eth_tx, tx.gas_limit);
        let mint_result = match self.minter.mint(&inscription).await {
            Ok(res) => res,
            Err(err) => return Err(format!("Failed to mint BRC20 inscription: {}", err).into()),
        };

        // Store the transaction in the database
        self.database
            .add_tx(
                sender_address.to_string().as_str(),
                signed_eth_tx,
                tx_hash.to_lower_hex_string().as_str(),
                format!("{}", mint_result.commit).as_str(),
                format!("{}", mint_result.reveal).as_str(),
                format!("{}", mint_result.send_to_op_return).as_str(),
                mint_result.total_fee as i64,
                tx.nonce as i64,
            )
            .await;

        Ok(tx_hash)
    }

    async fn get_balance(&self) -> Result<u64, StatusCode> {
        Ok(self.minter.get_balance().await)
    }
}

pub async fn reset(config: &Config) {
    let server = ServerState::new(&config).await;
    server.clear_tables().await;
    tracing::info!("Cleared database.");
}

pub async fn start(config: &Config) {
    let cors = CorsLayer::new().allow_origin(Any).allow_headers(Any); // Allow all origins (open policy)

    let app = Router::new()
        .route("/", post(handler))
        .with_state((
            Arc::new(Mutex::new(ServerState::new(&config).await)),
            config.brc20_rpc_url.clone(),
        ))
        .layer(cors);

    let addr: SocketAddr = config.proxy_server_address.parse().expect(
        format!(
            "Invalid server address format: {}",
            config.proxy_server_address
        )
        .as_str(),
    );
    tracing::info!("RPC server listening on {}", addr);
    axum_server::bind(addr)
        .serve(app.into_make_service())
        .await
        .expect("Failed to start server");
}

async fn handler(
    State((server, brc20_rpc_url)): State<(Arc<Mutex<ServerState>>, String)>,
    OriginalUri(_): OriginalUri,
    req: Request,
) -> Response {
    let headers = req.headers().clone();
    let body = req.into_body();

    let body = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            return wrap_error(
                -32700,
                format!("Parse error: {}", err),
                serde_json::Value::Null,
            );
        }
    };

    tracing::debug!("Request body: {}", String::from_utf8_lossy(&body));

    let is_batch = is_batch_request(&body);
    let mut requests = Vec::new();
    if is_batch {
        let batch = match parse_batch(&body) {
            Ok(batch) => batch,
            Err(err) => {
                return wrap_error(
                    -32700,
                    format!("Parse error: {}", err),
                    serde_json::Value::Null,
                );
            }
        };
        requests.extend(batch);
    } else {
        let request = match parse_request(&body) {
            Ok(request) => request,
            Err(err) => {
                return wrap_error(
                    -32700,
                    format!("Parse error: {}", err),
                    serde_json::Value::Null,
                );
            }
        };
        requests.push(request);
    }

    let mut responses = Vec::new();
    for request in &requests {
        tracing::info!(
            "Request method: {}, params: {:?}",
            request.method,
            request.params
        );
        let response = handle_single_request(
            server.clone(),
            brc20_rpc_url.clone(),
            headers.clone(),
            request,
        )
        .await;
        tracing::info!("Response: {}", response);
        responses.push(response);
    }

    if is_batch {
        make_response(serde_json::Value::Array(responses))
    } else if let Some(response) = responses.into_iter().next() {
        make_response(response)
    } else {
        wrap_error(
            -32603,
            "Internal error".to_string(),
            serde_json::Value::Null,
        )
    }
}

async fn handle_single_request(
    server: Arc<Mutex<ServerState>>,
    brc20_rpc_url: String,
    headers: HeaderMap,
    request: &JsonRpcRequest,
) -> serde_json::Value {
    static CLIENT: LazyLock<Client> = LazyLock::new(|| Client::new());

    let id = request.id.clone();
    let params = request
        .params
        .as_ref()
        .map(|p| p.as_array().cloned())
        .flatten()
        .unwrap_or(Vec::new());

    match request.method.as_str() {
        "eth_getBalance" => {
            return match server.lock().await.get_balance().await {
                Ok(balance) => wrap_json_rpc(serde_json::json!(U64ED::from(balance)), id),
                Err(err) => {
                    wrap_json_rpc_error(-32000, format!("Failed to get balance: {}", err), id)
                }
            };
        }
        "eth_getTransactionCount" => {
            if params.len() == 0 {
                return wrap_json_rpc_error(-32602, "Invalid param count".to_string(), id);
            }
            let Some(addr) = params[0].as_str().map(|s| s.to_string()) else {
                return wrap_json_rpc_error(-32602, "Invalid parameter type".to_string(), id);
            };
            let Ok(addr) = addr.parse::<Address>() else {
                return wrap_json_rpc_error(-32602, "Invalid address".to_string(), id);
            };
            let nonce = match server.lock().await.get_nonce(addr).await {
                Ok(nonce) => nonce,
                Err(err) => {
                    return wrap_json_rpc_error(
                        -32000,
                        format!("Failed to get nonce: {}", err),
                        id,
                    );
                }
            };

            return wrap_json_rpc(serde_json::json!(U64ED::from(nonce)), id);
        }
        "eth_estimateGas" => {
            if params.len() == 0 {
                return wrap_json_rpc_error(-32602, "Invalid param count".to_string(), id);
            }
            let call_params = remove_duplicate_input_and_data(&mut params[0].clone());
            let Ok(eth_call) = serde_json::from_value::<EthCall>(call_params) else {
                return wrap_json_rpc_error(-32602, "Failed to decode call params".to_string(), id);
            };
            let gas = match server.lock().await.estimate_fee(eth_call).await {
                Ok(gas) => gas,
                Err(err) => {
                    return wrap_json_rpc_error(
                        -32000,
                        format!("Failed to estimate fee: {}", err),
                        id,
                    );
                }
            };
            return wrap_json_rpc(serde_json::json!(U64ED::from(gas)), id);
        }
        "eth_sendRawTransaction" => {
            if params.len() == 0 {
                return wrap_json_rpc_error(-32602, "Invalid param count".to_string(), id);
            }
            let Ok(tx) = serde_json::from_value::<String>(params[0].clone()) else {
                return wrap_json_rpc_error(-32602, "Invalid params".to_string(), id);
            };
            let tx_hash = match server.lock().await.mint(&tx).await {
                Ok(tx_hash) => tx_hash,
                Err(err) => {
                    return wrap_json_rpc_error(
                        -32000,
                        format!("Failed to mint BRC20 inscription: {}", err),
                        id,
                    );
                }
            };
            return wrap_json_rpc(serde_json::json!(tx_hash), id);
        }
        "eth_accounts" => {
            // Return empty array to disable account management in clients
            return wrap_json_rpc(serde_json::json!([]), id);
        }
        _ => {}
    }

    let brc20_response = CLIENT
        .post(&brc20_rpc_url)
        .body(serde_json::to_vec(request).unwrap())
        .headers(headers.clone())
        .send()
        .await;

    match brc20_response {
        Ok(response) => {
            let response_bytes = response.bytes().await.unwrap();
            match serde_json::from_slice(&response_bytes) {
                Ok(value) => value,
                Err(_) => wrap_json_rpc_error(-32603, "Internal error".to_string(), id),
            }
        }
        Err(_) => wrap_json_rpc_error(
            -32000,
            "Failed to connect to BRC2.0 RPC server".to_string(),
            id,
        ),
    }
}
