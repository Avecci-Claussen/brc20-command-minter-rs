# BRC2.0 Command Minter

A Rust-based server for minting BRC2.0 inscriptions using an EVM wallet as a signer, and a Bitcoin wallet for funding transactions.

## Features

- **Bitcoin Wallet Integration**: Uses a Bitcoin wallet to fund transactions and inscribe them.
- **EVM Wallet Integration**: EVM interface using `eth_sendRawTransaction` to send pre-signed transactions.
- **Transaction Fee Management**: Automatically estimates and manages transaction fees.
- **Nonce Management**: Tracks and manages nonces for transactions, see caveats below.
- **Database Support**: Uses SQLite for storing previous transaction data and nonces.
- **EVM Address Configuration**: Specify the EVM address to be used for signing transactions to protect against unauthorized use.
- **Proxy BRC2.0 Server**: Routes unhandled requests to a specified BRC2.0 server for gas estimation and block, transaction data.

## Getting Started

1. **Clone the Repository and Build**:
   ```bash
   git clone https://github.com/bestinslot-xyz/brc20-command-minter-rs.git
   cd brc20-command-minter-rs
   cargo build --release
   ```

2. **Set Up Environment Variables**:
    Create a `.env` file in the project root based on the `env.sample` file
    ```bash
    cp env.sample .env
    ```

    Edit the `.env` file to set your configuration:
    - `EVM_ADDRESS`: The EVM address to be used for signing transactions (0x prefixed).
    - `SECRET`: The Bitcoin secret in 32-byte hex format.
    - `BRC20_RPC_URL`: The URL of the BRC2.0 server to proxy requests to.
    - `BITCOIN_RPC_URL`: The URL of your Bitcoin node's RPC interface.
    - `BITCOIN_RPC_USER`: The RPC username for your Bitcoin node.
    - `BITCOIN_RPC_PASSWORD`: The RPC password for your Bitcoin node.
    - `BITCOIN_NETWORK`: The Bitcoin network to use (e.g., `bitcoin`, `testnet`, `signet`).
    - `FEE_RATE_CATEGORY`: The fee rate category for mempool.space API. Allowed values are `fastest`, `halfHour`, `hour`, `economy`, `minimum`.
    - `PROXY_SERVER_ADDRESS`: Address and port to listen on (sample: `127.0.0.1:8545`). Non-loopback binds require `RPC_AUTH_TOKEN`, or `ALLOW_REMOTE_BIND=true`.
    - `DB_PATH` / `DATABASE_URL`: (Optional) SQLite URL. Defaults to `sqlite://brc20_minter.db`.
    - `MAX_INSCRIPTION_BYTES`: (Optional) Maximum BRC2.0 inscription size in bytes after gas-based space padding. Defaults to `1000000` (1 MB, the BRC2.0 calldata ceiling). Requests whose `gas_limit` would require a larger padded inscription are **rejected** before padding or minting.
    - `RPC_AUTH_TOKEN`: (Optional) When set, require `Authorization: Bearer <token>` on requests. Required for non-loopback binds unless `ALLOW_REMOTE_BIND=true`.
    - `ALLOW_REMOTE_BIND`: (Optional) Allow a non-loopback bind without `RPC_AUTH_TOKEN`.
    - `CORS_ALLOW_ORIGIN`: (Optional) Enable CORS (`*` or one origin). Unset leaves CORS off.

> [!CAUTION]
> **UTXO management is not inscription aware**: The server may burn the inscriptions that you've sent to the bitcoin wallet. Also those inscriptions may prevent the inscribed commands to run since re-inscriptions are not allowed on BRC20. Always use a fresh wallet with no inscriptions.

3. **Run the Server**:

    You can optionally set the `RUST_LOG` environment variable (e.g. `info`, `debug`, `trace`) to enable logging:

    ```bash
    cargo run --release
    ```
    or run the compiled binary:

    ```bash
    ./target/release/brc20-command-minter-rs
    ```

