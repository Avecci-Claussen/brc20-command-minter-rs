use bitcoin::Network;

static SECRET_KEY: &str = "SECRET";
static BRC20_RPC_URL_KEY: &str = "BRC20_RPC_URL";
static BITCOIN_RPC_URL_KEY: &str = "BITCOIN_RPC_URL";
static BITCOIN_RPC_USER_KEY: &str = "BITCOIN_RPC_USER";
static BITCOIN_RPC_PASSWORD_KEY: &str = "BITCOIN_RPC_PASSWORD";
static BITCOIN_NETWORK_KEY: &str = "BITCOIN_NETWORK";
static PROXY_SERVER_ADDRESS_KEY: &str = "PROXY_SERVER_ADDRESS";
static EVM_ADDRESS_KEY: &str = "EVM_ADDRESS";
static TO_SPEND_FEE_RATE_KEY: &str = "TO_SPEND_FEE_RATE";
static TO_SPEND_FEE_RATE_DEFAULT: f64 = 5.0; // sats per vbyte
static DB_URL_KEY: &str = "DATABASE_URL";
static DB_URL_DEFAULT: &str = "sqlite://brc20_minter.db";

pub struct Config {
    pub evm_address: String,          // EVM address for Ethereum-based operations
    pub secret: String,               // Bitcoin wallet secret, 32 bytes hex string
    pub brc20_rpc_url: String,        // URL of the BRC2.0 RPC server
    pub bitcoin_rpc_url: String,      // URL of the Bitcoin Core RPC server
    pub bitcoin_rpc_user: String,     // Username for Bitcoin Core RPC
    pub bitcoin_rpc_password: String, // Password for Bitcoin Core RPC
    pub bitcoin_network: Network,     // Bitcoin network (e.g., Bitcoin::Network::Testnet)
    pub to_spend_fee_rate: f64,       // Fee rate in sats per vbyte
    pub proxy_server_address: String, // Optional address of the proxy server
    pub db_url: String,               // Database connection URL
}

impl Default for Config {
    fn default() -> Self {
        let proxy_server_address =
            std::env::var(PROXY_SERVER_ADDRESS_KEY).expect("PROXY_SERVER_ADDRESS must be set");
        let secret = std::env::var(SECRET_KEY).expect("SECRET must be set");
        let brc20_rpc_url = std::env::var(BRC20_RPC_URL_KEY).expect("BRC20_RPC_URL must be set");
        let bitcoin_rpc_url =
            std::env::var(BITCOIN_RPC_URL_KEY).expect("BITCOIN_RPC_URL must be set");
        let bitcoin_rpc_user =
            std::env::var(BITCOIN_RPC_USER_KEY).expect("BITCOIN_RPC_USER must be set");
        let bitcoin_rpc_password =
            std::env::var(BITCOIN_RPC_PASSWORD_KEY).expect("BITCOIN_RPC_PASSWORD must be set");
        let bitcoin_network =
            std::env::var(BITCOIN_NETWORK_KEY).expect("BITCOIN_NETWORK must be set");
        let to_spend_fee_rate = std::env::var(TO_SPEND_FEE_RATE_KEY)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(TO_SPEND_FEE_RATE_DEFAULT);
        let evm_address = std::env::var(EVM_ADDRESS_KEY).expect("EVM_ADDRESS must be set");
        let db_url = std::env::var(DB_URL_KEY).unwrap_or_else(|_| DB_URL_DEFAULT.to_string());
        Self {
            brc20_rpc_url,
            evm_address,
            secret,
            bitcoin_rpc_url,
            bitcoin_rpc_user,
            bitcoin_rpc_password,
            bitcoin_network: bitcoin_network
                .parse()
                .expect("BITCOIN_NETWORK must be a valid Bitcoin network"),
            to_spend_fee_rate,
            proxy_server_address,
            db_url,
        }
    }
}
