use std::{error::Error, str::FromStr, u64};

use alloy::{
    consensus::TxLegacy,
    network::{EthereumWallet, TransactionBuilder},
    primitives::{Bytes, U256},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
};
use brc20_prog::types::{Base64Bytes, EthCall};

pub async fn generate_dummy_signed_tx(
    chain_id: u64,
    eth_call: EthCall,
) -> Result<String, Box<dyn Error>> {
    let wallet = EthereumWallet::new(PrivateKeySigner::random());

    let tx_builder: TransactionRequest = TxLegacy::default().into();
    let tx_builder = tx_builder
        .with_chain_id(chain_id)
        .with_nonce(u64::MAX) // Set to max, actual nonce will be fetched by the node
        .with_gas_price(u128::MAX) // Set to max, actual gas will be estimated by the node
        .with_gas_limit(u64::MAX) // Set to max, actual gas will be estimated by the node
        .with_to(eth_call.to.expect("EthCall must have 'to' address").address)
        .with_value(U256::MAX)
        .with_input(
            Bytes::from_str(&eth_call.data.expect("EthCall must have data").to_string()).unwrap(),
        );

    let built_tx = tx_builder.build(&wallet).await.unwrap();

    let signed_tx = built_tx.into_signed();

    let mut rlp_encoded = Vec::new();
    signed_tx.rlp_encode(&mut rlp_encoded);

    Ok(hex::encode(rlp_encoded))
}

static GAS_PER_BYTE: u64 = 12_000;

pub fn convert_to_brc20_inscription(pre_signed_eth_tx: &str, estimated_gas: u64) -> String {
    let required_inscription_size = estimated_gas / GAS_PER_BYTE as u64 + 1;
    let base64_data = Base64Bytes::from_bytes(
        Bytes::from_str(
            pre_signed_eth_tx
                .strip_prefix("0x")
                .unwrap_or(pre_signed_eth_tx),
        )
        .expect("Failed to decode pre-signed Ethereum transaction"),
    )
    .expect("Failed to convert to Base64Bytes");
    let inscription = format!(
        r#"{{"p":"brc20-prog","op":"t","b":"{}"}}"#,
        base64_data.to_string()
    );
    let inscription_length = inscription.len() as u64;

    // Pad with spaces if inscription is smaller than required size
    let padded_inscription = inscription
        + &" ".repeat(required_inscription_size.saturating_sub(inscription_length) as usize);
    padded_inscription
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_to_brc20_inscription() {
        let eth_tx = "0xf86c8008504a817c80082520894b60e8dd61c5d32be8058bb8eb970870f072331550880de0b6b3a76400008025a0b4c9";
        let estimated_gas = 5000000;
        let inscription = convert_to_brc20_inscription(eth_tx, estimated_gas);
        assert!(inscription.starts_with(r#"{"p":"brc20-prog","op":"t","b":"#));
        assert!(inscription.len() >= (estimated_gas / GAS_PER_BYTE) as usize);
    }
}
