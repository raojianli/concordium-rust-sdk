//! Definition of transactions and other transaction-like messages, together
//! with their serialization, signing, and similar auxiliary methods.

use super::{
    hashes, smart_contracts, AccountInfo, AccountThreshold, AggregateSigPairing,
    BakerAggregationVerifyKey, BakerElectionVerifyKey, BakerKeyPairs, BakerSignatureVerifyKey,
    ContractAddress, CredentialIndex, CredentialRegistrationID, Energy, Memo, Nonce,
    RegisteredData, UpdateKeysIndex, UpdatePayload, UpdateSequenceNumber,
};
use crate::constants::*;
use crypto_common::{
    derive::{Serial, Serialize},
    deserial_map_no_length,
    types::{
        Amount, KeyIndex, KeyPair, Signature, Timestamp, TransactionSignature, TransactionTime,
    },
    Buffer, Deserial, Get, ParseResult, Put, ReadBytesExt, SerdeDeserialize, SerdeSerialize,
    Serial,
};
use derive_more::*;
use encrypted_transfers::types::{EncryptedAmountTransferData, SecToPubAmountTransferData};
use id::types::{
    AccountAddress, AccountCredentialMessage, AccountKeys, CredentialDeploymentInfo,
    CredentialPublicKeys,
};
use rand::Rng;
use random_oracle::RandomOracle;
use sha2::Digest;
use std::{collections::BTreeMap, marker::PhantomData};

#[derive(
    Debug, Copy, Clone, Serial, SerdeSerialize, SerdeDeserialize, Into, From, Display, Eq, PartialEq,
)]
#[serde(transparent)]
/// Type safe wrapper to record the size of the transaction payload.
pub struct PayloadSize {
    size: u32,
}

impl Deserial for PayloadSize {
    fn deserial<R: ReadBytesExt>(source: &mut R) -> ParseResult<Self> {
        let size: u32 = source.get()?;
        anyhow::ensure!(
            size <= MAX_PAYLOAD_SIZE,
            "Size of the payload exceeds maximum allowed."
        );
        Ok(PayloadSize { size })
    }
}

#[derive(Debug, Clone, Serialize, SerdeSerialize, SerdeDeserialize)]
#[serde(rename_all = "camelCase")]
/// Header of an account transaction that contains basic data to check whether
/// the sender and the transaction is valid.
pub struct TransactionHeader {
    /// Sender account of the transaction.
    pub sender:        AccountAddress,
    /// Sequence number of the transaction.
    pub nonce:         Nonce,
    /// Maximum amount of energy the transaction can take to execute.
    pub energy_amount: Energy,
    /// Size of the transaction payload. This is used to deserialize the
    /// payload.
    pub payload_size:  PayloadSize,
    /// Latest time the transaction can be included in a block.
    pub expiry:        TransactionTime,
}

#[derive(Debug, Clone, SerdeSerialize, SerdeDeserialize)]
#[serde(transparent)]
/// An account transaction payload that has not yet been deserialized.
/// This is a simple wrapper around Vec<u8> with bespoke serialization.
pub struct EncodedPayload {
    #[serde(with = "crate::internal::byte_array_hex")]
    pub(crate) payload: Vec<u8>,
}

impl EncodedPayload {
    pub fn decode(&self) -> ParseResult<Payload> {
        let mut source = std::io::Cursor::new(&self.payload);
        let payload = source.get()?;
        // ensure payload length matches the stated size.
        let consumed = source.position();
        anyhow::ensure!(
            consumed == self.payload.len() as u64,
            "Payload length information is inaccurate: {} bytes of input remaining.",
            self.payload.len() as u64 - consumed
        );
        Ok(payload)
    }
}

/// This serial instance does not have an inverse. It needs a context with the
/// length.
impl Serial for EncodedPayload {
    fn serial<B: Buffer>(&self, out: &mut B) {
        out.write_all(&self.payload)
            .expect("Writing to buffer should succeed.");
    }
}

/// Parse an encoded payload of specified length.
pub fn get_encoded_payload<R: ReadBytesExt>(
    source: &mut R,
    len: PayloadSize,
) -> ParseResult<EncodedPayload> {
    // The use of deserial_bytes is safe here (no execessive allocations) because
    // payload_size is limited
    let payload = crypto_common::deserial_bytes(source, u32::from(len) as usize)?;
    Ok(EncodedPayload { payload })
}

/// A helper trait so that we can treat payload and encoded payload in the same
/// place.
pub trait PayloadLike {
    /// Encode the transaction payload by serializing.
    fn encode(&self) -> EncodedPayload;
    /// Encode the payload directly to a buffer. This will in general be more
    /// efficient than `encode`. However this will only matter if serialization
    /// was to be done in a tight loop.
    fn encode_to_buffer<B: Buffer>(&self, out: &mut B);
}

impl PayloadLike for EncodedPayload {
    fn encode(&self) -> EncodedPayload { self.clone() }

    fn encode_to_buffer<B: Buffer>(&self, out: &mut B) {
        out.write_all(&self.payload)
            .expect("Writing to buffer is always safe.");
    }
}

#[derive(Debug, Clone, SerdeDeserialize, SerdeSerialize)]
#[serde(rename_all = "camelCase")]
/// An account transaction signed and paid for by a sender account.
/// The payload type is a generic parameter to support two kinds of payloads,
/// a fully deserialized [Payload] type, and an [EncodedPayload]. The latter is
/// useful since deserialization of some types of payloads is expensive. It is
/// thus useful to delay deserialization until after we have checked signatures
/// and the sender account information.
pub struct AccountTransaction<PayloadType> {
    pub signature: TransactionSignature,
    pub header:    TransactionHeader,
    pub payload:   PayloadType,
}

impl<P: PayloadLike> Serial for AccountTransaction<P> {
    fn serial<B: Buffer>(&self, out: &mut B) {
        out.put(&self.signature);
        out.put(&self.header);
        self.payload.encode_to_buffer(out)
    }
}

impl Deserial for AccountTransaction<EncodedPayload> {
    fn deserial<R: ReadBytesExt>(source: &mut R) -> ParseResult<Self> {
        let signature = source.get()?;
        let header: TransactionHeader = source.get()?;
        let payload = get_encoded_payload(source, header.payload_size)?;
        Ok(AccountTransaction {
            signature,
            header,
            payload,
        })
    }
}

impl Deserial for AccountTransaction<Payload> {
    fn deserial<R: ReadBytesExt>(source: &mut R) -> ParseResult<Self> {
        let signature = source.get()?;
        let header: TransactionHeader = source.get()?;
        let payload_len = u64::from(u32::from(header.payload_size));
        let mut limited = <&mut R as std::io::Read>::take(source, payload_len);
        let payload = limited.get()?;
        // ensure payload length matches the stated size.
        anyhow::ensure!(
            limited.limit() == 0,
            "Payload length information is inaccurate: {} bytes of input remaining.",
            limited.limit()
        );
        Ok(AccountTransaction {
            signature,
            header,
            payload,
        })
    }
}

impl<P: PayloadLike> AccountTransaction<P> {
    /// Verify signature on the transaction given the public keys.
    pub fn verify_transaction_signature(&self, keys: &impl HasAccountAccessStructure) -> bool {
        let hash = compute_transaction_sign_hash(&self.header, &self.payload);
        verify_signature_transaction_sign_hash(keys, &hash, &self.signature)
    }
}

/// Marker for `BakerKeysPayload` indicating the proofs contained in
/// `BakerKeysPayload` have been generated for an `AddBaker` transaction.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum AddBakerKeysMarker {}

/// Marker for `BakerKeysPayload` indicating the proofs contained in
/// `BakerKeysPayload` have been generated for an `UpdateBakerKeys` transaction.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum UpdateBakerKeysMarker {}

#[derive(Debug, Clone, SerdeDeserialize, SerdeSerialize)]
#[serde(rename_all = "camelCase")]
/// Auxiliary type that contains public keys and proof of ownership of those
/// keys. This is used in the `AddBaker` and `UpdateBakerKeys` transaction
/// types.
/// The proofs are either constructed for `AddBaker` or `UpdateBakerKeys` and
/// the generic `V` is used as a marker to distinguish this in the type. See the
/// markers: `AddBakerKeysMarker` and `UpdateBakerKeysMarker`.
pub struct BakerKeysPayload<V> {
    #[serde(skip)] // use default when deserializing
    phantom:                    PhantomData<V>,
    /// New public key for participating in the election lottery.
    pub election_verify_key:    BakerElectionVerifyKey,
    /// New public key for verifying this baker's signatures.
    pub signature_verify_key:   BakerSignatureVerifyKey,
    /// New public key for verifying this baker's signature on finalization
    /// records.
    pub aggregation_verify_key: BakerAggregationVerifyKey,
    /// Proof of knowledge of the secret key corresponding to the signature
    /// verification key.
    pub proof_sig:              eddsa_ed25519::Ed25519DlogProof,
    /// Proof of knowledge of the election secret key.
    pub proof_election:         eddsa_ed25519::Ed25519DlogProof,
    /// Proof of knowledge of the secret key for signing finalization
    /// records.
    pub proof_aggregation:      aggregate_sig::Proof<AggregateSigPairing>,
}

