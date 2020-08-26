//! IPFS repo
use crate::error::Error;
use crate::p2p::KadResult;
use crate::path::IpfsPath;
use crate::subscription::{RequestKind, SubscriptionFuture, SubscriptionRegistry};
use crate::IpfsOptions;
use async_trait::async_trait;
use bitswap::Block;
use cid::{self, Cid};
use core::convert::TryFrom;
use core::fmt::Debug;
use futures::channel::{
    mpsc::{channel, Receiver, Sender},
    oneshot,
};
use futures::sink::SinkExt;
use libp2p::core::PeerId;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

pub mod fs;
pub mod mem;

pub trait RepoTypes: Send + Sync + 'static {
    type TBlockStore: BlockStore;
    type TDataStore: DataStore;
}

#[derive(Clone, Debug)]
pub struct RepoOptions {
    path: PathBuf,
}

impl From<&IpfsOptions> for RepoOptions {
    fn from(options: &IpfsOptions) -> Self {
        RepoOptions {
            path: options.ipfs_path.clone(),
        }
    }
}

pub fn create_repo<TRepoTypes: RepoTypes>(
    options: RepoOptions,
) -> (Repo<TRepoTypes>, Receiver<RepoEvent>) {
    Repo::new(options)
}

/// A wrapper for `Cid` that has a `Multihash`-based equality check
#[derive(Debug)]
pub struct RepoCid(Cid);

impl PartialEq for RepoCid {
    fn eq(&self, other: &Self) -> bool {
        self.0.hash() == other.0.hash()
    }
}
impl Eq for RepoCid {}

impl Hash for RepoCid {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash().hash(state)
    }
}

/// Describes the outcome of `BlockStore::put_block`
#[derive(Debug, PartialEq, Eq)]
pub enum BlockPut {
    /// A new block was written
    NewBlock,
    /// The block existed already
    Existed,
}

#[derive(Debug)]
pub enum BlockRm {
    Removed(Cid),
    // TODO: DownloadCancelled(Cid, Duration),
}

// pub struct BlockNotFound(Cid);

#[derive(Debug)]
pub enum BlockRmError {
    // TODO: Pinned(Cid),
    NotFound(Cid),
}

/// This API is being discussed and evolved, which will likely lead to breakage.
#[async_trait]
pub trait BlockStore: Debug + Send + Sync + Unpin + 'static {
    fn new(path: PathBuf) -> Self;
    async fn init(&self) -> Result<(), Error>;
    async fn open(&self) -> Result<(), Error>;
    async fn contains(&self, cid: &Cid) -> Result<bool, Error>;
    async fn get(&self, cid: &Cid) -> Result<Option<Block>, Error>;
    async fn put(&self, block: Block) -> Result<(Cid, BlockPut), Error>;
    async fn remove(&self, cid: &Cid) -> Result<Result<BlockRm, BlockRmError>, Error>;
    async fn list(&self) -> Result<Vec<Cid>, Error>;
    async fn wipe(&self);
}

#[async_trait]
pub trait DataStore: Debug + Send + Sync + Unpin + 'static {
    fn new(path: PathBuf) -> Self;
    async fn init(&self) -> Result<(), Error>;
    async fn open(&self) -> Result<(), Error>;
    async fn contains(&self, col: Column, key: &[u8]) -> Result<bool, Error>;
    async fn get(&self, col: Column, key: &[u8]) -> Result<Option<Vec<u8>>, Error>;
    async fn put(&self, col: Column, key: &[u8], value: &[u8]) -> Result<(), Error>;
    async fn remove(&self, col: Column, key: &[u8]) -> Result<(), Error>;
    async fn wipe(&self);
}

#[derive(Clone, Copy, Debug)]
pub enum Column {
    Ipns,
    Pin,
}

#[derive(Debug)]
pub struct Repo<TRepoTypes: RepoTypes> {
    block_store: TRepoTypes::TBlockStore,
    data_store: TRepoTypes::TDataStore,
    events: Sender<RepoEvent>,
    pub(crate) subscriptions: SubscriptionRegistry<Block, String>,
}

