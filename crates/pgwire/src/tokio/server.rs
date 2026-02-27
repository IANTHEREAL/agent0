use std::io::Error as IOError;
use std::sync::Arc;

use bytes::Buf;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
#[cfg(any(feature = "_ring", feature = "_aws-lc-rs"))]
use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tokio_util::codec::{Decoder, Encoder, Framed};
use tokio_util::sync::CancellationToken;

use crate::api::auth::StartupHandler;
use crate::api::copy::CopyHandler;
use crate::api::query::SimpleQueryHandler;
use crate::api::query::{send_ready_for_query, ExtendedQueryHandler};
use crate::api::{
    ClientInfo, ClientPortalStore, DefaultClient, ErrorHandler, PgWireConnectionState,
    PgWireServerHandlers,
};
use crate::api::METADATA_USER;
use crate::error::{ErrorInfo, PgWireError, PgWireResult};
use crate::messages::response::ReadyForQuery;
use crate::messages::response::{SslResponse, TransactionStatus};
use crate::messages::startup::{SslRequest, Startup};
use crate::messages::{Message, PgWireBackendMessage, PgWireFrontendMessage};

#[non_exhaustive]
#[derive(Debug, new)]
pub struct PgWireMessageServerCodec<S> {
    pub client_info: DefaultClient<S>,
}

impl<S> Decoder for PgWireMessageServerCodec<S> {
    type Item = PgWireFrontendMessage;
    type Error = PgWireError;

