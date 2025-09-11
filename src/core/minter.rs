use std::{error::Error, str::FromStr};

use bitcoin::{
    Address, Amount, CompressedPublicKey, EcdsaSighashType, Network, PrivateKey, ScriptBuf,
    Sequence, TapSighashType, Transaction, TxIn, TxOut, Witness,
    absolute::LockTime,
    consensus::encode,
    hashes::Hash,
    key::{
        Secp256k1, XOnlyPublicKey,
        rand::{self, Rng},
    },
    script::{self, Builder, write_scriptint},
    secp256k1::{self, PublicKey, SecretKey},
    sighash::{self, SighashCache},
    taproot::{self, ControlBlock, LeafVersion},
    transaction::Version,
};
use bitcoincore_rpc::{Auth, Client, RpcApi, jsonrpc::serde_json::Value};

#[derive(Clone)]
pub struct Utxo {
    pub txid: bitcoin::Txid,
    pub vout: u32,
    pub value: Amount,
    pub address: Address,

    pub pubkey: Option<bitcoin::PublicKey>, // p2pkh and p2wpkh only

    // p2tr script spend only
    pub tap_leaf_script: Option<ScriptBuf>,
    pub tap_leaf_control_block: Option<ControlBlock>,
}

pub struct InscriptionDetails {
    pub mime_type: Vec<u8>,
    pub metadata: Option<Vec<u8>>,
    pub metaprotocol: Option<Vec<u8>>,
    pub content_encoding: Option<Vec<u8>>,
    pub delegate: Option<Vec<u8>>,
    pub file_data: Vec<u8>,
}

pub struct MintTransactions {
    pub commit: Transaction,
    pub reveal: Transaction,
    pub send_to_op_return: Transaction,
    pub total_fee: u64,
}

pub struct MintResult {
    pub commit: bitcoin::Txid,
    pub reveal: bitcoin::Txid,
    pub send_to_op_return: bitcoin::Txid,
    pub total_fee: u64,
}

#[allow(dead_code)]
#[derive(serde::Deserialize)]
struct MempoolSpaceUtxoStatus {
    confirmed: bool,
    block_height: Option<u32>,
    block_time: Option<u64>,
}

#[allow(dead_code)]
#[derive(serde::Deserialize)]
struct MempoolSpaceUtxo {
    txid: String,
    vout: u32,
    value: u64,
    status: MempoolSpaceUtxoStatus,
}

const ENABLE_RBF_NO_LOCKTIME: u32 = 0xFFFFFFFD;

const DUST_VALUE_P2PKH: u64 = 546;
const DUST_VALUE_P2WPKH: u64 = 294;
const DUST_VALUE_P2SH: u64 = 540;
const DUST_VALUE_P2TR: u64 = 330;

pub struct Minter {
    network: Network,
    mempool_space_url: String,
    to_spend_fee_rate: f64, // sats per vbyte
    secp: Secp256k1<secp256k1::All>,
    secret_key: SecretKey,
    private_key: PrivateKey,
    compressed_public_key: CompressedPublicKey,
    secp_public_key: secp256k1::PublicKey,
    public_key: PublicKey,
    pub sender: Address,
    rpc_client: Client,
    dust_value: u64,
}