/// Baker keys payload containing proofs construct for a `AddBaker` transaction.
pub type BakerAddKeysPayload = BakerKeysPayload<AddBakerKeysMarker>;
/// Baker keys payload containing proofs construct for a `UpdateBakerKeys`
/// transaction.
pub type BakerUpdateKeysPayload = BakerKeysPayload<UpdateBakerKeysMarker>;

impl<T> BakerKeysPayload<T> {
    /// Construct a BakerKeysPayload taking a prefix for the challenge.
    fn new_payload<R: Rng>(
        baker_keys: &BakerKeyPairs,
        sender: AccountAddress,
        challenge_prefix: &[u8],
        csprng: &mut R,
    ) -> Self {
        let mut challenge = challenge_prefix.to_vec();

        sender.serial(&mut challenge);
        baker_keys.election_verify.serial(&mut challenge);
        baker_keys.signature_verify.serial(&mut challenge);
        baker_keys.aggregation_verify.serial(&mut challenge);

        let proof_election = eddsa_ed25519::prove_dlog_ed25519(
            &mut RandomOracle::domain(&challenge),
            &baker_keys.election_verify.verify_key,
            &baker_keys.election_sign.sign_key,
        );
        let proof_sig = eddsa_ed25519::prove_dlog_ed25519(
            &mut RandomOracle::domain(&challenge),
            &baker_keys.signature_verify.verify_key,
            &baker_keys.signature_sign.sign_key,
        );
        let proof_aggregation = baker_keys
            .aggregation_sign
            .prove(csprng, &mut RandomOracle::domain(&challenge));

        BakerKeysPayload {
            phantom: PhantomData::default(),
            election_verify_key: baker_keys.election_verify.clone(),
            signature_verify_key: baker_keys.signature_verify.clone(),
            aggregation_verify_key: baker_keys.aggregation_verify.clone(),
            proof_sig,
            proof_election,
            proof_aggregation,
        }
    }
}

impl BakerAddKeysPayload {
    /// Construct a BakerKeysPayload with proofs for adding a baker.
    pub fn new<T: Rng>(baker_keys: &BakerKeyPairs, sender: AccountAddress, csprng: &mut T) -> Self {
        BakerKeysPayload::new_payload(baker_keys, sender, b"addBaker", csprng)
    }
}

impl BakerUpdateKeysPayload {
    /// Construct a BakerKeysPayload with proofs for updating baker keys.
    pub fn new<T: Rng>(baker_keys: &BakerKeyPairs, sender: AccountAddress, csprng: &mut T) -> Self {
        BakerKeysPayload::new_payload(baker_keys, sender, b"updateBakerKeys", csprng)
    }
}

#[derive(Debug, Clone, SerdeDeserialize, SerdeSerialize)]
#[serde(rename_all = "camelCase")]
/// Payload of the `AddBaker` transaction. This transaction registers the
/// account as a baker.
pub struct AddBakerPayload {
    /// The keys with which the baker registered.
    #[serde(flatten)]
    pub keys:             BakerAddKeysPayload,
    /// Initial baking stake.
    pub baking_stake:     Amount,
    /// Whether to add earnings to the stake automatically or not.
    pub restake_earnings: bool,
}

#[derive(Debug, Clone, SerdeDeserialize, SerdeSerialize)]
#[serde(rename_all = "camelCase")]
/// Data needed to initialize a smart contract.
pub struct InitContractPayload {
    /// Deposit this amount of CCD.
    pub amount:    Amount,
    /// Reference to the module from which to initialize the instance.
    pub mod_ref:   smart_contracts::ModuleRef,
    /// Name of the contract in the module.
    pub init_name: smart_contracts::InitName,
    /// Message to invoke the initialization method with.
    pub param:     smart_contracts::Parameter,
}

#[derive(Debug, Clone, SerdeDeserialize, SerdeSerialize)]
#[serde(rename_all = "camelCase")]
/// Data needed to update a smart contract instance.
pub struct UpdateContractPayload {
    /// Send the given amount of CCD together with the message to the
    /// contract instance.
    pub amount:       Amount,
    /// Address of the contract instance to invoke.
    pub address:      ContractAddress,
    /// Name of the method to invoke on the contract.
    pub receive_name: smart_contracts::ReceiveName,
    /// Message to send to the contract instance.
    pub message:      smart_contracts::Parameter,
}

#[derive(Debug, Clone, SerdeDeserialize, SerdeSerialize)]
#[serde(rename_all = "camelCase")]
/// Payload of an account transaction.
pub enum Payload {
    /// Deploy a Wasm module with the given source.
    DeployModule {
        #[serde(rename = "mod")]
        module: smart_contracts::WasmModule,
    },
    /// Initialize a new smart contract instance.
    InitContract {
        #[serde(flatten)]
        payload: InitContractPayload,
    },
    /// Update a smart contract instance by invoking a specific function.
    Update {
        #[serde(flatten)]
        payload: UpdateContractPayload,
    },
    /// Transfer CCD to an account.
    Transfer {
        /// Address to send to.
        to_address: AccountAddress,
        /// Amount to send.
        amount:     Amount,
    },
    /// Register the sender account as a baker.
    AddBaker {
        #[serde(flatten)]
        payload: Box<AddBakerPayload>,
    },
    /// Deregister the account as a baker.
    RemoveBaker,
    /// Update baker's stake.
    UpdateBakerStake {
        /// The new stake.
        stake: Amount,
    },
    /// Modify whether to add earnings to the baker stake automatically or not.
    UpdateBakerRestakeEarnings {
        /// New value of the flag.
        restake_earnings: bool,
    },
    /// Update the baker's keys.
    UpdateBakerKeys {
        #[serde(flatten)]
        payload: Box<BakerUpdateKeysPayload>,
    },
    /// Update signing keys of a specific credential.
    UpdateCredentialKeys {
        /// Id of the credential whose keys are to be updated.
        cred_id: CredentialRegistrationID,
        /// The new public keys.
        keys:    CredentialPublicKeys,
    },
    /// Transfer an encrypted amount.
    EncryptedAmountTransfer {
        /// The recepient's address.
        to:   AccountAddress,
        /// The (encrypted) amount to transfer and proof of correctness of
        /// accounting.
        data: Box<EncryptedAmountTransferData<EncryptedAmountsCurve>>,
    },
    /// Transfer from public to encrypted balance of the sender account.
    TransferToEncrypted {
        /// The amount to transfer.
        amount: Amount,
    },
    /// Transfer an amount from encrypted to the public balance of the account.
    TransferToPublic {
        /// The amount to transfer and proof of correctness of accounting.
        #[serde(flatten)]
        data: Box<SecToPubAmountTransferData<EncryptedAmountsCurve>>,
    },
    /// Transfer an amount with schedule.
    TransferWithSchedule {
        /// The recepient.
        to:       AccountAddress,
        /// The release schedule. This can be at most 255 elements.
        schedule: Vec<(Timestamp, Amount)>,
    },
    /// Update the account's credentials.
    UpdateCredentials {
        /// New credentials to add.
        new_cred_infos: BTreeMap<
            CredentialIndex,
            CredentialDeploymentInfo<
                id::constants::IpPairing,
                id::constants::ArCurve,
                id::constants::AttributeKind,
            >,
        >,
        /// Ids of credentials to remove.
        remove_cred_ids: Vec<CredentialRegistrationID>,
        /// The new account threshold.
        new_threshold:   AccountThreshold,
    },
    /// Register the given data on the chain.
    RegisterData {
        /// The data to register.
        data: RegisteredData,
    },
    /// Transfer CCD to an account with an additional memo.
    TransferWithMemo {
        /// Address to send to.
        to_address: AccountAddress,
        /// Memo to include in the transfer.
        memo:       Memo,
        /// Amount to send.
        amount:     Amount,
    },
    /// Transfer an encrypted amount.
    EncryptedAmountTransferWithMemo {
        /// The recepient's address.
        to:   AccountAddress,
        /// Memo to include in the transfer.
        memo: Memo,
        /// The (encrypted) amount to transfer and proof of correctness of
        /// accounting.
        data: Box<EncryptedAmountTransferData<EncryptedAmountsCurve>>,
    },
    /// Transfer an amount with schedule.
    TransferWithScheduleAndMemo {
        /// The recepient.
        to:       AccountAddress,
        /// Memo to include in the transfer.
        memo:     Memo,
        /// The release schedule. This can be at most 255 elements.
        schedule: Vec<(Timestamp, Amount)>,
    },
}

