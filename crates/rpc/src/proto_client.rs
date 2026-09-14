use anyhow::{Context, Result};
use collections::{HashMap, TypeIdHashMap};
use futures::{
    Future, FutureExt as _, Stream, StreamExt as _,
    channel::oneshot,
    future::{BoxFuture, LocalBoxFuture},
    stream::BoxStream,
};
use gpui::{AnyEntity, AnyWeakEntity, App, AsyncApp, BackgroundExecutor, Entity, FutureExt as _};
use parking_lot::Mutex;
use proto::{
    AnyTypedEnvelope, EntityMessage, Envelope, EnvelopedMessage, LspRequestId, LspRequestMessage,
    RequestMessage, TypedEnvelope, error::ErrorExt as _,
};
use std::{
    any::{Any, TypeId},
    sync::{
        Arc, OnceLock,
        atomic::{self, AtomicU64},
    },
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct AnyProtoClient(Arc<State>);

type RequestIds = Arc<
    Mutex<
        HashMap<
            LspRequestId,
            oneshot::Sender<
                Result<
                    Option<TypedEnvelope<Vec<proto::ProtoLspResponse<Box<dyn AnyTypedEnvelope>>>>>,
                >,
            >,
        >,
    >,
>;

static NEXT_LSP_REQUEST_ID: OnceLock<Arc<AtomicU64>> = OnceLock::new();
static REQUEST_IDS: OnceLock<RequestIds> = OnceLock::new();

struct State {
    client: Arc<dyn ProtoClient>,
    next_lsp_request_id: Arc<AtomicU64>,
    request_ids: RequestIds,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("next_lsp_request_id", &self.next_lsp_request_id)
            .field("request_ids", &self.request_ids)
            .finish_non_exhaustive()
    }
}

pub trait ProtoClient: Send + Sync {
    fn request(
        &self,
        envelope: Envelope,
        request_type: &'static str,
    ) -> BoxFuture<'static, Result<Envelope>>;

    fn request_stream(
        &self,
        envelope: Envelope,
        request_type: &'static str,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Result<Envelope>>>> {
        async move {
            anyhow::bail!(
                "stream requests are not supported for {request_type}: {:?}",
                envelope.payload
            )
        }
        .boxed()
    }

