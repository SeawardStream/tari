// Copyright 2020, The Tari Project
//
// Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
// following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
// disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
// following disclaimer in the documentation and/or other materials provided with the distribution.
//
// 3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
// products derived from this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
// INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
// SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
// WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
// USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use super::messaging::{
    MessagingEvent,
    MessagingEventReceiver,
    MessagingProtocol,
    MessagingRequest,
    MESSAGING_PROTOCOL,
};
use crate::{
    message::{InboundMessage, MessageExt, MessageFlags, MessageTag, OutboundMessage},
    net_address::MultiaddressesWithStats,
    peer_manager::{AsyncPeerManager, NodeId, NodeIdentity, Peer, PeerFeatures, PeerFlags},
    proto::envelope::Envelope,
    protocol::{messaging::SendFailReason, ProtocolEvent, ProtocolNotification},
    test_utils::{
        create_connection_manager_mock,
        create_peer_connection_mock_pair,
        node_id,
        node_identity::build_node_identity,
        peer_manager::build_peer_manager,
        transport,
        ConnectionManagerMockState,
    },
    types::{CommsPublicKey, CommsSubstream},
};
use bytes::Bytes;
use futures::{channel::mpsc, SinkExt, StreamExt};
use prost::Message;
use rand::rngs::OsRng;
use std::{sync::Arc, time::Duration};
use tari_crypto::keys::PublicKey;
use tari_shutdown::Shutdown;
use tari_test_utils::{collect_stream, unpack_enum};
use tokio::{runtime::Handle, sync::broadcast, time};
use tokio_macros as runtime;

const TEST_MSG1: Bytes = Bytes::from_static(b"TEST_MSG1");

async fn spawn_messaging_protocol() -> (
    AsyncPeerManager,
    Arc<NodeIdentity>,
    ConnectionManagerMockState,
    mpsc::Sender<ProtocolNotification<CommsSubstream>>,
    mpsc::Sender<MessagingRequest>,
    mpsc::Receiver<InboundMessage>,
    MessagingEventReceiver,
    Shutdown,
) {
    let shutdown = Shutdown::new();
    let rt_handle = Handle::current();

    let (requester, mock) = create_connection_manager_mock(10);
    let mock_state = mock.get_shared_state();
    rt_handle.spawn(mock.run());

    let peer_manager: AsyncPeerManager = build_peer_manager().into();
    let node_identity = build_node_identity(PeerFeatures::COMMUNICATION_CLIENT);
    let (proto_tx, proto_rx) = mpsc::channel(10);
    let (request_tx, request_rx) = mpsc::channel(10);
    let (inbound_msg_tx, inbound_msg_rx) = mpsc::channel(100);
    let (events_tx, events_rx) = broadcast::channel(100);

    let msg_proto = MessagingProtocol::new(
        rt_handle.clone(),
        requester,
        peer_manager.clone(),
        node_identity.clone(),
        proto_rx,
        request_rx,
        events_tx,
        inbound_msg_tx,
        0,
        shutdown.to_signal(),
    );
    rt_handle.spawn(msg_proto.run());

    (
        peer_manager,
        node_identity,
        mock_state,
        proto_tx,
        request_tx,
        inbound_msg_rx,
        events_rx,
        shutdown,
    )
}