impl Serial for Payload {
    fn serial<B: Buffer>(&self, out: &mut B) {
        match &self {
            Payload::DeployModule { module } => {
                out.put(&0u8);
                out.put(module);
            }
            Payload::InitContract { payload } => {
                out.put(&1u8);
                out.put(payload)
            }
            Payload::Update { payload } => {
                out.put(&2u8);
                out.put(payload)
            }
            Payload::Transfer { to_address, amount } => {
                out.put(&3u8);
                out.put(to_address);
                out.put(amount);
            }
            Payload::AddBaker { payload } => {
                out.put(&4u8);
                out.put(payload);
            }
            Payload::RemoveBaker => {
                out.put(&5u8);
            }
            Payload::UpdateBakerStake { stake } => {
                out.put(&6u8);
                out.put(stake);
            }
            Payload::UpdateBakerRestakeEarnings { restake_earnings } => {
                out.put(&7u8);
                out.put(restake_earnings);
            }
            Payload::UpdateBakerKeys { payload } => {
                out.put(&8u8);
                out.put(payload)
            }
            Payload::UpdateCredentialKeys { cred_id, keys } => {
                out.put(&13u8);
                out.put(cred_id);
                out.put(keys);
            }
            Payload::EncryptedAmountTransfer { to, data } => {
                out.put(&16u8);
                out.put(to);
                out.put(data);
            }
            Payload::TransferToEncrypted { amount } => {
                out.put(&17u8);
                out.put(amount);
            }
            Payload::TransferToPublic { data } => {
                out.put(&18u8);
                out.put(data);
            }
            Payload::TransferWithSchedule { to, schedule } => {
                out.put(&19u8);
                out.put(to);
                out.put(&(schedule.len() as u8));
                crypto_common::serial_vector_no_length(schedule, out);
            }
            Payload::UpdateCredentials {
                new_cred_infos,
                remove_cred_ids,
                new_threshold,
            } => {
                out.put(&20u8);
                out.put(&(new_cred_infos.len() as u8));
                crypto_common::serial_map_no_length(new_cred_infos, out);
                out.put(&(remove_cred_ids.len() as u8));
                crypto_common::serial_vector_no_length(remove_cred_ids, out);
                out.put(new_threshold);
            }
            Payload::RegisterData { data } => {
                out.put(&21u8);
                out.put(data);
            }
            Payload::TransferWithMemo {
                to_address,
                memo,
                amount,
            } => {
                out.put(&22u8);
                out.put(to_address);
                out.put(memo);
                out.put(amount);
            }
            Payload::EncryptedAmountTransferWithMemo { to, memo, data } => {
                out.put(&23u8);
                out.put(to);
                out.put(memo);
                out.put(data);
            }
            Payload::TransferWithScheduleAndMemo { to, memo, schedule } => {
                out.put(&24u8);
                out.put(to);
                out.put(memo);
                out.put(&(schedule.len() as u8));
                crypto_common::serial_vector_no_length(schedule, out);
            }
        }
    }
}

impl Deserial for Payload {
    fn deserial<R: ReadBytesExt>(source: &mut R) -> ParseResult<Self> {
        let tag: u8 = source.get()?;
        match tag {
            0 => {
                let module = source.get()?;
                Ok(Payload::DeployModule { module })
            }
            1 => {
                let payload = source.get()?;
                Ok(Payload::InitContract { payload })
            }
            2 => {
                let payload = source.get()?;
                Ok(Payload::Update { payload })
            }
            3 => {
                let to_address = source.get()?;
                let amount = source.get()?;
                Ok(Payload::Transfer { to_address, amount })
            }
            4 => {
                let payload_data = source.get()?;
                Ok(Payload::AddBaker {
                    payload: Box::new(payload_data),
                })
            }
            5 => Ok(Payload::RemoveBaker),
            6 => {
                let stake = source.get()?;
                Ok(Payload::UpdateBakerStake { stake })
            }
            7 => {
                let restake_earnings = source.get()?;
                Ok(Payload::UpdateBakerRestakeEarnings { restake_earnings })
            }
            8 => {
                let payload_data = source.get()?;
                Ok(Payload::UpdateBakerKeys {
                    payload: Box::new(payload_data),
                })
            }
            13 => {
                let cred_id = source.get()?;
                let keys = source.get()?;
                Ok(Payload::UpdateCredentialKeys { cred_id, keys })
            }
            16 => {
                let to = source.get()?;
                let data = source.get()?;
                Ok(Payload::EncryptedAmountTransfer { to, data })
            }
            17 => {
                let amount = source.get()?;
                Ok(Payload::TransferToEncrypted { amount })
            }
            18 => {
                let data_data = source.get()?;
                Ok(Payload::TransferToPublic {
                    data: Box::new(data_data),
                })
            }
            19 => {
                let to = source.get()?;
                let len: u8 = source.get()?;
                let schedule = crypto_common::deserial_vector_no_length(source, len.into())?;
                Ok(Payload::TransferWithSchedule { to, schedule })
            }
            20 => {
                let cred_infos_len: u8 = source.get()?;
                let new_cred_infos =
                    crypto_common::deserial_map_no_length(source, cred_infos_len.into())?;
                let remove_cred_ids_len: u8 = source.get()?;
                let remove_cred_ids =
                    crypto_common::deserial_vector_no_length(source, remove_cred_ids_len.into())?;
                let new_threshold = source.get()?;
                Ok(Payload::UpdateCredentials {
                    new_cred_infos,
                    remove_cred_ids,
                    new_threshold,
                })
            }
            21 => {
                let data = source.get()?;
                Ok(Payload::RegisterData { data })
            }
            22 => {
                let to_address = source.get()?;
                let memo = source.get()?;
                let amount = source.get()?;
                Ok(Payload::TransferWithMemo {
                    to_address,
                    memo,
                    amount,
                })
            }
            23 => {
                let to = source.get()?;
                let memo = source.get()?;
                let data = source.get()?;
                Ok(Payload::EncryptedAmountTransferWithMemo { to, memo, data })
            }
            24 => {
                let to = source.get()?;
                let memo = source.get()?;
                let len: u8 = source.get()?;
                let schedule = crypto_common::deserial_vector_no_length(source, len.into())?;
                Ok(Payload::TransferWithScheduleAndMemo { to, memo, schedule })
            }
            _ => {
                anyhow::bail!("Unsupported transaction payload tag {}", tag)
            }
        }
    }
}

impl PayloadLike for Payload {
    fn encode(&self) -> EncodedPayload {
        let payload = crypto_common::to_bytes(&self);
        EncodedPayload { payload }
    }

    fn encode_to_buffer<B: Buffer>(&self, out: &mut B) { out.put(&self) }
}

impl EncodedPayload {
    pub fn size(&self) -> PayloadSize {
        let size = self.payload.len() as u32;
        PayloadSize { size }
    }
}

/// Compute the transaction sign hash from an encoded payload and header.
pub fn compute_transaction_sign_hash(
    header: &TransactionHeader,
    payload: &impl PayloadLike,
) -> hashes::TransactionSignHash {
    let mut hasher = sha2::Sha256::new();
    hasher.put(header);
    payload.encode_to_buffer(&mut hasher);
    hashes::HashBytes::new(hasher.result())
}

/// Abstraction of private keys.
pub trait TransactionSigner {
    /// Sign the specified transaction hash, allocating and returning the
    /// signatures.
    fn sign_transaction_hash(
        &self,
        hash_to_sign: &hashes::TransactionSignHash,
    ) -> TransactionSignature;
}

/// A signing implementation that knows the number of keys up-front.
pub trait ExactSizeTransactionSigner: TransactionSigner {
    /// Return the number of keys that the signer will sign with.
    /// This must match what [TransactionSigner::sign_transaction_hash] returns.
    fn num_keys(&self) -> u32;
}

/// This signs with the first `threshold` credentials and for each
/// credential with the first threshold keys for that credential.
impl TransactionSigner for AccountKeys {
    fn sign_transaction_hash(
        &self,
        hash_to_sign: &hashes::TransactionSignHash,
    ) -> TransactionSignature {
        let iter = self
            .keys
            .iter()
            .take(usize::from(u8::from(self.threshold)))
            .map(|(k, v)| {
                (k, {
                    let num = u8::from(v.threshold);
                    v.keys.iter().take(num.into())
                })
            });
        let mut signatures = BTreeMap::<CredentialIndex, BTreeMap<KeyIndex, _>>::new();
        for (ci, cred_keys) in iter {
            let cred_sigs = cred_keys
                .into_iter()
                .map(|(ki, kp)| (*ki, kp.sign(hash_to_sign.as_ref())))
                .collect::<BTreeMap<_, _>>();
            signatures.insert(*ci, cred_sigs);
        }
        TransactionSignature { signatures }
    }
}

