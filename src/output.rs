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
    let mut data = vec![0u8; state.output_channel_count];
    for (i, source) in state.outputters.iter().enumerate() {
        data[i * state.input_channel_count..(i + 1) * state.input_channel_count]
            .copy_from_slice(&source.data);
    }
    // Output the data
    for universe in 0..((state.output_channel_count + 509) / 510) {
        let mut this_data = [0u8; 512];
        let to_copy = (state.output_channel_count - universe * 510).min(510);
        this_data[..to_copy].copy_from_slice(&data[universe * 510..][..to_copy]);
        send_e131(&output, &this_data, universe as u16 + 1, state.frame as u8)
            .await
            .unwrap();
    }

    // Remove any sources from the queue that have disconnected (last packet was more than `state.timeout` ago). Can't use `retain` because we need to remove from `state.sources` as well.
    let mut i = 0;
    while i < state.queue.len() {
        let source = &state.queue[i];
        if source.last_packet + state.timeout < state.frame {
            state.sources.remove(&source.id);
            state.queue.remove(i);
        } else {
            i += 1;
        }
    }

    // Updating outputters is a bit complex for a few reasons. Firstly, we want the positions in `state.outputters` to be stable as long as
    // it doesn't shrink, so we must swap out removed outputters with queued sources. Secondly, any timed out outputters must be removed, even
    // if no sources are queued. Finally, if we are not at outputter capacity, we can add sources from the queue to the outputters. Any changes
    // to the number of outputters must be followed by an update to the `input_channel_count`, as well as updating all output data.
    // Flag for number of outputters changing:
    let mut outputter_count_changed = false;
    // Find indices of any outputters that must be removed due to disconnection
    let mut timed_out_outputters = vec![];
    for (i, source) in state.outputters.iter().enumerate() {
        if source.last_packet + state.timeout < state.frame {
            timed_out_outputters.push(i);
        }
    }

    // Remove timed out outputters that can be replaced with queued sources
    for i in timed_out_outputters.drain(..state.queue.len().min(timed_out_outputters.len())) {
        // Remove the timed out outputter from the lookup table
        state.sources.remove(&state.outputters[i].id);
        // Replace the timed out outputter with the queued source, updating its position in the lookup table
        let new_source = state.queue.pop_front().unwrap();
        state
            .sources
            .insert(new_source.id.clone(), SourcePosition::Outputting(i));
        state.outputters[i] = OutputtingSource::from(new_source, state);
    }
    // Now, either the queue or the list of timed out outputters is empty. If the queue is empty, we must remove all timed out outputters. Otherwise, we must attempt to add queued sources to outputters, possibly replacing finished outputters.
    if timed_out_outputters.len() > 0 {
        // The queue is empty, but there are still timed out outputters. Remove them in inverse order (so indices in timed_out_outputters don't change)
        timed_out_outputters.sort();
        for i in timed_out_outputters.into_iter().rev() {
            state.sources.remove(&state.outputters[i].id);
            state.outputters.remove(i);
        }
        // Recreate lookup table to account for index changes to remaining outputters
        for (i, source) in state.outputters.iter().enumerate() {
            state
                .sources
                .insert(source.id.clone(), SourcePosition::Outputting(i));
        }
        outputter_count_changed = true;
    } else if state.queue.len() > 0 {
        // The queue isn't empty after replacing all timed out outputters. We have two other things to try and do:
        // 1. We can expand the number of outputters
        // 2. We can remove outputters that have reached the maximum output time

        // If possible, expand outputters count, noting that we did so with `outputter_count_changed` flag
        if state.outputters.len() < state.outputter_capacity {
            let to_expand = state.outputter_capacity - state.outputters.len();
            for source in state.queue.drain(..to_expand.min(state.queue.len())) {
                state.sources.insert(
                    source.id.clone(),
                    SourcePosition::Outputting(state.outputters.len()),
                );
                // We manually construct OutputtingSource to avoid borrowing state; also we don't need to create data since we know it will be overwritten b/c input channel count change
                state.outputters.push(OutputtingSource {
                    id: source.id,
                    last_packet: source.last_packet,
                    start_frame: state.frame,
                    data: vec![], // Will be overwritten
                });
            }
            outputter_count_changed = true;
        }
        // For any remaining in the queue, we must remove outputters that have reached the maximum output time.
        if state.queue.len() > 0 {
            // Find indices of any outputters that have reached the maximum output time, along with what frame they started (for replacement order)
            let mut done_outputters = vec![];
            for (i, source) in state.outputters.iter().enumerate() {
                if source.start_frame + state.output_time <= state.frame {
                    done_outputters.push((i, source.start_frame));
                }
            }
            // Sort outputters to replace in correct order.
            done_outputters.sort_by_key(|&(_, start_frame)| start_frame);
            // Replace done outputters with queued sources
            for (i, _) in done_outputters.drain(..state.queue.len().min(done_outputters.len())) {
                // Get the replacement source from the queue and update the lookup table
                let new_source = state.queue.pop_front().unwrap();
                state
                    .sources
                    .insert(new_source.id.clone(), SourcePosition::Outputting(i));
                // Swap out the old source for the new one (between table lookups to avoid borrowing issues on addr (could refactor and store addr on stack for more logical order))
                let new_source = OutputtingSource::from(new_source, state);
                let old_source = std::mem::replace(&mut state.outputters[i], new_source);
                // Update lookup table and add old source to queue
                state.sources.insert(
                    old_source.id.clone(),
                    SourcePosition::Queue(state.queue.len()),
                );
                state.queue.push_back(QueuedSource::from(old_source));
            }
            // Whenever there are more done outputters than queued sources, the outputters we just removed will be first in the queue on the next frame. This means that the oldest outputter will replace a newer
            // done_outputter on the next frame, becoming the last-expiring outputter. To prevent this, we reset remaining `done_outputters` to half max output time, ensuring that they can't be replaced by the
            // one we just removed for a little bit. This is very much a hack, and has downsides if other outputters are close to expiring (they will then get replaced by the one we just kicked out; this only
            // can occur with >2 output slots though), and also is problematic if another new source joins the queue (it now has to wait despite an outputter having surpassed its time). However, we can't solve
            // this for real while still putting done outputters back onto the queue (ideally, they would be delayed from rejoining in this case while still allowing new source to queue normally).
            // Note that all of this only matters with multiple done outputters, which only occurs when transitioning from abundance to scarcity. After this code runs once, we will be in scarcity, and there
            // won't ever be multiple done outputters (unless two expire on the same frame, in which case we don't reset to half, so one will replace the other, causing unnecessary movement but not unfairness).
            if done_outputters.len() > 0 {
                for (i, _) in done_outputters.drain(..) {
                    // Use exclusive comparison to avoid resetting outputters that are just at the limit (fix for when multiple outputters expire at the same time in scarcity)
                    if state.outputters[i].start_frame + state.output_time < state.frame {
                        state.outputters[i].start_frame = state.frame - state.output_time / 2;
                    }
                }
            }
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
        trace!("\t{:?}", source.id)
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
    packet[111] = seq_number;
    packet[113..115].copy_from_slice(&universe.to_be_bytes());
    packet[126..].copy_from_slice(data);
    socket.send(&packet).await
}
