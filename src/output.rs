use tokio::net::UdpSocket;
use tracing::trace;

use crate::{OutputtingSource, QueuedSource, SourcePosition, State};

/// Performs one frame of the main loop.
///
/// This includes:
/// - Locking the state `RwLock` so we can update the queues / read stable data
/// - Sending the current frame's data to the physical controller
/// - Updating the queue and current outputters, including adding newcomers to the queue
pub async fn run_frame(state: &mut State, output: &UdpSocket) {
    // Assemble the data for this frame (done before queue update because data isn't stored for queued controllers)
    let mut data = [0u8; 512];
    for (i, source) in state.outputters.iter().enumerate() {
        data[i * state.input_channel_count..(i + 1) * state.input_channel_count]
            .copy_from_slice(&source.data);
    }
    // Output the data
    send_e131(&output, &data, 1, state.frame as u8)
        .await
        .unwrap();

    // Remove any sources from the queue that have disconnected (last packet was more than `state.timeout` ago). Can't use `retain` because we need to remove from `state.sources` as well.
    let mut i = 0;
    while i < state.queue.len() {
        let source = &state.queue[i].common;
        if source.last_packet + state.timeout < state.frame {
            state.sources.remove(&source.addr);
            state.queue.remove(i);
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
    // Find indices of any outputters that must be removed due to disconnection
    let mut timed_out_outputters = vec![];
    for (i, source) in state.outputters.iter().enumerate() {
        if source.common.last_packet + state.timeout < state.frame {
            timed_out_outputters.push(i);
        }
    }

    // Find indices of any outputters that have reached the maximum output time
    let mut done_outputters = vec![];
    for (i, source) in state.outputters.iter().enumerate() {
        if source.start_frame + state.output_time <= state.frame {
            done_outputters.push(i);
        }
    }

    // TODO: We should actually not do this if we have room to grow outputters instead. Maybe can just do all growth first?

    // Remove timed out outputters that can be replaced with queued sources
    for i in timed_out_outputters.drain(..state.queue.len().min(timed_out_outputters.len())) {
        // Remove the timed out outputter from the lookup table
        state.sources.remove(&state.outputters[i].common.addr);
        // Replace the timed out outputter with the queued source, updating its position in the lookup table
        let new_source = state.queue.pop_front().unwrap();
        state
            .sources
            .insert(new_source.common.addr, SourcePosition::Outputting(i));
        state.outputters[i] = OutputtingSource::from(new_source, state);
    }
    if timed_out_outputters.len() > 0 {
        // The queue is empty, but there are still timed out outputters. Remove them in inverse order (so timed out indices don't change)
        timed_out_outputters.sort();
        for i in timed_out_outputters.into_iter().rev() {
            state.sources.remove(&state.outputters[i].common.addr);
            state.outputters.remove(i);
        }
        // Recreate lookup table to account for index changes
        for (i, source) in state.outputters.iter().enumerate() {
            state
                .sources
                .insert(source.common.addr, SourcePosition::Outputting(i));
        }
        outputter_count_changed = true;
    } else {
        // The queue isn't empty after removing timed out outputters. We have two other things to try and do:
        // 1. We can expand the number of outputters
        // 2. We can remove outputters that have reached the maximum output time

        // If possible, expand outputters count
        if state.outputters.len() < state.outputter_capacity {
            let to_expand = state.outputter_capacity - state.outputters.len();
            for source in state.queue.drain(..to_expand.min(state.queue.len())) {
                state.sources.insert(
                    source.common.addr,
                    SourcePosition::Outputting(state.outputters.len()),
                );
                // We manually construct OutputtingSource to avoid borrowing state; also we don't need to create data since we know it will be overwritten b/c input channel count change
                state.outputters.push(OutputtingSource {
                    common: source.common,
                    start_frame: state.frame,
                    data: vec![], // Will be overwritten
                });
            }
            outputter_count_changed = true;
        }
        // For any remaining in the queue, we must remove outputters that have reached the maximum output time
        for i in done_outputters.drain(..state.queue.len().min(done_outputters.len())) {
            // Get the replacement source from the queue and update the lookup table
            let new_source = state.queue.pop_front().unwrap();
            state
                .sources
                .insert(new_source.common.addr, SourcePosition::Outputting(i));
            // Swap out the old source for the new one (between table lookups to avoid borrowing issues on addr (could refactor and store addr on stack for more logical order))
            let old_source = std::mem::replace(
                &mut state.outputters[i],
                OutputtingSource {
                    common: new_source.common,
                    start_frame: state.frame,
                    data: vec![0; state.input_channel_count],
                },
            );
            // Update lookup table and add old source to queue
            state.sources.insert(
                old_source.common.addr,
                SourcePosition::Queue(state.queue.len()),
            );
            state.queue.push_back(QueuedSource::from(old_source));
        }
    }

    // If the number of outputters has changed, we must update the `input_channel_count` and output data
    if outputter_count_changed {
        // Calculate channel count aligned to group size
        state.input_channel_count = if state.outputters.len() == 0 {
            state.output_channel_count
        } else {
            let naive_count = state.output_channel_count / state.outputters.len();
            naive_count - naive_count % state.channel_group_size
        };

        // Update output data arrays
        for source in state.outputters.iter_mut() {
            source.data.resize(state.input_channel_count, 0);
        }
    }

    trace!("Outputting Sources:");
    for source in state.outputters.iter() {
        trace!("\t{} ({})", source.common.addr, source.common.name,)
    }

    // Increment frame counter
    state.frame += 1;
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
