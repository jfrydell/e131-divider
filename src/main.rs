use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::Arc,
    time::Instant,
};

use gumdrop::Options;
use serde::Serialize;
use tokio::{net::UdpSocket, sync::Mutex};
use tracing::debug;

mod input;
mod output;
mod website;

#[tokio::main]
async fn main() {
    // Parse config
    let opts = Args::parse_args_default_or_exit();

    // Initialize tracing
    tracing_subscriber::fmt::init();
    // Initialize the state. Uses `Mutex` because both input thread and main thread need write access
    // (initially had interior mutability for different sources, but input thread can't actually process packets concurrently (each packet is processsed ~instantly with no awaiting))
    let state = Arc::new(Mutex::new(State::new(&opts)));

    // Start the webserver
    tokio::spawn(website::main(Arc::clone(&state)));
    // Listen for incoming E1.31 packets
    let e131_socket = UdpSocket::bind("0.0.0.0:5568").await.unwrap();
    tokio::spawn(input::handle_incoming(state.clone(), e131_socket));
    // Setup output
    let output_socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    output_socket.connect(opts.output_ip).await.unwrap();

    // Run the main loop
    let mut clock = tokio::time::interval(std::time::Duration::from_secs_f64(1.0 / opts.fps));
    loop {
        // Lock the state to start the frame (stops input thread)
        let mut state = state.lock().await;
        let frame_count = state.frame;
        let frame_start = Instant::now();

        // Run the frame and measure total time
        output::run_frame(&mut state, &output_socket).await;
        let frame_time = frame_start.elapsed();
        state.frame_time = frame_time.as_secs_f64();

        // Validate state (consists of `debug_assert`s)
        state.validate();

        // Drop the lock to allow input thread to continue. This marks the end of the main thread's actual processing
        drop(state);

        // Logging
        debug!(
            "Frame {frame_count} utilization: {}\t({}% utilization)",
            frame_time.as_secs_f64(),
            frame_time.as_secs_f64() * opts.fps * 100.0
        );

        // Wait for next frame
        clock.tick().await;
    }
}

#[derive(Options)]
struct Args {
    #[options(help = "Prints help information")]
    help: bool,
    #[options(
        help = "The IP address to output to",
        default = "127.0.0.1:5568",
        short = "o"
    )]
    output_ip: SocketAddr,
    #[options(help = "The number of output channels to use", required, short = "c")]
    output_channels: usize,
    #[options(
        help = "The size of an indivisible group of channels",
        default = "3",
        short = "g"
    )]
    group_size: usize,
    #[options(help = "The framerate to run at", default = "20", short = "f")]
    fps: f64,
    #[options(help = "The inactivity timeout in seconds", default = "2", short = "t")]
    timeout: f64,
    #[options(help = "The output timeout in seconds", default = "20", short = "T")]
    output_time: f64,
    #[options(help = "The maximum number of outputters", default = "2", short = "n")]
    outputter_capacity: usize,
}