4. **Using the Server**:

    The server listens for JSON-RPC requests. You can send requests to the server using tools like `curl` or use foundry wallets that support custom RPC endpoints:

    ```bash
    cast send <CONTRACT_ADDRESS> \
    "transfer(address,uint256)" <TO_ADDRESS> <AMOUNT> \
    --rpc-url <RPC_URL> \
    --private-key <EVM_PRIVATE_KEY> \
    --value 0 \
    --gas-limit 21000 \
    --gas-price 3wei \
    --async \
    --legacy
    ```

    Bitcoin miner fee-rate is received from "gas-price" parameter. For 5 sat/vB, send 5 wei as gas price.

    If gas-price is set to `0`, it'll use the configured mempool fee rate.
    
    If gas price is > `10 * mempool_rate` tx is rejected.

## Modified `eth_` Endpoints

- `eth_getBalance`: Returns the balance of the Bitcoin Wallet in satoshis for easy integration.

- `eth_estimateGas`: Estimates the total sats required for a transaction by calculating the required Bitcoin fee. This returns the estimated fee, not the actual gas usage, so you can fund the Bitcoin wallet before sending the transaction.

- `eth_getTransactionCount`: Returns the nonce for the specified EVM address, including pending transactions stored in the database.

- `eth_sendRawTransaction`: Accepts a pre-signed transaction, funds it with Bitcoin, and inscribes it as a BRC2.0 transaction. Inscription size scales with `gas_limit` (12 000 gas ≈ 1 byte, via space padding). Sizes above `MAX_INSCRIPTION_BYTES` are rejected before broadcast. Estimate gas via the proxied BRC2.0 server, or choose a reasonable default.

- `eth_accounts`: Returns an empty array to avoid confusion, as this server does not manage EVM accounts.

- `eth_gasPrice`: Returns the current fee rate of Bitcoin Network. Uses mempool.space API. Fee rate category can be configured via `FEE_RATE_CATEGORY`.

All other methods are proxied to the specified BRC2.0 server.

## Caveats

- **eth_estimateGas returns a BTC value**: This method estimates the total satoshis required for the transaction based on the size of the transaction, it assumes 1.0 sat/vB fee rate. It does not return the actual gas used by the EVM transaction, so you should use the underlying BRC2.0 server to get gas estimates and/or set a reasonable gas limit in your transaction accordingly. Otherwise, the transaction may fail due to insufficient gas. <u>BRC2.0 allows 12000 gas per 1 byte of inscription data.</u>
- **eth_sendTransaction is not supported**: This method is not supported. Only `eth_sendRawTransaction` is implemented, as this server is not intended for signing transactions, you should pre-sign your transactions using another wallet. `eth_accounts` will return an empty array to avoid confusion.
- **Nonce management may not work with multiple instances**: The server tracks nonces in a SQLite database. If multiple instances of the server are running with the same EVM address, nonce conflicts may occur. Ensure only one instance is managing a specific EVM address.
- **Security!**: Always keep your Bitcoin and EVM wallets secure, they are critical to this server’s operation.
- **Bind address / RPC auth**: Prefer `PROXY_SERVER_ADDRESS=127.0.0.1:…`. Non-loopback binds require `RPC_AUTH_TOKEN`, or `ALLOW_REMOTE_BIND=true`. Mint still requires a signature from `EVM_ADDRESS`. CORS is off unless `CORS_ALLOW_ORIGIN` is set.
- **Inscription size cap**: Inscription length scales with `gas_limit` via space padding. An absurd or fat-fingered `gas_limit` (e.g. `u64::MAX`) can request a huge inscription and OOM/panic during `" ".repeat`. The server enforces `MAX_INSCRIPTION_BYTES` (default 1 000 000, BRC2.0 calldata limit) and rejects oversized requests before padding or minting. Mint remains authenticated to `EVM_ADDRESS` on a trusted, non-public RPC.