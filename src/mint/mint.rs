use std::{str::FromStr};

use bitcoin::{
    absolute::LockTime, consensus::encode, hashes::Hash, key::{rand::{self, Rng}, XOnlyPublicKey}, script::{self, write_scriptint, Builder}, secp256k1::{self, PublicKey, SecretKey}, sighash::{self, SighashCache}, taproot::{self, ControlBlock, LeafVersion}, transaction::Version, Address, Amount, CompressedPublicKey, EcdsaSighashType, PrivateKey, ScriptBuf, Sequence, TapSighashType, Transaction, TxIn, TxOut, Witness
};

use bitcoincore_rpc::{jsonrpc::serde_json::Value, Auth, Client, RpcApi};

const NETWORK: bitcoin::Network = bitcoin::Network::Signet;
const PRIVATE_KEY: &str = "****";
const TO_SPEND_FEE_RATE: f64 = 5.0; // sats per vbyte
const RPC_URL: &str = "http://****:**";
const RPC_USER: &str = "***";
const RPC_PASSWORD: &str = "***";

const ENABLE_RBF_NO_LOCKTIME: u32 = 0xFFFFFFFD;

const DUST_VALUE_P2PKH: u64 = 546;
const DUST_VALUE_P2WPKH: u64 = 294;
const DUST_VALUE_P2SH: u64 = 540;
const DUST_VALUE_P2TR: u64 = 330;

#[derive(Clone)]
pub struct InscriptionDetails {
    pub mime_type: Vec<u8>,
    pub metadata: Option<Vec<u8>>,
    pub metaprotocol: Option<Vec<u8>>,
    pub content_encoding: Option<Vec<u8>>,
    pub delegate: Option<Vec<u8>>,
    pub file_data: Vec<u8>,
}

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

pub struct MintResult {
    commit_tx: Transaction,
    reveal_tx: Transaction,
    send_to_op_return_tx: Transaction,
    total_fee: u64,
}

