use std;
use std::u32;
use std::sync::{Arc, RwLock};

use chrono;

use opcua_core::comms::secure_channel::SecureChannel;
use opcua_core::crypto::SecurityPolicy;

use opcua_types::{UInt32, NodeId, UAString, DateTime, ExtensionObject};
use opcua_types::*;
use opcua_types::service_types::*;
use opcua_types::status_code::StatusCode;

use message_queue::MessageQueue;

const DEFAULT_REQUEST_TIMEOUT: u32 = 10 * 1000;
const SEND_BUFFER_SIZE: usize = 65536;
const RECEIVE_BUFFER_SIZE: usize = 65536;
const MAX_BUFFER_SIZE: usize = 65536;

/// Used for synchronous polling
const SYNC_POLLING_PERIOD: u64 = 50;

/// A simple handle factory for incrementing sequences of numbers.
struct Handle {
    next: UInt32,
    first: UInt32,
}

impl Handle {
    /// Creates a new handle factory, that starts with the supplied number
    pub fn new(first: UInt32) -> Handle {
        Handle {
            next: first,
            first,
        }
    }

    /// Returns the next handle to be issued, internally incrementing each time so the handle
    /// is always different until it wraps back to the start.
    pub fn next(&mut self) -> UInt32 {
        let next = self.next;
        // Increment next
        if self.next == u32::MAX {
            self.next = self.first;
        } else {
            self.next += 1;
        }
        next
    }
}

/// Session's state indicates connection status, negotiated times and sizes,
/// and security tokens.
pub struct SessionState {
    /// Secure channel information
    secure_channel: Arc<RwLock<SecureChannel>>,
    /// The request timeout is how long the session will wait from sending a request expecting a response
    /// if no response is received the rclient will terminate.
    request_timeout: u32,
    /// Size of the send buffer
    send_buffer_size: usize,
    /// Size of the
    receive_buffer_size: usize,
    /// Maximum message size
    max_message_size: usize,
    /// The session's id - used for diagnostic info
    session_id: NodeId,
    /// The sesion authentication token, used for session activation
    authentication_token: NodeId,
    /// The next handle to assign to a request
    request_handle: Handle,
    /// Next monitored item client side handle
    monitored_item_handle: Handle,
    /// Unacknowledged
    pub subscription_acknowledgements: Vec<SubscriptionAcknowledgement>,
    /// A flag which tells client to wait for a publish response before sending any new publish
    /// requests
    pub wait_for_publish_response: bool,
    /// The message queue
    pub message_queue: Arc<RwLock<MessageQueue>>,
}

impl SessionState {
    pub fn new(secure_channel: Arc<RwLock<SecureChannel>>, message_queue: Arc<RwLock<MessageQueue>>) -> SessionState {
        SessionState {
            secure_channel,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            send_buffer_size: SEND_BUFFER_SIZE,
            receive_buffer_size: RECEIVE_BUFFER_SIZE,
            max_message_size: MAX_BUFFER_SIZE,
            request_handle: Handle::new(1),
            session_id: NodeId::null(),
            authentication_token: NodeId::null(),
            monitored_item_handle: Handle::new(1000),
            message_queue,
            subscription_acknowledgements: Vec::new(),
            wait_for_publish_response: false,
        }
    }

    pub fn set_session_id(&mut self, session_id: NodeId) {
        self.session_id = session_id
    }

    pub fn session_id(&self) -> NodeId {
        self.session_id.clone()
    }

    pub fn receive_buffer_size(&self) -> usize {
        self.receive_buffer_size
    }

    pub fn max_message_size(&self) -> usize {
        self.max_message_size
    }

    pub fn request_timeout(&self) -> u32 {
        self.request_timeout
    }

    pub fn send_buffer_size(&self) -> usize {
        self.send_buffer_size
    }

    pub fn subscription_acknowledgements(&mut self) -> Vec<SubscriptionAcknowledgement> {
        self.subscription_acknowledgements.drain(..).collect()
    }

    pub fn authentication_token(&self) -> &NodeId {
        &self.authentication_token
    }

    pub fn set_authentication_token(&mut self, authentication_token: NodeId) {
        self.authentication_token = authentication_token;
    }

    /// Construct a request header for the session. All requests after create session are expected
    /// to supply an authentication token.
    pub fn make_request_header(&mut self) -> RequestHeader {
        let request_header = RequestHeader {
            authentication_token: self.authentication_token.clone(),
            timestamp: DateTime::now(),
            request_handle: self.request_handle.next(),
            return_diagnostics: 0,
            audit_entry_id: UAString::null(),
            timeout_hint: self.request_timeout,
            additional_header: ExtensionObject::null(),
        };
        request_header
    }

    /// Sends a publish request containing acknowledgements for previous notifications.
    /// TODO this function needs to be refactored as an asynchronous operation.
    pub fn async_publish(&mut self, subscription_acknowledgements: &[SubscriptionAcknowledgement]) -> Result<UInt32, StatusCode> {
        debug!("async_publish with {} subscription acknowledgements", subscription_acknowledgements.len());
        let request = PublishRequest {
            request_header: self.make_request_header(),
            subscription_acknowledgements: if subscription_acknowledgements.is_empty() { None } else { Some(subscription_acknowledgements.to_vec()) },
        };
        let request_handle = self.async_send_request(request, true)?;
        debug!("async_publish, request sent with handle {}", request_handle);
        Ok(request_handle)
    }