impl Minter {
    pub fn new(
        network: bitcoin::Network,
        to_spend_fee_rate: f64, // sats per vbyte
        private_key: &str,
        rpc_url: &str,
        rpc_user: &str,
        rpc_password: &str,
    ) -> Self {
        let secp = bitcoin::secp256k1::Secp256k1::new();
        let secret_key = SecretKey::from_str(private_key).expect("Invalid secret key");
        let private_key =
            PrivateKey::from_slice(&hex::decode(private_key).expect("Invalid hex"), network)
                .expect("Invalid private key");
        let secp_public_key = secret_key.public_key(&secp);
        let public_key = PublicKey::from_secret_key(&secp, &secret_key);
        let compressed_public_key = CompressedPublicKey::from_private_key(&secp, &private_key)
            .expect("Failed to get public key");
        let sender = Address::p2wpkh(&compressed_public_key, network);
        let rpc_client = Client::new(
            rpc_url,
            Auth::UserPass(rpc_user.to_string(), rpc_password.to_string()),
        )
        .expect("Failed to create RPC client");
        let dust_value = match sender.address_type() {
            Some(bitcoin::AddressType::P2pkh) => DUST_VALUE_P2PKH,
            Some(bitcoin::AddressType::P2wpkh) => DUST_VALUE_P2WPKH,
            Some(bitcoin::AddressType::P2sh) => DUST_VALUE_P2SH,
            Some(bitcoin::AddressType::P2tr) => DUST_VALUE_P2TR,
            _ => panic!("Unsupported address type"),
        };
        let mempool_space_url = match network {
            bitcoin::Network::Bitcoin => "https://mempool.space",
            bitcoin::Network::Testnet => "https://mempool.space/testnet",
            bitcoin::Network::Testnet4 => "https://mempool.space/testnet4",
            bitcoin::Network::Signet => "https://mempool.space/signet",
            _ => panic!("Unsupported network: {:?}", network),
        }
        .to_string();
        Minter {
            network,
            to_spend_fee_rate,
            rpc_client,
            secp,
            secret_key,
            private_key,
            secp_public_key,
            public_key,
            compressed_public_key,
            sender,
            dust_value,
            mempool_space_url,
        }
    }

    pub async fn get_fastest_fee_rate(&self) -> f64 {
        let url = format!("{}/api/v1/fees/recommended", self.mempool_space_url);
        let resp = reqwest::get(&url).await.expect("Failed to fetch fee rate");
        if !resp.status().is_success() {
            panic!("Failed to fetch fee rate: {}", resp.status());
        }
        let fee_data: serde_json::Value = resp.json().await.expect("Failed to parse fee rate");
        fee_data["fastestFee"]
            .as_f64()
            .expect("Failed to get fastest fee")
    }

    pub async fn check_fee_rate_warning(&self) {
        let fastest_fee = self.get_fastest_fee_rate().await;
        if self.to_spend_fee_rate < fastest_fee {
            tracing::warn!(
                "The configured to-spend fee rate ({}) is lower than the current fastest fee rate ({}). Transactions may take longer to confirm.",
                self.to_spend_fee_rate,
                fastest_fee
            );
        }
        if self.to_spend_fee_rate > fastest_fee * 3.0 {
            tracing::warn!(
                "The configured to-spend fee rate ({}) is significantly higher than the current fastest fee rate ({}). You may be overpaying for transaction fees.",
                self.to_spend_fee_rate,
                fastest_fee
            );
        }
    }

    pub async fn get_balance(&self) -> u64 {
        let utxos = self.get_utxos().await;

        let mut balance = Amount::from_sat(0);
        for utxo in utxos {
            balance = balance
                .checked_add(utxo.value)
                .expect("Overflow in balance");
        }
        balance.to_sat()
    }

    pub fn calculate_fee(&self, inputs: &Vec<Utxo>, outputs: &Vec<(ScriptBuf, Amount)>) -> u64 {
        let tx = construct_dummy_tx_from_in_outs(inputs, outputs);
        let vsize = tx.weight().to_vbytes_ceil() as f64;
        (vsize * self.to_spend_fee_rate).ceil() as u64
    }