impl ExactSizeTransactionSigner for AccountKeys {
    fn num_keys(&self) -> u32 {
        self.keys
            .values()
            .take(usize::from(u8::from(self.threshold)))
            .map(|v| u32::from(u8::from(v.threshold)))
            .sum::<u32>()
    }
}

impl TransactionSigner for BTreeMap<CredentialIndex, BTreeMap<KeyIndex, KeyPair>> {
    fn sign_transaction_hash(
        &self,
        hash_to_sign: &hashes::TransactionSignHash,
    ) -> TransactionSignature {
        let mut signatures = BTreeMap::<CredentialIndex, BTreeMap<KeyIndex, _>>::new();
        for (ci, cred_keys) in self {
            let cred_sigs = cred_keys
                .iter()
                .map(|(ki, kp)| (*ki, kp.sign(hash_to_sign.as_ref())))
                .collect::<BTreeMap<_, _>>();
            signatures.insert(*ci, cred_sigs);
        }
        TransactionSignature { signatures }
    }
}

impl ExactSizeTransactionSigner for BTreeMap<CredentialIndex, BTreeMap<KeyIndex, KeyPair>> {
    fn num_keys(&self) -> u32 { self.values().map(|v| v.len() as u32).sum::<u32>() }
}

/// Sign the header and payload, construct the transaction, and return it.
pub fn sign_transaction<S: TransactionSigner, P: PayloadLike>(
    signer: &S,
    header: TransactionHeader,
    payload: P,
) -> AccountTransaction<P> {
    let hash_to_sign = compute_transaction_sign_hash(&header, &payload);
    let signature = signer.sign_transaction_hash(&hash_to_sign);
    AccountTransaction {
        signature,
        header,
        payload,
    }
}

/// Implementations of this trait are structures which can produce public keys
/// with which transaction signatures can be verified.
pub trait HasAccountAccessStructure {
    fn threshold(&self) -> AccountThreshold;
    fn credential_keys(&self, idx: CredentialIndex) -> Option<&CredentialPublicKeys>;
}

#[derive(Debug, Clone)]
/// The most straighforward account access structure is a map of public keys
/// with the account threshold.
pub struct AccountAccessStructure {
    /// The number of credentials that needed to sign a transaction.
    pub threshold: AccountThreshold,
    /// Keys indexed by credential.
    pub keys:      BTreeMap<CredentialIndex, CredentialPublicKeys>,
}

impl HasAccountAccessStructure for AccountAccessStructure {
    fn threshold(&self) -> AccountThreshold { self.threshold }

    fn credential_keys(&self, idx: CredentialIndex) -> Option<&CredentialPublicKeys> {
        self.keys.get(&idx)
    }
}

impl HasAccountAccessStructure for AccountInfo {
    fn threshold(&self) -> AccountThreshold { self.account_threshold }

    fn credential_keys(&self, idx: CredentialIndex) -> Option<&CredentialPublicKeys> {
        let versioned_cred = self.account_credentials.get(&idx)?;
        match versioned_cred.value {
            id::types::AccountCredentialWithoutProofs::Initial { ref icdv } => {
                Some(&icdv.cred_account)
            }
            id::types::AccountCredentialWithoutProofs::Normal { ref cdv, .. } => {
                Some(&cdv.cred_key_info)
            }
        }
    }
}

