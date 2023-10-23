mod data_persist;
mod httpserver;

use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::Error;
use anyhow::Result;
use phactory::{gk, BaseBlockInfo, ChainStorage};
use phactory_api::blocks::BlockHeaderWithChanges;
use phala_mq::{MessageDispatcher, Path as MqPath, Sr25519Signer, Topic};
use phala_types::WorkerPublicKey;
use phaxt::rpc::ExtraRpcExt as _;
use pherry::types::{phaxt, subxt, BlockNumber, NumberOrHex, ParachainApi, StorageKey};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};

use crate::Args;

type RecordSender = mpsc::Sender<EventRecord>;

#[derive(Debug)]
struct EventRecord {
    sequence: i64,
    pubkey: WorkerPublicKey,
    block_number: BlockNumber,
    time_ms: u64,
    event: gk::EconomicEvent,
    v: gk::FixedPoint,
    p: gk::FixedPoint,
}

#[derive(Serialize, Deserialize)]
pub struct ReplayFactory {
    next_event_seq: i64,
    current_block: BlockNumber,
    storage: ChainStorage,
    #[serde(skip)]
    #[serde(default)]
    recv_mq: MessageDispatcher,
    gk: gk::ComputingEconomics<ReplayMsgChannel>,
    gk_launched: bool,
}

impl ReplayFactory {
    fn new(genesis_state: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        let mut recv_mq = MessageDispatcher::new();
        let mut storage = ChainStorage::default();
        storage.load(genesis_state.into_iter());
        let gk = gk::ComputingEconomics::new(&mut recv_mq, ReplayMsgChannel);
        Self {
            next_event_seq: 1,
            current_block: 0,
            storage,
            recv_mq,
            gk,
            gk_launched: false,
        }
    }

    async fn dispatch_block(
        &mut self,
        block: BlockHeaderWithChanges,
        event_tx: &Option<RecordSender>,
    ) -> Result<(), &'static str> {
        let (state_root, transaction) = self.storage.inner().calc_root_if_changes(
            &block.storage_changes.main_storage_changes,
            &block.storage_changes.child_storage_changes,
        );
        let header = &block.block_header;

        if header.state_root != state_root {
            return Err("State root mismatch");
        }