pub fn get_dust_value (address: &Address) -> u64 {
    match address.address_type() {
        Some(bitcoin::AddressType::P2pkh) => DUST_VALUE_P2PKH,
        Some(bitcoin::AddressType::P2wpkh) => DUST_VALUE_P2WPKH,
        Some(bitcoin::AddressType::P2sh) => DUST_VALUE_P2SH,
        Some(bitcoin::AddressType::P2tr) => DUST_VALUE_P2TR,
        _ => panic!("Unsupported address type"),
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
    pubkey: XOnlyPublicKey,
    inscription_details: &Vec<InscriptionDetails>,
    postage: u64,
) -> ScriptBuf {
    let mut builder = ScriptBuf::builder();
    builder = builder.push_x_only_key(&pubkey)
                     .push_opcode(bitcoin::blockdata::opcodes::all::OP_CHECKSIG);
    
    let mut idx = 0;
    for detail in inscription_details {
        builder = builder.push_opcode(bitcoin::blockdata::opcodes::OP_0)
                         .push_opcode(bitcoin::blockdata::opcodes::all::OP_IF)
                         .push_slice(b"ord");
        
        if idx != 0 {
            builder = builder.push_int_non_minimal(2)
                             .push_int(idx * postage as i64);
        }

        builder = builder.push_int_non_minimal(1)
                         .push_slice::<&script::PushBytes>(detail.mime_type.as_slice().try_into().unwrap());
        
        if let Some(metadata) = &detail.metadata {
            for chunk in metadata.chunks(520) {
                builder = builder.push_int_non_minimal(5)
                                 .push_slice::<&script::PushBytes>(chunk.try_into().unwrap());
            }
        }

        if let Some(metaprotocol) = &detail.metaprotocol {
            builder = builder.push_int_non_minimal(7)
                             .push_slice::<&script::PushBytes>(metaprotocol.as_slice().try_into().unwrap());
        }

        if let Some(content_encoding) = &detail.content_encoding {
            builder = builder.push_int_non_minimal(9)
                             .push_slice::<&script::PushBytes>(content_encoding.as_slice().try_into().unwrap());
        }

        if let Some(delegate) = &detail.delegate {
            builder = builder.push_int_non_minimal(11)
                             .push_slice::<&script::PushBytes>(delegate.as_slice().try_into().unwrap());
        }

        builder = builder.push_opcode(bitcoin::blockdata::opcodes::OP_0);
        for chunk in detail.file_data.chunks(520) {
            builder = builder.push_slice::<&script::PushBytes>(chunk.try_into().unwrap());
        }

        builder = builder.push_opcode(bitcoin::blockdata::opcodes::all::OP_ENDIF);
        idx += 1;
    }

    builder.into_script()
}

pub fn construct_dummy_tx_from_in_outs(
    inputs: &Vec<Utxo>,
    outputs: &Vec<(ScriptBuf, Amount)>,
) -> Transaction {
    let txins: Vec<bitcoin::TxIn> = inputs.iter().map(|u| {
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
                            u.pubkey.unwrap().inner.serialize().to_vec(),
                        ]),
                }
            },
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
                            u.tap_leaf_script.as_ref().unwrap().to_bytes(),
                            u.tap_leaf_control_block.as_ref().unwrap().serialize(),
                        ]),
                    }
                }
            },
            default => {
                panic!("Unsupported address type: {:?}", default);
            }
        }
    }).collect();

    let txouts: Vec<bitcoin::TxOut> = outputs.iter().map(|(script, value)| {
        bitcoin::TxOut {
            value: *value,
            script_pubkey: script.clone(),
        }
    }).collect();

    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: txins,
        output: txouts,
    }
}
pub fn estimate_fee(
    inputs: &Vec<Utxo>,
    outputs: &Vec<(ScriptBuf, Amount)>,
    fee_rate: f64, // sats per vbyte
) -> u64 {
    let tx = construct_dummy_tx_from_in_outs(inputs, outputs);
    let vsize = tx.weight().to_vbytes_ceil() as f64;
    (vsize * fee_rate).ceil() as u64
}
pub fn build_transaction(
    sender: Address, // assumes change wallet is the same
    utxos: &Vec<Utxo>,
    force_in_utxos: &Vec<Utxo>,
    outputs: &Vec<(ScriptBuf, Amount)>,
    fee_rate: f64, // sats per vbyte
) -> Transaction {
    let mut utxos = utxos.clone();

    // sort utxos by value ascending
    utxos.sort_by_key(|u| u.value);

    let mut inputs: Vec<Utxo> = Vec::new();
    let mut outputs = outputs.clone();
    for utxo in force_in_utxos.iter() {
        inputs.push(utxo.clone());

        // check utxos and if same utxo is there, remove it
        if let Some(pos) = utxos.iter().position(|x| x.txid == utxo.txid && x.vout == utxo.vout) {
            utxos.remove(pos);
        }
    }

    let mut total_input_value: u64 = inputs.iter().map(|u| u.value.to_sat()).sum();
    let total_target_value: u64 = outputs.iter().map(|(_, v)| v.to_sat()).sum();

    let mut fee = estimate_fee(&inputs, &outputs, fee_rate);
    while total_input_value < total_target_value + fee {
        println!("Total input value: {}, total target value + fee: {} + {} = {}, remaining utxo cnt: {}", total_input_value, total_target_value, fee, total_target_value + fee, utxos.len());
        if utxos.is_empty() {
            panic!("Insufficient funds");
        }

        let last_utxo = utxos.pop().unwrap();
        total_input_value += last_utxo.value.to_sat();
        inputs.push(last_utxo.clone());
        fee = estimate_fee(&inputs, &outputs, fee_rate);
    }

    let additional_change_output_fee = f64::ceil(((sender.script_pubkey().len() + 9) as f64) * fee_rate) as u64;
    let excess = total_input_value - fee - total_target_value;
    if excess > get_dust_value(&sender) + additional_change_output_fee {
        outputs.push((sender.script_pubkey(), Amount::from_sat(excess - additional_change_output_fee)));
    }

    construct_dummy_tx_from_in_outs(&inputs, &outputs)
}