/// Verify a signature on the transaction sign hash. This is a low-level
/// operation that is useful to avoid recomputing the transaction hash.
pub fn verify_signature_transaction_sign_hash(
    keys: &impl HasAccountAccessStructure,
    hash: &hashes::TransactionSignHash,
    signature: &TransactionSignature,
) -> bool {
    if usize::from(u8::from(keys.threshold())) > signature.signatures.len() {
        return false;
    }
    // There are enough signatures.
    for (&ci, cred_sigs) in signature.signatures.iter() {
        if let Some(cred_keys) = keys.credential_keys(ci) {
            if usize::from(u8::from(cred_keys.threshold)) > cred_sigs.len() {
                return false;
            }
            for (&ki, sig) in cred_sigs {
                if let Some(pk) = cred_keys.get(ki) {
                    if !pk.verify(hash, &sig) {
                        return false;
                    }
                } else {
                    return false;
                }
            }
        } else {
            return false;
        }
    }
    true
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct UpdateHeader {
    pub seq_number:     UpdateSequenceNumber,
    pub effective_time: TransactionTime,
    pub timeout:        TransactionTime,
    pub payload_size:   PayloadSize,
}

#[derive(Debug, Clone, Serial, Into)]
pub struct UpdateInstructionSignature {
    #[map_size_length = 2]
    signatures: BTreeMap<UpdateKeysIndex, Signature>,
}

impl Deserial for UpdateInstructionSignature {
    fn deserial<R: ReadBytesExt>(source: &mut R) -> ParseResult<Self> {
        let len = u16::deserial(source)?;
        anyhow::ensure!(len != 0, "There must be at least one signature.");
        let signatures = deserial_map_no_length(source, len as usize)?;
        Ok(Self { signatures })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInstruction {
    pub header:     UpdateHeader,
    pub payload:    UpdatePayload,
    pub signatures: UpdateInstructionSignature,
}

pub trait UpdateSigner {
    /// Sign the specified transaction hash, allocating and returning the
    /// signatures.
    fn sign_update_hash(&self, hash_to_sign: &hashes::UpdateSignHash)
        -> UpdateInstructionSignature;
}

impl UpdateSigner for BTreeMap<UpdateKeysIndex, KeyPair> {
    fn sign_update_hash(
        &self,
        hash_to_sign: &hashes::UpdateSignHash,
    ) -> UpdateInstructionSignature {
        let signatures = self
            .iter()
            .map(|(ki, kp)| (*ki, kp.sign(hash_to_sign.as_ref())))
            .collect::<BTreeMap<_, _>>();
        UpdateInstructionSignature { signatures }
    }
}

impl UpdateSigner for &[(UpdateKeysIndex, KeyPair)] {
    fn sign_update_hash(
        &self,
        hash_to_sign: &hashes::UpdateSignHash,
    ) -> UpdateInstructionSignature {
        let signatures = self
            .iter()
            .map(|(ki, kp)| (*ki, kp.sign(hash_to_sign.as_ref())))
            .collect::<BTreeMap<_, _>>();
        UpdateInstructionSignature { signatures }
    }
}

pub mod update {
    use std::io::Write;

    use crypto_common::to_bytes;

    use super::*;
    fn compute_sign_hash(
        header: &UpdateHeader,
        payload: &[u8], // serialized payload
    ) -> hashes::UpdateSignHash {
        let mut hasher = sha2::Sha256::new();
        header.serial(&mut hasher);
        hasher
            .write_all(payload)
            .expect("Writing to hasher does not fail.");
        <[u8; 32]>::from(hasher.finalize()).into()
    }

    /// Construct an update instruction and sign it.
    pub fn update(
        signer: impl UpdateSigner,
        seq_number: UpdateSequenceNumber,
        effective_time: TransactionTime,
        timeout: TransactionTime,
        payload: UpdatePayload,
    ) -> UpdateInstruction {
        let serialized_payload = to_bytes(&payload);
        let header = UpdateHeader {
            seq_number,
            effective_time,
            timeout,
            payload_size: PayloadSize {
                size: serialized_payload.len() as u32,
            },
        };
        let signatures = signer.sign_update_hash(&compute_sign_hash(&header, &serialized_payload));
        UpdateInstruction {
            header,
            payload,
            signatures,
        }
    }
}

#[derive(Debug, Clone)]
/// A block item are data items that are transmitted on the network either as
/// separate messages, or as part of blocks. They are the only user-generated
/// (as opposed to protocol-generated) message.
pub enum BlockItem<PayloadType> {
    /// Account transactions are messages which are signed and paid for by an
    /// account.
    AccountTransaction(AccountTransaction<PayloadType>),
    /// Credential deployments create new accounts. They are not paid for
    /// directly by the sender. Instead, bakers are rewarded by the protocol for
    /// including them.
    CredentialDeployment(
        Box<
            AccountCredentialMessage<
                id::constants::IpPairing,
                id::constants::ArCurve,
                id::constants::AttributeKind,
            >,
        >,
    ),
    UpdateInstruction(UpdateInstruction),
}

impl<PayloadType> From<AccountTransaction<PayloadType>> for BlockItem<PayloadType> {
    fn from(at: AccountTransaction<PayloadType>) -> Self { Self::AccountTransaction(at) }
}

impl<PayloadType>
    From<
        AccountCredentialMessage<
            id::constants::IpPairing,
            id::constants::ArCurve,
            id::constants::AttributeKind,
        >,
    > for BlockItem<PayloadType>
{
    fn from(
        at: AccountCredentialMessage<
            id::constants::IpPairing,
            id::constants::ArCurve,
            id::constants::AttributeKind,
        >,
    ) -> Self {
        Self::CredentialDeployment(Box::new(at))
    }
}

impl<PayloadType> From<UpdateInstruction> for BlockItem<PayloadType> {
    fn from(ui: UpdateInstruction) -> Self { Self::UpdateInstruction(ui) }
}

impl<PayloadType> BlockItem<PayloadType> {
    /// Compute the hash of the block item that identifies the block item on the
    /// chain.
    pub fn hash(&self) -> hashes::TransactionHash
    where
        BlockItem<PayloadType>: Serial, {
        let mut hasher = sha2::Sha256::new();
        hasher.put(&self);
        hashes::HashBytes::new(hasher.result())
    }
}

impl<V> Serial for BakerKeysPayload<V> {
    fn serial<B: Buffer>(&self, out: &mut B) {
        out.put(&self.election_verify_key);
        out.put(&self.signature_verify_key);
        out.put(&self.aggregation_verify_key);
        out.put(&self.proof_sig);
        out.put(&self.proof_election);
        out.put(&self.proof_aggregation);
    }
}

impl<V> Deserial for BakerKeysPayload<V> {
    fn deserial<R: ReadBytesExt>(source: &mut R) -> ParseResult<Self> {
        let election_verify_key = source.get()?;
        let signature_verify_key = source.get()?;
        let aggregation_verify_key = source.get()?;
        let proof_sig = source.get()?;
        let proof_election = source.get()?;
        let proof_aggregation = source.get()?;
        Ok(Self {
            phantom: PhantomData::default(),
            election_verify_key,
            signature_verify_key,
            aggregation_verify_key,
            proof_sig,
            proof_election,
            proof_aggregation,
        })
    }
}

impl Serial for AddBakerPayload {
    fn serial<B: Buffer>(&self, out: &mut B) {
        out.put(&self.keys);
        out.put(&self.baking_stake);
        out.put(&self.restake_earnings);
    }
}

impl Deserial for AddBakerPayload {
    fn deserial<R: ReadBytesExt>(source: &mut R) -> ParseResult<Self> {
        let keys = source.get()?;
        let baking_stake = source.get()?;
        let restake_earnings = source.get()?;
        Ok(Self {
            keys,
            baking_stake,
            restake_earnings,
        })
    }
}

impl Serial for InitContractPayload {
    fn serial<B: Buffer>(&self, out: &mut B) {
        out.put(&self.amount);
        out.put(&self.mod_ref);
        out.put(&self.init_name);
        out.put(&self.param);
    }
}

impl Deserial for InitContractPayload {
    fn deserial<R: ReadBytesExt>(source: &mut R) -> ParseResult<Self> {
        let amount = source.get()?;
        let mod_ref = source.get()?;
        let init_name = source.get()?;
        let param = source.get()?;
        Ok(InitContractPayload {
            amount,
            mod_ref,
            init_name,
            param,
        })
    }
}

impl Serial for UpdateContractPayload {
    fn serial<B: Buffer>(&self, out: &mut B) {
        out.put(&self.amount);
        out.put(&self.address);
        out.put(&self.receive_name);
        out.put(&self.message);
    }
}

impl Deserial for UpdateContractPayload {
    fn deserial<R: ReadBytesExt>(source: &mut R) -> ParseResult<Self> {
        let amount = source.get()?;
        let address = source.get()?;
        let receive_name = source.get()?;
        let message = source.get()?;
        Ok(UpdateContractPayload {
            amount,
            address,
            receive_name,
            message,
        })
    }
}

impl<P: PayloadLike> Serial for BlockItem<P> {
    fn serial<B: Buffer>(&self, out: &mut B) {
        match &self {
            BlockItem::AccountTransaction(at) => {
                out.put(&0u8);
                out.put(at)
            }
            BlockItem::CredentialDeployment(acdi) => {
                out.put(&1u8);
                out.put(acdi);
            }
            BlockItem::UpdateInstruction(ui) => {
                out.put(&2u8);
                out.put(ui);
            }
        }
    }
}

impl Deserial for BlockItem<EncodedPayload> {
    fn deserial<R: ReadBytesExt>(source: &mut R) -> ParseResult<Self> {
        let tag: u8 = source.get()?;
        match tag {
            0 => {
                let at = source.get()?;
                Ok(BlockItem::AccountTransaction(at))
            }
            1 => {
                let acdi = source.get()?;
                Ok(BlockItem::CredentialDeployment(acdi))
            }
            2 => {
                let ui = source.get()?;
                Ok(BlockItem::UpdateInstruction(ui))
            }
            _ => anyhow::bail!("Unsupported block item type: {}.", tag),
        }
    }
}

impl Serial for UpdatePayload {
    fn serial<B: Buffer>(&self, out: &mut B) {
        match self {
            UpdatePayload::Protocol(pu) => {
                1u8.serial(out);
                pu.serial(out)
            }
            UpdatePayload::ElectionDifficulty(ed) => {
                2u8.serial(out);
                ed.serial(out);
            }
            UpdatePayload::EuroPerEnergy(ee) => {
                3u8.serial(out);
                ee.serial(out);
            }
            UpdatePayload::MicroGTUPerEuro(me) => {
                4u8.serial(out);
                me.serial(out);
            }
            UpdatePayload::FoundationAccount(fa) => {
                5u8.serial(out);
                fa.serial(out);
            }
            UpdatePayload::MintDistribution(md) => {
                6u8.serial(out);
                md.serial(out);
            }
            UpdatePayload::TransactionFeeDistribution(tf) => {
                7u8.serial(out);
                tf.serial(out);
            }
            UpdatePayload::GASRewards(gr) => {
                8u8.serial(out);
                gr.serial(out);
            }
            UpdatePayload::BakerStakeThreshold(bs) => {
                9u8.serial(out);
                bs.serial(out)
            }
            UpdatePayload::Root(ru) => {
                10u8.serial(out);
                ru.serial(out)
            }
            UpdatePayload::Level1(l1) => {
                11u8.serial(out);
                l1.serial(out)
            }
            UpdatePayload::AddAnonymityRevoker(add_ar) => {
                12u8.serial(out);
                add_ar.serial(out)
            }
            UpdatePayload::AddIdentityProvider(add_ip) => {
                13u8.serial(out);
                add_ip.serial(out)
            }
        }
    }
}

impl Deserial for UpdatePayload {
    fn deserial<R: ReadBytesExt>(source: &mut R) -> ParseResult<Self> {
        match u8::deserial(source)? {
            1u8 => Ok(UpdatePayload::Protocol(source.get()?)),
            2u8 => Ok(UpdatePayload::ElectionDifficulty(source.get()?)),
            3u8 => Ok(UpdatePayload::EuroPerEnergy(source.get()?)),
            4u8 => Ok(UpdatePayload::MicroGTUPerEuro(source.get()?)),
            5u8 => Ok(UpdatePayload::FoundationAccount(source.get()?)),
            6u8 => Ok(UpdatePayload::MintDistribution(source.get()?)),
            7u8 => Ok(UpdatePayload::TransactionFeeDistribution(source.get()?)),
            8u8 => Ok(UpdatePayload::GASRewards(source.get()?)),
            9u8 => Ok(UpdatePayload::BakerStakeThreshold(source.get()?)),
            10u8 => Ok(UpdatePayload::Root(source.get()?)),
            11u8 => Ok(UpdatePayload::Level1(source.get()?)),
            12u8 => Ok(UpdatePayload::AddAnonymityRevoker(source.get()?)),
            13u8 => Ok(UpdatePayload::AddIdentityProvider(source.get()?)),
            tag => anyhow::bail!("Unknown update payload tag {}", tag),
        }
    }
}

/// Energy costs of transactions.
pub mod cost {
    use crate::types::CredentialType;

    use super::*;

    /// The B constant for NRG assignment. This scales the effect of the number
    /// of signatures on the energy.
    pub const A: u64 = 100;

    /// The A constant for NRG assignment. This scales the effect of transaction
    /// size on the energy.
    pub const B: u64 = 1;

    /// Base cost of a transaction is the minimum cost that accounts for
    /// transaction size and signature checking. In addition to base cost
    /// each transaction has a transaction-type specific cost.
    pub fn base_cost(transaction_size: u64, num_signatures: u32) -> Energy {
        Energy::from(B * transaction_size + A * u64::from(num_signatures))
    }

    /// Additional cost of a normal, account to account, transfer.
    pub const SIMPLE_TRANSFER: Energy = Energy { energy: 300 };

    /// Additional cost of an encrypted transfer.
    pub const ENCRYPTED_TRANSFER: Energy = Energy { energy: 27000 };

    /// Additional cost of a transfer from public to encrypted balance.
    pub const TRANSFER_TO_ENCRYPTED: Energy = Energy { energy: 600 };

    /// Additional cost of a transfer from encrypted to public balance.
    pub const TRANSFER_TO_PUBLIC: Energy = Energy { energy: 14850 };

    /// Cost of a scheduled transfer, parametrized by the number of releases.
    pub fn scheduled_transfer(num_releases: u16) -> Energy {
        Energy::from(u64::from(num_releases) * (300 + 64))
    }

    /// Additional cost of registerding the account as a baker.
    pub const ADD_BAKER: Energy = Energy { energy: 4050 };

    /// Additional cost of updating baker's keys.
    pub const UPDATE_BAKER_KEYS: Energy = Energy { energy: 4050 };

    /// Additional cost of updating the baker's stake, either increasing or
    /// lowering it.
    pub const UPDATE_BAKER_STAKE: Energy = Energy { energy: 300 };

    /// Additional cost of updating the baker's restake flag.
    pub const UPDATE_BAKER_RESTAKE: Energy = Energy { energy: 300 };

    /// Additional cost of removing a baker.
    pub const REMOVE_BAKER: Energy = Energy { energy: 300 };

    /// Additional cost of updating account's credentials, parametrized by
    /// - the number of credentials on the account before the update
    /// - list of keys of credentials to be added.
    pub fn update_credentials(num_credentials_before: u16, num_keys: &[u16]) -> Energy {
        UPDATE_CREDENTIALS_BASE + update_credentials_variable(num_credentials_before, num_keys)
    }

    /// Additional cost of registering a piece of data.
    pub const REGISTER_DATA: Energy = Energy { energy: 300 };

    /// Additional cost of deploying a smart contract module, parametrized by
    /// the size of the module, which is defined to be the size of
    /// the binary `.wasm` file that is sent as part of the transaction.
    pub fn deploy_module(module_size: u64) -> Energy { Energy::from(module_size / 10) }

    /// There is a non-trivial amount of lookup
    /// that needs to be done before we can start any checking. This ensures
    /// that those lookups are not a problem. If the credential updates are
    /// genuine then this cost is going to be negligible compared to
    /// verifying the credential.
    const UPDATE_CREDENTIALS_BASE: Energy = Energy { energy: 500 };

    /// Additional cost of deploying a credential of the given type and with the
    /// given number of keys.
    pub fn deploy_credential(ty: CredentialType, num_keys: u16) -> Energy {
        match ty {
            CredentialType::Initial => Energy::from(1000 + 100 * u64::from(num_keys)),
            CredentialType::Normal => Energy::from(54000 + 100 * u64::from(num_keys)),
        }
    }

    /// Helper function. This together with [UPDATE_CREDENTIALS_BASE] determine
    /// the cost of deploying a credential.
    fn update_credentials_variable(num_credentials_before: u16, num_keys: &[u16]) -> Energy {
        // the 500 * num_credentials_before is to account for transactions which do
        // nothing, e.g., don't add don't remove, and don't update the
        // threshold. These still have a cost since the way the accounts are
        // stored it will update the stored account data, which does take up
        // quite a bit of space per credential.
        let energy: u64 = 500 * u64::from(num_credentials_before)
            + num_keys
                .iter()
                .map(|&nk| u64::from(deploy_credential(CredentialType::Normal, nk)))
                .sum::<u64>();
        Energy::from(energy)
    }
}

/// High level wrappers for making transactions with minimal user input.
/// These wrappers handle encoding, setting energy costs when those are fixed
/// for transaction.
/// See also the [send] module above which combines construction with signing.
pub mod construct {
    use super::*;

    /// A transaction that is prepared to be signed.
    /// The serde instance serializes the structured payload and skips
    /// serializing the encoded one.
    #[derive(Debug, Clone, SerdeSerialize)]
    #[serde(rename_all = "camelCase")]
    pub struct PreAccountTransaction {
        pub header:       TransactionHeader,
        /// The payload.
        pub payload:      Payload,
        /// The encoded payload. This is already serialized payload that is
        /// constructed during construction of the prepared transaction
        /// since we need it to compute the cost.
        #[serde(skip_serializing)]
        pub encoded:      EncodedPayload,
        /// Hash of the transaction to sign.
        pub hash_to_sign: hashes::TransactionSignHash,
    }

    impl PreAccountTransaction {
        /// Sign the transaction with the provided signer. Note that this signer
        /// must match the account address and the number of keys that
        /// were used in construction, otherwise the transaction will be
        /// invalid.
        pub fn sign(self, signer: &impl TransactionSigner) -> AccountTransaction<EncodedPayload> {
            sign_transaction(signer, self.header, self.encoded)
        }
    }

    /// Serialize only the header and payload, so that this can be deserialized
    /// as a transaction body.
    impl Serial for PreAccountTransaction {
        fn serial<B: Buffer>(&self, out: &mut B) {
            self.header.serial(out);
            self.encoded.serial(out);
        }
    }

    impl Deserial for PreAccountTransaction {
        fn deserial<R: ReadBytesExt>(source: &mut R) -> ParseResult<Self> {
            let header: TransactionHeader = source.get()?;
            let encoded = get_encoded_payload(source, header.payload_size)?;
            let payload = encoded.decode()?;
            let hash_to_sign = compute_transaction_sign_hash(&header, &encoded);
            Ok(Self {
                header,
                payload,
                encoded,
                hash_to_sign,
            })
        }
    }

    /// Helper structure to store the intermediate state of a transaction.
    /// The problem this helps solve is that to compute the exact energy
    /// requirements for the transaction we need to know its exact size when
    /// serialized. For some we could compute this manually, but in general it
    /// is less error prone to serialize and get the length. To avoid doing
    /// double work we first serialize with a dummy `energy_amount` value, then
    /// in the [TransactionBuilder::finalize] method we compute the correct
    /// energy amount and overwrite it in the transaction, before signing
    /// it.
    /// This is deliberately made private so that the inconsistent internal
    /// state does not leak.
    struct TransactionBuilder {
        header:  TransactionHeader,
        payload: Payload,
        encoded: EncodedPayload,
    }

    /// Size of a transaction header. This is currently always 60 bytes.
    /// Future chain updates might revise this, but this is a big change so this
    /// is expected to change seldomly.
    pub const TRANSACTION_HEADER_SIZE: u64 = 32 + 8 + 8 + 4 + 8;

    impl TransactionBuilder {
        pub fn new(
            sender: AccountAddress,
            nonce: Nonce,
            expiry: TransactionTime,
            payload: Payload,
        ) -> Self {
            let encoded = payload.encode();
            let header = TransactionHeader {
                sender,
                nonce,
                energy_amount: 0.into(),
                payload_size: encoded.size(),
                expiry,
            };
            Self {
                header,
                payload,
                encoded,
            }
        }

        #[inline]
        fn size(&self) -> u64 {
            TRANSACTION_HEADER_SIZE + u64::from(u32::from(self.header.payload_size))
        }

        #[inline]
        pub fn construct(mut self, f: impl FnOnce(u64) -> Energy) -> PreAccountTransaction {
            let size = self.size();
            self.header.energy_amount = f(size);
            let hash_to_sign = compute_transaction_sign_hash(&self.header, &self.encoded);
            PreAccountTransaction {
                header: self.header,
                payload: self.payload,
                encoded: self.encoded,
                hash_to_sign,
            }
        }
    }

    /// Construct a transfer transaction.
    pub fn transfer(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        receiver: AccountAddress,
        amount: Amount,
    ) -> PreAccountTransaction {
        let payload = Payload::Transfer {
            to_address: receiver,
            amount,
        };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add {
                num_sigs,
                energy: cost::SIMPLE_TRANSFER,
            },
            payload,
        )
    }

    /// Construct a transfer transaction with a memo.
    pub fn transfer_with_memo(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        receiver: AccountAddress,
        amount: Amount,
        memo: Memo,
    ) -> PreAccountTransaction {
        let payload = Payload::TransferWithMemo {
            to_address: receiver,
            memo,
            amount,
        };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add {
                num_sigs,
                energy: cost::SIMPLE_TRANSFER,
            },
            payload,
        )
    }

    /// Make an encrypted transfer. The payload can be constructed using
    /// [encrypted_transfers::make_transfer_data].
    pub fn encrypted_transfer(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        receiver: AccountAddress,
        data: EncryptedAmountTransferData<EncryptedAmountsCurve>,
    ) -> PreAccountTransaction {
        let payload = Payload::EncryptedAmountTransfer {
            to:   receiver,
            data: Box::new(data),
        };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add {
                num_sigs,
                energy: cost::ENCRYPTED_TRANSFER,
            },
            payload,
        )
    }

    /// Make an encrypted transfer with a memo. The payload can be constructed
    /// using [encrypted_transfers::make_transfer_data].
    pub fn encrypted_transfer_with_memo(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        receiver: AccountAddress,
        data: EncryptedAmountTransferData<EncryptedAmountsCurve>,
        memo: Memo,
    ) -> PreAccountTransaction {
        // FIXME: This payload could be returned as well since it is only borrowed.
        let payload = Payload::EncryptedAmountTransferWithMemo {
            to: receiver,
            memo,
            data: Box::new(data),
        };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add {
                num_sigs,
                energy: cost::ENCRYPTED_TRANSFER,
            },
            payload,
        )
    }

    /// Transfer the given amount from public to encrypted balance of the given
    /// account.
    pub fn transfer_to_encrypted(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        amount: Amount,
    ) -> PreAccountTransaction {
        let payload = Payload::TransferToEncrypted { amount };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add {
                num_sigs,
                energy: cost::TRANSFER_TO_ENCRYPTED,
            },
            payload,
        )
    }

    /// Transfer the given amount from encrypted to public balance of the given
    /// account. The payload may be constructed using
    /// [encrypted_transfers::make_sec_to_pub_transfer_data]
    pub fn transfer_to_public(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        data: SecToPubAmountTransferData<EncryptedAmountsCurve>,
    ) -> PreAccountTransaction {
        // FIXME: This payload could be returned as well since it is only borrowed.
        let payload = Payload::TransferToPublic {
            data: Box::new(data),
        };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add {
                num_sigs,
                energy: cost::TRANSFER_TO_PUBLIC,
            },
            payload,
        )
    }

    /// Construct a transfer with schedule transaction, sending to the given
    /// account.
    pub fn transfer_with_schedule(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        receiver: AccountAddress,
        schedule: Vec<(Timestamp, Amount)>,
    ) -> PreAccountTransaction {
        let num_releases = schedule.len() as u16;
        let payload = Payload::TransferWithSchedule {
            to: receiver,
            schedule,
        };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add {
                num_sigs,
                energy: cost::scheduled_transfer(num_releases),
            },
            payload,
        )
    }

    /// Construct a transfer with schedule and memo transaction, sending to the
    /// given account.
    pub fn transfer_with_schedule_and_memo(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        receiver: AccountAddress,
        schedule: Vec<(Timestamp, Amount)>,
        memo: Memo,
    ) -> PreAccountTransaction {
        let num_releases = schedule.len() as u16;
        let payload = Payload::TransferWithScheduleAndMemo {
            to: receiver,
            memo,
            schedule,
        };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add {
                num_sigs,
                energy: cost::scheduled_transfer(num_releases),
            },
            payload,
        )
    }

    /// Register the sender account as a baker.
    /// TODO: Make a function for constructing the keys payload, with correct
    /// proofs and context.
    pub fn add_baker(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        baking_stake: Amount,
        restake_earnings: bool,
        keys: BakerAddKeysPayload,
    ) -> PreAccountTransaction {
        let payload = Payload::AddBaker {
            payload: Box::new(AddBakerPayload {
                keys,
                baking_stake,
                restake_earnings,
            }),
        };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add {
                num_sigs,
                energy: cost::ADD_BAKER,
            },
            payload,
        )
    }

    /// Update keys of the baker associated with the sender account.
    /// TODO: Make a function for constructing the keys payload, with correct
    /// proofs and context.
    pub fn update_baker_keys(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        keys: BakerUpdateKeysPayload,
    ) -> PreAccountTransaction {
        // FIXME: This payload could be returned as well since it is only borrowed.
        let payload = Payload::UpdateBakerKeys {
            payload: Box::new(keys),
        };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add {
                num_sigs,
                energy: cost::UPDATE_BAKER_KEYS,
            },
            payload,
        )
    }

    /// Deregister the account as a baker.
    pub fn remove_baker(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
    ) -> PreAccountTransaction {
        // FIXME: This payload could be returned as well since it is only borrowed.
        let payload = Payload::RemoveBaker;
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add {
                num_sigs,
                energy: cost::REMOVE_BAKER,
            },
            payload,
        )
    }

    /// Update the amount the account stakes for being a baker.
    pub fn update_baker_stake(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        new_stake: Amount,
    ) -> PreAccountTransaction {
        // FIXME: This payload could be returned as well since it is only borrowed.
        let payload = Payload::UpdateBakerStake { stake: new_stake };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add {
                num_sigs,
                energy: cost::UPDATE_BAKER_STAKE,
            },
            payload,
        )
    }

    pub fn update_baker_restake_earnings(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        restake_earnings: bool,
    ) -> PreAccountTransaction {
        // FIXME: This payload could be returned as well since it is only borrowed.
        let payload = Payload::UpdateBakerRestakeEarnings { restake_earnings };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add {
                num_sigs,
                energy: cost::UPDATE_BAKER_RESTAKE,
            },
            payload,
        )
    }

    /// Construct a transction to register the given piece of data.
    pub fn register_data(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        data: RegisteredData,
    ) -> PreAccountTransaction {
        let payload = Payload::RegisterData { data };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add {
                num_sigs,
                energy: cost::REGISTER_DATA,
            },
            payload,
        )
    }

    /// Deploy the given Wasm module. The module is given as a binary source,
    /// and no processing is done to the module.
    pub fn deploy_module(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        source: smart_contracts::ModuleSource,
    ) -> PreAccountTransaction {
        let module_size = source.size();
        let payload = Payload::DeployModule {
            module: smart_contracts::WasmModule { version: 0, source },
        };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add {
                num_sigs,
                energy: cost::deploy_module(module_size),
            },
            payload,
        )
    }

    /// Initialize a smart contract, giving it the given amount of energy for
    /// execution. The unique parameters are
    /// - `energy` -- the amount of energy that can be used for contract
    ///   execution. The base energy amount for transaction verification will be
    ///   added to this cost.
    pub fn init_contract(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        payload: InitContractPayload,
        energy: Energy,
    ) -> PreAccountTransaction {
        let payload = Payload::InitContract { payload };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add { num_sigs, energy },
            payload,
        )
    }

    /// Update a smart contract intance, giving it the given amount of energy
    /// for execution. The unique parameters are
    /// - `energy` -- the amount of energy that can be used for contract
    ///   execution. The base energy amount for transaction verification will be
    ///   added to this cost.
    pub fn update_contract(
        num_sigs: u32,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        payload: UpdateContractPayload,
        energy: Energy,
    ) -> PreAccountTransaction {
        let payload = Payload::Update { payload };
        make_transaction(
            sender,
            nonce,
            expiry,
            GivenEnergy::Add { num_sigs, energy },
            payload,
        )
    }

    pub enum GivenEnergy {
        /// Use this exact amount of energy.
        Absolute(Energy),
        /// Add the given amount of energy to the base amount.
        /// The base amount covers transaction size and signature checking.
        Add { energy: Energy, num_sigs: u32 },
    }

    /// A convenience wrapper around `sign_transaction` that construct the
    /// transaction and signs it. Compared to transaction-type-specific wrappers
    /// above this allows selecting the amount of energy
    pub fn make_transaction(
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        energy: GivenEnergy,
        payload: Payload,
    ) -> PreAccountTransaction {
        let builder = TransactionBuilder::new(sender, nonce, expiry, payload);
        let cost = |size| match energy {
            GivenEnergy::Absolute(energy) => energy,
            GivenEnergy::Add { num_sigs, energy } => cost::base_cost(size, num_sigs) + energy,
        };
        builder.construct(cost)
    }
}