#[derive(Debug)]
pub enum RepoEvent {
    WantBlock(Cid),
    UnwantBlock(Cid),
    ProvideBlock(
        Cid,
        oneshot::Sender<Result<SubscriptionFuture<KadResult, String>, anyhow::Error>>,
    ),
    UnprovideBlock(Cid),
}

impl TryFrom<RequestKind> for RepoEvent {
    type Error = &'static str;

    fn try_from(req: RequestKind) -> Result<Self, Self::Error> {
        if let RequestKind::GetBlock(cid) = req {
            Ok(RepoEvent::UnwantBlock(cid))
        } else {
            Err("logic error: RepoEvent can only be created from a Request::GetBlock")
        }
    }
}

impl<TRepoTypes: RepoTypes> Repo<TRepoTypes> {
    pub fn new(options: RepoOptions) -> (Self, Receiver<RepoEvent>) {
        let mut blockstore_path = options.path.clone();
        let mut datastore_path = options.path;
        blockstore_path.push("blockstore");
        datastore_path.push("datastore");
        let block_store = TRepoTypes::TBlockStore::new(blockstore_path);
        let data_store = TRepoTypes::TDataStore::new(datastore_path);
        let (sender, receiver) = channel(1);
        (
            Repo {
                block_store,
                data_store,
                events: sender,
                subscriptions: Default::default(),
            },
            receiver,
        )
    }

    /// Shutdowns the repo, cancelling any pending subscriptions; Likely going away after some
    /// refactoring, see notes on [`Ipfs::exit_daemon`].
    pub fn shutdown(&self) {
        self.subscriptions.shutdown();
    }

    pub async fn init(&self) -> Result<(), Error> {
        let f1 = self.block_store.init();
        let f2 = self.data_store.init();
        let (r1, r2) = futures::future::join(f1, f2).await;
        if r1.is_err() {
            r1
        } else {
            r2
        }
    }

    pub async fn open(&self) -> Result<(), Error> {
        let f1 = self.block_store.open();
        let f2 = self.data_store.open();
        let (r1, r2) = futures::future::join(f1, f2).await;
        if r1.is_err() {
            r1
        } else {
            r2
        }
    }

    /// Puts a block into the block store.
    pub async fn put_block(&self, block: Block) -> Result<(Cid, BlockPut), Error> {
        let cid = block.cid.clone();
        let (_cid, res) = self.block_store.put(block.clone()).await?;
        self.subscriptions
            .finish_subscription(cid.clone().into(), Ok(block));

        // FIXME: this doesn't cause actual DHT providing yet, only some
        // bitswap housekeeping; RepoEvent::ProvideBlock should probably
        // be renamed to ::NewBlock and we might want to not ignore the
        // channel errors when we actually start providing on the DHT
        if let BlockPut::NewBlock = res {
            // sending only fails if no one is listening anymore
            // and that is okay with us.
            let (tx, rx) = oneshot::channel();

            self.events
                .clone()
                .send(RepoEvent::ProvideBlock(cid.clone(), tx))
                .await
                .ok();

            if let Ok(Ok(kad_subscription)) = rx.await {
                kad_subscription.await?;
            }
        }

        Ok((cid, res))
    }

    /// Retrives a block from the block store, or starts fetching it from the network and awaits
    /// until it has been fetched.
    pub async fn get_block(&self, cid: &Cid) -> Result<Block, Error> {
        if let Some(block) = self.block_store.get(&cid).await? {
            Ok(block)
        } else {
            let subscription = self
                .subscriptions
                .create_subscription(cid.clone().into(), Some(self.events.clone()));
            // sending only fails if no one is listening anymore
            // and that is okay with us.
            self.events
                .clone()
                .send(RepoEvent::WantBlock(cid.clone()))
                .await
                .ok();
            Ok(subscription.await?)
        }
    }

    /// Retrives a block from the block store if it's available locally.
    pub async fn get_block_now(&self, cid: &Cid) -> Result<Option<Block>, Error> {
        Ok(self.block_store.get(&cid).await?)
    }

    pub async fn list_blocks(&self) -> Result<Vec<Cid>, Error> {
        Ok(self.block_store.list().await?)
    }

