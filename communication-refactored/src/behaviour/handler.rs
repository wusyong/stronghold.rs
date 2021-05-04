// Copyright 2020-2021 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

// Copyright 2020 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.

mod protocol;
use crate::behaviour::{types::*, EMPTY_QUEUE_SHRINK_THRESHOLD};
use futures::{channel::oneshot, future::BoxFuture, prelude::*, stream::FuturesUnordered};
use libp2p::{
    core::upgrade::{NegotiationError, UpgradeError},
    swarm::{
        protocols_handler::{KeepAlive, ProtocolsHandler, ProtocolsHandlerEvent, ProtocolsHandlerUpgrErr},
        SubstreamProtocol,
    },
};
pub use protocol::{CommunicationProtocol, ProtocolSupport, RequestProtocol, ResponseProtocol};
use smallvec::SmallVec;
use std::{
    collections::VecDeque,
    io,
    marker::PhantomData,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

pub type ProtocolsHandlerEventType<Rq, Rs> = ProtocolsHandlerEvent<
    RequestProtocol<Rq, Rs>,
    RequestId,
    <ConnectionHandler<Rq, Rs> as ProtocolsHandler>::OutEvent,
    <ConnectionHandler<Rq, Rs> as ProtocolsHandler>::Error,
>;
pub type PendingInboundFuture<Rq, Rs> = BoxFuture<'static, Result<(RequestId, Request<Rq, Rs>), oneshot::Canceled>>;

#[doc(hidden)]
pub struct ConnectionHandler<Rq, Rs>
where
    Rq: RqRsMessage,
    Rs: RqRsMessage,
{
    supported_protocols: SmallVec<[CommunicationProtocol; 2]>,
    protocol_support: ProtocolSupport,
    keep_alive_timeout: Duration,
    substream_timeout: Duration,
    keep_alive: KeepAlive,
    pending_error: Option<ProtocolsHandlerUpgrErr<io::Error>>,
    pending_events: VecDeque<HandlerOutEvent<Rq, Rs>>,
    outbound: VecDeque<HandlerInEvent<Rq, Rs>>,
    inbound: FuturesUnordered<PendingInboundFuture<Rq, Rs>>,
    inbound_request_id: Arc<AtomicU64>,
}

impl<Rq, Rs> ConnectionHandler<Rq, Rs>
where
    Rq: RqRsMessage,
    Rs: RqRsMessage,
{
    pub(super) fn new(
        supported_protocols: SmallVec<[CommunicationProtocol; 2]>,
        protocol_support: ProtocolSupport,
        keep_alive_timeout: Duration,
        substream_timeout: Duration,
        inbound_request_id: Arc<AtomicU64>,
    ) -> Self {
        Self {
            supported_protocols,
            protocol_support,
            keep_alive: KeepAlive::Yes,
            keep_alive_timeout,
            substream_timeout,
            outbound: VecDeque::new(),
            inbound: FuturesUnordered::new(),
            pending_events: VecDeque::new(),
            pending_error: None,
            inbound_request_id,
        }
    }

    fn new_outbound_protocol(
        &mut self,
        request_id: RequestId,
        request: Request<Rq, Rs>,
    ) -> SubstreamProtocol<RequestProtocol<Rq, Rs>, RequestId> {
        let proto = RequestProtocol {
            protocols: self.supported_protocols.clone(),
            request,
            marker: PhantomData,
        };
        SubstreamProtocol::new(proto, request_id).with_timeout(self.substream_timeout)
    }

    fn new_inbound_protocol(&self) -> SubstreamProtocol<ResponseProtocol<Rq, Rs>, RequestId> {
        let request_id = RequestId::new(self.inbound_request_id.fetch_add(1, Ordering::Relaxed));

        let (rq_send, rq_recv) = oneshot::channel();

        let (rs_send, rs_recv) = oneshot::channel();

        let protocols = self
            .protocol_support
            .inbound()
            .then(|| self.supported_protocols.clone())
            .unwrap_or_default();

        let proto = ResponseProtocol {
            protocols,
            request_sender: rq_send,
            response_receiver: rs_recv,
        };

        self.inbound.push(
            rq_recv
                .map_ok(move |rq| {
                    (
                        request_id,
                        Request {
                            message: rq,
                            response_sender: rs_send,
                        },
                    )
                })
                .boxed(),
        );
        SubstreamProtocol::new(proto, request_id).with_timeout(self.substream_timeout)
    }
}

impl<Rq, Rs> ProtocolsHandler for ConnectionHandler<Rq, Rs>
where
    Rq: RqRsMessage,
    Rs: RqRsMessage,
{
    type InEvent = HandlerInEvent<Rq, Rs>;
    type OutEvent = HandlerOutEvent<Rq, Rs>;
    type Error = ProtocolsHandlerUpgrErr<io::Error>;
    type InboundProtocol = ResponseProtocol<Rq, Rs>;
    type OutboundProtocol = RequestProtocol<Rq, Rs>;
    type InboundOpenInfo = RequestId;
    type OutboundOpenInfo = RequestId;

    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
        self.new_inbound_protocol()
    }

    fn inject_fully_negotiated_inbound(&mut self, send_response: bool, request_id: RequestId) {
        let event = send_response
            .then(|| HandlerOutEvent::SentResponse(request_id))
            .unwrap_or(HandlerOutEvent::SendResponseOmission(request_id));
        self.pending_events.push_back(event);
    }

    fn inject_fully_negotiated_outbound(&mut self, received_response: bool, request_id: RequestId) {
        let event = received_response
            .then(|| HandlerOutEvent::ReceivedResponse(request_id))
            .unwrap_or(HandlerOutEvent::RecvResponseOmission(request_id));
        self.pending_events.push_back(event);
    }

    fn inject_event(&mut self, event: Self::InEvent) {
        self.keep_alive = KeepAlive::Yes;
        self.outbound.push_back(event)
    }

    fn inject_dial_upgrade_error(&mut self, info: RequestId, error: ProtocolsHandlerUpgrErr<io::Error>) {
        match error {
            ProtocolsHandlerUpgrErr::Timeout => {
                self.pending_events.push_back(HandlerOutEvent::OutboundTimeout(info));
            }
            ProtocolsHandlerUpgrErr::Upgrade(UpgradeError::Select(NegotiationError::Failed)) => {
                self.pending_events
                    .push_back(HandlerOutEvent::OutboundUnsupportedProtocols(info));
            }
            _ => {
                self.pending_error = Some(error);
            }
        }
    }

    fn inject_listen_upgrade_error(&mut self, _: RequestId, error: ProtocolsHandlerUpgrErr<io::Error>) {
        match error {
            ProtocolsHandlerUpgrErr::Timeout
            | ProtocolsHandlerUpgrErr::Timer
            | ProtocolsHandlerUpgrErr::Upgrade(UpgradeError::Select(NegotiationError::Failed)) => {}
            _ => {
                self.pending_error = Some(error);
            }
        }
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        self.keep_alive
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<ProtocolsHandlerEventType<Rq, Rs>> {
        if let Some(err) = self.pending_error.take() {
            return Poll::Ready(ProtocolsHandlerEvent::Close(err));
        }
        if let Some(event) = self.pending_events.pop_front() {
            return Poll::Ready(ProtocolsHandlerEvent::Custom(event));
        }
        if self.pending_events.capacity() > EMPTY_QUEUE_SHRINK_THRESHOLD {
            self.pending_events.shrink_to_fit();
        }
        while let Poll::Ready(Some(result)) = self.inbound.poll_next_unpin(cx) {
            if let Ok((request_id, request)) = result {
                self.keep_alive = KeepAlive::Yes;
                return Poll::Ready(ProtocolsHandlerEvent::Custom(HandlerOutEvent::ReceivedRequest {
                    request_id,
                    request,
                }));
            }
        }
        if let Some(HandlerInEvent { request, request_id }) = self.outbound.pop_front() {
            let protocol = self.new_outbound_protocol(request_id, request);
            return Poll::Ready(ProtocolsHandlerEvent::OutboundSubstreamRequest { protocol });
        }
        if self.outbound.capacity() > EMPTY_QUEUE_SHRINK_THRESHOLD {
            self.outbound.shrink_to_fit();
        }
        if self.inbound.is_empty() && self.keep_alive.is_yes() {
            let until = Instant::now() + self.substream_timeout + self.keep_alive_timeout;
            self.keep_alive = KeepAlive::Until(until);
        }
        Poll::Pending
    }
}
