use alloc::boxed::Box;
use core::sync::atomic::{AtomicU32, Ordering};
use uefi::boot::{self, SearchType};
use uefi::proto::network::snp::{NetworkState, ReceiveFlags, SimpleNetwork};
use uefi::{Handle, Identify, Status, guid};

#[path = "network_packet.rs"]
mod packet;
#[path = "network_session.rs"]
mod session;
pub use packet::DhcpLease;
use session::Session;
static TRANSACTION: AtomicU32 = AtomicU32::new(0x534f5344);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Up,
    Down,
    Unknown,
}

#[derive(Clone, Copy, Debug)]
pub struct Adapter {
    pub mac: Option<[u8; 6]>,
    pub link: Link,
    pub status: Option<Status>,
}

pub struct NetworkInventory {
    pub adapters: [Option<Adapter>; 8],
    pub adapter_count: usize,
    pub wifi2_count: usize,
    pub status: Option<Status>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeResult {
    Unsupported,
    NoLink,
    Timeout,
    Lease(DhcpLease),
    FirmwareError(Status),
}

fn mac(network: &SimpleNetwork) -> Option<[u8; 6]> {
    if network.mode().hw_address_size != 6 {
        return None;
    }
    network.mode().current_address.0[..6].try_into().ok()
}

fn link(network: &SimpleNetwork) -> Link {
    if !bool::from(network.mode().media_present_supported) {
        return Link::Unknown;
    }
    if bool::from(network.mode().media_present) { Link::Up } else { Link::Down }
}

fn with_network<T>(handle: Handle, operation: impl FnOnce(&SimpleNetwork) -> T) -> Result<T, Status> {
    let result = match boot::open_protocol_exclusive::<SimpleNetwork>(handle) {
        Ok(network) => {
            let result = operation(&network);
            drop(network);
            Ok(result)
        }
        Err(error) => Err(error.status()),
    };
    boot::connect_controller(handle, None, None, true).map_err(|error| error.status())?;
    result
}

pub fn inventory() -> NetworkInventory {
    let mut report = NetworkInventory { adapters: [None; 8], adapter_count: 0, wifi2_count: 0, status: None };
    let wifi = guid!("1b0fb9bf-699d-4fdd-a7c3-2546681bf63b");
    match boot::locate_handle_buffer(SearchType::ByProtocol(&wifi)) {
        Ok(handles) => report.wifi2_count = handles.len(),
        Err(error) if error.status() == Status::NOT_FOUND => {}
        Err(error) => report.status = Some(error.status()),
    }
    match boot::locate_handle_buffer(SearchType::ByProtocol(&SimpleNetwork::GUID)) {
        Ok(handles) => {
            for handle in handles.iter().take(report.adapters.len()) {
                match with_network(*handle, |network| {
                    let status = if network.mode().state == NetworkState::INITIALIZED {
                        network.get_interrupt_status().err().map(|e| e.status())
                    } else {
                        Some(Status::NOT_STARTED)
                    };
                    Adapter {
                        mac: mac(network),
                        link: if status.is_none() { link(network) } else { Link::Unknown },
                        status,
                    }
                }) {
                    Ok(adapter) => {
                        report.adapters[report.adapter_count] = Some(adapter);
                        report.adapter_count += 1;
                    }
                    Err(status) => report.status = Some(status),
                }
            }
        }
        Err(error) if error.status() == Status::NOT_FOUND => {}
        Err(error) => report.status = Some(error.status()),
    }
    report
}

pub fn probe_dhcp() -> ProbeResult {
    let handles = match boot::locate_handle_buffer(SearchType::ByProtocol(&SimpleNetwork::GUID)) {
        Ok(handles) => handles,
        Err(error) if error.status() == Status::NOT_FOUND => return ProbeResult::Unsupported,
        Err(error) => return ProbeResult::FirmwareError(error.status()),
    };
    let Some(handle) = handles.first() else {
        return ProbeResult::Unsupported;
    };
    match with_network(*handle, probe_adapter) {
        Ok(result) => result,
        Err(status) => ProbeResult::FirmwareError(status),
    }
}

fn probe_adapter(network: &SimpleNetwork) -> ProbeResult {
    let initial = network.mode().state;
    if initial != NetworkState::STOPPED
        && initial != NetworkState::STARTED
        && initial != NetworkState::INITIALIZED
    {
        return ProbeResult::Unsupported;
    }
    if network.mode().if_type != 1 || network.mode().media_header_size != 14 {
        return ProbeResult::Unsupported;
    }
    let mut session = Session {
        network,
        started: false,
        initialized: false,
        tx: [None, None],
        pending: [false; 2],
        added_filters: ReceiveFlags::empty(),
    };
    let result = run_probe(&mut session, initial);
    match session.restore() {
        Ok(()) => result,
        Err(status) => ProbeResult::FirmwareError(status),
    }
}

fn run_probe(session: &mut Session<'_>, initial: NetworkState) -> ProbeResult {
    let network = session.network;
    if initial == NetworkState::STOPPED {
        if let Err(error) = network.start() {
            return ProbeResult::FirmwareError(error.status());
        }
        session.started = true;
    }
    if initial != NetworkState::INITIALIZED {
        if let Err(error) = network.initialize(0, 0) {
            return ProbeResult::FirmwareError(error.status());
        }
        session.initialized = true;
    }
    if let Err(error) = network.get_interrupt_status() {
        return ProbeResult::FirmwareError(error.status());
    }
    if link(network) == Link::Down {
        return ProbeResult::NoLink;
    }
    let Some(mac) = mac(network) else {
        return ProbeResult::Unsupported;
    };
    let added = (ReceiveFlags::UNICAST | ReceiveFlags::BROADCAST)
        & !ReceiveFlags::from_bits_retain(network.mode().receive_filter_setting);
    if let Err(error) = network.receive_filters(added, ReceiveFlags::empty(), false, None) {
        return ProbeResult::FirmwareError(error.status());
    }
    session.added_filters = added;
    let xid =
        u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]) ^ TRANSACTION.fetch_add(1, Ordering::Relaxed);
    session.tx[0] = Some(Box::new(packet::request(mac, xid, None)));
    let mut offered = None;
    let mut sent = [false; 2];
    let mut rx = [0_u8; 1536];
    for _ in 0..400 {
        let phase = usize::from(offered.is_some());
        if !sent[phase]
            && let Some(buffer) = session.tx[phase].as_ref()
        {
            match network.transmit(0, buffer.as_slice(), None, None, None) {
                Ok(()) => {
                    sent[phase] = true;
                    session.pending[phase] = true;
                }
                Err(error) if error.status() == Status::NOT_READY => {}
                Err(error) => return ProbeResult::FirmwareError(error.status()),
            }
        }
        if let Err(error) = network.get_interrupt_status() {
            return ProbeResult::FirmwareError(error.status());
        }
        match network.get_recycled_transmit_buffer_status() {
            Ok(Some(recycled)) => {
                for index in 0..2 {
                    if session.tx[index]
                        .as_ref()
                        .is_some_and(|buffer| core::ptr::NonNull::from(&**buffer).cast::<u8>() == recycled)
                    {
                        session.pending[index] = false;
                    }
                }
            }
            Ok(None) => {}
            Err(error) => return ProbeResult::FirmwareError(error.status()),
        }
        if link(network) == Link::Down {
            return ProbeResult::NoLink;
        }
        match network.receive(&mut rx, None, None, None, None) {
            Ok(length) if length <= rx.len() => {
                if sent[phase]
                    && let Some(reply) =
                        packet::parse_reply(&rx[..length], xid, mac, if phase == 0 { 2 } else { 5 })
                {
                    match offered {
                        None => {
                            offered = Some(reply);
                            session.tx[1] = Some(Box::new(packet::request(mac, xid, Some(reply))));
                        }
                        Some(offer) if reply.address == offer.address && reply.server == offer.server => {
                            return ProbeResult::Lease(reply);
                        }
                        Some(_) => {}
                    }
                }
            }
            Ok(_) => return ProbeResult::FirmwareError(Status::BAD_BUFFER_SIZE),
            Err(error)
                if error.status() == Status::NOT_READY || error.status() == Status::BUFFER_TOO_SMALL => {}
            Err(error) => return ProbeResult::FirmwareError(error.status()),
        }
        boot::stall(10_000);
    }
    ProbeResult::Timeout
}