/// High level wrappers for making transactions with minimal user input.
/// These wrappers handle encoding, setting energy costs when those are fixed
/// for transaction.
pub mod send {
    use super::*;

    /// Construct a transfer transaction.
    pub fn transfer(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        receiver: AccountAddress,
        amount: Amount,
    ) -> AccountTransaction<EncodedPayload> {
        construct::transfer(signer.num_keys(), sender, nonce, expiry, receiver, amount).sign(signer)
    }

    /// Construct a transfer transaction with a memo.
    pub fn transfer_with_memo(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        receiver: AccountAddress,
        amount: Amount,
        memo: Memo,
    ) -> AccountTransaction<EncodedPayload> {
        construct::transfer_with_memo(
            signer.num_keys(),
            sender,
            nonce,
            expiry,
            receiver,
            amount,
            memo,
        )
        .sign(signer)
    }

    /// Make an encrypted transfer. The payload can be constructed using
    /// [encrypted_transfers::make_transfer_data].
    pub fn encrypted_transfer(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        receiver: AccountAddress,
        data: EncryptedAmountTransferData<EncryptedAmountsCurve>,
    ) -> AccountTransaction<EncodedPayload> {
        construct::encrypted_transfer(signer.num_keys(), sender, nonce, expiry, receiver, data)
            .sign(signer)
    }