    pub fn build_transaction(
        &self,
        utxos: &Vec<Utxo>,
        force_in_utxos: &Vec<Utxo>,
        outputs: &Vec<(ScriptBuf, Amount)>,
    ) -> Result<Transaction, Box<dyn Error>> {
        let mut utxos = utxos.clone();
        // sort utxos by value ascending
        utxos.sort_by_key(|u| u.value);
        let mut inputs: Vec<Utxo> = Vec::new();
        let mut outputs = outputs.clone();
        for utxo in force_in_utxos.iter() {
            inputs.push(utxo.clone());
            // check utxos and if same utxo is there, remove it
            if let Some(pos) = utxos
                .iter()
                .position(|x| x.txid == utxo.txid && x.vout == utxo.vout)
            {
                utxos.remove(pos);
            }
        }
        let mut total_input_value: u64 = inputs.iter().map(|u| u.value.to_sat()).sum();
        let total_target_value: u64 = outputs.iter().map(|(_, v)| v.to_sat()).sum();
        let mut fee = self.calculate_fee(&inputs, &outputs);
        while total_input_value < total_target_value + fee {
            tracing::debug!(
                "Total input value: {}, total target value + fee: {} + {} = {}, remaining utxo cnt: {}",
                total_input_value,
                total_target_value,
                fee,
                total_target_value + fee,
                utxos.len()
            );
            if utxos.is_empty() {
                return Err("Insufficient funds".into());
            }
            let last_utxo = utxos.pop().expect("No more UTXOs");
            total_input_value += last_utxo.value.to_sat();
            inputs.push(last_utxo.clone());
            fee = self.calculate_fee(&inputs, &outputs);
        }
        let additional_change_output_fee =
            f64::ceil(((self.sender.script_pubkey().len() + 9) as f64) * self.to_spend_fee_rate)
                as u64;
        let excess = total_input_value - fee - total_target_value;
        if excess > self.dust_value + additional_change_output_fee {
            outputs.push((
                self.sender.script_pubkey(),
                Amount::from_sat(excess - additional_change_output_fee),
            ));
        }
        Ok(construct_dummy_tx_from_in_outs(&inputs, &outputs))
    }

    pub fn build_commit_tx(
        &self,
        secret: &str,
        inscription_details: &Vec<InscriptionDetails>,
        postage: u64,
        utxos: &Vec<Utxo>,
    ) -> Result<Transaction, Box<dyn Error>> {
        let secp = bitcoin::secp256k1::Secp256k1::new();
        let taproot_secret_key = SecretKey::from_str(secret).expect("Invalid secret");
        let taproot_public_key = PublicKey::from_secret_key(&secp, &taproot_secret_key);
        let taproot_xonly_pk = XOnlyPublicKey::from(taproot_public_key);
        let reveal_script = build_reveal_script(&taproot_xonly_pk, &inscription_details, postage);
        let taproot_builder = taproot::TaprootBuilder::new()
            .add_leaf(0, reveal_script.clone())
            .expect("Failed to add leaf to TaprootBuilder");
        let spend_info = taproot_builder
            .finalize(&secp, taproot_xonly_pk)
            .expect("Failed to finalize TaprootBuilder");
        let taproot_output_key = spend_info.output_key();
        let taproot_address = Address::p2tr_tweaked(taproot_output_key, self.network);
        let control_block = spend_info
            .control_block(&(reveal_script.clone(), LeafVersion::TapScript))
            .expect("Failed to get control block");
        let mut dummy_reveal_outputs: Vec<TxOut> = Vec::new();
        for _ in inscription_details.iter() {
            dummy_reveal_outputs.push(TxOut {
                script_pubkey: self.sender.script_pubkey(),
                value: Amount::from_sat(postage),
            });
        }
        let mut script_witness = Witness::new();
        script_witness.push([0u8; 72].to_vec()); // placeholder for signature
        script_witness.push(reveal_script.to_bytes());
        script_witness.push(control_block.serialize());
        let dummy_reveal_inputs = vec![TxIn {
            previous_output: bitcoin::OutPoint {
                txid: bitcoin::Txid::all_zeros(),
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::from_consensus(ENABLE_RBF_NO_LOCKTIME),
            witness: script_witness,
        }];
        let dummy_reveal_tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: dummy_reveal_inputs,
            output: dummy_reveal_outputs,
        };
        let dummy_reveal_tx_fee =
            f64::ceil((dummy_reveal_tx.weight().to_vbytes_ceil() as f64) * self.to_spend_fee_rate)
                as u64;
        let total_postage = postage * (inscription_details.len() as u64);
        let total_needed = total_postage + dummy_reveal_tx_fee;
        self.build_transaction(
            utxos,
            &vec![],
            &vec![(
                taproot_address.script_pubkey(),
                Amount::from_sat(total_needed),
            )],
        )
    }