        self.storage
            .inner_mut()
            .apply_changes(state_root, transaction);
        self.handle_inbound_messages(header.number, event_tx)
            .await?;
        self.current_block = header.number;
        Ok(())
    }

    async fn handle_inbound_messages(
        &mut self,
        block_number: BlockNumber,
        event_tx: &Option<RecordSender>,
    ) -> Result<(), &'static str> {
        // Dispatch events
        let messages = self
            .storage
            .mq_messages()
            .or(Err("Can not get mq messages from storage"))?;

        let now_ms = self.storage.timestamp_now();

        let block = BaseBlockInfo {
            block_number,
            now_ms,
            storage: &self.storage,
            recv_mq: &mut self.recv_mq,
            send_mq: &mut Default::default(),
        };

        block.recv_mq.reset_local_index();

        let next_seq = &mut self.next_event_seq;
        let mut records = vec![];
        let mut event_handler = |event: gk::EconomicEvent, state: &gk::WorkerInfo| {
            log::debug!(target: "event", "event={event:?}, state={state:?}");
            let record = EventRecord {
                sequence: *next_seq as _,
                pubkey: *state.pubkey(),
                block_number,
                time_ms: now_ms,
                event,
                v: state.tokenomic_info().v,
                p: state.tokenomic_info().p_instant,
            };
            records.push(record);
            *next_seq += 1;
        };

        self.gk.will_process_block(&block);
        for message in messages {
            log::debug!(
                target: "event",
                "mq message: sender={}, dst={:?}, payload={}",
                message.sender,
                message.destination,
                crate::helper::try_decode_message(message.destination.path(), &message.payload)
            );
            if !self.gk_launched {
                if !crate::helper::is_gk_launch(&message) {
                    continue;
                }
                log::info!("GK launched");
                if let Some(params) = self.storage.tokenomic_parameters() {
                    self.gk.update_tokenomic_parameters(params);
                }
                self.gk_launched = true;
            }
            block.recv_mq.dispatch(message);
            self.gk.process_messages(&block, &mut event_handler);
        }
        if self.gk_launched {
            self.gk.did_process_block(&block, &mut event_handler);

            if let Some(tx) = event_tx.as_ref() {
                for record in records {
                    match tx.send(record).await {
                        Ok(()) => (),
                        Err(err) => {
                            log::error!("Can not send event to replay: {}", err);
                        }
                    }
                }
            }
        }

        let n_unhandled = self.recv_mq.clear();
        if n_unhandled > 0 {
            log::warn!("There are {} unhandled messages dropped", n_unhandled);
        }

        Ok(())
    }

    fn load(reader: impl Read) -> Self {
        let mut dispatcher = Default::default();
        let mut factory: Self =
            phala_mq::checkpoint_helper::using_dispatcher(&mut dispatcher, move || {
                serde_cbor::from_reader(reader).expect("Failed to load checkpoint")
            });
        factory.recv_mq = dispatcher;
        factory
    }

    fn dump(&self, writer: impl Write) {
        serde_cbor::to_writer(writer, self).expect("Failed to take checkpoint");
    }

    fn load_from_file(filename: &str) -> Self {
        let mut file = File::open(filename).expect("Failed to open checkpoint file");
        Self::load(&mut file)
    }

    fn dump_to_file(&self, filename: &str) {
        let mut file = File::create(filename).expect("Failed to create checkpoint file");
        self.dump(&mut file);
    }
}

#[derive(Serialize, Deserialize)]
struct ReplayMsgChannel;

impl phala_mq::traits::MessageChannel for ReplayMsgChannel {
    type Signer = Sr25519Signer;
    fn push_data(&self, data: Vec<u8>, to: impl Into<MqPath>) {
        let topic = Topic::new(to);
        log::debug!(
            target: "gk_egress",
            "gk egress: dst={:?}, payload={}",
            topic,
            crate::helper::try_decode_message(topic.path(), &data)
        );
    }
}

pub async fn fetch_genesis_storage(
    api: &ParachainApi,
    pos: BlockNumber,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let pos = subxt::rpc::types::BlockNumber::from(NumberOrHex::Number(pos.into()));
    let hash = api.rpc().block_hash(Some(pos)).await?;
    let response = api
        .extra_rpc()
        .storage_pairs(StorageKey(vec![]), hash)
        .await?;
    let storage = response.into_iter().map(|(k, v)| (k.0, v.0)).collect();
    Ok(storage)
}

async fn finalized_number(api: &ParachainApi) -> Result<BlockNumber> {
    let hash = api.rpc().finalized_head().await?;
    let header = api.rpc().header(Some(hash)).await?;
    Ok(header
        .ok_or_else(|| anyhow::anyhow!("Header not found"))?
        .number)
}

