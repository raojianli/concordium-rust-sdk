use crate::{
    generated_types::{
        self, node_info_response, p2p_client, AccountAddress, BlockHash, BlockHashAndAmount,
        BlockHeight, Empty, GetAddressInfoRequest, GetModuleSourceRequest,
        GetTransactionStatusInBlockRequest, JsonResponse, PeerConnectRequest, PeerElement,
        PeersRequest, SendTransactionRequest, TransactionHash,
    },
    types::{
        self, network, queries,
        transactions::{self, PayloadLike},
        BakerId,
    },
};
use anyhow::anyhow;
use crypto_common::{types::TransactionSignature, Serial, Versioned};
use derive_more::From;
use id::{
    constants::{ArCurve, IpPairing},
    types::{ArInfo, GlobalContext, IpInfo},
};
use sha2::Digest;
use std::{borrow::Borrow, convert::TryInto, net::IpAddr, sync::Arc, time::UNIX_EPOCH};
use thiserror::Error;
pub use tonic::transport::Endpoint;
use tonic::{
    metadata::{errors::InvalidMetadataValue, MetadataValue},
    transport::Channel,
    Response,
};

#[derive(Error, Debug)]
/// Authentication, connection, or response parsing error.
pub enum RPCError {
    #[error("Call failed: {0}")]
    CallError(#[from] tonic::Status),
    #[error(transparent)]
    InvalidMetadata(#[from] InvalidMetadataValue),
    #[error("Error parsing JSON result: {0}")]
    ParseError(#[from] anyhow::Error),
}

impl From<serde_json::Error> for RPCError {
    fn from(x: serde_json::Error) -> Self { Self::ParseError(x.into()) }
}

impl From<semver::Error> for RPCError {
    fn from(x: semver::Error) -> Self { Self::ParseError(x.into()) }
}

#[derive(Error, Debug)]
/// Errors that can occur when making queries. This can either be a general
/// connection/authentication error, or the requested item is not found.
pub enum QueryError {
    #[error("RPC error: {0}")]
    /// A general RPC error occurred.
    RPCError(#[from] RPCError),
    #[error("Requested object not found.")]
    /// The requested item was not found.
    NotFound,
}

impl QueryError {
    /// Whether this error is the NotFound variant.
    pub fn is_not_found(&self) -> bool { matches!(self, Self::NotFound) }
}

impl From<tonic::Status> for QueryError {
    fn from(s: tonic::Status) -> Self { Self::RPCError(s.into()) }
}

impl From<InvalidMetadataValue> for QueryError {
    fn from(s: InvalidMetadataValue) -> Self { Self::RPCError(s.into()) }
}

impl From<serde_json::Error> for QueryError {
    fn from(s: serde_json::Error) -> Self { Self::RPCError(s.into()) }
}

/// Result a GRPC query. This is a simple alias for [std::Result](https://doc.rust-lang.org/std/result/enum.Result.html)
/// that fixes the error type to be [RPCError].
pub type RPCResult<A> = Result<A, RPCError>;

/// Result a GRPC query where the item lookup might fail.
/// This is a simple alias for [std::Result](https://doc.rust-lang.org/std/result/enum.Result.html) that fixes the error type to be [QueryResult].
pub type QueryResult<A> = Result<A, QueryError>;

/// Input to the [Client::get_blocks_at_height] query.
#[derive(Clone, Copy, Debug, From)]
pub enum BlocksAtHeightInput {
    Absolute {
        /// Height from the beginning of the chain.
        height: types::AbsoluteBlockHeight,
    },
    /// Query relative to an explicit genesis index.
    Relative {
        /// Genesis index to start from.
        genesis_index: types::GenesisIndex,
        /// Height starting from the genesis block at the genesis index.
        height:        types::BlockHeight,
        /// Whether to return results only from the specified genesis index
        /// (`true`), or allow results from more recent genesis indices
        /// as well (`false`).
        restrict:      bool,
    },
}

#[derive(Clone)]
/// Client that can perform queries.
/// All endpoints take a &mut self as an argument which means that a single
/// instance cannot be used concurrently. However instead of putting the Client
/// behind a Mutex, the intended way to use it is to clone it. Cloning is very
/// cheap and will reuse the underlying connection.
pub struct Client {
    client: p2p_client::P2pClient<Channel>,
    token:  Arc<String>,
}

impl Client {
    /// Internal helper that attaches the authentication token to the given
    /// request.
    fn construct_request<T>(&self, message: T) -> RPCResult<tonic::Request<T>> {
        let mut req = tonic::Request::new(message);
        let mv = MetadataValue::from_str(self.token.as_str())?;
        req.metadata_mut().insert("authentication", mv);
        Ok(req)
    }

    /// Construct a new client by connecting to the specified destination.
    pub async fn connect<D: TryInto<Endpoint>>(
        dst: D,
        token: String,
    ) -> Result<Self, tonic::transport::Error>
    where
        <D as TryInto<Endpoint>>::Error: std::error::Error + Send + Sync + 'static, {
        let client = p2p_client::P2pClient::connect(dst).await?;
        Ok(Client {
            client,
            token: Arc::new(token),
        })
    }

    /// Instruct the node to try to connect to the given peer.
    /// This also adds the address to the list of trusted addresses.
    /// These are addresses to which the node will try to keep connected to at
    /// all times.
    pub async fn peer_connect(&mut self, ip: &IpAddr, port: u16) -> RPCResult<bool> {
        let request = self.construct_request(PeerConnectRequest {
            ip:   Some(ip.to_string()),
            port: Some(port.into()),
        })?;
        let response = self.client.peer_connect(request).await?;
        Ok(response.into_inner().value)
    }

    /// Query for the node's uptime.
    pub async fn uptime(&mut self) -> RPCResult<chrono::Duration> {
        let request = self.construct_request(Empty {})?;
        let response = self.client.peer_uptime(request).await?;
        // the `as i64` is really safe since uptimes larger than that are not going to
        // happen
        Ok(chrono::Duration::milliseconds(
            response.into_inner().value as i64,
        ))
    }

    /// Query for the total number of packets that the node has sent thus far.
    pub async fn total_sent(&mut self) -> RPCResult<u64> {
        let request = self.construct_request(Empty {})?;
        let response = self.client.peer_total_sent(request).await?;
        Ok(response.into_inner().value)
    }

    /// Query for the total number of packets that the node has received thus
    /// far.
    pub async fn total_received(&mut self) -> RPCResult<u64> {
        let request = self.construct_request(Empty {})?;
        let response = self.client.peer_total_received(request).await?;
        Ok(response.into_inner().value)
    }

    /// Query for the node version.
    pub async fn version(&mut self) -> RPCResult<semver::Version> {
        let request = self.construct_request(Empty {})?;
        let response = self.client.peer_version(request).await?;
        let version = semver::Version::parse(&response.into_inner().value)?;
        Ok(version)
    }

    /// FIXME: Better return type.
    pub async fn peer_statistics(
        &mut self,
        include_bootstrappers: bool,
    ) -> RPCResult<types::PeerStatsResponse> {
        let request = self.construct_request(PeersRequest {
            include_bootstrappers,
        })?;
        let response = self.client.peer_stats(request).await?;
        Ok(response.into_inner())
    }

    /// Get the list of peers, possibly including any bootstrappers the node is
    /// currently connected to.
    pub async fn peer_list(
        &mut self,
        include_bootstrappers: bool,
    ) -> RPCResult<Vec<network::PeerElement>> {
        let request = self.construct_request(PeersRequest {
            include_bootstrappers,
        })?;
        let response = self.client.peer_list(request).await?;
        let response_data = response.into_inner();
        response_data
            .peers
            .into_iter()
            .map(|pe| {
                let catchup_status = pe.catchup_status();
                let node_id = pe
                    .node_id
                    .ok_or_else(|| RPCError::ParseError(anyhow!("Peer without node ID.")))?;
                let port = pe
                    .port
                    .ok_or_else(|| RPCError::ParseError(anyhow!("Peer without port.")))?
                    as u16;
                let ip = pe
                    .ip
                    .ok_or_else(|| anyhow!("Peer IP not present."))?
                    .parse::<IpAddr>()
                    .map_err(|err| RPCError::ParseError(err.into()))?;
                Ok(network::PeerElement {
                    node_id,
                    port,
                    ip,
                    catchup_status,
                })
            })
            .collect::<RPCResult<_>>()
    }

    /// Ban a specific node. See [Client::unban_node] for the dual.
    /// Note that this will also cause the node to drop any connections to a
    /// matching node.
    pub async fn ban_node(&mut self, ban_method: queries::BanMethod) -> RPCResult<bool> {
        let pe = match ban_method {
            queries::BanMethod::Ip(ip) => PeerElement {
                ip: Some(ip.to_string()),
                ..Default::default()
            },
            queries::BanMethod::Id(id) => PeerElement {
                node_id: Some(id.to_string()),
                ..Default::default()
            },
        };
        let request = self.construct_request(pe)?;
        let response = self.client.ban_node(request).await?;
        Ok(response.into_inner().value)
    }

    /// Unban a specific node. See [Client::ban_node] for the dual.
    /// Note that this will only remove the ban, but it will not re-establish
    /// any connections. To connect use [Client::peer_connect].
    pub async fn unban_node(&mut self, ip: IpAddr) -> RPCResult<bool> {
        let pe = PeerElement {
            ip: Some(ip.to_string()),
            ..Default::default()
        };
        let request = self.construct_request(pe)?;
        let response = self.client.unban_node(request).await?;
        Ok(response.into_inner().value)
    }

    /// Ask the node to join the specified network.
    pub async fn join_network(&mut self, network_id: network::NetworkId) -> RPCResult<bool> {
        let request = self.construct_request(generated_types::NetworkChangeRequest {
            network_id: Some(u16::from(network_id).into()),
        })?;
        let response = self.client.join_network(request).await?;
        Ok(response.into_inner().value)
    }

    /// Ask the node to leave the specified network.
    pub async fn leave_network(&mut self, network_id: network::NetworkId) -> RPCResult<bool> {
        let request = self.construct_request(generated_types::NetworkChangeRequest {
            network_id: Some(u16::from(network_id).into()),
        })?;
        let response = self.client.leave_network(request).await?;
        Ok(response.into_inner().value)
    }

    /// Get some general information about a running node. See the return type
    /// for details of the data that is returned.
    pub async fn node_info(&mut self) -> RPCResult<queries::NodeInfo> {
        let request = self.construct_request(Empty {})?;
        let response = self.client.node_info(request).await?;
        let ni = response.into_inner();
        let local_time = chrono::DateTime::<chrono::Utc>::from(UNIX_EPOCH)
            + chrono::Duration::seconds(ni.current_localtime as i64);
        let peer_details = match ni.peer_type.as_str() {
            "Bootstrapper" => queries::PeerDetails::Bootstrapper,
            "Node" => {
                let consensus_state = if ni.consensus_running {
                    if ni.consensus_baker_running {
                        use node_info_response::IsInBakingCommittee::*;
                        let active_state = match ni.consensus_baker_committee() {
                            NotInCommittee => queries::ActiveConsensusState::NotInCommittee,
                            AddedButNotActiveInCommittee => {
                                queries::ActiveConsensusState::NotYetActive
                            }
                            AddedButWrongKeys => queries::ActiveConsensusState::IncorrectKeys,
                            ActiveInCommittee => queries::ActiveConsensusState::Active {
                                baker_id:  BakerId::from(ni.consensus_baker_id.ok_or_else(
                                    || anyhow!("Invalid response, active but no baker id."),
                                )?),
                                finalizer: ni.consensus_finalizer_committee,
                            },
                        };
                        queries::ConsensusState::Active { active_state }
                    } else {
                        queries::ConsensusState::Passive
                    }
                } else {
                    queries::ConsensusState::NotRunning
                };
                queries::PeerDetails::Node { consensus_state }
            }
            e => {
                return Err(RPCError::ParseError(anyhow!(
                    "Unrecognized peer type: {}",
                    e
                )))
            }
        };
        let node_id = ni.node_id.ok_or_else(|| anyhow!("Node ID not set."))?;
        Ok(queries::NodeInfo {
            node_id,
            local_time,
            peer_details,
        })
    }

    /// Get consensus information from the node. This is an overview of the
    /// node's view of the chain.
    pub async fn get_consensus_status(&mut self) -> RPCResult<queries::ConsensusInfo> {
        let request = self.construct_request(Empty {})?;
        let response = self.client.get_consensus_status(request).await?;
        let consensus_info =
            serde_json::from_str::<queries::ConsensusInfo>(&response.into_inner().value)?;
        Ok(consensus_info)
    }

    /// Get information about a specific block, if it exists.
    pub async fn get_block_info(
        &mut self,
        block_hash: &types::hashes::BlockHash,
    ) -> QueryResult<queries::BlockInfo> {
        let request = self.construct_request(BlockHash {
            block_hash: block_hash.to_string(),
        })?;
        let response = self.client.get_block_info(request).await?;
        parse_json_response(response)
    }

    /// Get the ancestors of a given block, if any.
    pub async fn get_ancestors(
        &mut self,
        block: &types::hashes::BlockHash,
        num: u64,
    ) -> QueryResult<Vec<types::hashes::BlockHash>> {
        let request = self.construct_request(BlockHashAndAmount {
            block_hash: block.to_string(),
            amount:     num,
        })?;
        let response = self.client.get_ancestors(request).await?;
        parse_json_response(response)
    }

    /// Get the branches of the node's tree. Branches are all live blocks that
    /// are successors of the last finalized block. In particular this means
    /// that blocks which do not have a parent are not included in this
    /// response.
    pub async fn get_branches(&mut self) -> RPCResult<queries::Branch> {
        let request = self.construct_request(Empty {})?;
        let response = self.client.get_branches(request).await?;
        let branches = serde_json::from_str::<queries::Branch>(&response.into_inner().value)?;
        Ok(branches)
    }

    /// Get the list of block hashes at the given height. If there are no blocks
    /// at the given height an empty list is returned.
    pub async fn get_blocks_at_height(
        &mut self,
        bh: BlocksAtHeightInput,
    ) -> RPCResult<Vec<types::hashes::BlockHash>> {
        let request = match bh {
            BlocksAtHeightInput::Absolute { height } => self.construct_request(BlockHeight {
                block_height:              height.into(),
                from_genesis_index:        0,
                restrict_to_genesis_index: false,
            })?,
            BlocksAtHeightInput::Relative {
                genesis_index,
                height,
                restrict,
            } => self.construct_request(BlockHeight {
                block_height:              height.into(),
                from_genesis_index:        genesis_index.into(),
                restrict_to_genesis_index: restrict,
            })?,
        };
        let response = self.client.get_blocks_at_height(request).await?;
        let blocks = serde_json::from_str::<Vec<_>>(response.into_inner().value.as_str())?;
        Ok(blocks)
    }

    /// FIXME: This currently does nothing on the node, hence it is private.
    async fn _start_baker(&mut self) -> RPCResult<bool> {
        let request = self.construct_request(Empty {})?;
        let response = self.client.start_baker(request).await?;
        Ok(response.into_inner().value)
    }

    /// Stop the baker thread. The node will still keep running and responding
    /// to queries.
    pub async fn stop_baker(&mut self) -> RPCResult<bool> {
        let request = self.construct_request(Empty {})?;
        let response = self.client.stop_baker(request).await?;
        Ok(response.into_inner().value)
    }

    /// Get the list of ips that the node will currently not connect to, nor
    /// accept connections for. See [Client::ban_node] and
    /// [Client::unban_node] for functions to add and remove ips from the list.
    pub async fn get_banned_ips(&mut self) -> RPCResult<Vec<IpAddr>> {
        let request = self.construct_request(Empty {})?;
        let response = self.client.get_banned_peers(request).await?;
        let res = response
            .into_inner()
            .peers
            .iter()
            .map(|peer| {
                let ip_string = peer
                    .ip
                    .as_ref()
                    .ok_or_else(|| RPCError::ParseError(anyhow!("IP address not given")))?;
                let ip = ip_string
                    .parse::<IpAddr>()
                    .map_err(|e| RPCError::ParseError(anyhow!("Cannot parse IP address: {}", e)))?;
                Ok(ip)
            })
            .collect::<RPCResult<_>>()?;
        Ok(res)
    }

    /// Stop the node. After this is called the node will stop.
    pub async fn shutdown(&mut self) -> RPCResult<bool> {
        let request = self.construct_request(Empty {})?;
        let response = self.client.shutdown(request).await?;
        Ok(response.into_inner().value)
    }

    /// Get the list of accounts in the given block. If the block does not exist
    /// [QueryError::NotFound] is returned.
    pub async fn get_account_list(
        &mut self,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<Vec<id::types::AccountAddress>> {
        let request = self.construct_request(BlockHash {
            block_hash: bh.to_string(),
        })?;
        let response = self.client.get_account_list(request).await?;
        parse_json_response(response)
    }

    /// Get the list of smart contract instances in a given block. If the block
    /// does not exist [QueryError::NotFound] is returned.
    pub async fn get_instances(
        &mut self,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<Vec<types::ContractAddress>> {
        let request = self.construct_request(BlockHash {
            block_hash: bh.to_string(),
        })?;
        let response = self.client.get_instances(request).await?;
        parse_json_response(response)
    }

    /// Get the information for the given account in the given block. If either
    /// the block or the account does not exist [QueryError::NotFound] is
    /// returned.
    pub async fn get_account_info(
        &mut self,
        addr: impl Borrow<id::types::AccountAddress>,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<types::AccountInfo> {
        let res = self.get_account_info_raw(addr, bh).await?;
        Ok(serde_json::from_value(res)?)
    }

    /// Get the information for the given account in the given block. If either
    /// the block or the account does not exist [QueryError::NotFound] is
    /// returned. In contrast to [Client::get_account_info] this function does
    /// not fully parse the result. Since parsing account responses can be
    /// relatively expensive this provides an option to delay parsing, or to
    /// only take out individual values. The return
    /// [Value](serde_json::Value) will always be parseable as a
    /// [AccountInfo](types::AccountInfo)
    pub async fn get_account_info_raw(
        &mut self,
        addr: impl Borrow<id::types::AccountAddress>,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<serde_json::Value> {
        let request = self.construct_request(GetAddressInfoRequest {
            address:    addr.borrow().to_string(),
            block_hash: bh.to_string(),
        })?;
        let response = self.client.get_account_info(request).await?;
        parse_json_response(response)
    }

    /// Get the information for the given account in the given block by
    /// credential registration id. This returns an account that is either
    /// currently, or was in the past, associated with a credential with the
    /// specified registration id.
    ///
    /// If either the block or the credential registration id does no not exist
    /// then [QueryError::NotFound] is returned.
    pub async fn get_account_info_by_cred_id(
        &mut self,
        addr: impl Borrow<crate::types::CredentialRegistrationID>,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<types::AccountInfo> {
        let res = self.get_account_info_by_cred_id_raw(addr, bh).await?;
        Ok(serde_json::from_value(res)?)
    }

    /// Get the information for the given account in the given block by
    /// credential registration id. This returns an account that is either
    /// currently, or was in the past, associated with a credential with the
    /// specified registration id.
    ///
    /// If either the block or the credential registration id does no not exist
    /// then [QueryError::NotFound] is returned.
    ///
    /// Compares to [Client::get_account_info_by_cred_id] analogously to how
    /// [Client::get_account_info] compares to [Client::get_account_info_raw]
    pub async fn get_account_info_by_cred_id_raw(
        &mut self,
        addr: impl Borrow<crate::types::CredentialRegistrationID>,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<serde_json::Value> {
        let request = self.construct_request(GetAddressInfoRequest {
            address:    addr.borrow().to_string(),
            block_hash: bh.to_string(),
        })?;
        let response = self.client.get_account_info(request).await?;
        parse_json_response(response)
    }

    /// Get the information for the given smart contract instance in the given
    /// block.
    pub async fn get_instance_info(
        &mut self,
        addr: types::ContractAddress,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<types::smart_contracts::InstanceInfo> {
        let request = self.construct_request(GetAddressInfoRequest {
            address:    serde_json::to_string(&addr).expect("Never fails."),
            block_hash: bh.to_string(),
        })?;
        let response = self.client.get_instance_info(request).await?;
        parse_json_response(response)
    }

    /// Get the information about total amount of CCD and the state of various
    /// administrative accounts.
    pub async fn get_reward_status(
        &mut self,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<types::RewardsOverview> {
        let request = self.construct_request(BlockHash {
            block_hash: bh.to_string(),
        })?;
        let response = self.client.get_reward_status(request).await?;
        parse_json_response(response)
    }

    /// Get consensus-relevant information for the specified block.
    pub async fn get_birk_parameters(
        &mut self,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<types::BirkParameters> {
        let request = self.construct_request(BlockHash {
            block_hash: bh.to_string(),
        })?;
        let response = self.client.get_birk_parameters(request).await?;
        parse_json_response(response)
    }

    /// Get the list of smart contract modules in the given block.
    pub async fn get_module_list(
        &mut self,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<Vec<types::smart_contracts::ModuleRef>> {
        let request = self.construct_request(BlockHash {
            block_hash: bh.to_string(),
        })?;
        let response = self.client.get_module_list(request).await?;
        parse_json_response(response)
    }

    // FIXME: Do not return just bytes, wrap it.
    pub async fn get_module_source(
        &mut self,
        mr: &types::smart_contracts::ModuleRef,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<Vec<u8>> {
        let request = self.construct_request(GetModuleSourceRequest {
            block_hash: bh.to_string(),
            module_ref: mr.to_string(),
        })?;
        let response = self.client.get_module_source(request).await?;
        let bs = response.into_inner().value;
        if bs.is_empty() {
            Err(QueryError::NotFound)
        } else {
            Ok(bs)
        }
    }

    /// Get the list of identity providers in the given block. If the block does
    /// not exist [QueryError::NotFound] is returned.
    pub async fn get_identity_providers(
        &mut self,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<Vec<IpInfo<IpPairing>>> {
        let request = self.construct_request(BlockHash {
            block_hash: bh.to_string(),
        })?;
        let response = self.client.get_identity_providers(request).await?;
        parse_json_response(response)
    }

    /// Get the list of anonymity revokers in the given block. If the block does
    /// not exist [QueryError::NotFound] is returned.
    pub async fn get_anonymity_revokers(
        &mut self,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<Vec<ArInfo<ArCurve>>> {
        let request = self.construct_request(BlockHash {
            block_hash: bh.to_string(),
        })?;
        let response = self.client.get_anonymity_revokers(request).await?;
        parse_json_response(response)
    }

    /// Get the currently used cryptographic parameters. If the block does
    /// not exist [QueryError::NotFound] is returned.
    pub async fn get_cryptographic_parameters(
        &mut self,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<GlobalContext<ArCurve>> {
        let request = self.construct_request(BlockHash {
            block_hash: bh.to_string(),
        })?;
        let response = self.client.get_cryptographic_parameters(request).await?;
        let versioned_ars: Versioned<_> = parse_json_response(response)?;
        // FIXME: Parse versioned, ensure it is 0.
        Ok(versioned_ars.value)
    }

    /// Get the list of transactions hashes for transactions that claim to be
    /// from the given account, but which are not yet finalized.
    /// They are either committed to a block or still pending.
    ///
    /// If the account does not exist an empty list will be returned.
    pub async fn get_account_non_finalized_transactions(
        &mut self,
        addr: &id::types::AccountAddress,
    ) -> RPCResult<Vec<types::hashes::TransactionHash>> {
        let request = self.construct_request(AccountAddress {
            account_address: addr.to_string(),
        })?;
        let response = self
            .client
            .get_account_non_finalized_transactions(request)
            .await?;
        // FIXME: Should this handle non-existent account address. Check the API.
        let txs = serde_json::from_str(response.into_inner().value.as_str())?;
        Ok(txs)
    }

    /// Get the status of a transaction in a given block. If the transaction is
    /// not known, or is committed or finalized, but not in the given block,
    /// [QueryError::NotFound] is returned.
    pub async fn get_transaction_status_in_block(
        &mut self,
        bh: &types::hashes::BlockHash,
        th: &types::hashes::TransactionHash,
    ) -> QueryResult<types::TransactionStatusInBlock> {
        let request = self.construct_request(GetTransactionStatusInBlockRequest {
            transaction_hash: th.to_string(),
            block_hash:       bh.to_string(),
        })?;
        let response = self.client.get_transaction_status_in_block(request).await?;
        parse_json_response(response)
    }

    /// Query the status of the transaction. If the transaction is not known to
    /// the node [QueryError::NotFound] is returned.
    pub async fn get_transaction_status(
        &mut self,
        th: &types::hashes::TransactionHash,
    ) -> QueryResult<types::TransactionStatus> {
        let request = self.construct_request(TransactionHash {
            transaction_hash: th.to_string(),
        })?;
        let response = self.client.get_transaction_status(request).await?;
        parse_json_response(response)
    }

    /// Get the summary of a block. This lists all transactions and special
    /// outcomes occurring in a given block, as well as the value of chain
    /// parameters at the given block. If the block is not in the node's tree
    /// [QueryError::NotFound] is returned.
    pub async fn get_block_summary(
        &mut self,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<types::BlockSummary> {
        let summary = self.get_block_summary_raw(bh).await?;
        Ok(serde_json::from_value(summary)?)
    }

    /// Get the summary of a block. This lists all transactions and special
    /// outcomes occurring in a given block, as well as the value of chain
    /// parameters at the given block. If the block is not in the node's tree
    /// [QueryError::NotFound] is returned. In contrast to
    /// [Client::get_block_summary] this function does not fully parse the
    /// result since that can be expensive and some applications might not need
    /// it.
    pub async fn get_block_summary_raw(
        &mut self,
        bh: &types::hashes::BlockHash,
    ) -> QueryResult<serde_json::Value> {
        let request = self.construct_request(BlockHash {
            block_hash: bh.to_string(),
        })?;
        let response = self.client.get_block_summary(request).await?;
        parse_json_response(response)
    }

    /// Get the next nonce for the account, with information on how reliable the
    /// information is.
    pub async fn get_next_account_nonce(
        &mut self,
        addr: &id::types::AccountAddress,
    ) -> RPCResult<queries::AccountNonceResponse> {
        let request = self.construct_request(AccountAddress {
            account_address: addr.to_string(),
        })?;
        let response = self.client.get_next_account_nonce(request).await?;
        let nn = serde_json::from_str(response.into_inner().value.as_str())?;
        Ok(nn)
    }

    /// Send the given block item on the given network.
    pub async fn send_transaction<PayloadType: PayloadLike>(
        &mut self,
        network_id: network::NetworkId,
        bi: &transactions::BlockItem<PayloadType>,
    ) -> RPCResult<bool> {
        let request = self.construct_request(SendTransactionRequest {
            network_id: u32::from(u16::from(network_id)),
            payload:    crypto_common::to_bytes(&crypto_common::Versioned::new(
                crypto_common::VERSION_0,
                bi,
            )),
        })?;
        let response = self.client.send_transaction(request).await?;
        Ok(response.into_inner().value)
    }

    /// Send the given account transaction item on the given network.
    /// This is a low-level function that can be useful in case a transaction is
    /// constructed by a third party. It avoids deserializing and converting
    /// data.
    /// If the transaciton is accepted by the node then the transaction hash
    /// that can be used to query the status is returned.
    pub async fn send_raw_account_transaction(
        &mut self,
        network_id: network::NetworkId,
        signatures: &TransactionSignature, // signatures for the transaction.
        body: &[u8],                       // body of the transaction (header + payload)
    ) -> RPCResult<types::hashes::TransactionHash> {
        let mut data = Vec::with_capacity(
            body.len() + transactions::construct::TRANSACTION_HEADER_SIZE as usize,
        );
        crypto_common::VERSION_0.serial(&mut data); // outer version number
        0u8.serial(&mut data); // tag for account transaction
        signatures.serial(&mut data); // signatures
        data.extend_from_slice(body); // header + payload
                                      // compute the hash of the transaction
        let hash = types::hashes::HashBytes::new(sha2::Sha256::digest(&data).into());
        let request = self.construct_request(SendTransactionRequest {
            network_id: u32::from(u16::from(network_id)),
            payload:    data,
        })?;
        let response = self.client.send_transaction(request).await?;
        if response.into_inner().value {
            Ok(hash)
        } else {
            Err(RPCError::CallError(tonic::Status::invalid_argument(
                "Transaction was invalid and thus not accepted by the node.",
            )))
        }
    }
}

/// Parse a response which is either `null` or can be parsed as a specified
/// value. `null` is mapped to [QueryError::NotFound].
fn parse_json_response<A: serde::de::DeserializeOwned>(
    r: Response<JsonResponse>,
) -> QueryResult<A> {
    let inner = r.into_inner();
    // We go through the intermediate Value to handle arbitrary precision floats
    // limitation of Serde.
    let val = serde_json::from_str::<serde_json::Value>(inner.value.as_str())?;
    if val.is_null() {
        Err(QueryError::NotFound)
    } else {
        let res = serde_json::from_value::<A>(val)?;
        Ok(res)
    }
}