pub fn build_commit_tx(
    sender: Address, // assumes change wallet is the same
    secret: String, // 32 byte hex string
    inscription_details: &Vec<InscriptionDetails>,
    fee_rate: f64, // sats per vbyte
    postage: u64,
    utxos: &Vec<Utxo>,
) -> Transaction {
    let secp = bitcoin::secp256k1::Secp256k1::new();

    let sk = SecretKey::from_str(&secret).expect("Invalid secret");
    let pk = PublicKey::from_secret_key(&secp, &sk);
    let xonly_pk = XOnlyPublicKey::from(pk);
    let reveal_script = build_reveal_script(xonly_pk, &inscription_details, postage);

    let taproot_builder = taproot::TaprootBuilder::new()
        .add_leaf(0, reveal_script.clone())
        .expect("Failed to add leaf to TaprootBuilder");
    let spend_info = taproot_builder.finalize(&secp, xonly_pk).expect("Failed to finalize TaprootBuilder");
    let taproot_output_key = spend_info.output_key();
    let taproot_address = Address::p2tr_tweaked(taproot_output_key, NETWORK);
    let control_block = spend_info.control_block(&(reveal_script.clone(), LeafVersion::TapScript)).expect("Failed to get control block");

    let mut dummy_reveal_outputs: Vec<TxOut> = Vec::new();
    for _ in inscription_details.iter() {
        dummy_reveal_outputs.push(TxOut { script_pubkey: sender.script_pubkey(), value: Amount::from_sat(postage) });
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
        witness: script_witness
    }];

    let dummy_reveal_tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: dummy_reveal_inputs,
        output: dummy_reveal_outputs,
    };

    let dummy_reveal_tx_fee = f64::ceil((dummy_reveal_tx.weight().to_vbytes_ceil() as f64) * fee_rate) as u64;
    let total_postage = postage * (inscription_details.len() as u64);
    let total_needed = total_postage + dummy_reveal_tx_fee;

    build_transaction(
        sender,
        utxos,
        &vec![],
        &vec![(taproot_address.script_pubkey(), Amount::from_sat(total_needed))],
        fee_rate,
    )
}

pub fn build_reveal_tx(
    sender: Address, // assumes change wallet is the same
    commit_tx: &Transaction,
    inscription_details: &Vec<InscriptionDetails>,
    secret: String, // 32 byte hex string
    postage: u64,
) -> Transaction {
    let secp = bitcoin::secp256k1::Secp256k1::new();

    let sk = SecretKey::from_str(&secret).expect("Invalid secret");
    let pk = PublicKey::from_secret_key(&secp, &sk);
    let xonly_pk = XOnlyPublicKey::from(pk);
    let reveal_script = build_reveal_script(xonly_pk, &inscription_details, postage);

    let taproot_builder = taproot::TaprootBuilder::new()
        .add_leaf(0, reveal_script.clone())
        .expect("Failed to add leaf to TaprootBuilder");
    let spend_info = taproot_builder.finalize(&secp, xonly_pk).expect("Failed to finalize TaprootBuilder");
    let control_block = spend_info.control_block(&(reveal_script.clone(), LeafVersion::TapScript)).expect("Failed to get control block");

    let mut script_witness = Witness::new();
    script_witness.push([0u8; 72].to_vec()); // placeholder for signature
    script_witness.push(reveal_script.to_bytes());
    script_witness.push(control_block.serialize());

    let mut reveal_outputs: Vec<TxOut> = Vec::new();
    for _ in inscription_details.iter() {
        reveal_outputs.push(TxOut { script_pubkey: sender.script_pubkey(), value: Amount::from_sat(postage) });
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
            witness: script_witness
        }],
        output: reveal_outputs.clone(),
    };

    let sighash_type = TapSighashType::All;
    let hash = SighashCache::new(&unsigned_tx).taproot_script_spend_signature_hash(
        0,
        &sighash::Prevouts::All(&[TxOut {
            value: commit_tx.output[0].value,
            script_pubkey: commit_tx.output[0].script_pubkey.clone(),
        }]),
        reveal_script.tapscript_leaf_hash(),
        sighash_type,
    ).expect("Failed to compute signature hash");

    let keypair = secp256k1::Keypair::from_seckey_slice(&secp, sk.as_ref()).unwrap();
    let msg = secp256k1::Message::from(hash);
    let signature = secp.sign_schnorr(&msg, &keypair);
    let final_signature = taproot::Signature { signature, sighash_type };

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
            witness: final_witness
        }],
        output: reveal_outputs.clone(),
    }
}

