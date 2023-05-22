use core::fmt::Debug;
use parity_scale_codec::Decode;
use phala_mq::{AccountId, BindTopic, Message};
use phala_pallets::{
    pallet_phat::{ClusterRegistryEvent, ContractRegistryEvent},
    pallet_registry::{GatekeeperRegistryEvent, RegistryEvent},
};
use phala_types::{
    contract::messaging::{ClusterEvent, ClusterOperation, ContractOperation, WorkerClusterReport},
    messaging::{
        GatekeeperChange, GatekeeperEvent, GatekeeperLaunch, KeyDistribution, SystemEvent,
        WorkingInfoUpdateEvent, WorkingReportEvent,
    },
};

pub(crate) fn try_decode<T: Debug + Decode + BindTopic>(topic: &[u8], mut payload: &[u8]) -> Option<T> {
    if T::topic() != topic {
        return None;
    }
    T::decode(&mut payload).ok()
}

fn try_decode_to_str<T: Debug + Decode + BindTopic>(topic: &[u8], payload: &[u8]) -> Option<String> {
    let decoded = try_decode::<T>(topic, payload)?;
    Some(format!("{decoded:?}"))
}

pub(crate) fn try_decode_message(topic: &[u8], payload: &[u8]) -> String {
    macro_rules! try_decode {
        ($($t:ty),*) => {
            $(
                if let Some(decoded) = try_decode_to_str::<$t>(topic, payload) {
                    return decoded;
                }
            )*
        }
    }
    type CodeHash = AccountId;
    type BlockNumber = u32;
    try_decode!(ClusterEvent);
    try_decode!(ContractOperation<CodeHash, AccountId>);
    try_decode!(WorkerClusterReport);
    try_decode!(ClusterOperation<AccountId>);
    try_decode!(SystemEvent);
    try_decode!(WorkingInfoUpdateEvent<BlockNumber>);
    try_decode!(WorkingReportEvent);
    try_decode!(GatekeeperLaunch);
    try_decode!(GatekeeperChange);
    try_decode!(KeyDistribution<BlockNumber>);
    try_decode!(GatekeeperEvent);
    try_decode!(ClusterRegistryEvent);
    try_decode!(ContractRegistryEvent);
    try_decode!(RegistryEvent);
    try_decode!(GatekeeperRegistryEvent);

    format!("{}", hex_fmt::HexFmt(payload))
}

pub(crate) fn is_gk_launch(msg: &Message) -> bool {
    if !msg.sender.is_pallet() {
        return false;
    }
    if msg.destination.path() != &GatekeeperLaunch::topic() {
        return false;
    }
    let mut data = &msg.payload[..];
    match GatekeeperLaunch::decode(&mut data) {
        Ok(event) => matches!(event, GatekeeperLaunch::MasterPubkeyOnChain(_)),
        Err(_) => false,
    }
}