    fn decode(&mut self, src: &mut bytes::BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        match self.client_info.state() {
            PgWireConnectionState::AwaitingSslRequest => {
                if src.remaining() >= SslRequest::BODY_SIZE {
                    self.client_info
                        .set_state(PgWireConnectionState::AwaitingStartup);

                    if let Some(request) = SslRequest::decode(src)? {
                        return Ok(Some(PgWireFrontendMessage::SslRequest(Some(request))));
                    } else {
                        // this is not a real message, but to indicate that
                        //  client will not init ssl handshake
                        return Ok(Some(PgWireFrontendMessage::SslRequest(None)));
                    }
                }

                Ok(None)
            }

            PgWireConnectionState::AwaitingStartup => {
                if let Some(startup) = Startup::decode(src)? {
                    Ok(Some(PgWireFrontendMessage::Startup(startup)))
                } else {
                    Ok(None)
                }
            }

            _ => PgWireFrontendMessage::decode(src),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::process_message;
    use super::process_error;
    use super::PgWireMessageServerCodec;
    use crate::api::auth::noop::NoopStartupHandler;
    use crate::api::copy::NoopCopyHandler;
    use crate::api::query::PlaceholderExtendedQueryHandler;
    use crate::api::query::SimpleQueryHandler;
    use crate::api::results::Response;
    use crate::api::{ClientInfo, DefaultClient, PgWireConnectionState};
    use crate::error::{ErrorInfo, PgWireError, PgWireResult};
    use crate::messages::copy::CopyDone;
    use crate::messages::extendedquery::Sync as SyncMessage;
    use crate::messages::response::TransactionStatus;
    use crate::messages::simplequery::Query;
    use crate::messages::startup::Startup;
    use crate::messages::{PgWireBackendMessage, PgWireFrontendMessage};
    use async_trait::async_trait;
    use bytes::BytesMut;
    use futures::Sink;
    use std::fmt::Debug;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use tokio::io::{duplex, AsyncReadExt};
    use tokio::time::{timeout, Duration};
    use tokio_util::codec::Framed;

    #[derive(Debug)]
    struct DummyStartupHandler;

    impl NoopStartupHandler for DummyStartupHandler {}

    #[derive(Debug)]
    struct DummySimpleQueryHandler;

    #[async_trait]
    impl SimpleQueryHandler for DummySimpleQueryHandler {
        async fn do_query<'a, 'b: 'a, C>(
            &'b self,
            _client: &mut C,
            _query: &'a str,
        ) -> PgWireResult<Vec<Response<'a>>>
        where
            C: crate::api::ClientInfo
                + Sink<crate::messages::PgWireBackendMessage>
                + Unpin
                + Send
                + Sync,
            C::Error: Debug,
            PgWireError: From<<C as Sink<crate::messages::PgWireBackendMessage>>::Error>,
        {
            Ok(vec![])
        }
    }

    async fn read_backend_message(client_io: &mut tokio::io::DuplexStream) -> PgWireBackendMessage {
        let mut buf = BytesMut::with_capacity(256);
        loop {
            if let Some(msg) = PgWireBackendMessage::decode(&mut buf).expect("decode backend msg") {
                return msg;
            }
            let n = client_io.read_buf(&mut buf).await.expect("read");
            assert!(n > 0, "EOF before backend message");
        }
    }

    fn get_error_field<'a>(
        err: &'a crate::messages::response::ErrorResponse,
        code: u8,
    ) -> Option<&'a str> {
        err.fields
            .iter()
            .find(|(field_code, _)| *field_code == code)
            .map(|(_, value)| value.as_str())
    }

    #[tokio::test]
    async fn copy_done_simple_protocol_uses_current_transaction_status() {
        let (mut client_io, server_io) = duplex(64);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let client_info = DefaultClient::<String>::new(addr, false);
        let mut socket = Framed::new(server_io, PgWireMessageServerCodec::new(client_info));
        socket.set_state(PgWireConnectionState::CopyInProgress(false));
        socket.set_transaction_status(TransactionStatus::Transaction);

        process_message(
            PgWireFrontendMessage::CopyDone(CopyDone::new()),
            &mut socket,
            Arc::new(DummyStartupHandler),
            Arc::new(DummySimpleQueryHandler),
            Arc::new(PlaceholderExtendedQueryHandler),
            Arc::new(NoopCopyHandler),
        )
        .await
        .unwrap();

        let mut buf = [0u8; 6];
        client_io.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, [b'Z', 0, 0, 0, 5, b'T']);
    }

    #[tokio::test]
    async fn missing_user_in_startup_returns_28000_and_closes_socket() {
        let (mut client_io, server_io) = duplex(256);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let client_info = DefaultClient::<String>::new(addr, false);
        let mut socket = Framed::new(server_io, PgWireMessageServerCodec::new(client_info));
        socket.set_state(PgWireConnectionState::AwaitingStartup);

        let startup = Startup::default();
        let err = process_message(
            PgWireFrontendMessage::Startup(startup),
            &mut socket,
            Arc::new(DummyStartupHandler),
            Arc::new(DummySimpleQueryHandler),
            Arc::new(PlaceholderExtendedQueryHandler),
            Arc::new(NoopCopyHandler),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PgWireError::UserNameRequired));

        process_error(&mut socket, err, false).await.unwrap();

        let PgWireBackendMessage::ErrorResponse(err) = read_backend_message(&mut client_io).await
        else {
            panic!("expected ErrorResponse");
        };
        assert_eq!(get_error_field(&err, b'S'), Some("FATAL"));
        assert_eq!(get_error_field(&err, b'C'), Some("28000"));

        let mut extra = [0u8; 1];
        let n = timeout(Duration::from_millis(100), client_io.read(&mut extra))
            .await
            .expect("read should not block")
            .expect("read");
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn not_ready_for_query_is_error_and_waits_for_sync() {
        let (mut client_io, server_io) = duplex(512);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let client_info = DefaultClient::<String>::new(addr, false);
        let mut socket = Framed::new(server_io, PgWireMessageServerCodec::new(client_info));
        socket.set_state(PgWireConnectionState::QueryInProgress);

        let err = process_message(
            PgWireFrontendMessage::Query(Query::new("SELECT 1".to_owned())),
            &mut socket,
            Arc::new(DummyStartupHandler),
            Arc::new(DummySimpleQueryHandler),
            Arc::new(PlaceholderExtendedQueryHandler),
            Arc::new(NoopCopyHandler),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PgWireError::NotReadyForQuery));

        // Even for simple protocol, NotReadyForQuery should be treated as a protocol
        // violation and the server should wait for Sync to resynchronize.
        process_error(&mut socket, err, false).await.unwrap();
        assert_eq!(socket.state(), PgWireConnectionState::AwaitingSync);

        let PgWireBackendMessage::ErrorResponse(err) = read_backend_message(&mut client_io).await
        else {
            panic!("expected ErrorResponse");
        };
        assert_eq!(get_error_field(&err, b'S'), Some("ERROR"));
        assert_eq!(get_error_field(&err, b'C'), Some("08P01"));

        process_message(
            PgWireFrontendMessage::Sync(SyncMessage::new()),
            &mut socket,
            Arc::new(DummyStartupHandler),
            Arc::new(DummySimpleQueryHandler),
            Arc::new(PlaceholderExtendedQueryHandler),
            Arc::new(NoopCopyHandler),
        )
        .await
        .unwrap();

        let PgWireBackendMessage::ReadyForQuery(rfq) = read_backend_message(&mut client_io).await
        else {
            panic!("expected ReadyForQuery");
        };
        // Per PostgreSQL protocol: 'E' means "in a failed transaction block".
        // When idle (no BEGIN), to_error_state() correctly preserves Idle.
        assert_eq!(rfq.status, TransactionStatus::Idle);
    }

    #[tokio::test]
    async fn not_ready_for_query_in_transaction_returns_error_status() {
        let (mut client_io, server_io) = duplex(512);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let client_info = DefaultClient::<String>::new(addr, false);
        let mut socket = Framed::new(server_io, PgWireMessageServerCodec::new(client_info));
        socket.set_state(PgWireConnectionState::QueryInProgress);
        socket.set_transaction_status(TransactionStatus::Transaction);

        let err = process_message(
            PgWireFrontendMessage::Query(Query::new("SELECT 1".to_owned())),
            &mut socket,
            Arc::new(DummyStartupHandler),
            Arc::new(DummySimpleQueryHandler),
            Arc::new(PlaceholderExtendedQueryHandler),
            Arc::new(NoopCopyHandler),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PgWireError::NotReadyForQuery));

        process_error(&mut socket, err, false).await.unwrap();
        assert_eq!(socket.state(), PgWireConnectionState::AwaitingSync);

        let PgWireBackendMessage::ErrorResponse(err) = read_backend_message(&mut client_io).await
        else {
            panic!("expected ErrorResponse");
        };
        assert_eq!(get_error_field(&err, b'S'), Some("ERROR"));
        assert_eq!(get_error_field(&err, b'C'), Some("08P01"));

        process_message(
            PgWireFrontendMessage::Sync(SyncMessage::new()),
            &mut socket,
            Arc::new(DummyStartupHandler),
            Arc::new(DummySimpleQueryHandler),
            Arc::new(PlaceholderExtendedQueryHandler),
            Arc::new(NoopCopyHandler),
        )
        .await
        .unwrap();

        let PgWireBackendMessage::ReadyForQuery(rfq) = read_backend_message(&mut client_io).await
        else {
            panic!("expected ReadyForQuery");
        };
        // Per PostgreSQL protocol: 'E' means "in a failed transaction block".
        // When inside a transaction, to_error_state() correctly transitions to Error.
        assert_eq!(rfq.status, TransactionStatus::Error);
    }

    #[tokio::test]
    async fn fatal_user_error_closes_socket_and_preserves_sqlstate() {
        let (mut client_io, server_io) = duplex(256);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let client_info = DefaultClient::<String>::new(addr, false);
        let mut socket = Framed::new(server_io, PgWireMessageServerCodec::new(client_info));
        socket.set_state(PgWireConnectionState::ReadyForQuery);

        let error = PgWireError::UserError(Box::new(ErrorInfo::new(
            "FATAL".to_owned(),
            "3D000".to_owned(),
            "database \"nope\" does not exist".to_owned(),
        )));
        process_error(&mut socket, error, false).await.unwrap();

        let PgWireBackendMessage::ErrorResponse(err) = read_backend_message(&mut client_io).await
        else {
            panic!("expected ErrorResponse");
        };
        assert_eq!(get_error_field(&err, b'S'), Some("FATAL"));
        assert_eq!(get_error_field(&err, b'C'), Some("3D000"));

        let mut extra = [0u8; 1];
        let n = timeout(Duration::from_millis(100), client_io.read(&mut extra))
            .await
            .expect("read should not block")
            .expect("read");
        assert_eq!(n, 0);
    }

    /// Verify that `process_message` in `AwaitingStartup` state routes
    /// messages to `on_startup`, never to `on_parse`/`on_query`. This is the
    /// framework-level guarantee that downstream handlers can rely on:
    /// query methods are only called after authentication completes.
    #[tokio::test]
    async fn awaiting_startup_routes_parse_to_startup_handler_not_query() {
        let (_client_io, server_io) = duplex(512);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let client_info = DefaultClient::<String>::new(addr, false);
        let mut socket = Framed::new(server_io, PgWireMessageServerCodec::new(client_info));
        socket.set_state(PgWireConnectionState::AwaitingStartup);

        // A Parse message in AwaitingStartup goes to on_startup (which
        // treats it as an unknown message type and returns Ok(())), NOT
        // to on_parse on the extended query handler.
        let parse_msg = PgWireFrontendMessage::Parse(
            crate::messages::extendedquery::Parse::new(None, "SELECT 1".to_string(), vec![]),
        );
        process_message(
            parse_msg,
            &mut socket,
            Arc::new(DummyStartupHandler),
            Arc::new(DummySimpleQueryHandler),
            Arc::new(PlaceholderExtendedQueryHandler),
            Arc::new(NoopCopyHandler),
        )
        .await
        .expect("Parse in AwaitingStartup must route to startup handler, not query handler");
    }
}