pub fn mint_command(
    command: &str,
    utxos: &Vec<Utxo>,
    secret: &String,
    fee_rate: f64, // sats per vbyte
) -> MintResult {
    let secp = bitcoin::secp256k1::Secp256k1::new();
    let sk = PrivateKey::from_slice(&hex::decode(PRIVATE_KEY).unwrap(), NETWORK).unwrap();
    let pk = CompressedPublicKey::from_private_key(&secp, &sk).unwrap();
    let sender_address = Address::p2wpkh(&pk, NETWORK);

    let inscription_details = vec![
        InscriptionDetails {
            mime_type: b"text/plain".to_vec(),
            metadata: None,
            metaprotocol: None,
            content_encoding: None,
            delegate: None,
            file_data: command.as_bytes().to_vec(),
        }
    ];

    let mut postage = f64::ceil(100.0 * fee_rate + 1.0) as u64; // sats per inscription
    if postage < 330 {
        postage = 330; // minimum for p2tr output
    }

    let mut commit_tx = build_commit_tx(
        sender_address.clone(),
        secret.clone(),
        &inscription_details,
        fee_rate,
        postage,
        &utxos,
    );
    
    let sighash_type = EcdsaSighashType::All;
    let in_cnt = commit_tx.input.len();
    let mut utxos_to_spend: Vec<Utxo> = Vec::new();
    for input in commit_tx.input.iter() {
        let found_utxo = utxos.iter().find(|u| {
            u.txid == input.previous_output.txid &&
            u.vout == input.previous_output.vout
        }).expect("UTXO for input not found");
        utxos_to_spend.push(found_utxo.clone());
    }
    let mut sighasher = SighashCache::new(&mut commit_tx);
    let mut commit_tx_in_value = 0u64;
    for input_idx in 0..in_cnt {
        let found_utxo = utxos_to_spend.get(input_idx).expect("UTXO for input not found");
        commit_tx_in_value += found_utxo.value.to_sat();

        let sighash = sighasher
            .p2wpkh_signature_hash(input_idx, &found_utxo.address.script_pubkey(), found_utxo.value, sighash_type)
            .expect("failed to create sighash");

        // Sign the sighash using the secp256k1 library (exported by rust-bitcoin).
        let msg = secp256k1::Message::from(sighash);
        let signature = secp.sign_ecdsa(&msg, &sk.inner);

        // Update the witness stack.
        let signature = bitcoin::ecdsa::Signature { signature, sighash_type };
        let pk = sk.public_key(&secp);
        *sighasher.witness_mut(input_idx).unwrap() = Witness::p2wpkh(&signature, &pk.inner);
    }
    let mut commit_tx_out_value = 0u64;
    for output in commit_tx.output.iter() {
        commit_tx_out_value += output.value.to_sat();
    }
    let total_fee = commit_tx_in_value - commit_tx_out_value + postage;
    println!("Signed Commit Transaction: {:#?}", encode::serialize_hex(&commit_tx));

    let reveal_tx = build_reveal_tx(
        sender_address.clone(),
        &commit_tx,
        &inscription_details,
        secret.clone(),
        postage,
    );
    println!("Signed Reveal Transaction: {:#?}", encode::serialize_hex(&reveal_tx));
    
    let send_to_op_return_inputs = vec![
        TxIn {
            previous_output: bitcoin::OutPoint {
                txid: reveal_tx.compute_txid(),
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::from_consensus(ENABLE_RBF_NO_LOCKTIME),
            witness: Witness::from_slice(&[
                            vec![0u8; 72], // placeholder for signature
                            pk.0.serialize().to_vec(),
                        ]),
        }
    ];
    let send_to_op_return_outputs = vec![
        TxOut {
            value: Amount::from_sat(1),
            script_pubkey: ScriptBuf::new_op_return(b"BRC20PROG"),
        }
    ];
    let mut send_to_op_return_tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: send_to_op_return_inputs,
        output: send_to_op_return_outputs,
    };

    let sighash_type = EcdsaSighashType::All;
    let mut sighasher = SighashCache::new(&mut send_to_op_return_tx);
    let sighash = sighasher
            .p2wpkh_signature_hash(0, &reveal_tx.output[0].script_pubkey, reveal_tx.output[0].value, sighash_type)
            .expect("failed to create sighash");
    // Sign the sighash using the secp256k1 library (exported by rust-bitcoin).
    let msg = secp256k1::Message::from(sighash);
    let signature = secp.sign_ecdsa(&msg, &sk.inner);
    // Update the witness stack.
    let signature = bitcoin::ecdsa::Signature { signature, sighash_type };
    let pk = sk.public_key(&secp);
    *sighasher.witness_mut(0).unwrap() = Witness::p2wpkh(&signature, &pk.inner);
    println!("Signed Send to OP_RETURN Transaction: {:#?}", encode::serialize_hex(&send_to_op_return_tx));

    MintResult {
        commit_tx: commit_tx,
        reveal_tx: reveal_tx,
        send_to_op_return_tx: send_to_op_return_tx,
        total_fee: total_fee,
    }
}

