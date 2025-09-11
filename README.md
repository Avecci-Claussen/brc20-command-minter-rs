# BRC2.0 Command Minter

A Rust-based server for minting BRC2.0 inscriptions using an EVM wallet as a signer, and a Bitcoin wallet for funding transactions.

## Features

- **Bitcoin Wallet Integration**: Uses a Bitcoin wallet to fund and inscribe transactions.
- **EVM Wallet Integration**: EVM interface using `eth_sendRawTransaction` to send pre-signed transactions.
- **Transaction Fee Management**: Automatically estimates and manages transaction fees.
- **Nonce Management**: Tracks and manages nonces for transactions, see caveats.
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
    - `TO_SPEND_FEE_RATE`: The fee rate (in sat/vB) to use when funding transactions.
    - `PROXY_SERVER_ADDRESS`: The address and port for the server to listen on.
    - `DB_PATH`: (Optional) Path to the SQLite database file. Defaults to `brc20_minter.db`.

3. **Run the Server**:

    Optionally, you can set the `RUST_LOG` environment variable (e.g. `info`, `debug`, `trace`) to enable logging:

    ```bash
    cargo run --release
    ```
    or run the compiled binary directly:

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
    --value 0x0 \
    --gas-limit 21000 \
    --legacy
    ```

## Modified `eth_` Endpoints

- `eth_getBalance`: Returns the balance of the Bitcoin Wallet in satoshis for easy integration.

- `eth_estimateGas`: Estimates the total sats required for a transaction by calculating the necessary Bitcoin fee. This won't return the actual gas usage, but the estimated fee, so you can fund the Bitcoin wallet before sending the transaction.

- `eth_getTransactionCount`: Returns the nonce for the specified EVM address, including pending transactions stored in the database.

- `eth_sendRawTransaction`: Accepts a pre-signed transaction, funds it with Bitcoin, and inscribes it as a BRC2.0 transaction. Setting the gas limit in the transaction to a high value will increase the size of the inscription and the total fee, so set it appropriately after estimating the gas using the proxied BRC2.0 server, or use a reasonable default.

- `eth_accounts`: Returns an empty array to avoid confusion, as this server does not manage EVM accounts.

All other methods are proxied to the specified BRC2.0 server.

## Caveats

- **eth_estimateGas returns a BTC value**: This method estimates the total satoshis required for the transaction based on `TO_SPEND_FEE_RATE` and the size of the transaction. It does not return the actual gas used by the EVM transaction, so you should use the underlying BRC2.0 server to get gas estimates and/or set a reasonable gas limit in your transaction accordingly. Otherwise, the transaction may fail due to insufficient gas. <u>BRC2.0 allows 12000 gas per 1 byte of inscription data.</u>
- **eth_sendTransaction is not supported**: This method is not supported. Only `eth_sendRawTransaction` is implemented, as this server does not intended for signing transactions, you should pre-sign your transactions using another wallet. `eth_accounts` will return an empty array to avoid confusion.
- **Nonce Management might not work with multiple instances**: The server tracks nonces in a SQLite database. If multiple instances of the server are running with the same EVM address, nonce conflicts may occur. Ensure only one instance is managing a specific EVM address.
- **Transaction Fees are constant, and they should be monitored and changed if necessary**: The server estimates transaction fees based on the `TO_SPEND_FEE_RATE` environment variable. Ensure this value is appropriate for the current network conditions.
- **Security!**: Always ensure that your Bitcoin and EVM wallets are secure, as they are critical to the operation of this server.
