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
use http::{HeaderMap, HeaderValue};
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
    max_inscription_bytes: u64,
}

impl ServerState {
    async fn new(config: &Config) -> Self {
        // Initialize the server here
        let minter = Minter::new(
            config.bitcoin_network.clone(),
            &config.secret,
            &config.bitcoin_rpc_url,
            &config.bitcoin_rpc_user,
            &config.bitcoin_rpc_password,
            &config.fee_rate_category,
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
        tracing::info!(
            "Max inscription bytes (gas padding cap): {}",
            config.max_inscription_bytes
        );

        ServerState {
            minter,
            brc20_client,
            chain_id,
            evm_address: config.evm_address.clone(),
            database,
            max_inscription_bytes: config.max_inscription_bytes,
        }
    }

    async fn clear_tables(&self) {
        self.database.clear_tables().await;
    }

    async fn get_nonce(&self, address: Address) -> Result<u64, Box<dyn Error>> {
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

        let database_nonce = self
            .database
            .get_transaction_count(&address.to_string(), brc20_nonce)
            .await as u64;

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
        let inscription =
            match convert_to_brc20_inscription(&dummy_tx, estimated_gas, self.max_inscription_bytes)
            {
                Ok(inscription) => inscription,
                Err(err) => {
                    return Err(format!(
                        "Failed to create BRC20 inscription for fee estimation: {}",
                        err
                    )
                    .into());
                }
            };
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

        let mut fee_rate = tx.gas_price as f64; // in sat/vB
        if fee_rate < 1.0 {
            fee_rate = self.minter.get_mempool_fee_rate().await;
            tracing::warn!(
                "The provided gas price is too low ({} wei). Using current mempool fee rate of {} sat/vB instead.",
                tx.gas_price,
                fee_rate
            );
        }
        if !self.minter.check_fee_rate_warning(fee_rate).await {
            return Err(
                "Transaction aborted due to fee rate too high error. Please adjust the gas price."
                    .into(),
            );
        }

        // Convert the signed Ethereum transaction to a BRC20 inscription.
        // Reject absurd/fat-fingered gas_limit padding before mint construction.
        let inscription = match convert_to_brc20_inscription(
            signed_eth_tx,
            tx.gas_limit,
            self.max_inscription_bytes,
        ) {
            Ok(inscription) => inscription,
            Err(err) => {
                return Err(format!("Refusing to mint: {}", err).into());
            }
        };
        let mint_result = match self.minter.mint(&inscription, fee_rate).await {
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

    async fn get_fee_rate(&self) -> Result<u64, Box<dyn Error>> {
        let fee_rate = self.minter.get_mempool_fee_rate().await;
        let fee_rate_rounded = fee_rate.round() as u64;
        if fee_rate_rounded == 0 {
            Ok(1)
        } else {
            Ok(fee_rate_rounded)
        }
    }
}

pub async fn reset(config: &Config) {
    let server = ServerState::new(&config).await;
    server.clear_tables().await;
    tracing::info!("Cleared database.");
}

pub async fn start(config: &Config) {
    let addr: SocketAddr = config.proxy_server_address.parse().expect(
        format!(
            "Invalid server address format: {}",
            config.proxy_server_address
        )
        .as_str(),
    );
    validate_bind_policy(&addr, config);

    let mut app = Router::new()
        .route("/", post(handler))
        .with_state((
            Arc::new(Mutex::new(ServerState::new(config).await)),
            config.brc20_rpc_url.clone(),
            config.rpc_auth_token.clone(),
        ));

    if let Some(origin) = &config.cors_allow_origin {
        let cors = if origin == "*" {
            tracing::info!("CORS_ALLOW_ORIGIN=*");
            CorsLayer::new().allow_origin(Any).allow_headers(Any)
        } else {
            let value = HeaderValue::from_str(origin).unwrap_or_else(|_| {
                panic!(
                    "CORS_ALLOW_ORIGIN must be a valid header value (got {:?})",
                    origin
                )
            });
            CorsLayer::new()
                .allow_origin(value)
                .allow_headers(Any)
        };
        app = app.layer(cors);
        tracing::info!("CORS allow origin: {}", origin);
    }

    if config.rpc_auth_token.is_some() {
        tracing::info!("RPC_AUTH_TOKEN is set; Bearer auth required");
    }

    tracing::info!("RPC server listening on {}", addr);
    axum_server::bind(addr)
        .serve(app.into_make_service())
        .await
        .expect("Failed to start server");
}

fn validate_bind_policy(addr: &SocketAddr, config: &Config) {
    if addr.ip().is_loopback() {
        return;
    }
    if config.rpc_auth_token.is_some() {
        tracing::info!("Listening on non-loopback address {} with RPC_AUTH_TOKEN", addr);
        return;
    }
    if config.allow_remote_bind {
        tracing::warn!(
            "Listening on non-loopback address {} with ALLOW_REMOTE_BIND=true and no RPC_AUTH_TOKEN",
            addr
        );
        return;
    }
    panic!(
        "PROXY_SERVER_ADDRESS {} is not loopback. Set RPC_AUTH_TOKEN, bind 127.0.0.1, \
         or set ALLOW_REMOTE_BIND=true if access control is handled elsewhere.",
        addr
    );
}

fn unauthorized_response() -> Response {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "error": { "code": -32001, "message": "Unauthorized" },
        "id": null
    });
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&body).unwrap().into())
        .expect("Failed to create unauthorized response")
}