    /// Make an encrypted transfer with a memo. The payload can be constructed
    /// using [encrypted_transfers::make_transfer_data].
    pub fn encrypted_transfer_with_memo(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        receiver: AccountAddress,
        data: EncryptedAmountTransferData<EncryptedAmountsCurve>,
        memo: Memo,
    ) -> AccountTransaction<EncodedPayload> {
        construct::encrypted_transfer_with_memo(
            signer.num_keys(),
            sender,
            nonce,
            expiry,
            receiver,
            data,
            memo,
        )
        .sign(signer)
    }

    /// Transfer the given amount from public to encrypted balance of the given
    /// account.
    pub fn transfer_to_encrypted(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        amount: Amount,
    ) -> AccountTransaction<EncodedPayload> {
        construct::transfer_to_encrypted(signer.num_keys(), sender, nonce, expiry, amount)
            .sign(signer)
    }

    /// Transfer the given amount from encrypted to public balance of the given
    /// account. The payload may be constructed using
    /// [encrypted_transfers::make_sec_to_pub_transfer_data]
    pub fn transfer_to_public(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        data: SecToPubAmountTransferData<EncryptedAmountsCurve>,
    ) -> AccountTransaction<EncodedPayload> {
        construct::transfer_to_public(signer.num_keys(), sender, nonce, expiry, data).sign(signer)
    }

