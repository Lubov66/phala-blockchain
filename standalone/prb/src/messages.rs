use crate::bus::Bus;
use crate::datasource::DataSourceManager;
use crate::tx::TxManager;
use crate::use_parachain_api;
use anyhow::Result;
use futures::StreamExt;
use log::{debug, error, info, trace, warn};
use phala_types::messaging::{MessageOrigin, SignedMessage};
use std::collections::{hash_map::Entry::{Occupied, Vacant}, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const TX_TIMEOUT_IN_BLOCKS: u32 = 6;

pub enum MessagesEvent {
    SyncMessages((String, u64, MessageOrigin, Vec<SignedMessage>)),
    DoSyncMessages((String, u64, MessageOrigin, Vec<SignedMessage>, Option<u64>)),
    Completed((String, MessageOrigin, u64, Result<()>)),
    RemoveSender(MessageOrigin),
    CurrentHeight(u32),
}

pub type MessagesRx = mpsc::UnboundedReceiver<MessagesEvent>;
pub type MessagesTx = mpsc::UnboundedSender<MessagesEvent>;

pub enum MessageState {
    Pending,
    Successful,
    Failure,
    Timeout,
}

pub struct MessageContext {
    sender: MessageOrigin,
    sequence: u64,
    state: MessageState,
    submitted_at: u32,
    prev_try_count: usize,
}

impl MessageContext {
    pub fn is_pending(&self, current_height: u32) -> bool {
        if matches!(self.state, MessageState::Timeout) {
            if current_height <= self.submitted_at {
                trace!("[{} #{}] Message was marked as timeout, but current H#{} <= {}, still treated as pending",
                    self.sender,
                    self.sequence,
                    current_height,
                    self.submitted_at,
                );
                return true;
            } else if current_height.saturating_sub(self.submitted_at) <= TX_TIMEOUT_IN_BLOCKS {
                trace!("[{} #{}] Message was marked as timeout, but current H#{} - {} <= {}, wait a little more time to allow potential success.",
                    self.sender,
                    self.sequence,
                    current_height,
                    self.submitted_at,
                    TX_TIMEOUT_IN_BLOCKS,
                );
                return true;
            } else {
                trace!("[{} #{}] Message was timeout because H#{} - {} > {}",
                    self.sender,
                    self.sequence,
                    current_height,
                    self.submitted_at,
                    TX_TIMEOUT_IN_BLOCKS,
                );
                return false;
            }
        } else if matches!(self.state, MessageState::Pending) {
            if current_height > self.submitted_at && current_height.saturating_sub(self.submitted_at) > TX_TIMEOUT_IN_BLOCKS {
                trace!("[{} #{}] Message is still pending, but H#{} - {} > {}, treat as timeout.",
                    self.sender,
                    self.sequence,
                    current_height,
                    self.submitted_at,
                    TX_TIMEOUT_IN_BLOCKS,
                );
                return false;
            } else {
                return true;
            }
        }
        false
    }

    pub fn is_pending_or_success(&self, current_height: u32) -> bool {
        self.is_pending(current_height) || matches!(self.state, MessageState::Successful)
    }

    pub fn is_timeout_or_failure(&self, current_height: u32) -> bool {
        !self.is_pending_or_success(current_height)
    }
}

pub struct SenderContext {
    sender: MessageOrigin,
    node_next_sequence: u64,
    pending_messages: HashMap<u64, MessageContext>,
}

impl SenderContext {
    pub fn calculate_next_sequence(&self, current_height: u32) -> u64 {
        let mut next_sequence = self.node_next_sequence;
        while
            self.pending_messages.get(&next_sequence)
                .map(|p_msg| p_msg.is_pending_or_success(current_height))
                .unwrap_or(false)
        {
            next_sequence += 1;
        }
        next_sequence
    }
}

pub async fn master_loop(
    mut rx: MessagesRx,
    bus: Arc<Bus>,
    dsm: Arc<DataSourceManager>,
    txm: Arc<TxManager>,
) -> Result<()> {
    let mut sender_contexts = HashMap::<MessageOrigin, SenderContext>::new();

    tokio::spawn(background_update_current_height(bus.clone(), dsm.clone()));
    tokio::time::sleep(Duration::from_secs(5)).await;

    let mut current_height: u32 = 0;
    loop {
        let messages_event = rx.recv().await;
        let event = messages_event;
        if event.is_none() {
            break
        }

        let event = event.unwrap();
        match event {
            MessagesEvent::SyncMessages((worker_id, pool_id, sender, messages)) => {
                trace!("[{}] Received {} messages, start filtering.", sender, messages.len());

                let messages = match sender_contexts.entry(sender.clone()) {
                    Occupied(entry) => {
                        let sender_context = entry.get();
                        messages
                            .into_iter()
                            .filter(|message| {
                                sender_context.pending_messages
                                    .get(&message.sequence)
                                    .map(|p_msg| p_msg.is_timeout_or_failure(current_height))
                                    .unwrap_or(true)
                            })
                            .collect::<Vec<_>>()
                    },
                    Vacant(_) => messages,
                };
                if messages.is_empty() {
                    trace!("[{}] all messages are pending or completed", sender);
                    continue;
                }

                trace!("[{}] {} messages needs will send for check.", sender, messages.len());
                tokio::spawn(do_update_next_sequence_and_sync_messages(
                    bus.clone(),
                    dsm.clone(),
                    worker_id,
                    pool_id,
                    sender,
                    messages
                ));
            },

            MessagesEvent::DoSyncMessages((worker_id, pool_id, sender, messages, next_sequence)) => {
                trace!("[{}] DoSync: Receveid {} messages.", sender, messages.len());

                let sender_context = match sender_contexts.entry(sender.clone()) {
                    Occupied(entry) => entry.into_mut(),
                    Vacant(entry) => match next_sequence {
                        Some(next_sequence) => {
                            entry.insert(SenderContext {
                                sender: sender.clone(),
                                node_next_sequence: next_sequence,
                                pending_messages: HashMap::new(),
                            })
                        },
                        None => {
                            error!("[{}] no last node sequence received for new sender.", sender);
                            continue;
                        },
                    },
                };

                if let Some(next_sequence) = next_sequence {
                    sender_context.node_next_sequence = next_sequence;
                }

                for message in messages {
                    let next_sequence = sender_context.calculate_next_sequence(current_height);
                    if message.sequence != next_sequence {
                        debug!("[{}] Ignoring #{} message since not matching next_sequence {}.",
                            sender, message.sequence, next_sequence);
                        continue;
                    }

                    match sender_context.pending_messages.entry(message.sequence) {
                        Occupied(entry) => {
                            trace!("[{}] Msg#{} has message_context, checking if retry needed.", sender, message.sequence);

                            let message_context = entry.into_mut();
                            if message_context.is_pending_or_success(current_height) {
                                trace!("[{}] message #{} is pending or successful.", sender, message.sequence);
                                continue;
                            }

                            debug!("[{}] Msg#{} needs to retry.", sender, message.sequence);

                            if matches!(message_context.state, MessageState::Pending) {
                                warn!("[{}] message #{} is pending, but {} is more than {} blocks, need retry.",
                                    sender,
                                    message.sequence,
                                    current_height.saturating_sub(message_context.submitted_at),
                                    TX_TIMEOUT_IN_BLOCKS,
                                );
                            }

                            message_context.state = MessageState::Pending;
                            message_context.submitted_at = current_height;
                            message_context.prev_try_count += 1;
                            info!(
                                "[{}] message #{} was failed for {} times. Trying again now..",
                                sender, message.sequence, message_context.prev_try_count
                            );
                        },
                        Vacant(entry) => {
                            debug!("[{}] Msg#{} is new.", sender, message.sequence);

                            entry.insert(MessageContext {
                                sender: sender.clone(),
                                sequence: message.sequence,
                                state: MessageState::Pending,
                                submitted_at: current_height,
                                prev_try_count: 0,
                            });
                        }
                    }

                    debug!("[{}] Sending #{} message", sender, message.sequence);
                    tokio::spawn(do_sync_message(
                        bus.clone(),
                        txm.clone(),
                        worker_id.clone(),
                        pool_id,
                        sender.clone(),
                        message
                    ));
                }
            },

            MessagesEvent::Completed((worker_id, sender, sequence, result)) => {
                let sender_context = match sender_contexts.get_mut(&sender) {
                    Some(ctx) => ctx,
                    None => {
                        error!("[{}] sender does not found", sender);
                        continue;
                    },
                };
                match sender_context.pending_messages.get_mut(&sequence) {
                    Some(ctx) => {
                        ctx.state = match &result {
                            Ok(_) => MessageState::Successful,
                            Err(err) => {
                                let err_str = err.to_string();
                                if err_str.contains("Tx timed out!") {
                                    MessageState::Timeout
                                } else {
                                    MessageState::Failure
                                }
                            },
                        };
                    },
                    None => {
                        error!("[{}] sequence {} does not found, cannot remove", sender, sequence);
                        continue;
                    },
                };
                if let Err(err) = result {
                    error!("[{}] sync offchain message completed with error. {}", sender, err);
                    let _ = bus.send_worker_update_message(
                        worker_id,
                        format!("Sync offchain message met error, will retry. {}", err)
                    );
                }
            },

            MessagesEvent::RemoveSender(sender) => {
                match sender_contexts.remove(&sender) {
                    Some(_) => {
                        trace!("[{}] Removed from SenderContext", sender);
                    },
                    None => {
                        trace!("[{}] Does not exist in SenderContext", sender);
                    },
                }
            },

            MessagesEvent::CurrentHeight(height) => {
                current_height = height;
                trace!("Updated Current Para Height #{}", current_height);
            },
        }
    }

    Ok(())
}

async fn do_update_next_sequence_and_sync_messages(
    bus: Arc<Bus>,
    dsm: Arc<DataSourceManager>,
    worker_id: String,
    pool_id: u64,
    sender: MessageOrigin,
    messages: Vec<SignedMessage>,
) {
    let next_sequence = match use_parachain_api!(dsm, false) {
        Some(para_api) => {
            match pherry::chain_client::mq_next_sequence(&para_api, &sender).await {
                Ok(next_sequence) => Some(next_sequence),
                Err(err) => {
                    warn!("[{}] met error, will use last node sequence: {}", sender, err);
                    None
                },
            }
        },
        None => None,
    };
    let _ = bus.send_messages_event(MessagesEvent::DoSyncMessages((
        worker_id,
        pool_id,
        sender,
        messages,
        next_sequence,
    )));
}

async fn do_sync_message(
    bus: Arc<Bus>,
    txm: Arc<TxManager>,
    worker_id: String,
    pool_id: u64,
    sender: MessageOrigin,
    message: SignedMessage,
) {
    let sequence = message.sequence;
    let result = txm.sync_offchain_message(pool_id, message).await;
    let _ = bus.send_messages_event(
        MessagesEvent::Completed((worker_id, sender.clone(), sequence, result))
    );
}

pub async fn background_update_current_height(
    bus: Arc<Bus>,
    dsm: Arc<DataSourceManager>,
) -> Result<()> {
    loop {
        let para_api = match use_parachain_api!(dsm, false) {
            Some(instance) => instance,
            None => {
                error!("No valid data source, wait 1 seconds");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            },
        };

        let blocks_sub = para_api.blocks().subscribe_best().await;
        let mut blocks_sub = match blocks_sub {
            Ok(blocks_sub) => blocks_sub,
            Err(e) => {
                error!("Subscribe best blocks failed, wait 1 seconds. {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            },
        };

        while let Some(block) = blocks_sub.next().await {
            let block = match block {
                Ok(block) => block,
                Err(e) => {
                    error!("Got error for next block. {e}");
                    continue;
                },
            };

            let _ = bus.send_messages_event(MessagesEvent::CurrentHeight(block.number()));
        }
    }
}