use std::{net::SocketAddr, sync::Arc};

use tokio::{net::UdpSocket, sync::RwLock};
use tracing::trace;

use crate::{SourcePosition, SourceState, State};

/// Handles all incoming packets.
pub async fn handle_incoming(state: Arc<RwLock<State>>, socket: UdpSocket) {
    loop {
        // Receive a packet from the socket
        let mut buf = Vec::with_capacity(639); // Max packet size is 638, and this is checked in `validate`, so clamping larger packets is fine (they will fail validation for either incorrect universe_size or universe_size > 512)
        let (_, source) = socket.recv_buf_from(&mut buf).await.unwrap();
        // Acquire a read lock on the state. Ideally, we'd pass this `&State` to `handle_input`, but we can't without using unsafe to make the guard live long enough (passing in the `Arc` as well to ensure it does). We still acquire the lock here, though, so we stop receiving packets (and spawning tasks) while we're running a frame.
        let _ = state.read().await;
        // Handle the packet in a separate task so we can continue receiving packets, processing them concurrently due to shared references (via `RwLock`)
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let state = state.read().await;
            handle_input(&state, &source, buf).await
        });
    }
}

/// Handles one input from a source.
async fn handle_input(state: &State, addr: &SocketAddr, data: Vec<u8>) {
    // Get packet data
    let Some((name, _, data)) = validate(&data) else {return;};
    trace!("Received packet from {addr} ({name})");
    // Lookup source
    match state.sources.get(addr) {
        Some(source_state) => {
            // The source exists, so update its data (last packet time and current output data if applicable)
            let mut source_state = source_state.lock().unwrap();
            source_state.last_packet = state.frame;
            match source_state.position {
                SourcePosition::Outputting {
                    ref mut current_data,
                    ..
                } => {
                    let to_copy = data.len().min(current_data.len());
                    current_data[..to_copy].copy_from_slice(&data[..to_copy]);
                }
                SourcePosition::Queue => {}
            }
        }
        None => {
            // The source doesn't exist, so add it to the list of newly connected sources
            state.new_sources.lock().unwrap().push((
                *addr,
                SourceState {
                    name: name.to_string(),
                    last_packet: state.frame,
                    position: SourcePosition::Queue,
                },
            ));
        }
    }
}

/// Validates an E1.31 data packet, returning the source name, universe, and data if valid.
pub fn validate(data: &Vec<u8>) -> Option<(&str, usize, &[u8])> {
    if data.len() < 126 {
        // Too small
        return None;
    }
    // # Root Layer
    // Validate first 16 bytes
    let first_16 = [
        0x00, 0x10, 0x00, 0x00, 0x41, 0x53, 0x43, 0x2D, 0x45, 0x31, 0x2E, 0x31, 0x37, 0x00, 0x00,
        0x00,
    ];
    if data[..16] != first_16 {
        // Wrong header
        return None;
    }

    // Validate next 2 bytes, which give PDU length (should be universe size + 110)
    let root_pdu_length = u16::from_be_bytes([data[16], data[17]]);
    if root_pdu_length & 0xf000 != 0x7000 {
        // Wrong protocol flgas
        return None;
    }
    let root_pdu_length = root_pdu_length & 0x0fff;
    if root_pdu_length as usize + 16 != data.len() {
        // Wrong size
        return None;
    }
    let universe_size = root_pdu_length as usize - 110;
    // Validate next 4 bytes, which should be VECTOR_ROOT_E131_DATA
    if data[18..22] != [0x00, 0x00, 0x00, 0x04] {
        // Wrong vector
        return None;
    }
    // We ignore sender CID (bytes 22..38)

    // # Frame Layer
    // Validate PDU length of frame layer (should be 88 + universe size)
    let frame_pdu_length = u16::from_be_bytes([data[38], data[39]]);
    if frame_pdu_length & 0xf000 != 0x7000 {
        // Wrong protocol flags
        return None;
    }
    let frame_pdu_length = frame_pdu_length & 0x0fff;
    if frame_pdu_length as usize != 88 + universe_size {
        // Wrong frame layer length
        return None;
    }
    // Validate next 4 bytes, which should be VECTOR_E131_DATA_PACKET
    if data[40..44] != [0x00, 0x00, 0x00, 0x02] {
        // Wrong vector
        return None;
    }
    // Get source name
    let source_name = &data[44..108];
    let source_name = std::str::from_utf8(source_name).ok()?;
    // Ignore priority (byte 108), synchronization address (bytes 109..111), sequence number (byte 111), and options (byte 112)
    // Get universe
    let universe = u16::from_be_bytes([data[113], data[114]]) as usize;

    // # DMP Layer
    // Validate PDU length of DMP layer (should be 11 + universe size)
    let dmp_pdu_length = u16::from_be_bytes([data[115], data[116]]);
    if dmp_pdu_length & 0xf000 != 0x7000 {
        // Wrong protocol flags
        return None;
    }
    let dmp_pdu_length = dmp_pdu_length & 0x0fff;
    if dmp_pdu_length as usize != 11 + universe_size {
        // Wrong DMP layer length
        return None;
    }
    // Validate next 6 bytes, which should be VECTOR_DMP_SET_PROPERTY (1 byte), address/data type (1 byte), first property address (2 bytes), and address increment (2 bytes)
    if data[117..123] != [0x02, 0xa1, 0x00, 0x00, 0x00, 0x01] {
        // Wrong vector
        return None;
    }
    // Validate property count (should be 1 + universe size)
    let property_count = u16::from_be_bytes([data[123], data[124]]) as usize;
    if property_count != 1 + universe_size {
        // Wrong property count
        return None;
    }

    // # Property Values
    // Validate DMX Start Code, which should be 0
    if data[125] != 0x00 {
        // Wrong DMX start code
        return None;
    }
    // Return data (guaranteed to be universe_size bytes due to earlier root_pdu_length size check)
    Some((source_name, universe, &data[126..]))
}