    pub fn build_reveal_tx(
        &self,
        secret: &str,
        commit_tx: &Transaction,
        inscription_details: &Vec<InscriptionDetails>,
        postage: u64,
    ) -> Transaction {
        let sk = SecretKey::from_str(secret).expect("Invalid secret");
        let pk = PublicKey::from_secret_key(&self.secp, &sk);
        let xonly_pk = XOnlyPublicKey::from(pk);
        let reveal_script = build_reveal_script(&xonly_pk, &inscription_details, postage);
        let taproot_builder = taproot::TaprootBuilder::new()
            .add_leaf(0, reveal_script.clone())
            .expect("Failed to add leaf to TaprootBuilder");
        let spend_info = taproot_builder
            .finalize(&self.secp, xonly_pk)
            .expect("Failed to finalize TaprootBuilder");
        let control_block = spend_info
            .control_block(&(reveal_script.clone(), LeafVersion::TapScript))
            .expect("Failed to get control block");
        let mut script_witness = Witness::new();
        script_witness.push([0u8; 72].to_vec()); // placeholder for signature
        script_witness.push(reveal_script.to_bytes());
        script_witness.push(control_block.serialize());
        let mut reveal_outputs: Vec<TxOut> = Vec::new();
        for _ in inscription_details.iter() {
            reveal_outputs.push(TxOut {
                script_pubkey: self.sender.script_pubkey(),
                value: Amount::from_sat(postage),
            });
        }
        let unsigned_tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: bitcoin::OutPoint {
                    txid: commit_tx.compute_txid(),
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::from_consensus(ENABLE_RBF_NO_LOCKTIME),
                witness: script_witness,
            }],
            output: reveal_outputs.clone(),
        };
        let sighash_type = TapSighashType::All;
        let hash = SighashCache::new(&unsigned_tx)
            .taproot_script_spend_signature_hash(
                0,
                &sighash::Prevouts::All(&[TxOut {
                    value: commit_tx.output[0].value,
                    script_pubkey: commit_tx.output[0].script_pubkey.clone(),
                }]),
                reveal_script.tapscript_leaf_hash(),
                sighash_type,
            )
            .expect("Failed to compute signature hash");
        let keypair = secp256k1::Keypair::from_seckey_slice(&self.secp, sk.as_ref())
            .expect("Failed to create keypair");
        let msg = secp256k1::Message::from(hash);
        let signature = self.secp.sign_schnorr(&msg, &keypair);
        let final_signature = taproot::Signature {
            signature,
            sighash_type,
        };
        let mut final_witness = Witness::new();
        final_witness.push(final_signature.to_vec());
        final_witness.push(reveal_script.to_bytes());
        final_witness.push(control_block.serialize());
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: bitcoin::OutPoint {
                    txid: commit_tx.compute_txid(),
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::from_consensus(ENABLE_RBF_NO_LOCKTIME),
                witness: final_witness,
            }],
            output: reveal_outputs.clone(),
        }
    }

    pub fn construct_mint_txes(
        &self,
        command: &str,
        utxos: &Vec<Utxo>,
        secret: &str,
    ) -> Result<MintTransactions, Box<dyn Error>> {
        let inscription_details = vec![InscriptionDetails {
            mime_type: b"text/plain".to_vec(),
            metadata: None,
            metaprotocol: None,
            content_encoding: None,
            delegate: None,
            file_data: command.as_bytes().to_vec(),
        }];
        let mut postage = f64::ceil(100.0 * self.to_spend_fee_rate + 1.0) as u64; // sats per inscription
        if postage < 330 {
            postage = 330; // minimum for p2tr output
        }
        let mut commit_tx = self.build_commit_tx(secret, &inscription_details, postage, &utxos)?;
        let sighash_type = EcdsaSighashType::All;
        let in_cnt = commit_tx.input.len();
        let mut utxos_to_spend: Vec<Utxo> = Vec::new();
        for input in commit_tx.input.iter() {
            let found_utxo = utxos
                .iter()
                .find(|u| {
                    u.txid == input.previous_output.txid && u.vout == input.previous_output.vout
                })
                .expect("UTXO for input not found");
            utxos_to_spend.push(found_utxo.clone());
        }
        let mut sighasher = SighashCache::new(&mut commit_tx);
        let mut commit_tx_in_value = 0u64;
        for input_idx in 0..in_cnt {
            let found_utxo = utxos_to_spend
                .get(input_idx)
                .expect("UTXO for input not found");
            commit_tx_in_value += found_utxo.value.to_sat();
            let sighash = sighasher
                .p2wpkh_signature_hash(
                    input_idx,
                    &found_utxo.address.script_pubkey(),
                    found_utxo.value,
                    sighash_type,
                )
                .expect("failed to create sighash");
            // Sign the sighash using the secp256k1 library (exported by rust-bitcoin).
            let msg = secp256k1::Message::from(sighash);
            let signature = self.secp.sign_ecdsa(&msg, &self.secret_key);
            // Update the witness stack.
            let signature = bitcoin::ecdsa::Signature {
                signature,
                sighash_type,
            };
            let public_key = self.private_key.public_key(&self.secp);
            *sighasher
                .witness_mut(input_idx)
                .expect("Failed to get witness") = Witness::p2wpkh(&signature, &public_key.inner);
        }
        let mut commit_tx_out_value = 0u64;
        for output in commit_tx.output.iter() {
            commit_tx_out_value += output.value.to_sat();
        }
        let total_fee = commit_tx_in_value - commit_tx_out_value + postage;
        let reveal_tx = self.build_reveal_tx(secret, &commit_tx, &inscription_details, postage);
        let send_to_op_return_inputs = vec![TxIn {
            previous_output: bitcoin::OutPoint {
                txid: reveal_tx.compute_txid(),
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::from_consensus(ENABLE_RBF_NO_LOCKTIME),
            witness: Witness::from_slice(&[
                vec![0u8; 72], // placeholder for signature
                self.compressed_public_key.0.serialize().to_vec(),
            ]),
        }];
        let send_to_op_return_outputs = vec![TxOut {
            value: Amount::from_sat(1),
            script_pubkey: ScriptBuf::new_op_return(b"BRC20PROG"),
        }];
        let mut send_to_op_return_tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: send_to_op_return_inputs,
            output: send_to_op_return_outputs,
        };
        let sighash_type = EcdsaSighashType::All;
        let mut sighasher = SighashCache::new(&mut send_to_op_return_tx);
        let sighash = sighasher
            .p2wpkh_signature_hash(
                0,
                &reveal_tx.output[0].script_pubkey,
                reveal_tx.output[0].value,
                sighash_type,
            )
            .expect("failed to create sighash");
        // Sign the sighash using the secp256k1 library (exported by rust-bitcoin).
        let msg = secp256k1::Message::from(sighash);
        let signature = self.secp.sign_ecdsa(&msg, &self.private_key.inner);
        // Update the witness stack.
        let signature = bitcoin::ecdsa::Signature {
            signature,
            sighash_type,
        };
        *sighasher.witness_mut(0).expect("Failed to get witness") =
            Witness::p2wpkh(&signature, &self.secp_public_key);
        Ok(MintTransactions {
            commit: commit_tx,
            reveal: reveal_tx,
            send_to_op_return: send_to_op_return_tx,
            total_fee: total_fee,
        })
    }

    pub async fn get_utxos(&self) -> Vec<Utxo> {
        // use mempool.space API to get UTXOs for the address
        let url = format!(
            "{}/api/address/{}/utxo",
            self.mempool_space_url, self.sender
        );
        let resp = reqwest::get(&url).await.expect("Failed to fetch UTXOs");
        if !resp.status().is_success() {
            panic!("Failed to fetch UTXOs: {}", resp.status());
        }
        let mempool_space_utxos: Vec<MempoolSpaceUtxo> =
            resp.json().await.expect("Failed to parse UTXOs");
        let mut utxos: Vec<Utxo> = Vec::new();
        for ms_utxo in mempool_space_utxos {
            let txid = bitcoin::Txid::from_str(&ms_utxo.txid).expect("Invalid txid");
            utxos.push(Utxo {
                txid,
                vout: ms_utxo.vout,
                value: Amount::from_sat(ms_utxo.value),
                address: self.sender.clone(),
                pubkey: Some(self.public_key.into()),
                tap_leaf_script: None,
                tap_leaf_control_block: None,
            });
        }
        utxos
    }

    pub fn test_mempool_accept(&self, txes: &Vec<&Transaction>) -> bool {
        // test txes
        let test_res = self.rpc_client.test_mempool_accept(txes);
        if test_res.is_ok() {
            let test_res = test_res.expect("Failed to get test mempool accept result");
            if test_res.iter().all(|r| r.allowed) {
                true
            } else {
                tracing::error!("One or more transactions were not accepted by mempool:");
                for res in test_res.iter().filter(|r| !r.allowed) {
                    tracing::debug!("{:#?}", res);
                }
                false
            }
        } else {
            tracing::error!("Error testing mempool accept: {:#?}", test_res.err());
            false
        }
    }

    pub fn send_raw_transaction(&self, tx: &Transaction) -> bitcoin::Txid {
        let txid: bitcoin::Txid = self
            .rpc_client
            .call(
                "sendrawtransaction",
                &[
                    encode::serialize_hex(tx).into(),
                    Value::String("0.1".to_string()), // Max fee rate
                    Value::String("0.1".to_string()), // Max burn amount (OP_RETURN needs to capture a sat)
                ],
            )
            .expect("Failed to send raw transaction");
        txid
    }

    fn get_dummy_utxos(&self) -> Vec<Utxo> {
        vec![Utxo {
            txid: bitcoin::Txid::from_str(
                "1decafcafe0badf00ddeadbeefba5eba11c0ffee1234567890abcdefedc0deaf",
            )
            .expect("Invalid txid"),
            vout: 0,
            value: Amount::from_sat(100_000),
            address: self.sender.clone(),
            pubkey: Some(self.public_key.into()),
            tap_leaf_script: None,
            tap_leaf_control_block: None,
        }]
    }

    pub async fn estimate(&self, command: &str) -> Result<u64, Box<dyn Error>> {
        let utxos = self.get_dummy_utxos();
        let secret = get_secret();

        let mint_txes = self.construct_mint_txes(&command, &utxos, &secret)?;

        Ok(mint_txes.total_fee)
    }

    pub async fn mint(&self, command: &str) -> Result<MintResult, Box<dyn Error>> {
        tracing::info!("Starting minting process...");
        let utxos = self.get_utxos().await;
        let secret = get_secret();

        let mint_txes = self.construct_mint_txes(&command, &utxos, &secret)?;

        // test commit
        let test_res = self.test_mempool_accept(&[&mint_txes.commit.clone()].to_vec());
        if !test_res {
            return Err("Commit tx is not allowed in mempool".into());
        }

        // test reveal
        let test_res = self
            .test_mempool_accept(&[&mint_txes.commit.clone(), &mint_txes.reveal.clone()].to_vec());
        if !test_res {
            return Err("Reveal tx is not allowed in mempool".into());
        }

        // test send to op_return
        let test_res = self.test_mempool_accept(
            &[
                &mint_txes.commit.clone(),
                &mint_txes.reveal.clone(),
                &mint_txes.send_to_op_return.clone(),
            ]
            .to_vec(),
        );
        if !test_res {
            return Err("Send to OP_RETURN tx is not allowed in mempool".into());
        }

        let commit_txid = self.send_raw_transaction(&mint_txes.commit);
        tracing::info!("Commit TX: {}/tx/{}", self.mempool_space_url, commit_txid);
        let reveal_txid = self.send_raw_transaction(&mint_txes.reveal);
        tracing::info!("Reveal TX: {}/tx/{}", self.mempool_space_url, reveal_txid);
        let send_to_op_return_txid = self.send_raw_transaction(&mint_txes.send_to_op_return);
        tracing::info!(
            "Send to OP_RETURN `BRC20PROG` TX: {}/tx/{}",
            self.mempool_space_url,
            send_to_op_return_txid
        );
        tracing::info!(
            "Minting process completed. Total fee: {} sats",
            mint_txes.total_fee
        );
        Ok(MintResult {
            commit: commit_txid,
            reveal: reveal_txid,
            send_to_op_return: send_to_op_return_txid,
            total_fee: mint_txes.total_fee,
        })
    }
}