#[runtime::test_basic]
async fn new_inbound_substream_handling() {
    let (peer_manager, _, _, mut proto_tx, _, mut inbound_msg_rx, mut events_rx, _shutdown) =
        spawn_messaging_protocol().await;

    let expected_node_id = node_id::random();
    let (sk, pk) = CommsPublicKey::random_keypair(&mut OsRng);
    peer_manager
        .add_peer(Peer::new(
            pk.clone(),
            expected_node_id.clone(),
            MultiaddressesWithStats::default(),
            PeerFlags::empty(),
            PeerFeatures::COMMUNICATION_CLIENT,
            &[],
        ))
        .await
        .unwrap();

    // Create connected memory sockets - we use each end of the connection as if they exist on different nodes
    let (_, muxer_ours, mut muxer_theirs) = transport::build_multiplexed_connections().await;

    // Notify the messaging protocol that a new substream has been established that wants to talk the messaging.
    let stream_ours = muxer_ours.get_yamux_control().open_stream().await.unwrap();
    proto_tx
        .send(ProtocolNotification::new(
            MESSAGING_PROTOCOL.clone(),
            ProtocolEvent::NewInboundSubstream(Box::new(expected_node_id.clone()), stream_ours),
        ))
        .await
        .unwrap();

    let stream_theirs = muxer_theirs.incoming_mut().next().await.unwrap().unwrap();
    let mut framed_theirs = MessagingProtocol::framed(stream_theirs);

    let envelope = Envelope::construct_signed(&sk, &pk, TEST_MSG1, MessageFlags::empty()).unwrap();

    framed_theirs
        .send(Bytes::copy_from_slice(&envelope.to_encoded_bytes().unwrap()))
        .await
        .unwrap();

    let in_msg = time::timeout(Duration::from_secs(5), inbound_msg_rx.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(in_msg.source_peer.node_id, expected_node_id);
    assert_eq!(in_msg.body, TEST_MSG1);

    let expected_tag = in_msg.tag;
    let event = time::timeout(Duration::from_secs(5), events_rx.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    unpack_enum!(MessagingEvent::MessageReceived(node_id, tag) = &*event);
    assert_eq!(tag, &expected_tag);
    assert_eq!(**node_id, expected_node_id);
}

#[runtime::test_basic]
async fn send_message_request() {
    let (_, node_identity, conn_man_mock, _, mut request_tx, _, _, _shutdown) = spawn_messaging_protocol().await;

    let peer_node_id = node_id::random();

    let (conn1, peer_conn_mock1, _, peer_conn_mock2) =
        create_peer_connection_mock_pair(1, node_identity.node_id().clone(), peer_node_id.clone()).await;

    // Add mock peer connection to connection manager mock for node 2
    conn_man_mock.add_active_connection(peer_node_id.clone(), conn1).await;

    // Send a message to node
    let out_msg = OutboundMessage::new(peer_node_id, MessageFlags::NONE, TEST_MSG1);
    request_tx.send(MessagingRequest::SendMessage(out_msg)).await.unwrap();

    // Check that node got the message
    let stream = peer_conn_mock2.next_incoming_substream().await.unwrap();
    let mut framed = MessagingProtocol::framed(stream);
    let msg = framed.next().await.unwrap().unwrap();
    let msg = Envelope::decode(msg).unwrap();
    assert_eq!(msg.body, TEST_MSG1);

    // Got the call to create a substream
    assert_eq!(peer_conn_mock1.call_count(), 1);
}

#[runtime::test_basic]
async fn send_message_dial_failed() {
    let (_, _, conn_manager_mock, _, mut request_tx, _, mut event_tx, _shutdown) = spawn_messaging_protocol().await;

    let node_id = node_id::random();
    let out_msg = OutboundMessage::new(node_id, MessageFlags::NONE, TEST_MSG1);
    let expected_out_msg_tag = out_msg.tag;
    // Send a message to node 2
    request_tx.send(MessagingRequest::SendMessage(out_msg)).await.unwrap();

    let event = event_tx.next().await.unwrap().unwrap();
    unpack_enum!(MessagingEvent::SendMessageFailed(out_msg, reason) = &*event);
    unpack_enum!(SendFailReason::PeerDialFailed = reason);
    assert_eq!(out_msg.tag, expected_out_msg_tag);

    let calls = conn_manager_mock.take_calls().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("DialPeer"));
}

#[runtime::test_basic]
async fn send_message_substream_bulk_failure() {
    const NUM_MSGS: usize = 10;
    let (_, node_identity, conn_manager_mock, _, mut request_tx, _, mut event_tx, _shutdown) =
        spawn_messaging_protocol().await;

    let peer_node_id = node_id::random();

    let (conn1, _, _, peer_conn_mock2) =
        create_peer_connection_mock_pair(1, node_identity.node_id().clone(), peer_node_id.clone()).await;

    // Add mock peer connection to connection manager mock for node 2
    conn_manager_mock
        .add_active_connection(peer_node_id.clone(), conn1)
        .await;

    async fn send_msg(request_tx: &mut mpsc::Sender<MessagingRequest>, node_id: NodeId) -> MessageTag {
        let out_msg = OutboundMessage::new(node_id, MessageFlags::NONE, TEST_MSG1);
        let msg_tag = out_msg.tag;
        // Send a message to node 2
        request_tx.send(MessagingRequest::SendMessage(out_msg)).await.unwrap();
        msg_tag
    }

    let mut expected_out_msg_tags = Vec::with_capacity(NUM_MSGS);
    expected_out_msg_tags.push(send_msg(&mut request_tx, peer_node_id.clone()).await);

    let _ = peer_conn_mock2.next_incoming_substream().await.unwrap();
    // Close destination peer's channel before receiving the message
    peer_conn_mock2.disconnect().await;

    for _ in 0..NUM_MSGS - 1 {
        expected_out_msg_tags.push(send_msg(&mut request_tx, peer_node_id.clone()).await);
    }

    let event = event_tx.next().await.unwrap().unwrap();
    unpack_enum!(MessagingEvent::MessageSent(tag) = &*event);
    assert_eq!(tag, &expected_out_msg_tags.remove(0));

    for _ in 0..NUM_MSGS - 1 {
        let event = event_tx.next().await.unwrap().unwrap();
        unpack_enum!(MessagingEvent::SendMessageFailed(out_msg, reason) = &*event);
        unpack_enum!(SendFailReason::SubstreamSendFailed = reason);
        assert_eq!(out_msg.tag, expected_out_msg_tags.remove(0));
    }
}

#[runtime::test_basic]
async fn many_concurrent_send_message_requests() {
    const NUM_MSGS: usize = 100;
    let (_, _, conn_man_mock, _, mut request_tx, _, events_rx, _shutdown) = spawn_messaging_protocol().await;

    let node_id1 = node_id::random();
    let node_id2 = node_id::random();

    let (conn1, peer_conn_mock1, _, peer_conn_mock2) =
        create_peer_connection_mock_pair(1, node_id1.clone(), node_id2.clone()).await;

    // Add mock peer connection to connection manager mock for node 2
    conn_man_mock.add_active_connection(node_id2.clone(), conn1).await;

    // Send many messages to node
    let mut msg_tags = Vec::with_capacity(NUM_MSGS);
    for _ in 0..NUM_MSGS {
        let out_msg = OutboundMessage::new(node_id2.clone(), MessageFlags::NONE, TEST_MSG1);
        msg_tags.push(out_msg.tag);
        request_tx.send(MessagingRequest::SendMessage(out_msg)).await.unwrap();
    }

    // Check that the node got the messages
    let stream = peer_conn_mock2.next_incoming_substream().await.unwrap();
    let framed = MessagingProtocol::framed(stream);
    let messages = collect_stream!(framed, take = NUM_MSGS, timeout = Duration::from_secs(10));
    assert_eq!(messages.len(), NUM_MSGS);

    // Check that we got message success events
    let events = collect_stream!(events_rx, take = NUM_MSGS, timeout = Duration::from_secs(10));
    assert_eq!(events.len(), NUM_MSGS);
    for event in events {
        let event = event.unwrap();
        unpack_enum!(MessagingEvent::MessageSent(tag) = &*event);
        // Assert that each tag is emitted only once
        let index = msg_tags.iter().position(|t| t == tag).unwrap();
        msg_tags.remove(index);
    }

    // Got a single call to create a substream
    assert_eq!(peer_conn_mock1.call_count(), 1);
}