    /// Synchronously sends a request. The return value is the response to the request
    pub(crate) fn send_request<T>(&mut self, request: T) -> Result<SupportedMessage, StatusCode> where T: Into<SupportedMessage> {
        // Send the request
        let request_handle = self.async_send_request(request, false)?;
        // Wait for the response
        let request_timeout = self.request_timeout();
        self.wait_for_sync_response(request_handle, request_timeout)
    }

    /// Asynchronously sends a request. The return value is the request handle of the request
    pub(crate) fn async_send_request<T>(&mut self, request: T, async: bool) -> Result<UInt32, StatusCode> where T: Into<SupportedMessage> {
        let request = request.into();
        match request {
            SupportedMessage::OpenSecureChannelRequest(_) | SupportedMessage::CloseSecureChannelRequest(_) => {}
            _ => {
                // Make sure secure channel token hasn't expired
                let _ = self.ensure_secure_channel_token();
            }
        }

        // TODO should error here if not connected

        // Enqueue the request
        let request_handle = request.request_handle();
        self.add_request(request, async);

        Ok(request_handle)
    }

    /// Wait for a response with a matching request handle. If request handle is 0 then no match
    /// is performed and in fact the function is expected to receive no messages except asynchronous
    /// and housekeeping events from the server. A 0 handle will cause the wait to process at most
    /// one async message before returning.
    fn wait_for_sync_response(&mut self, request_handle: UInt32, request_timeout: UInt32) -> Result<SupportedMessage, StatusCode> {
        if request_handle == 0 {
            panic!("Request handle must be non zero");
        }

        // Receive messages until the one expected comes back. Publish responses will be consumed
        // silently.
        let start = chrono::Utc::now();
        loop {
            if let Some(response) = self.take_response(request_handle) {
                // Got the response
                return Ok(response);
            } else {
                let now = chrono::Utc::now();
                let request_duration = now.signed_duration_since(start);
                if request_duration.num_milliseconds() >= request_timeout as i64 {
                    info!("Timeout waiting for response from server");
                    self.request_has_timed_out(request_handle);
                    return Err(StatusCode::BadTimeout);
                }
                // Sleep before trying again
                std::thread::sleep(std::time::Duration::from_millis(SYNC_POLLING_PERIOD));
            }
        }
    }

    fn take_response(&self, request_handle: UInt32) -> Option<SupportedMessage> {
        let mut message_queue = trace_write_lock_unwrap!(self.message_queue);
        message_queue.take_response(request_handle)
    }

    fn request_has_timed_out(&self, request_handle: UInt32) {
        let mut message_queue = trace_write_lock_unwrap!(self.message_queue);
        message_queue.request_has_timed_out(request_handle)
    }

    fn add_request(&mut self, request: SupportedMessage, async: bool) {
        let mut message_queue = trace_write_lock_unwrap!(self.message_queue);
        message_queue.add_request(request, async)
    }

    /// Checks if secure channel token needs to be renewed and renews it
    fn ensure_secure_channel_token(&mut self) -> Result<(), StatusCode> {
        let should_renew_security_token = {
            let secure_channel = trace_read_lock_unwrap!(self.secure_channel);
            secure_channel.should_renew_security_token()
        };
        if should_renew_security_token {
            self.issue_or_renew_secure_channel(SecurityTokenRequestType::Renew)
        } else {
            Ok(())
        }
    }

    pub(crate) fn issue_or_renew_secure_channel(&mut self, request_type: SecurityTokenRequestType) -> Result<(), StatusCode> {
        trace!("issue_or_renew_secure_channel({:?})", request_type);

        const REQUESTED_LIFETIME: UInt32 = 60000; // TODO

        let (security_mode, security_policy, client_nonce) = {
            let mut secure_channel = trace_write_lock_unwrap!(self.secure_channel);
            let client_nonce = secure_channel.security_policy().random_nonce();
            secure_channel.set_local_nonce(client_nonce.as_ref());
            (secure_channel.security_mode(), secure_channel.security_policy(), client_nonce)
        };

        info!("Making secure channel request");
        info!("security_mode = {:?}", security_mode);
        info!("security_policy = {:?}", security_policy);

        let requested_lifetime = REQUESTED_LIFETIME;
        let request = OpenSecureChannelRequest {
            request_header: self.make_request_header(),
            client_protocol_version: 0,
            request_type,
            security_mode,
            client_nonce,
            requested_lifetime,
        };
        let response = self.send_request(SupportedMessage::OpenSecureChannelRequest(request))?;
        if let SupportedMessage::OpenSecureChannelResponse(response) = response {
            debug!("Setting transport's security token");
            {
                let mut secure_channel = trace_write_lock_unwrap!(self.secure_channel);
                secure_channel.set_security_token(response.security_token);

                if security_policy != SecurityPolicy::None && (security_mode == MessageSecurityMode::Sign || security_mode == MessageSecurityMode::SignAndEncrypt) {
                    secure_channel.set_remote_nonce_from_byte_string(&response.server_nonce)?;
                    secure_channel.derive_keys();
                }
            }
            Ok(())
        } else {
            Err(::process_unexpected_response(response))
        }
    }

    /// Returns the next monitored item handle
    pub fn next_monitored_item_handle(&mut self) -> UInt32 {
        self.monitored_item_handle.next()
    }
}