fn check_rpc_auth(headers: &HeaderMap, expected: &Option<String>) -> Result<(), Response> {
    let Some(token) = expected else {
        return Ok(());
    };
    let Some(header) = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return Err(unauthorized_response());
    };
    let expected_header = format!("Bearer {}", token);
    if header == expected_header {
        Ok(())
    } else {
        Err(unauthorized_response())
    }
}

async fn handler(
    State((server, brc20_rpc_url, rpc_auth_token)): State<(
        Arc<Mutex<ServerState>>,
        String,
        Option<String>,
    )>,
    OriginalUri(_): OriginalUri,
    req: Request,
) -> Response {
    let headers = req.headers().clone();
    if let Err(response) = check_rpc_auth(&headers, &rpc_auth_token) {
        return response;
    }
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
        "eth_gasPrice" => {
            return match server.lock().await.get_fee_rate().await {
                Ok(fee_rate) => wrap_json_rpc(serde_json::json!(U64ED::from(fee_rate)), id),
                Err(err) => {
                    wrap_json_rpc_error(-32000, format!("Failed to get fee rate: {}", err), id)
                }
            };
        }
        _ => {}
    }

    let brc20_response = CLIENT
        .post(&brc20_rpc_url)
        .body(serde_json::to_vec(request).unwrap())
        .headers(drop_accept_encoding(headers))
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

fn drop_accept_encoding(mut headers: HeaderMap) -> HeaderMap {
    // Drop hop / client auth headers before proxying to BRC20_RPC_URL.
    let remove_keys = vec!["accept-encoding", "host", "authorization"];
    for key in remove_keys {
        headers.remove(key);
    }
    headers.append("Accept-Encoding", HeaderValue::from_static("identity"));
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn auth_skipped_when_token_unset() {
        let headers = HeaderMap::new();
        assert!(check_rpc_auth(&headers, &None).is_ok());
    }

    #[test]
    fn auth_rejects_missing_header() {
        let headers = HeaderMap::new();
        assert!(check_rpc_auth(&headers, &Some("secret".into())).is_err());
    }

    #[test]
    fn auth_accepts_bearer_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        assert!(check_rpc_auth(&headers, &Some("secret".into())).is_ok());
    }

    #[test]
    fn auth_rejects_wrong_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        );
        assert!(check_rpc_auth(&headers, &Some("secret".into())).is_err());
    }

    #[test]
    fn proxy_header_strip_removes_authorization() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        headers.insert(http::header::HOST, HeaderValue::from_static("evil.example"));
        let cleaned = drop_accept_encoding(headers);
        assert!(cleaned.get(http::header::AUTHORIZATION).is_none());
        assert!(cleaned.get(http::header::HOST).is_none());
    }

    #[test]
    fn loopback_bind_allowed_without_token() {
        let config = Config {
            evm_address: "0x0".into(),
            secret: "00".repeat(32),
            brc20_rpc_url: "http://127.0.0.1:1".into(),
            bitcoin_rpc_url: "http://127.0.0.1:1".into(),
            bitcoin_rpc_user: "u".into(),
            bitcoin_rpc_password: "p".into(),
            bitcoin_network: bitcoin::Network::Regtest,
            proxy_server_address: "127.0.0.1:8545".into(),
            db_url: "sqlite://:memory:".into(),
            fee_rate_category: crate::core::config::FeeRateCategory::Fastest,
            max_inscription_bytes: 1_000_000,
            rpc_auth_token: None,
            allow_remote_bind: false,
            cors_allow_origin: None,
        };
        let addr: SocketAddr = "127.0.0.1:8545".parse().unwrap();
        validate_bind_policy(&addr, &config);
    }

    #[test]
    #[should_panic(expected = "is not loopback")]
    fn non_loopback_without_token_panics() {
        let config = Config {
            evm_address: "0x0".into(),
            secret: "00".repeat(32),
            brc20_rpc_url: "http://127.0.0.1:1".into(),
            bitcoin_rpc_url: "http://127.0.0.1:1".into(),
            bitcoin_rpc_user: "u".into(),
            bitcoin_rpc_password: "p".into(),
            bitcoin_network: bitcoin::Network::Regtest,
            proxy_server_address: "0.0.0.0:8545".into(),
            db_url: "sqlite://:memory:".into(),
            fee_rate_category: crate::core::config::FeeRateCategory::Fastest,
            max_inscription_bytes: 1_000_000,
            rpc_auth_token: None,
            allow_remote_bind: false,
            cors_allow_origin: None,
        };
        let addr: SocketAddr = "0.0.0.0:8545".parse().unwrap();
        validate_bind_policy(&addr, &config);
    }
}
