use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use tokio::{net::UdpSocket, sync::RwLock};
use tracing::{debug, trace};

const FPS: usize = 20;

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt::init();
    // Initialize the state
    let state = Arc::new(RwLock::new(State::new(5, 100, FPS, 10 * FPS)));
    // Listen for incoming E1.31 packets
    let e131_socket = UdpSocket::bind("0.0.0.0:5568").await.unwrap();
    tokio::spawn(handle_incoming(state.clone(), e131_socket));
    // Run the main loop
    let output_socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    output_socket.connect("127.0.0.1:558").await.unwrap();
    loop {
        debug!("Frame {}", state.read().await.frame);
        let frame_end = tokio::time::sleep(std::time::Duration::from_secs_f64(1.0 / FPS as f64));
        let mut state = state.write().await;
        run_frame(&mut state, &output_socket).await;
        drop(state);
        frame_end.await;
    }
}

/// Performs one frame of the main loop.
///
/// This includes:
/// - Locking the state `RwLock` so we can update the queues / read stable data
/// - Sending the current frame's data to the physical controller
/// - Updating the queue and current outputters, including adding newcomers to the queue
pub async fn run_frame(state: &mut State, output: &UdpSocket) {
    // Assemble the data for this frame (done before queue update because data isn't stored for queued controllers)
    let mut data = [0u8; 512];
    for (i, outputter) in state.outputters.iter().enumerate() {
        let source = state.sources.get_mut(outputter).unwrap().get_mut().unwrap();
        let source_data = match &source.position {
            SourcePosition::Outputting { current_data, .. } => current_data,
            SourcePosition::Queue => unreachable!(),
        };
        data[i * state.input_channel_count..(i + 1) * state.input_channel_count]
            .copy_from_slice(&source_data);
    }
    // Output the data
    send_e131(&output, &data, 1, state.frame as u8)
        .await
        .unwrap();

    // Add any new sources to the queue. If a source is already in the `sources` map, it must be a duplicate (as it would have been added on a previous iteration), so it is ignored.
    for (addr, source_state) in state.new_sources.get_mut().unwrap().drain(..) {
        // Add the source to the `HashMap`, and to the queue if it isn't a duplicate
        if !state
            .sources
            .insert(addr, Mutex::new(source_state))
            .is_some()
        {
            state.queue.push_back(addr);
        }
    }
    trace!("Added new sources. Queue size: {}", state.queue.len());

    // Remove any sources that have disconnected (last packet was more than `state.timeout` ago)
    let mut i = 0;
    while i < state.queue.len() {
        let addr = state.queue[i];
        let source = state.source(&addr);
        if source.last_packet + state.timeout < state.frame {
            state.queue.remove(i);
            state.sources.remove(&addr);
        } else {
            i += 1;
        }
    }
    trace!(
        "Removed disconnected sources. Queue size: {}",
        state.queue.len()
    );

    // Updating outputters is a bit complex for a few reasons. Firstly, we want the positions in `state.outputters` to be stable as long as
    // it doesn't shrink, so we must swap out removed outputters with queued sources. Secondly, any timed out outputters must be removed, even
    // if no sources are queued. Finally, if we are not at outputter capacity, we can add sources from the queue to the outputters. Any changes
    // to the number of outputters must be followed by an update to the `input_channel_count`, as well as updating all output data.
    // Flag for number of outputters changing:
    let mut outputter_count_changed = false;
    #[derive(PartialEq, PartialOrd, Eq, Ord)]
    struct Outputter {
        addr: SocketAddr,
        position: usize,
    }
    // Find any outputters that must be removed due to disconnection
    let mut timed_out_outputters = vec![];
    for (position, &addr) in state.outputters.iter().enumerate() {
        let source = state.sources.get_mut(&addr).unwrap().get_mut().unwrap();
        if source.last_packet + state.timeout < state.frame {
            timed_out_outputters.push(Outputter { addr, position });
        }
    }

    // Find any outputters that have reached the maximum output time
    let mut done_outputters = vec![];
    for (position, &addr) in state.outputters.iter().enumerate() {
        let source_state = &state.sources.get_mut(&addr).unwrap().get_mut().unwrap();
        let start_frame = match source_state.position {
            SourcePosition::Outputting { start_frame, .. } => start_frame,
            SourcePosition::Queue => unreachable!(),
        };
        if start_frame + state.output_time <= state.frame {
            done_outputters.push(Outputter { addr, position });
        }
    }

    // Remove timed out outputters that can be replaced with queued sources
    for i in timed_out_outputters.drain(..state.queue.len().min(timed_out_outputters.len())) {
        let new_addr = state.queue.pop_front().unwrap();
        state.outputters[i.position] = new_addr;
        state.source(&new_addr).position = SourcePosition::Outputting {
            start_frame: state.frame,
            current_data: vec![0; state.input_channel_count],
        };
        state.sources.remove(&i.addr);
    }
    if timed_out_outputters.len() > 0 {
        // The queue is empty, but there are still timed out outputters. Remove them in inverse order (so the indices don't change)
        timed_out_outputters.sort_by_key(|i| i.position);
        for i in timed_out_outputters.into_iter().rev() {
            state.outputters.remove(i.position);
            state.sources.remove(&i.addr);
        }
        outputter_count_changed = true;
    } else {
        // The queue isn't empty, so we must remove outputters that have reached the maximum output time
        for i in done_outputters.drain(..state.queue.len().min(done_outputters.len())) {
            let new_addr = state.queue.pop_front().unwrap();
            state.outputters[i.position] = new_addr;
            state.source(&new_addr).position = SourcePosition::Outputting {
                start_frame: state.frame,
                current_data: vec![0; state.input_channel_count],
            };
            state.queue.push_back(i.addr);
            state.source(&i.addr).position = SourcePosition::Queue;
        }
        // If there are sources queued, we can try and expand the number of outputters to accommodate them
        if !state.queue.is_empty() && state.outputters.len() < state.outputter_capacity {
            for addr in state
                .queue
                .drain(..state.outputter_capacity.min(state.queue.len()))
            {
                state.outputters.push(addr);
                state
                    .sources
                    .get_mut(&addr)
                    .unwrap()
                    .get_mut()
                    .unwrap()
                    .position = SourcePosition::Outputting {
                    start_frame: state.frame,
                    // We don't need to initialize `current_data` here, as it will be overwritten due to the change in outputter count affecting `input_channel_count`.
                    current_data: vec![],
                };
            }
            outputter_count_changed = true;
        }
    }

    // If the number of outputters has changed, we must update the `input_channel_count` and output data
    if outputter_count_changed {
        state.input_channel_count =
            calculate_input_channel_count(state.output_channel_count, state.outputters.len());
        for addr in &state.outputters {
            match state
                .sources
                .get_mut(&addr)
                .unwrap()
                .get_mut()
                .unwrap()
                .position
            {
                SourcePosition::Outputting {
                    ref mut current_data,
                    ..
                } => {
                    current_data.resize(state.input_channel_count, 0);
                }
                SourcePosition::Queue => unreachable!(),
            }
        }
    }

    trace!("Outputting Sources:");
    for addr in state.outputters.iter() {
        let source = state.sources.get_mut(&addr).unwrap().get_mut().unwrap();
        trace!("\t{addr} ({}): {:?}", source.name, source.position)
    }

    // Increment frame counter
    state.frame += 1;
}

