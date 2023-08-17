use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Instant,
};

use tokio::{net::UdpSocket, sync::RwLock};
use tracing::debug;

mod input;
mod output;
mod website;

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt::init();
    // Set fps
    let fps = 20;
    // Initialize the state
    let state = Arc::new(RwLock::new(State::new(5, 100, 2 * fps, 10 * fps)));
    let site_state = Arc::new(RwLock::new(website::SiteState::new()));
    // Start the webserver
    tokio::spawn(website::main(Arc::clone(&site_state)));
    // Listen for incoming E1.31 packets
    let e131_socket = UdpSocket::bind("0.0.0.0:5568").await.unwrap();
    tokio::spawn(input::handle_incoming(state.clone(), e131_socket));
    // Setup output
    let output_socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    output_socket.connect("192.168.168.200:5568").await.unwrap();

    // Run the main loop
    let fps = fps as f64;
    let mut clock = tokio::time::interval(std::time::Duration::from_secs_f64(1.0 / fps));
    loop {
        debug!("Frame {}", state.read().await.frame);
        let frame_start = Instant::now();

        // Acquire state lock to run frame and generate new site state
        let mut state = state.write().await;
        output::run_frame(&mut state, &output_socket).await;
        let new_site_state = website::SiteState::update(&mut state);
        drop(state);

        // Measure frame time
        let frame_time = frame_start.elapsed();
        debug!(
            "Frame utilization: {}\t({}% utilization)",
            frame_time.as_secs_f64(),
            frame_time.as_secs_f64() * fps * 100.0
        );

        // Update site state (done outside state lock to avoid unnecessary block for processing input)
        *site_state.write().await = new_site_state;

        // Wait for next frame
        clock.tick().await;
    }
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
