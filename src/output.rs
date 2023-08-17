use std::{net::SocketAddr, sync::Mutex};

use tokio::net::UdpSocket;
use tracing::trace;

use crate::{SourcePosition, State};

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