/// Calculates how many input channels are required per source to support the given number of output channels and outputters.
fn calculate_input_channel_count(output_channels: usize, output_count: usize) -> usize {
    if output_count == 0 {
        return 0;
    }
    let naive_count = output_channels / output_count;
    naive_count - naive_count % 3
}

/// Handles all incoming packets.
async fn handle_incoming(state: Arc<RwLock<State>>, socket: UdpSocket) {
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

/// Sends an E1.31 packet on the given socket.
async fn send_e131(
    socket: &UdpSocket,
    data: &[u8; 512],
    universe: u16,
    seq_number: u8,
) -> std::io::Result<usize> {
    let packet_header = [
        0, 16, 0, 0, 65, 83, 67, 45, 69, 49, 46, 49, 55, 0, 0, 0, 114, 110, 0, 0, 0, 4, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 114, 88, 0, 0, 0, 2, 69, 49, 46, 51, 49, 32, 68, 105,
        118, 105, 100, 101, 114, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 1, 114, 11, 2, 161, 0, 0, 0, 1, 2, 1, 0,
    ];
    let mut packet = [0u8; 638];
    packet[..126].copy_from_slice(&packet_header);
    packet[109..111].copy_from_slice(&universe.to_be_bytes());
    packet[111] = seq_number;
    packet[126..].copy_from_slice(data);
    socket.send(&packet).await
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

/// The current state of all sources.
#[derive(Debug)]
pub struct State {
    /// A map from IP address to source state. The source state is wrapped in a `Mutex` so that
    /// everyone can share access to the `State` while processing packets.
    sources: HashMap<SocketAddr, Mutex<SourceState>>,
    /// A queue of sources that are waiting to output.
    queue: VecDeque<SocketAddr>,
    /// A list of sources that are currently outputting, which preserves order (corresponds to positions in actual output).
    outputters: Vec<SocketAddr>,
    /// A list of new sources that have been added since the last frame. Unlike the `sources` map,
    /// this whole thing is wrapped in a `Mutex` to allow for new sources to be added with shared
    /// access to state.
    new_sources: Mutex<Vec<(SocketAddr, SourceState)>>,
    /// The maximum number of outputters we allow.
    outputter_capacity: usize,
    /// The total number of channels we output.
    output_channel_count: usize,
    /// The current number of channels we take from each source. This is at most `output_channel_count / outputters.len()`. However, it may be less
    /// due to maintaining a multiple of 3 channels per outputter.
    input_channel_count: usize,
    /// The number of frames of inactivity before a source is removed.
    timeout: usize,
    /// The number of frames of output before an active source is removed.
    output_time: usize,
    /// The current frame counter.
    frame: usize,
}
impl State {
    /// Creates a new `State` with the given configuration.
    pub fn new(
        outputter_capacity: usize,
        output_channel_count: usize,
        timeout: usize,
        output_time: usize,
    ) -> Self {
        Self {
            sources: HashMap::new(),
            queue: VecDeque::new(),
            outputters: Vec::new(),
            new_sources: Mutex::new(Vec::new()),
            outputter_capacity,
            output_channel_count,
            // We start with 0 input channels, as we have no outputters yet.
            input_channel_count: 0,
            timeout,
            output_time,
            frame: 0,
        }
    }
    /// Convenience function for getting a `SourceState` by `SocketAddr` with mutable access, which doesn't require locking.
    /// Panics if the source doesn't exist.
    ///
    /// (Because it takes `&mut self`, this can't be used while borrowing an unrelated field, unfortunately.)
    fn source(&mut self, addr: &SocketAddr) -> &mut SourceState {
        self.sources.get_mut(addr).unwrap().get_mut().unwrap()
    }
}

/// The current state of any given source.
#[derive(Debug, Clone)]
pub struct SourceState {
    /// The source's name, as sent in packets.
    name: String,
    /// The frame at which the last packet was received.
    last_packet: usize,
    /// The position of the source (either in queue or outputting).
    position: SourcePosition,
}

/// The position of a source, either in the queue or outputting.
#[derive(Debug, Clone)]
pub enum SourcePosition {
    Queue,
    Outputting {
        /// The frame at which the source started outputting.
        start_frame: usize,
        /// The current data being output. The length of this is guaranteed to be equal to `input_channel_count`.
        current_data: Vec<u8>,
    },
}