    fn send(&self, envelope: Envelope, message_type: &'static str) -> Result<()>;

    fn send_response(&self, envelope: Envelope, message_type: &'static str) -> Result<()>;

    fn message_handler_set(&self) -> &parking_lot::Mutex<ProtoMessageHandlerSet>;

    fn is_via_collab(&self) -> bool;
    fn has_wsl_interop(&self) -> bool;
}

/// Messages held because they arrived after `ChannelClient` started reading
/// the socket and before `Project::remote` subscribed stores.
///
/// 64: that window is a constructor, not a subscription. The known early push
/// is one `ExternalAgentsUpdated`; a burst already in the socket may add a
/// handful of entity updates. 64 is more than ten times that, and a hard cap
/// so a handshake that never reaches `Project::remote` (cancel, connect
/// failure, reconnect loop) cannot grow for the client's lifetime.
pub const MAX_QUEUED_EARLY_MESSAGES: usize = 64;

/// How long those messages may be held waiting for `Project::remote`.
///
/// 30s: well above a slow constructor (seconds) and the same bound this
/// connection already uses when a stream consumer has stopped reading.
/// Past 30s this is no longer a handshake, and further envelopes must be
/// reported unhandled again. Compared with `BackgroundExecutor::now` so
/// tests can advance it.
pub const MAX_QUEUED_EARLY_MESSAGES_AGE: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct ProtoMessageHandlerSet {
    pub entity_types_by_message_type: TypeIdHashMap<TypeId>,
    pub entities_by_type_and_remote_id: HashMap<(TypeId, u64), EntityMessageSubscriber>,
    pub entity_id_extractors: TypeIdHashMap<fn(&dyn AnyTypedEnvelope) -> u64>,
    pub entities_by_message_type: TypeIdHashMap<AnyWeakEntity>,
    pub message_handlers: TypeIdHashMap<ProtoMessageHandler>,
    /// When true, messages that arrive before their handler or entity is
    /// registered are held instead of reported as unhandled. SSH/WSL/Docker
    /// start reading the socket before `Project::remote` can subscribe.
    pub queue_early_messages: bool,
    queued_messages: Vec<Box<dyn AnyTypedEnvelope>>,
    queued_early_messages_since: Option<Instant>,
}

pub type ProtoMessageHandler = Arc<
    dyn Send
        + Sync
        + Fn(
            AnyEntity,
            Box<dyn AnyTypedEnvelope>,
            AnyProtoClient,
            AsyncApp,
        ) -> LocalBoxFuture<'static, Result<()>>,
>;

impl ProtoMessageHandlerSet {
    pub fn clear(&mut self) {
        self.message_handlers.clear();
        self.entities_by_message_type.clear();
        self.entities_by_type_and_remote_id.clear();
        self.entity_id_extractors.clear();
        self.queued_messages.clear();
        self.queued_early_messages_since = None;
        self.queue_early_messages = false;
    }

    fn add_message_handler(
        &mut self,
        message_type_id: TypeId,
        entity: gpui::AnyWeakEntity,
        handler: ProtoMessageHandler,
    ) {
        self.entities_by_message_type
            .insert(message_type_id, entity);
        let prev_handler = self.message_handlers.insert(message_type_id, handler);
        if prev_handler.is_some() {
            panic!("registered handler for the same message twice");
        }
    }

    fn add_entity_message_handler(
        &mut self,
        message_type_id: TypeId,
        entity_type_id: TypeId,
        entity_id_extractor: fn(&dyn AnyTypedEnvelope) -> u64,
        handler: ProtoMessageHandler,
    ) {
        self.entity_id_extractors
            .entry(message_type_id)
            .or_insert(entity_id_extractor);
        self.entity_types_by_message_type
            .insert(message_type_id, entity_type_id);
        let prev_handler = self.message_handlers.insert(message_type_id, handler);
        if prev_handler.is_some() {
            panic!("registered handler for the same message twice");
        }
    }

    pub fn handle_message(
        this: &parking_lot::Mutex<Self>,
        message: Box<dyn AnyTypedEnvelope>,
        client: AnyProtoClient,
        cx: AsyncApp,
    ) -> Option<LocalBoxFuture<'static, Result<()>>> {
        let payload_type_id = message.payload_type_id();
        let now = cx.background_executor().now();
        let mut this = this.lock();
        let queue = this.queue_early_messages;
        let Some(handler) = this.message_handlers.get(&payload_type_id).cloned() else {
            return Self::maybe_queue(&mut this, message, queue, now);
        };
        let entity = if let Some(entity) = this.entities_by_message_type.get(&payload_type_id) {
            match entity.upgrade() {
                Some(entity) => entity,
                None => return Self::maybe_queue(&mut this, message, queue, now),
            }
        } else {
            let Some(extract_entity_id) = this.entity_id_extractors.get(&payload_type_id).copied()
            else {
                return Self::maybe_queue(&mut this, message, queue, now);
            };
            let Some(entity_type_id) = this
                .entity_types_by_message_type
                .get(&payload_type_id)
                .copied()
            else {
                return Self::maybe_queue(&mut this, message, queue, now);
            };
            let entity_id = (extract_entity_id)(message.as_ref());
            match this
                .entities_by_type_and_remote_id
                .get_mut(&(entity_type_id, entity_id))
            {
                Some(EntityMessageSubscriber::Pending(pending)) => {
                    pending.push(message);
                    return None;
                }
                Some(EntityMessageSubscriber::Entity { handle }) => match handle.upgrade() {
                    Some(entity) => entity,
                    None => return Self::maybe_queue(&mut this, message, queue, now),
                },
                None => return Self::maybe_queue(&mut this, message, queue, now),
            }
        };
        drop(this);
        Some(handler(entity, message, client, cx))
    }

    fn maybe_queue(
        this: &mut Self,
        message: Box<dyn AnyTypedEnvelope>,
        queue: bool,
        now: Instant,
    ) -> Option<LocalBoxFuture<'static, Result<()>>> {
        if !queue {
            return None;
        }

        let since = *this.queued_early_messages_since.get_or_insert(now);
        if this.queued_messages.len() >= MAX_QUEUED_EARLY_MESSAGES
            || now.saturating_duration_since(since) >= MAX_QUEUED_EARLY_MESSAGES_AGE
        {
            this.queue_early_messages = false;
            return None;
        }

        this.queued_messages.push(message);
        Some(async { Ok(()) }.boxed_local())
    }

