//! The audio graph topology and render algorithm
use std::any::Any;
use std::cell::RefCell;
use std::panic::{self, AssertUnwindSafe};

use crate::context::AudioNodeId;
use smallvec::{smallvec, SmallVec};

use super::{Alloc, AudioParamValues, AudioProcessor, AudioRenderQuantum, NodeCollection};
use crate::node::ChannelConfig;
use crate::render::RenderScope;

/// Connection between two audio nodes
struct OutgoingEdge {
    /// index of the current Nodes output port
    self_index: usize,
    /// reference to the other Node
    other_id: AudioNodeId,
    /// index of the other Nodes input port
    other_index: usize,
}

impl std::fmt::Debug for OutgoingEdge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut format = f.debug_struct("OutgoingEdge");
        format
            .field("self_index", &self.self_index)
            .field("other_id", &self.other_id);
        if self.other_index == usize::MAX {
            format.field("other_index", &"HIDDEN");
        } else {
            format.field("other_index", &self.other_index);
        }
        format.finish()
    }
}

/// Renderer Node in the Audio Graph
pub struct Node {
    /// AudioNodeId, to be sent back to the control thread when this node is dropped
    reclaim_id: Option<llq::Node<AudioNodeId>>,
    /// Renderer: converts inputs to outputs
    processor: Box<dyn AudioProcessor>,
    /// Reusable input buffers
    inputs: Vec<AudioRenderQuantum>,
    /// Reusable output buffers, consumed by subsequent Nodes in this graph
    outputs: Vec<AudioRenderQuantum>,
    /// Channel configuration: determines up/down-mixing of inputs
    channel_config: ChannelConfig,
    /// Outgoing edges: tuple of outcoming node reference, our output index and their input index
    outgoing_edges: SmallVec<[OutgoingEdge; 2]>,
    /// Indicates if the control thread has dropped this Node
    free_when_finished: bool,
    /// Indicates if the node has any incoming connections (for lifecycle management)
    has_inputs_connected: bool,
    /// Indicates if the node can act as a cycle breaker (only DelayNode for now)
    cycle_breaker: bool,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("id", &self.reclaim_id.as_deref())
            .field("processor", &self.processor)
            .field("channel_config", &self.channel_config)
            .field("outgoing_edges", &self.outgoing_edges)
            .field("free_when_finished", &self.free_when_finished)
            .field("cycle_breaker", &self.cycle_breaker)
            .finish_non_exhaustive()
    }
}

impl Node {
    /// Render an audio quantum
    fn process(&mut self, params: AudioParamValues<'_>, scope: &RenderScope) -> bool {
        self.processor
            .process(&self.inputs[..], &mut self.outputs[..], params, scope)
    }

    /// Determine if this node is done playing and can be removed from the audio graph
    fn can_free(&self, tail_time: bool) -> bool {
        // Only drop when the Control thread has dropped its handle.
        // Otherwise the node can be reconnected/restarted etc.
        if !self.free_when_finished {
            return false;
        }

        // Drop when the node does not have any inputs and outputs
        if !self.has_inputs_connected && self.outgoing_edges.is_empty() {
            return true;
        }

        // Drop when the node does not have any inputs connected,
        // and if the processor reports it won't yield output.
        if !self.has_inputs_connected && !tail_time {
            return true;
        }

        // Otherwise, do not drop the node.
        // (Even if it has no outputs connected, it may have side effects)
        false
    }

    /// Get the current buffer for AudioParam values
    pub fn get_buffer(&self) -> &AudioRenderQuantum {
        self.outputs.first().unwrap()
    }
}

/// The audio graph
pub(crate) struct Graph {
    /// Processing Nodes
    nodes: NodeCollection,
    /// Allocator for audio buffers
    alloc: Alloc,
    /// Message channel to notify control thread of reclaimable AudioNodeIds
    reclaim_id_channel: llq::Producer<AudioNodeId>,
    /// Topological ordering of the nodes
    ordered: Vec<AudioNodeId>,
    /// Topological sorting helper
    marked: Vec<AudioNodeId>,
    /// Topological sorting helper
    marked_temp: Vec<AudioNodeId>,
    /// Topological sorting helper
    in_cycle: Vec<AudioNodeId>,
    /// Topological sorting helper
    cycle_breakers: Vec<AudioNodeId>,
}

impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Graph")
            .field("nodes", &self.nodes)
            .field("ordered", &self.ordered)
            .finish_non_exhaustive()
    }
}

impl Graph {
    pub fn new(reclaim_id_channel: llq::Producer<AudioNodeId>) -> Self {
        Graph {
            nodes: NodeCollection::new(),
            alloc: Alloc::with_capacity(64),
            reclaim_id_channel,
            ordered: vec![],
            marked: vec![],
            marked_temp: vec![],
            in_cycle: vec![],
            cycle_breakers: vec![],
        }
    }

    /// Check if the graph is fully initialized and can start rendering
    pub fn is_active(&self) -> bool {
        // currently we only require the destination node to be present
        !self.nodes.is_empty()
    }

    pub fn add_node(
        &mut self,
        index: AudioNodeId,
        reclaim_id: llq::Node<AudioNodeId>,
        processor: Box<dyn AudioProcessor>,
        number_of_inputs: usize,
        number_of_outputs: usize,
        channel_config: ChannelConfig,
    ) {
        // todo: pre-allocate the buffers on the control thread

        // set input and output buffers to single channel of silence, will be upmixed when
        // necessary
        let inputs = vec![AudioRenderQuantum::from(self.alloc.silence()); number_of_inputs];
        let outputs = vec![AudioRenderQuantum::from(self.alloc.silence()); number_of_outputs];

        self.nodes.insert(
            index,
            RefCell::new(Node {
                reclaim_id: Some(reclaim_id),
                processor,
                inputs,
                outputs,
                channel_config,
                outgoing_edges: smallvec![],
                free_when_finished: false,
                has_inputs_connected: false,
                cycle_breaker: false,
            }),
        );
    }

    pub fn add_edge(&mut self, source: (AudioNodeId, usize), dest: (AudioNodeId, usize)) {
        self.nodes[source.0]
            .get_mut()
            .outgoing_edges
            .push(OutgoingEdge {
                self_index: source.1,
                other_id: dest.0,
                other_index: dest.1,
            });

        self.ordered.clear(); // void current ordering
    }

    pub fn remove_edge(&mut self, source: AudioNodeId, dest: AudioNodeId) {
        self.nodes[source]
            .get_mut()
            .outgoing_edges
            .retain(|edge| edge.other_id != dest);

        self.ordered.clear(); // void current ordering
    }

    pub fn remove_edges_from(&mut self, source: AudioNodeId) {
        // Remove outgoing edges
        self.nodes[source].get_mut().outgoing_edges.clear();

        // Remove incoming edges - we need to traverse all nodes
        self.nodes.values_mut().for_each(|node| {
            // Retain edge when
            // - not connected to this node, or
            // - when this is an audioparam edge (only disconnect true audio nodes)
            node.get_mut()
                .outgoing_edges
                .retain(|edge| edge.other_id != source || edge.other_index == usize::MAX);
        });

        self.ordered.clear(); // void current ordering
    }

    pub fn mark_free_when_finished(&mut self, index: AudioNodeId) {
        // Issue #92, a race condition can occur for AudioParams. They may have already been
        // removed from the audio graph if the node they feed into was dropped.
        // Therefore, do not assume this node still exists:
        if let Some(node) = self.nodes.get_mut(index) {
            node.get_mut().free_when_finished = true;
        }
    }

    pub fn mark_cycle_breaker(&mut self, index: AudioNodeId) {
        self.nodes[index].get_mut().cycle_breaker = true;
    }

    pub fn route_message(&mut self, index: AudioNodeId, msg: &mut dyn Any) {
        self.nodes[index].get_mut().processor.onmessage(msg);
    }

