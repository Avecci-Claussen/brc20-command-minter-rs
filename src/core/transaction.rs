use std::{error::Error, str::FromStr};

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

/// BRC2.0 gas budget: ~12_000 gas per byte of inscription data.
pub const GAS_PER_BYTE: u64 = 12_000;

/// Bytes of inscription payload implied by an EVM gas budget (including +1).
pub fn inscription_bytes_for_gas(estimated_gas: u64) -> u64 {
    estimated_gas
        .checked_div(GAS_PER_BYTE)
        .unwrap_or(0)
        .saturating_add(1)
}

/// Convert a pre-signed EVM tx into a BRC2.0 `brc20-prog` inscription, padding
/// with spaces so inscription length matches the gas budget.
///
/// Returns `Err` if the required (or unpadded) size exceeds `max_inscription_bytes`.
/// This is fail-closed: we never allocate unbounded padding (H-02 fee drain / OOM).
pub fn convert_to_brc20_inscription(
    pre_signed_eth_tx: &str,
    estimated_gas: u64,
    max_inscription_bytes: u64,
) -> Result<String, String> {
    if max_inscription_bytes == 0 {
        return Err("max_inscription_bytes must be greater than 0".to_string());
    }

    let required_inscription_size = inscription_bytes_for_gas(estimated_gas);
    if required_inscription_size > max_inscription_bytes {
        return Err(format!(
            "Inscription size from gas_limit/estimate ({} bytes for gas {}) exceeds MAX_INSCRIPTION_BYTES ({}). \
             Reduce gas_limit or raise MAX_INSCRIPTION_BYTES.",
            required_inscription_size, estimated_gas, max_inscription_bytes
        ));
    }

    let base64_data = Base64Bytes::from_bytes(
        Bytes::from_str(
            pre_signed_eth_tx
                .strip_prefix("0x")
                .unwrap_or(pre_signed_eth_tx),
        )
        .map_err(|e| format!("Failed to decode pre-signed Ethereum transaction: {e}"))?,
    )
    .map_err(|e| format!("Failed to convert to Base64Bytes: {e}"))?;

    let inscription = format!(
        r#"{{"p":"brc20-prog","op":"t","b":"{}"}}"#,
        base64_data.to_string()
    );
    let inscription_length = inscription.len() as u64;

    if inscription_length > max_inscription_bytes {
        return Err(format!(
            "Unpadded inscription length ({} bytes) exceeds MAX_INSCRIPTION_BYTES ({}).",
            inscription_length, max_inscription_bytes
        ));
    }

    // Pad with spaces if inscription is smaller than required size
    let pad_len = required_inscription_size.saturating_sub(inscription_length) as usize;
    Ok(inscription + &" ".repeat(pad_len))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TX: &str =
        "0xf86c8008504a817c80082520894b60e8dd61c5d32be8058bb8eb970870f072331550880de0b6b3a76400008025a0b4c9";

    #[test]
    fn test_convert_to_brc20_inscription() {
        let estimated_gas = 5_000_000;
        let inscription =
            convert_to_brc20_inscription(SAMPLE_TX, estimated_gas, 100_000).expect("ok");
        assert!(inscription.starts_with(r#"{"p":"brc20-prog","op":"t","b":"#));
        assert!(inscription.len() >= (estimated_gas / GAS_PER_BYTE) as usize);
        assert!(inscription.len() as u64 <= 100_000);
    }

    #[test]
    fn test_rejects_gas_limit_padding_above_cap() {
        // gas that requests ~1_000_001 bytes of inscription → over 100_000 cap
        let gas = 100_000 * GAS_PER_BYTE + GAS_PER_BYTE; // => 100_001 bytes required
        let err = convert_to_brc20_inscription(SAMPLE_TX, gas, 100_000).unwrap_err();
        assert!(
            err.contains("MAX_INSCRIPTION_BYTES"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_rejects_u64_max_gas_without_allocating() {
        // Must fail closed before `" ".repeat(...)` (would OOM / panic otherwise).
        let err = convert_to_brc20_inscription(SAMPLE_TX, u64::MAX, 100_000).unwrap_err();
        assert!(err.contains("exceeds MAX_INSCRIPTION_BYTES"));
        let required = inscription_bytes_for_gas(u64::MAX);
        assert!(required > 100_000);
    }

    #[test]
    fn test_allows_exact_cap_boundary() {
        // Choose gas so required size == cap, and sample tx fits under that.
        let cap = 1_000u64;
        let gas = (cap - 1) * GAS_PER_BYTE; // required = cap
        assert_eq!(inscription_bytes_for_gas(gas), cap);
        let inscription = convert_to_brc20_inscription(SAMPLE_TX, gas, cap).expect("at cap");
        assert_eq!(inscription.len() as u64, cap);
    }

    #[test]
    fn test_zero_max_rejected() {
        let err = convert_to_brc20_inscription(SAMPLE_TX, 21_000, 0).unwrap_err();
        assert!(err.contains("greater than 0"));
    }
}