    /// Construct a transfer with schedule transaction, sending to the given
    /// account.
    pub fn transfer_with_schedule(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        receiver: AccountAddress,
        schedule: Vec<(Timestamp, Amount)>,
    ) -> AccountTransaction<EncodedPayload> {
        construct::transfer_with_schedule(
            signer.num_keys(),
            sender,
            nonce,
            expiry,
            receiver,
            schedule,
        )
        .sign(signer)
    }

    /// Construct a transfer with schedule and memo transaction, sending to the
    /// given account.
    pub fn transfer_with_schedule_and_memo(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        receiver: AccountAddress,
        schedule: Vec<(Timestamp, Amount)>,
        memo: Memo,
    ) -> AccountTransaction<EncodedPayload> {
        construct::transfer_with_schedule_and_memo(
            signer.num_keys(),
            sender,
            nonce,
            expiry,
            receiver,
            schedule,
            memo,
        )
        .sign(signer)
    }

    /// Register the sender account as a baker.
    pub fn add_baker(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        baking_stake: Amount,
        restake_earnings: bool,
        keys: BakerAddKeysPayload,
    ) -> AccountTransaction<EncodedPayload> {
        construct::add_baker(
            signer.num_keys(),
            sender,
            nonce,
            expiry,
            baking_stake,
            restake_earnings,
            keys,
        )
        .sign(signer)
    }

    /// Update keys of the baker associated with the sender account.
    pub fn update_baker_keys(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        keys: BakerUpdateKeysPayload,
    ) -> AccountTransaction<EncodedPayload> {
        construct::update_baker_keys(signer.num_keys(), sender, nonce, expiry, keys).sign(signer)
    }

    /// Deregister the account as a baker.
    pub fn remove_baker(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
    ) -> AccountTransaction<EncodedPayload> {
        construct::remove_baker(signer.num_keys(), sender, nonce, expiry).sign(signer)
    }

    /// Update the amount the account stakes for being a baker.
    pub fn update_baker_stake(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        new_stake: Amount,
    ) -> AccountTransaction<EncodedPayload> {
        construct::update_baker_stake(signer.num_keys(), sender, nonce, expiry, new_stake)
            .sign(signer)
    }

    pub fn update_baker_restake_earnings(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        restake_earnings: bool,
    ) -> AccountTransaction<EncodedPayload> {
        construct::update_baker_restake_earnings(
            signer.num_keys(),
            sender,
            nonce,
            expiry,
            restake_earnings,
        )
        .sign(signer)
    }

    /// Construct a transction to register the given piece of data.
    pub fn register_data(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        data: RegisteredData,
    ) -> AccountTransaction<EncodedPayload> {
        construct::register_data(signer.num_keys(), sender, nonce, expiry, data).sign(signer)
    }

    /// Deploy the given Wasm module. The module is given as a binary source,
    /// and no processing is done to the module.
    pub fn deploy_module(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        source: smart_contracts::ModuleSource,
    ) -> AccountTransaction<EncodedPayload> {
        construct::deploy_module(signer.num_keys(), sender, nonce, expiry, source).sign(signer)
    }

    /// Initialize a smart contract, giving it the given amount of energy for
    /// execution. The unique parameters are
    /// - `energy` -- the amount of energy that can be used for contract
    ///   execution. The base energy amount for transaction verification will be
    ///   added to this cost.
    pub fn init_contract(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        payload: InitContractPayload,
        energy: Energy,
    ) -> AccountTransaction<EncodedPayload> {
        construct::init_contract(signer.num_keys(), sender, nonce, expiry, payload, energy)
            .sign(signer)
    }

    /// Update a smart contract intance, giving it the given amount of energy
    /// for execution. The unique parameters are
    /// - `energy` -- the amount of energy that can be used for contract
    ///   execution. The base energy amount for transaction verification will be
    ///   added to this cost.
    pub fn update_contract(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        payload: UpdateContractPayload,
        energy: Energy,
    ) -> AccountTransaction<EncodedPayload> {
        construct::update_contract(signer.num_keys(), sender, nonce, expiry, payload, energy)
            .sign(signer)
    }

    pub enum GivenEnergy {
        /// Use this exact amount of energy.
        Absolute(Energy),
        /// Add the given amount of energy to the base amount.
        /// The base amount covers transaction size and signature checking.
        Add(Energy),
    }

    /// A convenience wrapper around `sign_transaction` that construct the
    /// transaction and signs it. Compared to transaction-type-specific wrappers
    /// above this allows selecting the amount of energy
    pub fn make_and_sign_transaction(
        signer: &impl ExactSizeTransactionSigner,
        sender: AccountAddress,
        nonce: Nonce,
        expiry: TransactionTime,
        energy: GivenEnergy,
        payload: Payload,
    ) -> AccountTransaction<EncodedPayload> {
        match energy {
            GivenEnergy::Absolute(energy) => construct::make_transaction(
                sender,
                nonce,
                expiry,
                construct::GivenEnergy::Absolute(energy),
                payload,
            )
            .sign(signer),
            GivenEnergy::Add(energy) => construct::make_transaction(
                sender,
                nonce,
                expiry,
                construct::GivenEnergy::Add {
                    energy,
                    num_sigs: signer.num_keys(),
                },
                payload,
            )
            .sign(signer),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::types::hashes::TransactionSignHash;
    use id::types::{SignatureThreshold, VerifyKey};
    use rand::Rng;
    use std::convert::TryFrom;

    use super::*;
    #[test]
    fn test_transaction_signature_check() {
        let mut rng = rand::thread_rng();
        let mut keys = BTreeMap::<CredentialIndex, BTreeMap<KeyIndex, KeyPair>>::new();
        let bound: usize = rng.gen_range(1, 20);
        for _ in 0..bound {
            let c_idx = CredentialIndex::from(rng.gen::<u8>());
            if keys.get(&c_idx).is_none() {
                let inner_bound: usize = rng.gen_range(1, 20);
                let mut cred_keys = BTreeMap::new();
                for _ in 0..inner_bound {
                    let k_idx = KeyIndex::from(rng.gen::<u8>());
                    cred_keys.insert(k_idx, KeyPair::generate(&mut rng));
                }
                keys.insert(c_idx, cred_keys);
            }
        }
        let hash = TransactionSignHash::new(rng.gen());
        let sig = keys.sign_transaction_hash(&hash);
        let threshold =
            AccountThreshold::try_from(rng.gen_range(1, (keys.len() + 1) as u8)).unwrap();
        let pub_keys = keys
            .iter()
            .map(|(&ci, keys)| {
                let threshold = SignatureThreshold(rng.gen_range(1, keys.len() + 1) as u8);
                let keys = keys
                    .iter()
                    .map(|(&ki, kp)| (ki, VerifyKey::from(kp)))
                    .collect();
                (ci, CredentialPublicKeys { keys, threshold })
            })
            .collect::<BTreeMap<_, _>>();
        let mut access_structure = AccountAccessStructure {
            threshold,
            keys: pub_keys,
        };
        assert!(
            verify_signature_transaction_sign_hash(&access_structure, &hash, &sig),
            "Transaction signature must validate."
        );

        access_structure.threshold = AccountThreshold::try_from((keys.len() + 1) as u8).unwrap();

        assert!(
            !verify_signature_transaction_sign_hash(&access_structure, &hash, &sig),
            "Transaction signature must not validate with invalid threshold."
        );
    }
}
