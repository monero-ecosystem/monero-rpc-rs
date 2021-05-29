//! Monero daemon and wallet RPC.

#[macro_use]
mod util;
mod models;

pub use self::{models::*, util::*};

use async_trait::async_trait;
use jsonrpc_core::types::{Id, *};
use monero::{cryptonote::hash::Hash as CryptoNoteHash, util::address::PaymentId, Address};
use serde::{de::IgnoredAny, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    convert::TryFrom,
    fmt::Debug,
    iter::{empty, once},
    num::NonZeroU64,
    ops::{Bound, Deref, RangeBounds, RangeInclusive},
    sync::Arc,
};
use tracing::*;
use uuid::Uuid;

enum RpcParams {
    Array(Box<dyn Iterator<Item = Value> + Send + 'static>),
    Map(Box<dyn Iterator<Item = (String, Value)> + Send + 'static>),
    None,
}

impl RpcParams {
    fn array<A>(v: A) -> Self
    where
        A: Iterator<Item = Value> + Send + 'static,
    {
        RpcParams::Array(Box::new(v))
    }

    fn map<M>(v: M) -> Self
    where
        M: Iterator<Item = (&'static str, Value)> + Send + 'static,
    {
        RpcParams::Map(Box::new(v.map(|(k, v)| (k.to_string(), v))))
    }
}

impl From<RpcParams> for Params {
    fn from(value: RpcParams) -> Self {
        match value {
            RpcParams::Map(v) => Params::Map(v.collect()),
            RpcParams::Array(v) => Params::Array(v.collect()),
            RpcParams::None => Params::None,
        }
    }
}

#[async_trait]
trait JsonRpcCaller: Debug + Send + Sync + 'static {
    async fn call(
        &self,
        method: &'static str,
        params: RpcParams,
    ) -> anyhow::Result<jsonrpc_core::Result<Value>>;
}

#[derive(Debug)]
struct RemoteCaller {
    http_client: reqwest::Client,
    addr: String,
}

#[async_trait]
impl JsonRpcCaller for RemoteCaller {
    async fn call(
        &self,
        method: &'static str,
        params: RpcParams,
    ) -> anyhow::Result<jsonrpc_core::Result<Value>> {
        let client = self.http_client.clone();
        let uri = format!("{}/json_rpc", &self.addr);

        let method_call = MethodCall {
            jsonrpc: Some(Version::V2),
            method: method.to_string(),
            params: params.into(),
            id: Id::Str(Uuid::new_v4().to_string()),
        };

        trace!("Sending JSON-RPC method call: {:?}", method_call);

        let rsp = client
            .post(&uri)
            .json(&method_call)
            .send()
            .await?
            .json::<response::Output>()
            .await?;

        trace!("Received JSON-RPC response: {:?}", rsp);

        let v = jsonrpc_core::Result::<Value>::from(rsp);

        Ok(v)
    }
}

#[derive(Clone, Debug)]
struct CallerWrapper(Arc<dyn JsonRpcCaller>);

impl CallerWrapper {
    async fn request<T>(&self, method: &'static str, params: RpcParams) -> anyhow::Result<T>
    where
        T: for<'de> Deserialize<'de> + Send + 'static,
    {
        let c = self.0.call(method, params);
        Ok(serde_json::from_value(c.await??)?)
    }
}

/// Base RPC client. It is useless on its own, please see the attached methods instead.
#[derive(Clone, Debug)]
pub struct RpcClient {
    inner: CallerWrapper,
}

impl RpcClient {
    pub fn new(addr: String) -> Self {
        Self {
            inner: CallerWrapper(Arc::new(RemoteCaller {
                http_client: reqwest::ClientBuilder::new().build().unwrap(),
                addr,
            })),
        }
    }

    /// Create a daemon client.
    pub fn daemon(self) -> DaemonClient {
        let Self { inner } = self;
        DaemonClient { inner }
    }

    /// Create a wallet client.
    pub fn wallet(self) -> WalletClient {
        let Self { inner } = self;
        WalletClient { inner }
    }
}

#[derive(Clone, Debug)]
pub struct DaemonClient {
    inner: CallerWrapper,
}

#[derive(Clone, Debug)]
pub struct RegtestDaemonClient(pub DaemonClient);

impl Deref for RegtestDaemonClient {
    type Target = DaemonClient;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub enum GetBlockHeaderSelector {
    Last,
    Hash(BlockHash),
    Height(u64),
}

impl DaemonClient {
    /// Look up how many blocks are in the longest chain known to the node.
    pub async fn get_block_count(&self) -> anyhow::Result<NonZeroU64> {
        #[derive(Deserialize)]
        struct Rsp {
            count: NonZeroU64,
        }

        Ok(self
            .inner
            .request::<MoneroResult<Rsp>>("get_block_count", RpcParams::array(empty()))
            .await?
            .into_inner()
            .count)
    }

    /// Look up a block's hash by its height.
    pub async fn on_get_block_hash(&self, height: u64) -> anyhow::Result<BlockHash> {
        self.inner
            .request::<HashString<BlockHash>>(
                "on_get_block_hash",
                RpcParams::array(once(height.into())),
            )
            .await
            .map(|v| v.0)
    }

    /// Get a block template on which mining a new block.
    pub async fn get_block_template(
        &self,
        wallet_address: Address,
        reserve_size: u64,
    ) -> anyhow::Result<BlockTemplate> {
        Ok(self
            .inner
            .request::<MoneroResult<BlockTemplate>>(
                "get_block_template",
                RpcParams::array(
                    empty()
                        .chain(once(serde_json::to_value(wallet_address).unwrap()))
                        .chain(once(reserve_size.into())),
                ),
            )
            .await?
            .into_inner())
    }

    /// Submit a mined block to the network.
    pub async fn submit_block(&self, block_blob_data: String) -> anyhow::Result<String> {
        self.inner
            .request(
                "submit_block",
                RpcParams::array(once(block_blob_data.into())),
            )
            .await
    }

    /// Retrieve block header information matching selected filter.
    pub async fn get_block_header(
        &self,
        selector: GetBlockHeaderSelector,
    ) -> anyhow::Result<BlockHeaderResponse> {
        #[derive(Deserialize)]
        struct Rsp {
            block_header: BlockHeaderResponseR,
        }

        let (request, params) = match selector {
            GetBlockHeaderSelector::Last => ("get_last_block_header", RpcParams::None),
            GetBlockHeaderSelector::Hash(hash) => (
                "get_block_header_by_hash",
                RpcParams::map(
                    Some(("hash", serde_json::to_value(HashString(hash)).unwrap())).into_iter(),
                ),
            ),
            GetBlockHeaderSelector::Height(height) => (
                "get_block_header_by_height",
                RpcParams::map(Some(("height", height.into())).into_iter()),
            ),
        };

        Ok(self
            .inner
            .request::<Rsp>(request, params)
            .await?
            .block_header
            .into())
    }

    /// Similar to get_block_header_by_height above, but for a range of blocks. This method includes a starting block height and an ending block height as parameters to retrieve basic information about the range of blocks.
    pub async fn get_block_headers_range(
        &self,
        range: RangeInclusive<u64>,
    ) -> anyhow::Result<(Vec<BlockHeaderResponse>, bool)> {
        #[derive(Deserialize)]
        struct Rsp {
            headers: Vec<BlockHeaderResponseR>,
            untrusted: bool,
        }

        let params = empty()
            .chain(once(("start_height", (*range.start()).into())))
            .chain(once(("end_height", (*range.end()).into())));

        let Rsp { headers, untrusted } = self
            .inner
            .request::<MoneroResult<Rsp>>("get_block_headers_range", RpcParams::map(params))
            .await?
            .into_inner();

        Ok((headers.into_iter().map(From::from).collect(), untrusted))
    }

    /// Enable additional functions for regtest mode
    pub fn regtest(self) -> RegtestDaemonClient {
        RegtestDaemonClient(self)
    }
}

impl RegtestDaemonClient {
    /// Generate blocks and give mining rewards to specified address.
    pub async fn generate_blocks(
        &self,
        amount_of_blocks: u64,
        wallet_address: Address,
    ) -> anyhow::Result<u64> {
        #[derive(Deserialize)]
        struct Rsp {
            height: u64,
        }

        let params = empty()
            .chain(once((
                "amount_of_blocks",
                serde_json::to_value(amount_of_blocks).unwrap(),
            )))
            .chain(once((
                "wallet_address",
                serde_json::to_value(wallet_address).unwrap(),
            )));

        Ok(self
            .inner
            .request::<MoneroResult<Rsp>>("generateblocks", RpcParams::map(params))
            .await?
            .into_inner()
            .height)
    }
}

impl Serialize for TransferPriority {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(match self {
            TransferPriority::Default => 0,
            TransferPriority::Unimportant => 1,
            TransferPriority::Elevated => 2,
            TransferPriority::Priority => 3,
        })
    }
}

impl<'de> Deserialize<'de> for TransferPriority {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let v = u8::deserialize(deserializer)?;
        Ok(match v {
            0 => TransferPriority::Default,
            1 => TransferPriority::Unimportant,
            2 => TransferPriority::Elevated,
            3 => TransferPriority::Priority,
            other => {
                return Err(serde::de::Error::custom(format!(
                    "Invalid variant {}, expected 0-3",
                    other
                )))
            }
        })
    }
}

#[derive(Clone, Debug)]
pub struct WalletClient {
    inner: CallerWrapper,
}

impl WalletClient {
    /// Return the wallet's balance.
    pub async fn get_balance(
        &self,
        account: u64,
        addresses: Option<Vec<u64>>,
    ) -> anyhow::Result<BalanceData> {
        let params = empty()
            .chain(once(account.into()))
            .chain(addresses.map(Value::from));

        self.inner
            .request("get_balance", RpcParams::array(params))
            .await
    }

    /// Return the wallet's addresses for an account. Optionally filter for specific set of subaddresses.
    pub async fn get_address(
        &self,
        account: u64,
        addresses: Option<Vec<u64>>,
    ) -> anyhow::Result<AddressData> {
        let params = empty()
            .chain(once(("account_index", account.into())))
            .chain(addresses.map(|v| {
                (
                    "address_index",
                    v.into_iter().map(Value::from).collect::<Vec<_>>().into(),
                )
            }));

        self.inner
            .request("get_address", RpcParams::map(params))
            .await
    }

    /// Get account and address indexes from a specific (sub)address.
    pub async fn get_address_index(&self, address: Address) -> anyhow::Result<(u64, u64)> {
        #[derive(Deserialize)]
        struct Rsp {
            index: SubaddressIndex,
        }

        let params = once(("address", address.to_string().into()));

        let rsp = self
            .inner
            .request::<Rsp>("get_address_index", RpcParams::map(params))
            .await?;

        Ok((rsp.index.major, rsp.index.minor))
    }

    /// Create a new address for an account. Optionally, label the new address.
    pub async fn create_address(
        &self,
        account_index: u64,
        label: Option<String>,
    ) -> anyhow::Result<(Address, u64)> {
        #[derive(Deserialize)]
        struct Rsp {
            address: Address,
            address_index: u64,
        }

        let params = empty()
            .chain(once(("account_index", Value::Number(account_index.into()))))
            .chain(label.map(|v| ("label", Value::String(v))));

        let rsp = self
            .inner
            .request::<Rsp>("create_address", RpcParams::map(params))
            .await?;

        Ok((rsp.address, rsp.address_index))
    }

    /// Label an address.
    pub async fn label_address(
        &self,
        account_index: u64,
        address_index: u64,
        label: String,
    ) -> anyhow::Result<()> {
        let params = empty()
            .chain(once((
                "index",
                json!(SubaddressIndex {
                    major: account_index,
                    minor: address_index,
                }),
            )))
            .chain(once(("label", label.into())));

        self.inner
            .request::<IgnoredAny>("label_address", RpcParams::map(params))
            .await?;

        Ok(())
    }

    /// Get all accounts for a wallet. Optionally filter accounts by tag.
    pub async fn get_accounts(&self, tag: Option<String>) -> anyhow::Result<GetAccountsData> {
        let params = empty().chain(tag.map(|v| ("tag", v.into())));

        self.inner
            .request("get_accounts", RpcParams::map(params))
            .await
    }

    /// Get a list of incoming payments using a given payment id.
    pub async fn get_payments(&self, payment_id: PaymentId) -> anyhow::Result<Vec<Payment>> {
        let params = empty().chain(once((
            "payment_id",
            HashString(payment_id).to_string().into(),
        )));

        self.inner
            .request("get_payments", RpcParams::map(params))
            .await
    }

    /// Get a list of incoming payments using a given payment id, or a list of payments ids, from a given height.
    /// This method is the preferred method over `WalletClient::get_payments` because it has the same functionality but is more extendable.
    /// Either is fine for looking up transactions by a single payment ID.
    pub async fn get_bulk_payments(
        &self,
        payment_ids: Vec<PaymentId>,
        min_block_height: u64,
    ) -> anyhow::Result<Vec<Payment>> {
        #[derive(Deserialize)]
        struct Rsp {
            #[serde(default)]
            payments: Vec<Payment>,
        }

        let params = empty()
            .chain(once((
                "payment_ids",
                payment_ids
                    .into_iter()
                    .map(|s| HashString(s).to_string())
                    .collect::<Vec<_>>()
                    .into(),
            )))
            .chain(once(("min_block_height", min_block_height.into())));

        self.inner
            .request::<Rsp>("get_bulk_payments", RpcParams::map(params))
            .await
            .map(|rsp| rsp.payments)
    }

    /// Return the view private key.
    pub async fn query_view_key(&self) -> anyhow::Result<monero::PrivateKey> {
        #[derive(Deserialize)]
        struct Rsp {
            key: HashString<Vec<u8>>,
        }

        let params = empty().chain(once(("key_type", "view_key".into())));

        let rsp = self
            .inner
            .request::<Rsp>("query_key", RpcParams::map(params))
            .await?;

        Ok(monero::PrivateKey::from_slice(&rsp.key.0)?)
    }

    /// Returns the wallet's current block height.
    pub async fn get_height(&self) -> anyhow::Result<NonZeroU64> {
        #[derive(Deserialize)]
        struct Rsp {
            height: NonZeroU64,
        }

        Ok(self
            .inner
            .request::<Rsp>("get_height", RpcParams::None)
            .await?
            .height)
    }

    /// Send monero to a number of recipients.
    pub async fn transfer(
        &self,
        destinations: HashMap<Address, u64>,
        priority: TransferPriority,
        options: TransferOptions,
    ) -> anyhow::Result<TransferData> {
        let params = empty()
            .chain(once((
                "destinations",
                destinations
                    .into_iter()
                    .map(|(address, amount)| json!({"address": address, "amount": amount}))
                    .collect::<Vec<Value>>()
                    .into(),
            )))
            .chain(once(("priority", serde_json::to_value(priority)?)))
            .chain(options.account_index.map(|v| ("account_index", v.into())))
            .chain(options.subaddr_indices.map(|v| {
                (
                    "subaddr_indices",
                    v.into_iter().map(From::from).collect::<Vec<Value>>().into(),
                )
            }))
            .chain(options.mixin.map(|v| ("mixin", v.into())))
            .chain(options.ring_size.map(|v| ("ring_size", v.into())))
            .chain(options.unlock_time.map(|v| ("unlock_time", v.into())))
            .chain(
                options
                    .payment_id
                    .map(|v| ("payment_id", serde_json::to_value(HashString(v)).unwrap())),
            )
            .chain(options.do_not_relay.map(|v| ("do_not_relay", v.into())))
            .chain(once(("get_tx_key", true.into())))
            .chain(once(("get_tx_hex", true.into())))
            .chain(once(("get_tx_metadata", true.into())));

        self.inner.request("transfer", RpcParams::map(params)).await
    }

    /// Sign a transaction created on a read-only wallet (in cold-signing process).
    pub async fn sign_transfer(
        &self,
        unsigned_txset: Vec<u8>,
    ) -> anyhow::Result<SignedTransferOutput> {
        #[derive(Deserialize)]
        struct Rsp {
            signed_txset: HashString<Vec<u8>>,
            tx_hash_list: Vec<HashString<CryptoNoteHash>>,
            tx_raw_list: Vec<HashString<Vec<u8>>>,
        }

        impl From<Rsp> for SignedTransferOutput {
            fn from(value: Rsp) -> Self {
                Self {
                    signed_txset: value.signed_txset.0,
                    tx_hash_list: value.tx_hash_list.into_iter().map(|v| v.0).collect(),
                    tx_raw_list: value.tx_raw_list.into_iter().map(|v| v.0).collect(),
                }
            }
        }

        let params = empty()
            .chain(once((
                "unsigned_txset",
                serde_json::to_value(HashString(unsigned_txset)).unwrap(),
            )))
            .chain(once(("export_raw", true.into())));

        self.inner
            .request::<Rsp>("sign_transfer", RpcParams::map(params))
            .await
            .map(From::from)
    }

    /// Submit a previously signed transaction on a read-only wallet (in cold-signing process).
    pub async fn submit_transfer(
        &self,
        tx_data_hex: Vec<u8>,
    ) -> anyhow::Result<Vec<CryptoNoteHash>> {
        #[derive(Deserialize)]
        struct Rsp {
            tx_hash_list: Vec<HashString<CryptoNoteHash>>,
        }

        let params = empty().chain(once((
            "tx_data_hex",
            HashString(tx_data_hex).to_string().into(),
        )));

        self.inner
            .request::<Rsp>("submit_transfer", RpcParams::map(params))
            .await
            .map(|v| v.tx_hash_list.into_iter().map(|v| v.0).collect())
    }

    /// Returns a list of transfers.
    pub async fn get_transfers<T>(
        &self,
        selector: GetTransfersSelector<T>,
    ) -> anyhow::Result<HashMap<GetTransfersCategory, Vec<GotTransfer>>>
    where
        T: RangeBounds<u64> + Send,
    {
        let GetTransfersSelector {
            category_selector,
            filter_by_height,
            account_index,
            subaddr_indices,
        } = selector;

        let params = empty()
            .chain(
                category_selector
                    .into_iter()
                    .map(|(cat, b)| (cat.into(), b.into())),
            )
            .chain({
                filter_by_height
                    .map(|range| {
                        empty()
                            .chain(Some(("filter_by_height", true.into())))
                            .chain({
                                match range.start_bound() {
                                    Bound::Included(b) => Some(b - 1),
                                    Bound::Excluded(b) => Some(*b),
                                    Bound::Unbounded => None,
                                }
                                .map(|b| ("min_height", b.into()))
                            })
                            .chain({
                                match range.end_bound() {
                                    Bound::Included(b) => Some(*b),
                                    Bound::Excluded(b) => Some(b.checked_sub(1).unwrap_or(0)),
                                    Bound::Unbounded => None,
                                }
                                .map(|b| ("max_height", b.into()))
                            })
                    })
                    .into_iter()
                    .flatten()
            })
            .chain(account_index.map(|v| ("account_index", v.into())))
            .chain(subaddr_indices.map(|v| ("subaddr_indices", v.into())));

        self.inner
            .request("get_transfers", RpcParams::map(params))
            .await
    }

    pub async fn get_transfer(
        &self,
        txid: CryptoNoteHash,
        account_index: Option<u64>,
    ) -> anyhow::Result<Option<GotTransfer>> {
        #[derive(Deserialize)]
        struct Rsp {
            transfer: GotTransfer,
        }

        let params = empty()
            .chain(Some(("txid", HashString(txid).to_string().into())))
            .chain(account_index.map(|v| ("account_index", v.into())));

        let rsp = match self
            .inner
            .0
            .call("get_transfer_by_txid", RpcParams::map(params))
            .await?
        {
            Ok(v) => serde_json::from_value::<Rsp>(v)?,
            Err(e) => {
                if e.code == jsonrpc_core::ErrorCode::ServerError(-8) {
                    return Ok(None);
                } else {
                    return Err(e.into());
                }
            }
        };

        Ok(Some(rsp.transfer))
    }

    /// Export a signed set of key images.
    pub async fn export_key_images(&self) -> anyhow::Result<Vec<SignedKeyImage>> {
        #[derive(Deserialize)]
        struct R {
            key_image: HashString<Vec<u8>>,
            signature: HashString<Vec<u8>>,
        }

        #[derive(Deserialize)]
        struct Rsp {
            #[serde(default)]
            signed_key_images: Vec<R>,
        }

        impl From<Rsp> for Vec<SignedKeyImage> {
            fn from(rsp: Rsp) -> Self {
                rsp.signed_key_images
                    .into_iter()
                    .map(
                        |R {
                             key_image,
                             signature,
                         }| SignedKeyImage {
                            key_image: key_image.0,
                            signature: signature.0,
                        },
                    )
                    .collect()
            }
        }

        self.inner
            .request::<Rsp>("export_key_images", RpcParams::None)
            .await
            .map(From::from)
    }

    /// Import signed key images list and verify their spent status.
    pub async fn import_key_images(
        &self,
        signed_key_images: Vec<SignedKeyImage>,
    ) -> anyhow::Result<KeyImageImportResponse> {
        let params = empty().chain(once((
            "signed_key_images",
            signed_key_images
                .into_iter()
                .map(
                    |SignedKeyImage {
                         key_image,
                         signature,
                     }| {
                        json!({
                            "key_image": HashString(key_image),
                            "signature": HashString(signature),
                        })
                    },
                )
                .collect::<Vec<_>>()
                .into(),
        )));

        self.inner
            .request("import_key_images", RpcParams::map(params))
            .await
    }

    /// Check a tx_key is valid given a txid and receiver address.
    pub async fn check_tx_key(
        &self,
        txid: CryptoNoteHash,
        tx_key: CryptoNoteHash,
        address: Address,
    ) -> anyhow::Result<(NonZeroU64, bool, NonZeroU64)> {
        #[derive(Deserialize)]
        struct Rsp {
            confirmations: NonZeroU64,
            in_pool: bool,
            received: NonZeroU64,
        }

        let params = empty()
            .chain(once(("txid", HashString(txid).to_string().into())))
            .chain(once(("tx_key", HashString(tx_key).to_string().into())))
            .chain(once(("address", address.to_string().into())));

        let rsp = self
            .inner
            .request::<Rsp>("check_tx_key", RpcParams::map(params))
            .await?;

        Ok((rsp.confirmations, rsp.in_pool, rsp.received))
    }

    /// Get RPC version Major & Minor integer-format, where Major is the first 16 bits and Minor the last 16 bits.
    pub async fn get_version(&self) -> anyhow::Result<(u16, u16)> {
        #[derive(Deserialize)]
        struct Rsp {
            version: u32,
        }

        let version = self
            .inner
            .request::<Rsp>("get_version", RpcParams::None)
            .await?;

        let major = version.version >> 16;
        let minor = version.version - (major << 16);

        Ok((u16::try_from(major)?, u16::try_from(minor)?))
    }
}