    /// Remove block from the block store.
    pub async fn remove_block(&self, cid: &Cid) -> Result<Cid, Error> {
        if self.is_pinned(&cid).await? {
            return Err(anyhow::anyhow!("block to remove is pinned"));
        }

        // sending only fails if the background task has exited
        self.events
            .clone()
            .send(RepoEvent::UnprovideBlock(cid.clone()))
            .await
            .ok();

        // FIXME: Need to change location of pinning logic.
        // I like this pattern of the repo abstraction being some sort of
        // "clearing house" for the underlying result enums, but this
        // could potentially be pushed out out of here up to Ipfs, idk
        match self.block_store.remove(&cid).await? {
            Ok(success) => match success {
                BlockRm::Removed(_cid) => Ok(cid.clone()),
            },
            Err(err) => match err {
                BlockRmError::NotFound(_cid) => Err(anyhow::anyhow!("block not found")),
            },
        }
    }

    /// Get an ipld path from the datastore.
    pub async fn get_ipns(&self, ipns: &PeerId) -> Result<Option<IpfsPath>, Error> {
        use std::str::FromStr;

        let data_store = &self.data_store;
        let key = ipns.to_owned();
        let bytes = data_store.get(Column::Ipns, key.as_bytes()).await?;
        match bytes {
            Some(ref bytes) => {
                let string = String::from_utf8_lossy(bytes);
                let path = IpfsPath::from_str(&string)?;
                Ok(Some(path))
            }
            None => Ok(None),
        }
    }

    /// Put an ipld path into the datastore.
    pub async fn put_ipns(&self, ipns: &PeerId, path: &IpfsPath) -> Result<(), Error> {
        let string = path.to_string();
        let value = string.as_bytes();
        self.data_store
            .put(Column::Ipns, ipns.as_bytes(), value)
            .await
    }

    /// Remove an ipld path from the datastore.
    pub async fn remove_ipns(&self, ipns: &PeerId) -> Result<(), Error> {
        self.data_store.remove(Column::Ipns, ipns.as_bytes()).await
    }

    pub async fn pin_block(&self, cid: &Cid) -> Result<(), Error> {
        let pin_value = self.data_store.get(Column::Pin, &cid.to_bytes()).await?;

        match pin_value {
            Some(pin_count) => {
                if pin_count[0] == std::u8::MAX {
                    return Err(anyhow::anyhow!("Block cannot be pinned more times"));
                }
                self.data_store
                    .put(Column::Pin, &cid.to_bytes(), &[pin_count[0] + 1])
                    .await
            }
            None => {
                self.data_store
                    .put(Column::Pin, &cid.to_bytes(), &[1])
                    .await
            }
        }
    }

    pub async fn is_pinned(&self, cid: &Cid) -> Result<bool, Error> {
        self.data_store.contains(Column::Pin, &cid.to_bytes()).await
    }

    pub async fn unpin_block(&self, cid: &Cid) -> Result<(), Error> {
        let pin_value = self.data_store.get(Column::Pin, &cid.to_bytes()).await?;

        match pin_value {
            Some(pin_count) if pin_count[0] == 1 => {
                self.data_store.remove(Column::Pin, &cid.to_bytes()).await
            }
            Some(pin_count) => {
                self.data_store
                    .put(Column::Pin, &cid.to_bytes(), &[pin_count[0] - 1])
                    .await
            }
            // This is a no-op
            None => Ok(()),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::env::temp_dir;

    #[derive(Clone)]
    pub struct Types;

    impl RepoTypes for Types {
        type TBlockStore = mem::MemBlockStore;
        type TDataStore = mem::MemDataStore;
    }

    pub fn create_mock_repo() -> (Repo<Types>, Receiver<RepoEvent>) {
        let mut tmp = temp_dir();
        tmp.push("rust-ipfs-repo");
        let options: RepoOptions = RepoOptions { path: tmp };
        Repo::new(options)
    }

    #[tokio::test(max_threads = 1)]
    async fn test_repo() {
        let (repo, _) = create_mock_repo();
        repo.init().await.unwrap();
    }
}