impl<S> Encoder<PgWireBackendMessage> for PgWireMessageServerCodec<S> {
    type Error = IOError;

    fn encode(
        &mut self,
        item: PgWireBackendMessage,
        dst: &mut bytes::BytesMut,
    ) -> Result<(), Self::Error> {
        item.encode(dst).map_err(Into::into)
    }
}

impl<T, S> ClientInfo for Framed<T, PgWireMessageServerCodec<S>> {
    fn socket_addr(&self) -> std::net::SocketAddr {
        self.codec().client_info.socket_addr
    }

    fn is_secure(&self) -> bool {
        self.codec().client_info.is_secure
    }

    fn state(&self) -> PgWireConnectionState {
        self.codec().client_info.state
    }

    fn set_state(&mut self, new_state: PgWireConnectionState) {
        self.codec_mut().client_info.set_state(new_state);
    }

    fn metadata(&self) -> &std::collections::HashMap<String, String> {
        self.codec().client_info.metadata()
    }

    fn metadata_mut(&mut self) -> &mut std::collections::HashMap<String, String> {
        self.codec_mut().client_info.metadata_mut()
    }

    fn transaction_status(&self) -> TransactionStatus {
        self.codec().client_info.transaction_status()
    }

    fn set_transaction_status(&mut self, new_status: TransactionStatus) {
        self.codec_mut()
            .client_info
            .set_transaction_status(new_status);
    }
}