pub fn get_secret() -> String {
    // get 32 bytes random hex string
    let mut rng = rand::thread_rng();
    let random_bytes: [u8; 32] = rng.r#gen();
    hex::encode(random_bytes)
}

pub fn construct_dummy_tx_from_in_outs(
    inputs: &Vec<Utxo>,
    outputs: &Vec<(ScriptBuf, Amount)>,
) -> Transaction {
    let txins: Vec<bitcoin::TxIn> = inputs
        .iter()
        .map(|u| {
            match u.address.address_type() {
                Some(bitcoin::AddressType::P2wpkh) => {
                    bitcoin::TxIn {
                        previous_output: bitcoin::OutPoint {
                            txid: u.txid,
                            vout: u.vout,
                        },
                        script_sig: ScriptBuf::new(),
                        sequence: Sequence::from_consensus(ENABLE_RBF_NO_LOCKTIME),
                        witness: Witness::from_slice(&[
                            vec![0u8; 72], // placeholder for signature
                            u.pubkey.expect("Missing pubkey").inner.serialize().to_vec(),
                        ]),
                    }
                }
                Some(bitcoin::AddressType::P2tr) => {
                    if u.tap_leaf_script.is_none() && u.tap_leaf_control_block.is_none() {
                        bitcoin::TxIn {
                            previous_output: bitcoin::OutPoint {
                                txid: u.txid,
                                vout: u.vout,
                            },
                            script_sig: ScriptBuf::new(),
                            sequence: Sequence::from_consensus(ENABLE_RBF_NO_LOCKTIME),
                            witness: Witness::from_slice(&[
                                vec![0u8; 65], // placeholder for signature
                            ]),
                        }
                    } else {
                        bitcoin::TxIn {
                            previous_output: bitcoin::OutPoint {
                                txid: u.txid,
                                vout: u.vout,
                            },
                            script_sig: ScriptBuf::new(),
                            sequence: Sequence::from_consensus(ENABLE_RBF_NO_LOCKTIME),
                            witness: Witness::from_slice(&[
                                vec![0u8; 65], // placeholder for signature
                                u.tap_leaf_script
                                    .as_ref()
                                    .expect("Missing tap leaf script")
                                    .to_bytes(),
                                u.tap_leaf_control_block
                                    .as_ref()
                                    .expect("Missing tap leaf control block")
                                    .serialize(),
                            ]),
                        }
                    }
                }
                default => {
                    panic!("Unsupported address type: {:?}", default);
                }
            }
        })
        .collect();
    let txouts: Vec<bitcoin::TxOut> = outputs
        .iter()
        .map(|(script, value)| bitcoin::TxOut {
            value: *value,
            script_pubkey: script.clone(),
        })
        .collect();
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: txins,
        output: txouts,
    }
}