fn get_mempool_space_url() -> &'static str {
    match NETWORK {
        bitcoin::Network::Bitcoin => "https://mempool.space",
        bitcoin::Network::Testnet => "https://mempool.space/testnet",
        bitcoin::Network::Testnet4 => "https://mempool.space/testnet4",
        bitcoin::Network::Signet => "https://mempool.space/signet",
        default => panic!("Unsupported network: {:?}", default),
    }
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

pub fn get_utxos() -> Vec<Utxo> {
    let secp = bitcoin::secp256k1::Secp256k1::new();
    let sk = PrivateKey::from_slice(&hex::decode(PRIVATE_KEY).unwrap(), NETWORK).unwrap();
    let pk = CompressedPublicKey::from_private_key(&secp, &sk).unwrap();
    let sender_address = Address::p2wpkh(&pk, NETWORK);

    // use mempool.space API to get UTXOs for the address
    let url = format!("{}/api/address/{}/utxo", get_mempool_space_url(), sender_address);
    let resp = reqwest::blocking::get(&url).expect("Failed to fetch UTXOs");
    if !resp.status().is_success() {
        panic!("Failed to fetch UTXOs: {}", resp.status());
    }

    let mempool_space_utxos: Vec<MempoolSpaceUtxo> = resp.json().expect("Failed to parse UTXOs");
    let mut utxos: Vec<Utxo> = Vec::new();
    for ms_utxo in mempool_space_utxos {
        let txid = bitcoin::Txid::from_str(&ms_utxo.txid).expect("Invalid txid");
        utxos.push(Utxo {
            txid,
            vout: ms_utxo.vout,
            value: Amount::from_sat(ms_utxo.value),
            address: sender_address.clone(),
            pubkey: Some(pk.into()),
            tap_leaf_script: None,
            tap_leaf_control_block: None,
        });
    }
    utxos
}