    /// Helper function for `order_nodes` - traverse node and outgoing edges
    ///
    /// The return value indicates `cycle_breaker_applied`:
    /// - true: a cycle was found and a cycle breaker was applied, current ordering is invalidated
    /// - false: visiting this leg was successful and no topological changes were applied
    fn visit(
        &self,
        node_id: AudioNodeId,
        marked: &mut Vec<AudioNodeId>,
        marked_temp: &mut Vec<AudioNodeId>,
        ordered: &mut Vec<AudioNodeId>,
        in_cycle: &mut Vec<AudioNodeId>,
        cycle_breakers: &mut Vec<AudioNodeId>,
    ) -> bool {
        // If this node is in the cycle detection list, it is part of a cycle!
        if let Some(pos) = marked_temp.iter().position(|&m| m == node_id) {
            // check if we can find some node that can break the cycle
            let cycle_breaker_node = marked_temp
                .iter()
                .skip(pos)
                .find(|&&node_id| self.nodes[node_id].borrow().cycle_breaker);

            match cycle_breaker_node {
                Some(&node_id) => {
                    // store node id to clear the node outgoing edges
                    cycle_breakers.push(node_id);

                    return true;
                }
                None => {
                    // Mark all nodes in the cycle
                    in_cycle.extend_from_slice(&marked_temp[pos..]);
                    // Do not continue, as we already have visited all these nodes
                    return false;
                }
            }
        }

        // Do not visit nodes multiple times
        if marked.contains(&node_id) {
            return false;
        }

        // Add node to the visited list
        marked.push(node_id);
        // Add node to the current cycle detection list
        marked_temp.push(node_id);

        // Visit outgoing nodes, and call `visit` on them recursively
        for edge in self.nodes[node_id].borrow().outgoing_edges.iter() {
            let cycle_breaker_applied = self.visit(
                edge.other_id,
                marked,
                marked_temp,
                ordered,
                in_cycle,
                cycle_breakers,
            );
            if cycle_breaker_applied {
                return true;
            }
        }

        // Then add this node to the ordered list
        ordered.push(node_id);

        // Finished visiting all nodes in this leg, clear the current cycle detection list
        marked_temp.retain(|marked| *marked != node_id);

        false
    }

    /// Determine the order of the audio nodes for rendering
    ///
    /// By inspecting the audio node connections, we can determine which nodes should render before
    /// other nodes. For example, in a graph with an audio source, a gain node and the destination
    /// node, at every render quantum the source should render first and after that the gain node.
    ///
    /// Inspired by the spec recommendation at
    /// <https://webaudio.github.io/web-audio-api/#rendering-loop>
    ///
    /// The goals are:
    /// - Perform a topological sort of the graph
    /// - Break cycles when possible (if there is a DelayNode present)
    /// - Mute nodes that are still in a cycle
    /// - For performance: no new allocations (reuse Vecs)
    fn order_nodes(&mut self) {
        // For borrowck reasons, we need the `visit` call to be &self.
        // So move out the bookkeeping Vecs, and pass them around as &mut.
        let mut ordered = std::mem::take(&mut self.ordered);
        let mut marked = std::mem::take(&mut self.marked);
        let mut marked_temp = std::mem::take(&mut self.marked_temp);
        let mut in_cycle = std::mem::take(&mut self.in_cycle);
        let mut cycle_breakers = std::mem::take(&mut self.cycle_breakers);

        // When a cycle breaker is applied, the graph topology changes and we need to run the
        // ordering again
        loop {
            // Clear previous administration
            ordered.clear();
            marked.clear();
            marked_temp.clear();
            in_cycle.clear();
            cycle_breakers.clear();

            // Visit all registered nodes, and perform a depth first traversal.
            //
            // We cannot just start from the AudioDestinationNode and visit all nodes connecting to it,
            // since the audio graph could contain legs detached from the destination and those should
            // still be rendered.
            let mut cycle_breaker_applied = false;
            for node_id in self.nodes.keys() {
                cycle_breaker_applied = self.visit(
                    node_id,
                    &mut marked,
                    &mut marked_temp,
                    &mut ordered,
                    &mut in_cycle,
                    &mut cycle_breakers,
                );

                if cycle_breaker_applied {
                    break;
                }
            }

            if cycle_breaker_applied {
                // clear the outgoing edges of the nodes that have been recognized as cycle breaker
                let nodes = &mut self.nodes;
                cycle_breakers.iter().for_each(|node_id| {
                    nodes[*node_id].get_mut().outgoing_edges.clear();
                });

                continue;
            }

            break;
        }

        // Remove nodes from the ordering if they are part of a cycle. The spec mandates that their
        // outputs should be silenced, but with our rendering algorithm that is not necessary.
        // `retain` leaves the ordering in place
        ordered.retain(|o| !in_cycle.contains(o));

        // The `visit` function adds child nodes before their parent, so reverse the order
        ordered.reverse();

        // Re-instate Vecs
        self.ordered = ordered;
        self.marked = marked;
        self.marked_temp = marked_temp;
        self.in_cycle = in_cycle;
        self.cycle_breakers = cycle_breakers;
    }