/// The current state of all sources.
#[derive(Debug, Serialize, Clone)]
pub struct State {
    /// A map from IP address to source positions (either `Queue` or `Outputting`). Used for quick lookup on incoming packets.
    #[serde(skip)]
    sources: HashMap<SocketAddr, SourcePosition>,
    /// A queue of sources that are waiting to output.
    queue: VecDeque<QueuedSource>,
    /// A list of sources that are currently outputting, which preserves order (corresponds to positions in actual output).
    outputters: Vec<OutputtingSource>,
    /// The maximum number of outputters we allow.
    outputter_capacity: usize,
    /// The total number of channels we output.
    output_channel_count: usize,
    /// The number of channels per indivisible group for the purposes of dividing between outputters (3 for RGB)
    channel_group_size: usize,
    /// The current number of channels we take from each source. This is at most `output_channel_count / outputters.len()`. However, it may be less
    /// due to maintaining a multiple of 3 channels per outputter.
    input_channel_count: usize,
    /// The number of frames of inactivity before a source is removed.
    timeout: usize,
    /// The number of frames of output before an active source is removed.
    output_time: usize,
    /// The current frame counter.
    frame: usize,
    /// The time (in seconds) it took to run the last frame on the main thread. If this reaches 1/FPS, lock contention between the main thread and the input thread will occur.
    #[serde(skip)]
    frame_time: f64,
}
impl State {
    /// Creates a new `State` with the given configuration.
    fn new(opts: &Args) -> Self {
        let timeout = (opts.timeout * opts.fps).ceil() as usize;
        let output_time = (opts.output_time * opts.fps).ceil() as usize;
        assert!(opts.fps > 0.0, "FPS must be greater than 0");
        assert!(timeout > 0, "Timeout must be greater than 0");
        assert!(output_time > 0, "Output time must be greater than 0");
        Self {
            sources: HashMap::new(),
            queue: VecDeque::new(),
            outputters: Vec::new(),
            outputter_capacity: opts.outputter_capacity,
            output_channel_count: opts.output_channels,
            channel_group_size: opts.group_size,
            // We start with no outputters, so just cap to output channel count as placeholder
            input_channel_count: opts.output_channels,
            timeout,
            output_time,
            frame: 0,
            frame_time: 0.0,
        }
    }
    /// Validates that state invariants are upheld, panicking if not.
    fn validate(&self) {
        debug_assert!(
            self.outputters.len() <= self.outputter_capacity,
            "Outputters: {}, Capacity: {}",
            self.outputters.len(),
            self.outputter_capacity
        );
        debug_assert!(
            self.input_channel_count * self.outputters.len() <= self.output_channel_count,
            "Input channel count: {}, Output channel count: {}, Outputters: {}",
            self.input_channel_count,
            self.output_channel_count,
            self.outputters.len()
        );
        debug_assert!(
            self.queue.len() + self.outputters.len() == self.sources.len(),
            "Queue: {}, Outputters: {}, Sources: {}",
            self.queue.len(),
            self.outputters.len(),
            self.sources.len()
        );
        // Validate sources HashMap
        for (&addr, position) in &self.sources {
            match position {
                SourcePosition::Queue(i) => {
                    debug_assert!(self.queue[*i].common.addr == addr)
                }
                SourcePosition::Outputting(i) => {
                    debug_assert!(self.outputters[*i].common.addr == addr)
                }
            }
        }
    }
}

/// A source that is queued to output.
#[derive(Debug, Clone, Serialize)]
pub struct QueuedSource {
    /// The source's common info.
    common: SourceCommon,
}
impl QueuedSource {
    /// Creates a new `QueuedSource` given the source's name and IP address, along with the current state (for frame).
    pub fn new(name: String, addr: SocketAddr, current_state: &State) -> Self {
        Self {
            common: SourceCommon {
                name,
                addr,
                last_packet: current_state.frame,
            },
        }
    }
    /// Creates a new `QueuedSource` from the given `OutputtingSource` that is moving back to the queue.
    pub fn from(outputting: OutputtingSource) -> Self {
        Self {
            common: outputting.common,
        }
    }
}

/// A source that is currently outputting.
#[derive(Debug, Clone, Serialize)]
pub struct OutputtingSource {
    /// The source's common info.
    common: SourceCommon,
    /// The frame at which the source started outputting.
    start_frame: usize,
    /// The current output data. Guarenteed to be equal in length to `input_channel_count`.
    data: Vec<u8>,
}
impl OutputtingSource {
    /// Creates a new `OutputtingSource` from the given `QueuedSource` that is moving to outputting.
    /// The new source has it's outputting start frame set to the current frame and it's data set to 0's for all `input_channel_count` channels.
    pub fn from(queued: QueuedSource, current_state: &State) -> Self {
        Self {
            common: queued.common,
            start_frame: current_state.frame,
            data: vec![0; current_state.input_channel_count],
        }
    }
}

/// The info we keep track of for all sources.
#[derive(Debug, Clone, Serialize)]
pub struct SourceCommon {
    /// The source's IP address.
    addr: SocketAddr,
    /// The source's name, as sent in packets.
    name: String,
    /// The frame at which the last packet was received.
    last_packet: usize,
}

/// The position of a source, either in the queue or outputting.
#[derive(Debug, Clone)]
pub enum SourcePosition {
    Queue(usize),
    Outputting(usize),
}