async fn wait_for_block(
    api: &ParachainApi,
    block: BlockNumber,
    assume_finalized: u32,
) -> Result<()> {
    loop {
        let finalized = finalized_number(api).await.unwrap_or(0);
        let state = api.extra_rpc().system_sync_state().await?;
        if block <= state.current_block as BlockNumber && block <= finalized.max(assume_finalized) {
            return Ok(());
        }
        log::info!(
            "Waiting for {} to be finalized. (finalized={}, assume_finalized={}, latest={})",
            block,
            finalized,
            assume_finalized,
            state.current_block
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

pub async fn replay(args: Args) -> Result<()> {
    let db_uri = args.persist_events_to;
    let bind_addr = args.bind_addr;
    let assume_finalized = args.assume_finalized;

    let mut api: ParachainApi = pherry::subxt_connect(&args.node_uri)
        .await
        .expect("Failed to connect to substrate");
    log::info!("Connected to substrate at: {}", args.node_uri);

    let genesis_state = fetch_genesis_storage(&api, args.start_at).await?;
    let event_tx = if !db_uri.is_empty() {
        let (event_tx, event_rx) = mpsc::channel(1024 * 5);
        let _db_task =
            tokio::spawn(async move { data_persist::run_persist(event_rx, &db_uri).await });
        Some(event_tx)
    } else {
        None
    };

    let factory = match get_checkpoint_path(&args.restore_from) {
        Some(filename) => {
            log::info!("Restoring from checkpoint: {}", filename);
            ReplayFactory::load_from_file(&filename)
        }
        None => ReplayFactory::new(genesis_state),
    };
    let mut last_checkpoint_block: BlockNumber = factory.current_block;
    let factory = Arc::new(Mutex::new(factory));

    let _http_task = std::thread::spawn({
        let factory = factory.clone();
        move || {
            let system = actix_rt::System::new();
            system.block_on(httpserver::serve(bind_addr, factory))
        }
    });

    let mut block_number = if last_checkpoint_block == 0 {
        args.start_at + 1
    } else {
        last_checkpoint_block + 1
    };

    let cache = args
        .cache_uri
        .as_ref()
        .map(|uri| pherry::headers_cache::Client::new(uri));

    loop {
        loop {
            if block_number >= args.stop_at.unwrap_or(std::u32::MAX) {
                log::info!("Replay finished");
                wait_forever().await;
            }
            if let Err(err) = wait_for_block(&api, block_number, assume_finalized).await {
                log::error!("{}", err);
                if restart_required(&err) {
                    break;
                }
            }
            log::info!("Fetching block {}", block_number);
            match pherry::fetch_storage_changes(&api, cache.as_ref(), block_number, block_number)
                .await
            {
                Ok(mut blocks) => {
                    let mut block = blocks.pop().expect("Expected one block");
                    let (header, _hash) = pherry::get_header_at(&api, Some(block_number)).await?;
                    block.block_header = header;
                    log::info!("Replaying block {}", block_number);
                    let mut factory = factory.lock().await;
                    factory
                        .dispatch_block(block, &event_tx)
                        .await
                        .expect("Block is valid");
                    if args.checkpoint_interval > 0
                        && block_number >= args.checkpoint_interval + last_checkpoint_block
                    {
                        let filename = format!("checkpoint.{block_number}");
                        log::info!("Taking checkpoint: {}", filename);
                        factory.dump_to_file(&filename);
                        let link = Path::new("checkpoint.latest");
                        if link.is_symlink() {
                            std::fs::remove_file(link)
                                .expect("Failed to remove the checkpoint symlink");
                        }
                        std::os::unix::fs::symlink(filename, link)
                            .expect("Failed to create symlink for latest checkpoint");
                        last_checkpoint_block = block_number;
                    }
                    block_number += 1;
                }
                Err(err) => {
                    log::error!("{}", err);
                    if restart_required(&err) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }

        api = loop {
            log::info!("Reconnecting to substrate");
            let api = match pherry::subxt_connect(&args.node_uri).await {
                Ok(client) => client,
                Err(err) => {
                    log::error!("Failed to connect to substrate: {}", err);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            break api;
        }
    }
}

async fn wait_forever() {
    loop {
        tokio::time::sleep(Duration::from_secs(1000)).await;
    }
}

fn restart_required(error: &Error) -> bool {
    format!("{error}").contains("restart required")
}

fn get_checkpoint_path(from: &Option<String>) -> Option<String> {
    match from {
        Some(filename) => {
            if !filename.is_empty() {
                Some(filename.clone())
            } else {
                None
            }
        }
        None => {
            let default = "checkpoint.latest";
            if std::path::PathBuf::from(default).exists() {
                Some(default.to_owned())
            } else {
                None
            }
        }
    }
}