pub fn get_secret() -> String {
    // get 32 bytes random hex string
    let mut rng = rand::thread_rng();
    let random_bytes: [u8; 32] = rng.r#gen();
    hex::encode(random_bytes)
}

pub fn test_mempool_accept(rpc: &Client, txes: &Vec<&Transaction>) -> bool {
    // test txes
    let test_res = rpc.test_mempool_accept(txes);
    if test_res.is_ok() {
        let test_res = test_res.unwrap();
        println!("Test mempool accept result: {:#?}", test_res);
        if test_res.iter().all(|r| r.allowed) {
            true
        } else {
            println!("One or more transactions were not accepted by mempool:");
            for res in test_res.iter().filter(|r| !r.allowed) {
                println!("{:#?}", res);
            }
            false
        }
    } else {
        println!("Error testing mempool accept: {:#?}", test_res.err());
        false
    }
}

pub fn send_raw_transaction(rpc: &Client, tx: &Transaction) -> bitcoin::Txid {
    let txid: bitcoin::Txid = rpc.call("sendrawtransaction", &[encode::serialize_hex(tx).into(), Value::String("0.1".to_string()), Value::String("0.1".to_string())]).unwrap();
    txid
}

pub fn start_minting() {
    println!("Minting process started.");

    let utxos = get_utxos();
    println!("Fetched {} UTXOs from mempool.space", utxos.len());

    let secret = get_secret();
    println!("Generated random hex: {}", secret);

    let fee_rate = TO_SPEND_FEE_RATE; // sats per vbyte
    let command = r#"{"p":"brc20-prog","op":"c","c":"0x70e44eDF27672250184E102159aE1F9842036C3A","b":"AVUkEHf/HgH0"}"#;
    let mint_result = mint_command(command, &utxos, &secret, fee_rate);

    let rpc = Client::new(
        RPC_URL,
        Auth::UserPass(RPC_USER.to_string(), RPC_PASSWORD.to_string()),
    ).unwrap();

    // test txes
    let test_res = test_mempool_accept(&rpc, &[&mint_result.commit_tx.clone(), &mint_result.reveal_tx.clone(), &mint_result.send_to_op_return_tx.clone()].to_vec());
    if test_res {
        let commit_txid = send_raw_transaction(&rpc, &mint_result.commit_tx);
        println!("Commit transaction sent: {}", commit_txid);
        let reveal_txid = send_raw_transaction(&rpc, &mint_result.reveal_tx);
        println!("Reveal transaction sent: {}", reveal_txid);
        let send_to_op_return_txid = send_raw_transaction(&rpc, &mint_result.send_to_op_return_tx);
        println!("Send to OP_RETURN transaction sent: {}", send_to_op_return_txid);
    } else {
        println!("Txs are not allowed in mempool");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_reveal_script() {
        let reveal_script = build_reveal_script(
            XOnlyPublicKey::from_str("5b7b6f4d07932c4e9a8e66d830aa65b5fddef8c9db5e4a3aca99387970240992").unwrap(),
            &vec![
                InscriptionDetails {
                    mime_type: b"text/plain".to_vec(),
                    metadata: Some(b"Example metadata".to_vec()),
                    metaprotocol: None,
                    content_encoding: None,
                    delegate: None,
                    file_data: b"Hello, Bitcoin!".to_vec(),
                }
            ],
            1000,
        );

        let expected = "OP_PUSHBYTES_32 5b7b6f4d07932c4e9a8e66d830aa65b5fddef8c9db5e4a3aca99387970240992 OP_CHECKSIG OP_0 OP_IF OP_PUSHBYTES_3 6f7264 OP_PUSHBYTES_1 01 OP_PUSHBYTES_10 746578742f706c61696e OP_PUSHBYTES_1 05 OP_PUSHBYTES_16 4578616d706c65206d65746164617461 OP_0 OP_PUSHBYTES_15 48656c6c6f2c20426974636f696e21 OP_ENDIF";
        assert_eq!(reveal_script.to_string(), expected);
    }
}