    fn take_queued_early_messages(&mut self) -> Vec<Box<dyn AnyTypedEnvelope>> {
        self.queue_early_messages = false;
        self.queued_early_messages_since = None;
        std::mem::take(&mut self.queued_messages)
    }

    pub fn queued_early_message_count(&self) -> usize {
        self.queued_messages.len()
    }
}

pub enum EntityMessageSubscriber {
    Entity { handle: AnyWeakEntity },
    Pending(Vec<Box<dyn AnyTypedEnvelope>>),
}

impl std::fmt::Debug for EntityMessageSubscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EntityMessageSubscriber::Entity { handle } => f
                .debug_struct("EntityMessageSubscriber::Entity")
                .field("handle", handle)
                .finish(),
            EntityMessageSubscriber::Pending(vec) => f
                .debug_struct("EntityMessageSubscriber::Pending")
                .field(
                    "envelopes",
                    &vec.iter()
                        .map(|envelope| envelope.payload_type_name())
                        .collect::<Vec<_>>(),
                )
                .finish(),
        }
    }
}

impl<T> From<Arc<T>> for AnyProtoClient
where
    T: ProtoClient + 'static,
{
    fn from(client: Arc<T>) -> Self {
        Self::new(client)
    }
}

impl AnyProtoClient {
    pub fn new<T: ProtoClient + 'static>(client: Arc<T>) -> Self {
        Self(Arc::new(State {
            client,
            next_lsp_request_id: NEXT_LSP_REQUEST_ID
                .get_or_init(|| Arc::new(AtomicU64::new(0)))
                .clone(),
            request_ids: REQUEST_IDS.get_or_init(RequestIds::default).clone(),
        }))
    }

    pub fn is_via_collab(&self) -> bool {
        self.0.client.is_via_collab()
    }

    pub fn request<T: RequestMessage>(
        &self,
        request: T,
    ) -> impl Future<Output = Result<T::Response>> + use<T> {
        let envelope = request.into_envelope(0, None, None);
        let response = self.0.client.request(envelope, T::NAME);
        async move {
            T::Response::from_envelope(response.await?)
                .context("received response of the wrong type")
        }
    }

    pub fn request_stream<T: RequestMessage>(
        &self,
        request: T,
    ) -> impl Future<Output = Result<BoxStream<'static, Result<T::Response>>>> + use<T> {
        let envelope = request.into_envelope(0, None, None);
        let response_stream = self.0.client.request_stream(envelope, T::NAME);
        async move {
            Ok(response_stream
                .await?
                .map(|response| {
                    T::Response::from_envelope(response?)
                        .context("received response of the wrong type")
                })
                .boxed())
        }
    }

    pub fn send<T: EnvelopedMessage>(&self, request: T) -> Result<()> {
        let envelope = request.into_envelope(0, None, None);
        self.0.client.send(envelope, T::NAME)
    }

    pub fn send_response<T: EnvelopedMessage>(&self, request_id: u32, request: T) -> Result<()> {
        let envelope = request.into_envelope(0, Some(request_id), None);
        self.0.client.send(envelope, T::NAME)
    }

    pub fn request_lsp<T>(
        &self,
        project_id: u64,
        server_id: Option<u64>,
        timeout: Duration,
        executor: BackgroundExecutor,
        request: T,
    ) -> impl Future<
        Output = Result<Option<TypedEnvelope<Vec<proto::ProtoLspResponse<T::Response>>>>>,
    > + use<T>
    where
        T: LspRequestMessage,
    {
        let new_id = LspRequestId(
            self.0
                .next_lsp_request_id
                .fetch_add(1, atomic::Ordering::Acquire),
        );
        let (tx, rx) = oneshot::channel();
        {
            self.0.request_ids.lock().insert(new_id, tx);
        }

        let query = proto::LspQuery {
            project_id,
            server_id,
            lsp_request_id: new_id.0,
            request: Some(request.to_proto_query()),
        };
        let request = self.request(query);
        let request_ids = self.0.request_ids.clone();
        async move {
            match request.await {
                Ok(_request_enqueued) => {}
                Err(e) => {
                    request_ids.lock().remove(&new_id);
                    return Err(e).context("sending LSP proto request");
                }
            }

            let response = rx.with_timeout(timeout, &executor).await;
            {
                request_ids.lock().remove(&new_id);
            }
            match response {
                Ok(Ok(response)) => {
                    let response = response
                        .context("waiting for LSP proto response")?
                        .map(|response| {
                            anyhow::Ok(TypedEnvelope {
                                payload: response
                                    .payload
                                    .into_iter()
                                    .map(|lsp_response| lsp_response.into_response::<T>())
                                    .collect::<Result<Vec<_>>>()?,
                                sender_id: response.sender_id,
                                original_sender_id: response.original_sender_id,
                                message_id: response.message_id,
                                received_at: response.received_at,
                            })
                        })
                        .transpose()
                        .context("converting LSP proto response")?;
                    Ok(response)
                }
                Err(_cancelled_due_timeout) => Ok(None),
                Ok(Err(_channel_dropped)) => Ok(None),
            }
        }
    }

    pub fn send_lsp_response<T: LspRequestMessage>(
        &self,
        project_id: u64,
        peer_id: proto::PeerId,
        lsp_request_id: LspRequestId,
        server_responses: HashMap<u64, T::Response>,
    ) -> Result<()> {
        self.send(proto::LspQueryResponse {
            project_id,
            peer_id: Some(peer_id),
            lsp_request_id: lsp_request_id.0,
            responses: server_responses
                .into_iter()
                .map(|(server_id, response)| proto::LspResponse {
                    server_id,
                    response: Some(T::response_to_proto_query(response)),
                })
                .collect(),
        })
    }

    pub fn handle_lsp_response(&self, mut envelope: TypedEnvelope<proto::LspQueryResponse>) {
        let request_id = LspRequestId(envelope.payload.lsp_request_id);
        let mut response_senders = self.0.request_ids.lock();
        if let Some(tx) = response_senders.remove(&request_id) {
            let responses = envelope.payload.responses.drain(..).collect::<Vec<_>>();
            tx.send(Ok(Some(proto::TypedEnvelope {
                sender_id: envelope.sender_id,
                original_sender_id: envelope.original_sender_id,
                message_id: envelope.message_id,
                received_at: envelope.received_at,
                payload: responses
                    .into_iter()
                    .filter_map(|response| {
                        use proto::lsp_response::Response;

                        let server_id = response.server_id;
                        let response = match response.response? {
                            Response::GetReferencesResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetDocumentColorResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetHoverResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetCodeActionsResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetSignatureHelpResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetCodeLensResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetDocumentDiagnosticsResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetDefinitionResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetEditPredictionDefinitionResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetDeclarationResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetTypeDefinitionResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetEditPredictionTypeDefinitionResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetImplementationResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::InlayHintsResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::SemanticTokensResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetFoldingRangesResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetDocumentSymbolsResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetDocumentLinksResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::PrepareCallHierarchyResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetIncomingCallsResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                            Response::GetOutgoingCallsResponse(response) => {
                                to_any_envelope(&envelope, response)
                            }
                        };
                        Some(proto::ProtoLspResponse {
                            server_id,
                            response,
                        })
                    })
                    .collect(),
            })))
            .ok();
        }
    }

    pub fn add_request_handler<M, E, H, F>(&self, entity: gpui::WeakEntity<E>, handler: H)
    where
        M: RequestMessage,
        E: 'static,
        H: 'static + Sync + Fn(Entity<E>, TypedEnvelope<M>, AsyncApp) -> F + Send + Sync,
        F: 'static + Future<Output = Result<M::Response>>,
    {
        self.0
            .client
            .message_handler_set()
            .lock()
            .add_message_handler(
                TypeId::of::<M>(),
                entity.into(),
                Arc::new(move |entity, envelope, client, cx| {
                    let entity = entity.downcast::<E>().unwrap();
                    let envelope = envelope.into_any().downcast::<TypedEnvelope<M>>().unwrap();
                    let request_id = envelope.message_id();
                    handler(entity, *envelope, cx)
                        .then(move |result| async move {
                            match result {
                                Ok(response) => {
                                    client.send_response(request_id, response)?;
                                    Ok(())
                                }
                                Err(error) => {
                                    client.send_response(request_id, error.to_proto())?;
                                    Err(error)
                                }
                            }
                        })
                        .boxed_local()
                }),
            )
    }

    pub fn add_entity_request_handler<M, E, H, F>(&self, handler: H)
    where
        M: EnvelopedMessage + RequestMessage + EntityMessage,
        E: 'static,
        H: 'static + Sync + Send + Fn(gpui::Entity<E>, TypedEnvelope<M>, AsyncApp) -> F,
        F: 'static + Future<Output = Result<M::Response>>,
    {
        let message_type_id = TypeId::of::<M>();
        let entity_type_id = TypeId::of::<E>();
        let entity_id_extractor = |envelope: &dyn AnyTypedEnvelope| {
            (envelope as &dyn Any)
                .downcast_ref::<TypedEnvelope<M>>()
                .unwrap()
                .payload
                .remote_entity_id()
        };
        self.0
            .client
            .message_handler_set()
            .lock()
            .add_entity_message_handler(
                message_type_id,
                entity_type_id,
                entity_id_extractor,
                Arc::new(move |entity, envelope, client, cx| {
                    let entity = entity.downcast::<E>().unwrap();
                    let envelope = envelope.into_any().downcast::<TypedEnvelope<M>>().unwrap();
                    let request_id = envelope.message_id();
                    handler(entity, *envelope, cx)
                        .then(move |result| async move {
                            match result {
                                Ok(response) => {
                                    client.send_response(request_id, response)?;
                                    Ok(())
                                }
                                Err(error) => {
                                    client.send_response(request_id, error.to_proto())?;
                                    Err(error)
                                }
                            }
                        })
                        .boxed_local()
                }),
            );
    }

    pub fn add_entity_stream_request_handler<M, E, H, F, S>(&self, handler: H)
    where
        M: EnvelopedMessage + RequestMessage + EntityMessage,
        E: 'static,
        H: 'static + Sync + Send + Fn(gpui::Entity<E>, TypedEnvelope<M>, AsyncApp) -> F,
        F: 'static + Future<Output = Result<S>>,
        S: 'static + Stream<Item = Result<M::Response>>,
    {
        let message_type_id = TypeId::of::<M>();
        let entity_type_id = TypeId::of::<E>();
        let entity_id_extractor = |envelope: &dyn AnyTypedEnvelope| {
            (envelope as &dyn Any)
                .downcast_ref::<TypedEnvelope<M>>()
                .unwrap()
                .payload
                .remote_entity_id()
        };
        self.0
            .client
            .message_handler_set()
            .lock()
            .add_entity_message_handler(
                message_type_id,
                entity_type_id,
                entity_id_extractor,
                Arc::new(move |entity, envelope, client, cx| {
                    let entity = entity.downcast::<E>().unwrap();
                    let envelope = envelope.into_any().downcast::<TypedEnvelope<M>>().unwrap();
                    let request_id = envelope.message_id();
                    let stream = handler(entity, *envelope, cx);
                    async move {
                        // An Error response is itself a terminal stream frame on
                        // both transports (Peer and ChannelClient), so we don't
                        // need to follow it with an EndStream.
                        match stream.await {
                            Ok(stream) => {
                                futures::pin_mut!(stream);
                                while let Some(result) = stream.next().await {
                                    match result {
                                        Ok(response) => {
                                            client.send_response(request_id, response)?
                                        }
                                        Err(error) => {
                                            client.send_response(request_id, error.to_proto())?;
                                            return Err(error);
                                        }
                                    }
                                }
                                client.send_response(request_id, proto::EndStream {})?;
                                Ok(())
                            }
                            Err(error) => {
                                client.send_response(request_id, error.to_proto())?;
                                Err(error)
                            }
                        }
                    }
                    .boxed_local()
                }),
            );
    }

    pub fn add_entity_message_handler<M, E, H, F>(&self, handler: H)
    where
        M: EnvelopedMessage + EntityMessage,
        E: 'static,
        H: 'static + Sync + Send + Fn(gpui::Entity<E>, TypedEnvelope<M>, AsyncApp) -> F,
        F: 'static + Future<Output = Result<()>>,
    {
        let message_type_id = TypeId::of::<M>();
        let entity_type_id = TypeId::of::<E>();
        let entity_id_extractor = |envelope: &dyn AnyTypedEnvelope| {
            (envelope as &dyn Any)
                .downcast_ref::<TypedEnvelope<M>>()
                .unwrap()
                .payload
                .remote_entity_id()
        };
        self.0
            .client
            .message_handler_set()
            .lock()
            .add_entity_message_handler(
                message_type_id,
                entity_type_id,
                entity_id_extractor,
                Arc::new(move |entity, envelope, _, cx| {
                    let entity = entity.downcast::<E>().unwrap();
                    let envelope = envelope.into_any().downcast::<TypedEnvelope<M>>().unwrap();
                    handler(entity, *envelope, cx).boxed_local()
                }),
            );
    }

    pub fn subscribe_to_entity<E: 'static>(&self, remote_id: u64, entity: &Entity<E>) {
        let id = (TypeId::of::<E>(), remote_id);

        let mut message_handlers = self.0.client.message_handler_set().lock();
        if message_handlers
            .entities_by_type_and_remote_id
            .contains_key(&id)
        {
            panic!("already subscribed to entity");
        }

        message_handlers.entities_by_type_and_remote_id.insert(
            id,
            EntityMessageSubscriber::Entity {
                handle: entity.downgrade().into(),
            },
        );
    }

    /// Dispatches messages held because they arrived before this client had
    /// handlers, then stops holding later ones. Call once the SSH/WSL/Docker
    /// project has subscribed its stores.
    pub fn flush_queued_early_messages(&self, cx: &App) {
        let queued = {
            let mut handlers = self.0.client.message_handler_set().lock();
            handlers.take_queued_early_messages()
        };
        let async_cx = cx.to_async();
        for message in queued {
            let type_name = message.payload_type_name();
            if let Some(future) = ProtoMessageHandlerSet::handle_message(
                self.0.client.message_handler_set(),
                message,
                self.clone(),
                async_cx.clone(),
            ) {
                cx.foreground_executor()
                    .spawn(async move {
                        if let Err(error) = future.await {
                            tracing::error!(
                                "error handling queued remote message. type:{type_name}, error:{error:#}"
                            );
                        }
                    })
                    .detach();
            } else {
                tracing::debug!("dropping queued remote message name:{type_name}");
            }
        }
    }

    pub fn has_wsl_interop(&self) -> bool {
        self.0.client.has_wsl_interop()
    }
}