trait ScriptBuilderWithNonMinimalIntPush {
    fn push_int_non_minimal(self, data: i64) -> Self;
}

impl ScriptBuilderWithNonMinimalIntPush for Builder {
    fn push_int_non_minimal(self, data: i64) -> Self {
        let mut buf = [0u8; 8];
        let len = write_scriptint(&mut buf, data);
        self.push_slice(&<&script::PushBytes>::from(&buf)[..len])
    }
}

pub fn build_reveal_script(
    xonly_pk: &XOnlyPublicKey,
    inscription_details: &Vec<InscriptionDetails>,
    postage: u64,
) -> ScriptBuf {
    let mut builder = ScriptBuf::builder();
    builder = builder
        .push_x_only_key(xonly_pk)
        .push_opcode(bitcoin::blockdata::opcodes::all::OP_CHECKSIG);
    let mut idx = 0;
    for detail in inscription_details {
        builder = builder
            .push_opcode(bitcoin::blockdata::opcodes::OP_0)
            .push_opcode(bitcoin::blockdata::opcodes::all::OP_IF)
            .push_slice(b"ord");
        if idx != 0 {
            builder = builder
                .push_int_non_minimal(2)
                .push_int(idx * postage as i64);
        }
        builder = builder
            .push_int_non_minimal(1)
            .push_slice::<&script::PushBytes>(
                detail
                    .mime_type
                    .as_slice()
                    .try_into()
                    .expect("MIME type too long"),
            );
        if let Some(metadata) = &detail.metadata {
            for chunk in metadata.chunks(520) {
                builder = builder
                    .push_int_non_minimal(5)
                    .push_slice::<&script::PushBytes>(
                        chunk.try_into().expect("Metadata chunk too long"),
                    );
            }
        }
        if let Some(metaprotocol) = &detail.metaprotocol {
            builder = builder
                .push_int_non_minimal(7)
                .push_slice::<&script::PushBytes>(
                    metaprotocol
                        .as_slice()
                        .try_into()
                        .expect("Metaprotocol too long"),
                );
        }
        if let Some(content_encoding) = &detail.content_encoding {
            builder = builder
                .push_int_non_minimal(9)
                .push_slice::<&script::PushBytes>(
                    content_encoding
                        .as_slice()
                        .try_into()
                        .expect("Content encoding too long"),
                );
        }
        if let Some(delegate) = &detail.delegate {
            builder = builder
                .push_int_non_minimal(11)
                .push_slice::<&script::PushBytes>(
                    delegate.as_slice().try_into().expect("Delegate too long"),
                );
        }
        builder = builder.push_opcode(bitcoin::blockdata::opcodes::OP_0);
        for chunk in detail.file_data.chunks(520) {
            builder = builder.push_slice::<&script::PushBytes>(
                chunk.try_into().expect("File data chunk too long"),
            );
        }
        builder = builder.push_opcode(bitcoin::blockdata::opcodes::all::OP_ENDIF);
        idx += 1;
    }
    builder.into_script()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_reveal_script() {
        let xonly_public_key = XOnlyPublicKey::from_str(
            "5b7b6f4d07932c4e9a8e66d830aa65b5fddef8c9db5e4a3aca99387970240992",
        )
        .expect("Invalid xonly public key");
        let reveal_script = build_reveal_script(
            &xonly_public_key,
            &vec![InscriptionDetails {
                mime_type: b"text/plain".to_vec(),
                metadata: Some(b"Example metadata".to_vec()),
                metaprotocol: None,
                content_encoding: None,
                delegate: None,
                file_data: b"Hello, Bitcoin!".to_vec(),
            }],
            1000,
        );
        let expected = "OP_PUSHBYTES_32 5b7b6f4d07932c4e9a8e66d830aa65b5fddef8c9db5e4a3aca99387970240992 OP_CHECKSIG OP_0 OP_IF OP_PUSHBYTES_3 6f7264 OP_PUSHBYTES_1 01 OP_PUSHBYTES_10 746578742f706c61696e OP_PUSHBYTES_1 05 OP_PUSHBYTES_16 4578616d706c65206d65746164617461 OP_0 OP_PUSHBYTES_15 48656c6c6f2c20426974636f696e21 OP_ENDIF";
        assert_eq!(reveal_script.to_string(), expected);
    }
}