impl<T, S> ClientPortalStore for Framed<T, PgWireMessageServerCodec<S>> {
    type PortalStore = <DefaultClient<S> as ClientPortalStore>::PortalStore;

    fn portal_store(&self) -> &Self::PortalStore {
        self.codec().client_info.portal_store()
    }
}

async fn process_message<S, A, Q, EQ, C>(
    message: PgWireFrontendMessage,
    socket: &mut Framed<S, PgWireMessageServerCodec<EQ::Statement>>,
    authenticator: Arc<A>,
    query_handler: Arc<Q>,
    extended_query_handler: Arc<EQ>,
    copy_handler: Arc<C>,
) -> PgWireResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync,
    A: StartupHandler,
    Q: SimpleQueryHandler,
    EQ: ExtendedQueryHandler,
    C: CopyHandler,
{
    match socket.state() {
        PgWireConnectionState::AwaitingStartup => {
            if let PgWireFrontendMessage::Startup(ref startup) = message {
                let user = startup
                    .parameters
                    .get(METADATA_USER)
                    .map(|v| v.trim())
                    .unwrap_or_default();
                if user.is_empty() {
                    return Err(PgWireError::UserNameRequired);
                }
            }
            authenticator.on_startup(socket, message).await?;
        }
        PgWireConnectionState::AuthenticationInProgress => {
            authenticator.on_startup(socket, message).await?;
        }
        // From Postgres docs:
        // When an error is detected while processing any extended-query
        // message, the backend issues ErrorResponse, then reads and discards
        // messages until a Sync is reached, then issues ReadyForQuery and
        // returns to normal message processing.
        PgWireConnectionState::AwaitingSync => {
            if let PgWireFrontendMessage::Sync(sync) = message {
                extended_query_handler.on_sync(socket, sync).await?;
                // TODO: confirm if we need to track transaction state there
                socket.set_state(PgWireConnectionState::ReadyForQuery);
            }
        }
        PgWireConnectionState::CopyInProgress(is_extended_query) => {
            // query or query in progress
            match message {
                PgWireFrontendMessage::CopyData(copy_data) => {
                    copy_handler.on_copy_data(socket, copy_data).await?;
                }
                PgWireFrontendMessage::CopyDone(copy_done) => {
                    let result = copy_handler.on_copy_done(socket, copy_done).await;
                    if !is_extended_query {
                        // If the copy was initiated from a simple protocol
                        // query, we should leave the CopyInProgress state
                        // before returning the error in order to resume normal
                        // operation after handling it in process_error.
                        socket.set_state(PgWireConnectionState::ReadyForQuery);
                    }
                    match result {
                        Ok(_) => {
                            if !is_extended_query {
                                // If the copy was initiated from a simple protocol
                                // query, notify the client that we are not ready
                                // for the next query.
                                send_ready_for_query(socket, socket.transaction_status()).await?
                            } else {
                                // In the extended protocol (at least as
                                // implemented by rust-postgres) we get a Sync
                                // after the CopyDone, so we should let the
                                // on_sync handler send the ReadyForQuery.
                            }
                        }
                        err => return err,
                    }
                }
                PgWireFrontendMessage::CopyFail(copy_fail) => {
                    let error = copy_handler.on_copy_fail(socket, copy_fail).await;
                    if !is_extended_query {
                        // If the copy was initiated from a simple protocol query,
                        // we should leave the CopyInProgress state
                        // before returning the error in order to resume normal
                        // operation after handling it in process_error.
                        socket.set_state(PgWireConnectionState::ReadyForQuery);
                    }
                    return Err(error);
                }
                _ => {}
            }
        }
        _ => {
            // query or query in progress
            match message {
                PgWireFrontendMessage::Query(query) => {
                    query_handler.on_query(socket, query).await?;
                }
                PgWireFrontendMessage::Parse(parse) => {
                    extended_query_handler.on_parse(socket, parse).await?;
                }
                PgWireFrontendMessage::Bind(bind) => {
                    extended_query_handler.on_bind(socket, bind).await?;
                }
                PgWireFrontendMessage::Execute(execute) => {
                    extended_query_handler.on_execute(socket, execute).await?;
                }
                PgWireFrontendMessage::Describe(describe) => {
                    extended_query_handler.on_describe(socket, describe).await?;
                }
                PgWireFrontendMessage::Flush(flush) => {
                    extended_query_handler.on_flush(socket, flush).await?;
                }
                PgWireFrontendMessage::Sync(sync) => {
                    extended_query_handler.on_sync(socket, sync).await?;
                }
                PgWireFrontendMessage::Close(close) => {
                    extended_query_handler.on_close(socket, close).await?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

async fn process_error<S, ST>(
    socket: &mut Framed<S, PgWireMessageServerCodec<ST>>,
    error: PgWireError,
    wait_for_sync: bool,
) -> Result<(), IOError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync,
{
    let mut wait_for_sync = wait_for_sync;
    match error {
        PgWireError::UserError(error_info) => {
            let fatal = matches!(error_info.severity.as_str(), "FATAL" | "PANIC");
            if fatal {
                socket
                    .send(PgWireBackendMessage::ErrorResponse((*error_info).into()))
                    .await?;
                return socket.close().await;
            } else {
                socket
                    .feed(PgWireBackendMessage::ErrorResponse((*error_info).into()))
                    .await?;
            }
        }
        PgWireError::ApiError(e) => {
            let error_info = ErrorInfo::new("ERROR".to_owned(), "XX000".to_owned(), e.to_string());
            socket
                .feed(PgWireBackendMessage::ErrorResponse(error_info.into()))
                .await?;
        }
        PgWireError::UserNameRequired => {
            let error_info = ErrorInfo::new(
                "FATAL".to_owned(),
                "28000".to_owned(),
                "no PostgreSQL user name specified in startup packet".to_owned(),
            );
            socket
                .send(PgWireBackendMessage::ErrorResponse(error_info.into()))
                .await?;
            return socket.close().await;
        }
        PgWireError::NotReadyForQuery => {
            wait_for_sync = true;
            let error_info = ErrorInfo::new(
                "ERROR".to_owned(),
                "08P01".to_owned(),
                "connection is not ready for query".to_owned(),
            );
            socket
                .feed(PgWireBackendMessage::ErrorResponse(error_info.into()))
                .await?;
        }
        _ => {
            // Internal error
            let error_info =
                ErrorInfo::new("FATAL".to_owned(), "XX000".to_owned(), error.to_string());
            socket
                .send(PgWireBackendMessage::ErrorResponse(error_info.into()))
                .await?;
            return socket.close().await;
        }
    }

    let transaction_status = socket.transaction_status().to_error_state();
    socket.set_transaction_status(transaction_status);

    if wait_for_sync {
        socket.set_state(PgWireConnectionState::AwaitingSync);
    } else {
        socket.set_state(PgWireConnectionState::ReadyForQuery);
        socket
            .feed(PgWireBackendMessage::ReadyForQuery(ReadyForQuery::new(
                transaction_status,
            )))
            .await?;
    }
    socket.flush().await?;

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum SslNegotiationType {
    Postgres,
    Direct,
    None,
}

async fn check_ssl_direct_negotiation(tcp_socket: &TcpStream) -> Result<bool, IOError> {
    let mut buf = [0u8; 1];
    let n = tcp_socket.peek(&mut buf).await?;

    Ok(n > 0 && buf[0] == 0x16)
}

async fn peek_for_sslrequest<ST>(
    socket: &mut Framed<TcpStream, PgWireMessageServerCodec<ST>>,
    ssl_supported: bool,
) -> Result<SslNegotiationType, IOError> {
    if check_ssl_direct_negotiation(socket.get_ref()).await? {
        Ok(SslNegotiationType::Direct)
    } else if let Some(Ok(PgWireFrontendMessage::SslRequest(Some(_)))) = socket.next().await {
        if ssl_supported {
            socket
                .send(PgWireBackendMessage::SslResponse(SslResponse::Accept))
                .await?;
            Ok(SslNegotiationType::Postgres)
        } else {
            socket
                .send(PgWireBackendMessage::SslResponse(SslResponse::Refuse))
                .await?;
            Ok(SslNegotiationType::None)
        }
    } else {
        Ok(SslNegotiationType::None)
    }
}

async fn do_process_socket<S, A, Q, EQ, C, E>(
    socket: &mut Framed<S, PgWireMessageServerCodec<EQ::Statement>>,
    startup_handler: Arc<A>,
    simple_query_handler: Arc<Q>,
    extended_query_handler: Arc<EQ>,
    copy_handler: Arc<C>,
    error_handler: Arc<E>,
    cancel_token: Option<CancellationToken>,
) -> Result<(), IOError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync,
    A: StartupHandler,
    Q: SimpleQueryHandler,
    EQ: ExtendedQueryHandler,
    C: CopyHandler,
    E: ErrorHandler,
{
    loop {
        let msg = match &cancel_token {
            Some(token) => tokio::select! {
                biased;
                _ = token.cancelled() => {
                    let err = PgWireError::UserError(Box::new(ErrorInfo::new(
                        "FATAL".into(),
                        "25P03".into(),
                        "terminating connection due to idle-in-transaction timeout".into(),
                    )));
                    process_error(socket, err, false).await?;
                    break;
                }
                msg = socket.next() => match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                },
            },
            None => match socket.next().await {
                Some(Ok(m)) => m,
                Some(Err(_)) => break,
                None => break,
            },
        };

        // Re-check cancel token before dispatching. Closes the window where
        // the watchdog fires cancel between socket.next() returning and
        // process_message() beginning (Timeline A in #1124).
        if let Some(token) = &cancel_token {
            if token.is_cancelled() {
                let err = PgWireError::UserError(Box::new(ErrorInfo::new(
                    "FATAL".into(),
                    "25P03".into(),
                    "terminating connection due to idle-in-transaction timeout".into(),
                )));
                process_error(socket, err, false).await?;
                break;
            }
        }

        let is_extended_query = match socket.state() {
            PgWireConnectionState::CopyInProgress(is_extended_query) => is_extended_query,
            _ => msg.is_extended_query(),
        };
        if let Err(mut e) = process_message(
            msg,
            socket,
            startup_handler.clone(),
            simple_query_handler.clone(),
            extended_query_handler.clone(),
            copy_handler.clone(),
        )
        .await
        {
            error_handler.on_error(socket, &mut e);
            process_error(socket, e, is_extended_query).await?;
        }
    }

    Ok(())
}

#[cfg(any(feature = "_ring", feature = "_aws-lc-rs"))]
fn check_alpn_for_direct_ssl<IO>(tls_socket: &TlsStream<IO>) -> Result<(), IOError> {
    let (_, the_conn) = tls_socket.get_ref();
    let mut accept = false;

    if let Some(alpn) = the_conn.alpn_protocol() {
        if alpn == b"postgresql" {
            accept = true;
        }
    }

    if !accept {
        Err(IOError::new(
            std::io::ErrorKind::InvalidData,
            "received direct SSL connection request without ALPN protocol negotiation extension",
        ))
    } else {
        Ok(())
    }
}

pub async fn process_socket<H>(
    tcp_socket: TcpStream,
    #[cfg(any(feature = "_ring", feature = "_aws-lc-rs"))] tls_acceptor: Option<Arc<TlsAcceptor>>,
    handlers: H,
    cancel_token: Option<CancellationToken>,
) -> Result<(), IOError>
where
    H: PgWireServerHandlers,
{
    let addr = tcp_socket.peer_addr()?;
    tcp_socket.set_nodelay(true)?;

    let client_info = DefaultClient::new(addr, false);
    let mut tcp_socket = Framed::new(tcp_socket, PgWireMessageServerCodec::new(client_info));

    #[cfg(any(feature = "_ring", feature = "_aws-lc-rs"))]
    let ssl = peek_for_sslrequest(&mut tcp_socket, tls_acceptor.is_some()).await?;
    #[cfg(not(any(feature = "_ring", feature = "_aws-lc-rs")))]
    let ssl = peek_for_sslrequest(&mut tcp_socket, false).await?;

    let startup_handler = handlers.startup_handler();
    let simple_query_handler = handlers.simple_query_handler();
    let extended_query_handler = handlers.extended_query_handler();
    let copy_handler = handlers.copy_handler();
    let error_handler = handlers.error_handler();

    if ssl == SslNegotiationType::None {
        // use an already configured socket.
        let mut socket = tcp_socket;

        do_process_socket(
            &mut socket,
            startup_handler,
            simple_query_handler,
            extended_query_handler,
            copy_handler,
            error_handler,
            cancel_token,
        )
        .await
    } else {
        #[cfg(any(feature = "_ring", feature = "_aws-lc-rs"))]
        {
            // mention the use of ssl
            let client_info = DefaultClient::new(addr, true);
            // safe to unwrap tls_acceptor here
            let ssl_socket = tls_acceptor
                .unwrap()
                .accept(tcp_socket.into_inner())
                .await?;

            // check alpn for direct ssl connection
            if ssl == SslNegotiationType::Direct {
                check_alpn_for_direct_ssl(&ssl_socket)?;
            }

            let mut socket = Framed::new(ssl_socket, PgWireMessageServerCodec::new(client_info));

            do_process_socket(
                &mut socket,
                startup_handler,
                simple_query_handler,
                extended_query_handler,
                copy_handler,
                error_handler,
                cancel_token,
            )
            .await
        }

        #[cfg(not(any(feature = "_ring", feature = "_aws-lc-rs")))]
        Ok(())
    }
}