fn to_any_envelope<T: EnvelopedMessage>(
    envelope: &TypedEnvelope<proto::LspQueryResponse>,
    response: T,
) -> Box<dyn AnyTypedEnvelope> {
    Box::new(proto::TypedEnvelope {
        sender_id: envelope.sender_id,
        original_sender_id: envelope.original_sender_id,
        message_id: envelope.message_id,
        received_at: envelope.received_at,
        payload: response,
    }) as Box<_>
}

#[cfg(any(test, feature = "test-support"))]
pub struct NoopProtoClient {
    handler_set: parking_lot::Mutex<ProtoMessageHandlerSet>,
}

#[cfg(any(test, feature = "test-support"))]
impl NoopProtoClient {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            handler_set: parking_lot::Mutex::new(ProtoMessageHandlerSet::default()),
        })
    }
}

#[cfg(any(test, feature = "test-support"))]
impl ProtoClient for NoopProtoClient {
    fn request(
        &self,
        _: proto::Envelope,
        _: &'static str,
    ) -> futures::future::BoxFuture<'static, Result<proto::Envelope>> {
        unimplemented!()
    }
    fn send(&self, _: proto::Envelope, _: &'static str) -> Result<()> {
        Ok(())
    }
    fn send_response(&self, _: proto::Envelope, _: &'static str) -> Result<()> {
        Ok(())
    }
    fn message_handler_set(&self) -> &parking_lot::Mutex<ProtoMessageHandlerSet> {
        &self.handler_set
    }
    fn is_via_collab(&self) -> bool {
        false
    }
    fn has_wsl_interop(&self) -> bool {
        false
    }
}