    /// Render a single audio quantum by traversing the node list
    pub fn render(&mut self, scope: &RenderScope) -> &AudioRenderQuantum {
        // if the audio graph was changed, determine the new ordering
        if self.ordered.is_empty() {
            self.order_nodes();
        }

        // keep track of end-of-lifecyle nodes
        let mut nodes_dropped = false;

        // for borrow-checker reasons, move mutable borrow of nodes out of self
        let nodes = &mut self.nodes;

        // process every node, in topological sorted order
        self.ordered.iter().for_each(|index| {
            // acquire a mutable borrow of the current processing node
            let mut node = nodes[*index].borrow_mut();

            // make sure all input buffers have the correct number of channels, this might not be
            // the case if the node has no inputs connected or the channel count has just changed
            let interpretation = node.channel_config.interpretation();
            let count = node.channel_config.count();
            node.inputs
                .iter_mut()
                .for_each(|i| i.mix(count, interpretation));

            // let the current node process (catch any panics that may occur)
            let params = AudioParamValues::from(nodes);
            scope.node_id.set(*index);
            let (success, tail_time) = {
                // We are abusing AssertUnwindSafe here, we cannot guarantee it upholds.
                // This may lead to logic bugs later on, but it is the best that we can do.
                // The alternative is to crash and reboot the render thread.
                let catch_me = AssertUnwindSafe(|| node.process(params, scope));

                match panic::catch_unwind(catch_me) {
                    Ok(tail_time) => (true, tail_time),
                    Err(e) => {
                        node.outgoing_edges.clear();
                        scope.report_error(e);
                        (false, false)
                    }
                }
            };

            // iterate all outgoing edges, lookup these nodes and add to their input
            node.outgoing_edges
                .iter()
                // audio params are connected to the 'hidden' usize::MAX output, ignore them here
                .filter(|edge| edge.other_index != usize::MAX)
                .for_each(|edge| {
                    let mut output_node = nodes[edge.other_id].borrow_mut();
                    output_node.has_inputs_connected = true;
                    let signal = &node.outputs[edge.self_index];
                    let channel_config = &output_node.channel_config.clone();

                    output_node.inputs[edge.other_index].add(signal, channel_config);
                });

            let can_free = !success || node.can_free(tail_time);

            // Node is not dropped.
            if !can_free {
                // Reset input buffers as they will be summed up in the next render quantum.
                node.inputs
                    .iter_mut()
                    .for_each(AudioRenderQuantum::make_silent);

                // Reset input state
                node.has_inputs_connected = false;
            }

            drop(node); // release borrow of self.nodes

            // Check if we can decommission this node (end of life)
            if can_free {
                // Node is dropped, remove it from the node list
                let mut node = nodes.remove(*index).into_inner();
                self.reclaim_id_channel
                    .push(node.reclaim_id.take().unwrap());
                drop(node);

                // And remove it from the ordering after we have processed all nodes
                nodes_dropped = true;

                // Nodes are only dropped when they do not have incoming connections.
                // But they may have AudioParams feeding into them, these can de dropped too.
                nodes.retain(|id, node| {
                    let node = node.get_mut(); // unwrap the RefCell

                    // Check if this node was connected to the dropped node. In that case, it is
                    // either an AudioParam (which can be dropped), or the AudioListener that feeds
                    // into a PannerNode (which can be disconnected).
                    let was_connected = {
                        let outgoing_edges = &mut node.outgoing_edges;
                        let prev_len = outgoing_edges.len();
                        outgoing_edges.retain(|e| e.other_id != *index);
                        outgoing_edges.len() != prev_len
                    };

                    // Retain when
                    // - special node (destination = id 0, listener = id 1), or
                    // - not connected to this dropped node, or
                    // - if the control thread still has a handle to it.
                    let retain = id.0 < 2 || !was_connected || !node.free_when_finished;

                    if !retain {
                        self.reclaim_id_channel
                            .push(node.reclaim_id.take().unwrap());
                    }
                    retain
                })
            }
        });

        // If there were any nodes decommissioned, remove from graph order
        if nodes_dropped {
            let mut i = 0;
            while i < self.ordered.len() {
                if nodes.get(self.ordered[i]).is_none() {
                    self.ordered.remove(i);
                } else {
                    i += 1;
                }
            }
        }

        // Return the output buffer of destination node
        &self.nodes[AudioNodeId(0)].get_mut().outputs[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::DESTINATION_NODE_ID;

    #[derive(Debug, Clone)]
    struct TestNode {
        tail_time: bool,
    }

    impl AudioProcessor for TestNode {
        fn process(
            &mut self,
            _inputs: &[AudioRenderQuantum],
            _outputs: &mut [AudioRenderQuantum],
            _params: AudioParamValues<'_>,
            _scope: &RenderScope,
        ) -> bool {
            self.tail_time
        }
    }

    fn config() -> ChannelConfig {
        crate::node::ChannelConfigOptions {
            count: 2,
            count_mode: crate::node::ChannelCountMode::Explicit,
            interpretation: crate::node::ChannelInterpretation::Speakers,
        }
        .into()
    }

    fn add_node(graph: &mut Graph, id: u64, node: Box<dyn AudioProcessor>) {
        let id = AudioNodeId(id);
        let reclaim_id = llq::Node::new(id);
        graph.add_node(id, reclaim_id, node, 1, 1, config());
    }

    fn add_edge(graph: &mut Graph, from: u64, to: u64) {
        graph.add_edge((AudioNodeId(from), 0), (AudioNodeId(to), 0));
    }

    fn add_audioparam(graph: &mut Graph, from: u64, to: u64) {
        graph.add_edge((AudioNodeId(from), 0), (AudioNodeId(to), usize::MAX));
    }

    // regression test for:
    // https://github.com/orottier/web-audio-api-rs/issues/389
    #[test]
    fn test_active() {
        let mut graph = Graph::new(llq::Queue::new().split().0);
        assert!(!graph.is_active());
        // graph is active only when AudioDestination is set up
        let node = Box::new(TestNode { tail_time: false });
        add_node(&mut graph, DESTINATION_NODE_ID.0, node.clone());
        assert!(graph.is_active());
    }

    #[test]
    fn test_add_remove() {
        let mut graph = Graph::new(llq::Queue::new().split().0);

        let node = Box::new(TestNode { tail_time: false });
        add_node(&mut graph, 0, node.clone());
        add_node(&mut graph, 1, node.clone());
        add_node(&mut graph, 2, node.clone());
        add_node(&mut graph, 3, node);

        add_edge(&mut graph, 1, 0);
        add_edge(&mut graph, 2, 1);
        add_edge(&mut graph, 3, 0);

        graph.order_nodes();

        // sorting is not deterministic, but this should uphold:
        assert_eq!(graph.ordered.len(), 4); // all nodes present
        assert_eq!(graph.ordered[3], AudioNodeId(0)); // root node comes last

        let pos1 = graph
            .ordered
            .iter()
            .position(|&n| n == AudioNodeId(1))
            .unwrap();
        let pos2 = graph
            .ordered
            .iter()
            .position(|&n| n == AudioNodeId(2))
            .unwrap();
        assert!(pos2 < pos1); // node 1 depends on node 2

        // Detach node 1 (and thus node 2) from the root node
        graph.remove_edge(AudioNodeId(1), AudioNodeId(0));
        graph.order_nodes();

        // sorting is not deterministic, but this should uphold:
        assert_eq!(graph.ordered.len(), 4); // all nodes present
        let pos1 = graph
            .ordered
            .iter()
            .position(|&n| n == AudioNodeId(1))
            .unwrap();
        let pos2 = graph
            .ordered
            .iter()
            .position(|&n| n == AudioNodeId(2))
            .unwrap();
        assert!(pos2 < pos1); // node 1 depends on node 2
    }

    #[test]
    fn test_remove_all() {
        let mut graph = Graph::new(llq::Queue::new().split().0);

        let node = Box::new(TestNode { tail_time: false });
        add_node(&mut graph, 0, node.clone());
        add_node(&mut graph, 1, node.clone());
        add_node(&mut graph, 2, node);

        // link 1->0, 1->2 and 2->0
        add_edge(&mut graph, 1, 0);
        add_edge(&mut graph, 1, 2);
        add_edge(&mut graph, 2, 0);

        graph.order_nodes();

        assert_eq!(
            graph.ordered,
            vec![AudioNodeId(1), AudioNodeId(2), AudioNodeId(0)]
        );

        graph.remove_edges_from(AudioNodeId(1));
        graph.order_nodes();

        // sorting is not deterministic, but this should uphold:
        assert_eq!(graph.ordered.len(), 3); // all nodes present
        let pos0 = graph
            .ordered
            .iter()
            .position(|&n| n == AudioNodeId(0))
            .unwrap();
        let pos2 = graph
            .ordered
            .iter()
            .position(|&n| n == AudioNodeId(2))
            .unwrap();
        assert!(pos2 < pos0); // node 1 depends on node 0
    }

    #[test]
    fn test_cycle() {
        let mut graph = Graph::new(llq::Queue::new().split().0);

        let node = Box::new(TestNode { tail_time: false });
        add_node(&mut graph, 0, node.clone());
        add_node(&mut graph, 1, node.clone());
        add_node(&mut graph, 2, node.clone());
        add_node(&mut graph, 3, node.clone());
        add_node(&mut graph, 4, node);

        // link 4->2, 2->1, 1->0, 1->2, 3->0
        add_edge(&mut graph, 4, 2);
        add_edge(&mut graph, 2, 1);
        add_edge(&mut graph, 1, 0);
        add_edge(&mut graph, 1, 2);
        add_edge(&mut graph, 3, 0);

        graph.order_nodes();

        let pos0 = graph.ordered.iter().position(|&n| n == AudioNodeId(0));
        let pos1 = graph.ordered.iter().position(|&n| n == AudioNodeId(1));
        let pos2 = graph.ordered.iter().position(|&n| n == AudioNodeId(2));
        let pos3 = graph.ordered.iter().position(|&n| n == AudioNodeId(3));
        let pos4 = graph.ordered.iter().position(|&n| n == AudioNodeId(4));

        // cycle 1<>2 should be removed
        assert_eq!(pos1, None);
        assert_eq!(pos2, None);
        // detached leg from cycle will still be rendered
        assert!(pos4.is_some());
        // a-cyclic part should be present
        assert!(pos3.unwrap() < pos0.unwrap());
    }

    #[test]
    fn test_lifecycle_and_reclaim() {
        let (node_id_producer, mut node_id_consumer) = llq::Queue::new().split();
        let mut graph = Graph::new(node_id_producer);

        let node = Box::new(TestNode { tail_time: false });

        // Destination Node is always node id 0, and should never drop
        add_node(&mut graph, 0, node.clone());

        // AudioListener Node is always node id 1, and should never drop
        add_node(&mut graph, 1, node.clone());

        // Add a regular node at id 3, it has tail time false so after rendering it should be
        // dropped and the AudioNodeId(3) should be reclaimed
        add_node(&mut graph, 2, node.clone());
        // Mark the node as 'detached from the control thread', so it is allowed to drop
        graph.nodes[AudioNodeId(2)].get_mut().free_when_finished = true;

        // Connect the regular node to the AudioDestinationNode
        add_edge(&mut graph, 2, 0);

        // Render a single quantum
        let scope = RenderScope {
            current_frame: 0,
            current_time: 0.,
            sample_rate: 48000.,
            node_id: std::cell::Cell::new(AudioNodeId(0)),
            event_sender: None,
        };
        graph.render(&scope);

        // The dropped node should be our regular node, not the AudioListener
        let reclaimed = node_id_consumer
            .pop()
            .expect("should have decommisioned node");
        assert_eq!(reclaimed.0, 2);

        // No other dropped nodes
        assert!(node_id_consumer.pop().is_none());
    }

    #[test]
    fn test_audio_param_lifecycle() {
        let (node_id_producer, mut node_id_consumer) = llq::Queue::new().split();
        let mut graph = Graph::new(node_id_producer);

        let node = Box::new(TestNode { tail_time: false });

        // Destination Node is always node id 0, and should never drop
        add_node(&mut graph, 0, node.clone());

        // AudioListener Node is always node id 1, and should never drop
        add_node(&mut graph, 1, node.clone());

        // Add a regular node at id 3, it has tail time false so after rendering it should be
        // dropped and the AudioNodeId(3) should be reclaimed
        add_node(&mut graph, 2, node.clone());
        // Mark the node as 'detached from the control thread', so it is allowed to drop
        graph.nodes[AudioNodeId(2)].get_mut().free_when_finished = true;

        // Connect the regular node to the AudioDestinationNode
        add_edge(&mut graph, 2, 0);

        // Add an AudioParam at id 4, it should be dropped alongside the regular node
        let param = Box::new(TestNode { tail_time: true }); // audio params have tail time true
        add_node(&mut graph, 3, param);
        // Mark the node as 'detached from the control thread', so it is allowed to drop
        graph.nodes[AudioNodeId(3)].get_mut().free_when_finished = true;

        // Connect the audioparam to the regular node
        add_audioparam(&mut graph, 3, 2);

        // Render a single quantum
        let scope = RenderScope {
            current_frame: 0,
            current_time: 0.,
            sample_rate: 48000.,
            node_id: std::cell::Cell::new(AudioNodeId(0)),
            event_sender: None,
        };
        graph.render(&scope);

        // First the regular node should be dropped, then the audioparam
        assert_eq!(node_id_consumer.pop().unwrap().0, 2);
        assert_eq!(node_id_consumer.pop().unwrap().0, 3);

        // No other dropped nodes
        assert!(node_id_consumer.pop().is_none());
    }

    #[test]
    fn test_release_orphaned_source_nodes() {
        let (node_id_producer, mut node_id_consumer) = llq::Queue::new().split();
        let mut graph = Graph::new(node_id_producer);

        let node = Box::new(TestNode { tail_time: true });

        // Destination Node is always node id 0, and should never drop
        add_node(&mut graph, 0, node.clone());

        // AudioListener Node is always node id 1, and should never drop
        add_node(&mut graph, 1, node.clone());

        // Add a regular node at id 3, it has tail time true but since we drop the control handle
        // and there aren't any inputs and outputs, it will still be dropped and the AudioNodeId(3)
        // should be reclaimed
        add_node(&mut graph, 2, node);

        // Mark the node as 'detached from the control thread', so it is allowed to drop
        graph.nodes[AudioNodeId(2)].get_mut().free_when_finished = true;

        // Render a single quantum
        let scope = RenderScope {
            current_frame: 0,
            current_time: 0.,
            sample_rate: 48000.,
            node_id: std::cell::Cell::new(AudioNodeId(0)),
            event_sender: None,
        };
        graph.render(&scope);

        // The dropped node should be our orphaned node
        let reclaimed = node_id_consumer
            .pop()
            .expect("should have decommisioned node");
        assert_eq!(reclaimed.0, 2);

        // No other dropped nodes
        assert!(node_id_consumer.pop().is_none());
    }
}
