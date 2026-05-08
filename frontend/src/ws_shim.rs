//! Minimal browser-side WebSocket client for the freenet client API.
//!
//! This replaces `freenet_stdlib::client_api::WebApi` for the dashboard's
//! one use case (subscribe + publish to a single contract). The stdlib's
//! browser implementation receives binary frames as a `Blob` and shovels
//! them through `FileReader.readAsArrayBuffer` to get the bytes — which
//! works in normal pages but **silently breaks inside a sandboxed iframe
//! at opaque (`null`) origin**: the FileReader's `onloadend` callback
//! never fires, so `SubscribeResponse` / `UpdateResponse` frames vanish
//! and the dashboard hangs at "subscribing…". Setting the WebSocket's
//! `binaryType` to `"arraybuffer"` (instead of the default `"blob"`)
//! gives us bytes synchronously on every `message` event and sidesteps
//! the issue entirely.
//!
//! Historically we worked around this by vendoring a one-line patch of
//! freenet-stdlib at `../stdlib-0.6.1-patched/`. This module ships the
//! same fix as part of our own crate so the patched fork can be
//! deleted.
//!
//! Scope is intentionally tight:
//! - one connection at a time, no reconnect (caller does that),
//! - no streaming reassembly: stdlib's `CHUNK_THRESHOLD` is 512 KB,
//!   our payloads are <50 KB, we never see `StreamHeader` /
//!   `StreamChunk` in practice. If a payload ever does chunk we'll
//!   get a warn-log and the message is silently dropped (same
//!   behaviour as the patched fork's "incremental streaming not
//!   supported" code path).

use std::cell::RefCell;
use std::rc::Rc;

use freenet_stdlib::client_api::{ClientError, ClientRequest, Error as WebApiError, HostResponse};
use wasm_bindgen::{prelude::Closure, JsCast, JsValue};
use web_sys::js_sys::{ArrayBuffer, Uint8Array};
use web_sys::{BinaryType, ErrorEvent, MessageEvent, WebSocket};

/// Minimal stand-in for `freenet_stdlib::client_api::WebApi`.
pub struct WsShim {
    socket: WebSocket,
    /// Closures the WebSocket is holding handlers to. Keeping ownership
    /// here means they live as long as `WsShim` does and drop together
    /// when the user closes the connection — avoids the
    /// `Closure::forget` leak the stdlib version uses.
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_open: Closure<dyn FnMut()>,
    _on_error: Closure<dyn FnMut(ErrorEvent)>,
    _on_close: Closure<dyn FnMut(web_sys::CloseEvent)>,
}

impl WsShim {
    /// Open `socket` with our handlers wired up. Result-decoding happens
    /// before `result_handler` fires; protocol-level errors (frame
    /// bincode failure, stream-mode messages we don't support) go to
    /// `error_handler`. `onopen_handler` runs once when the socket
    /// transitions to OPEN.
    pub fn start<RFn, EFn, OFn>(
        socket: WebSocket,
        result_handler: RFn,
        error_handler: EFn,
        onopen_handler: OFn,
    ) -> Self
    where
        RFn: FnMut(Result<HostResponse, ClientError>) + 'static,
        EFn: FnMut(WebApiError) + Clone + 'static,
        OFn: FnOnce() + 'static,
    {
        // Crucial: receive bytes as ArrayBuffer (synchronous), not as
        // Blob (which requires FileReader and dies in null-origin
        // sandboxes — see module docs).
        socket.set_binary_type(BinaryType::Arraybuffer);

        let result_handler = Rc::new(RefCell::new(result_handler));

        let on_message = {
            let result_handler = result_handler.clone();
            let mut error_handler = error_handler.clone();
            Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
                let data: JsValue = e.data();
                let buf = match data.dyn_into::<ArrayBuffer>() {
                    Ok(b) => b,
                    Err(_) => {
                        // Server should never send us text/Blob frames
                        // — log and drop. Don't emit an error to avoid
                        // confusing the UI on benign protocol noise.
                        web_sys::console::log_1(
                            &"[ws-shim] non-ArrayBuffer frame ignored".into(),
                        );
                        return;
                    }
                };
                let bytes = Uint8Array::new(&buf).to_vec();
                let decoded: Result<HostResponse, ClientError> =
                    match bincode::deserialize(&bytes) {
                        Ok(r) => r,
                        Err(err) => {
                            error_handler(WebApiError::ConnectionError(serde_json::json!({
                                "error": format!("{err}"),
                                "source": "host response deserialization"
                            })));
                            return;
                        }
                    };
                // Ignore stream-mode framing: stdlib only emits these
                // for payloads >512 KB. Our state never approaches
                // that. If we ever see them we want a visible warn,
                // not silent corruption.
                if let Ok(HostResponse::StreamHeader { .. }) = &decoded {
                    web_sys::console::log_1(
                        &"[ws-shim] StreamHeader received — chunked responses not supported".into(),
                    );
                    return;
                }
                if let Ok(HostResponse::StreamChunk { .. }) = &decoded {
                    web_sys::console::log_1(
                        &"[ws-shim] StreamChunk received — chunked responses not supported".into(),
                    );
                    return;
                }
                (result_handler.borrow_mut())(decoded);
            })
        };
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        let onopen_handler = Rc::new(RefCell::new(Some(onopen_handler)));
        let on_open = Closure::<dyn FnMut()>::new(move || {
            if let Some(h) = onopen_handler.borrow_mut().take() {
                h();
            }
        });
        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));

        let on_error = {
            let mut error_handler = error_handler.clone();
            Closure::<dyn FnMut(ErrorEvent)>::new(move |e: ErrorEvent| {
                error_handler(WebApiError::ConnectionError(serde_json::json!({
                    "error": format!(
                        "{file}:{lineno}: {msg}",
                        file = e.filename(),
                        lineno = e.lineno(),
                        msg = e.message()
                    ),
                    "source": "exec error"
                })));
            })
        };
        socket.set_onerror(Some(on_error.as_ref().unchecked_ref()));

        let on_close = {
            let mut error_handler = error_handler;
            Closure::<dyn FnMut(web_sys::CloseEvent)>::new(move |_: web_sys::CloseEvent| {
                error_handler(WebApiError::ConnectionError(serde_json::json!({
                    "error": "connection closed",
                    "source": "close"
                })));
            })
        };
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));

        Self {
            socket,
            _on_message: on_message,
            _on_open: on_open,
            _on_error: on_error,
            _on_close: on_close,
        }
    }

    /// Bincode-encode a request and send it as a single binary frame.
    /// Returns the same error shape stdlib does so call sites don't
    /// need to change.
    ///
    /// `async` to mirror stdlib's signature; the underlying
    /// `WebSocket.send` is itself synchronous, but the existing call
    /// sites are written around `await`-ing the result.
    pub async fn send(&mut self, request: ClientRequest<'static>) -> Result<(), WebApiError> {
        let bytes =
            bincode::serialize(&request).map_err(|e| WebApiError::OtherError(e.into()))?;
        self.socket
            .send_with_u8_array(&bytes)
            .map_err(|e| WebApiError::ConnectionError(serde_json::json!({
                "error": format!("{e:?}"),
                "source": "websocket send"
            })))
    }
}

impl Drop for WsShim {
    fn drop(&mut self) {
        // Detach JS-side handlers FIRST. `socket.close()` synchronously
        // dispatches an `onclose` event, which would otherwise try to
        // invoke our `_on_close` Closure right as it's about to drop —
        // wasm-bindgen reports this as "closure invoked recursively or
        // after being dropped" and panics. Clearing the slots breaks
        // the JS→Closure pointer before the Closure storage goes away.
        self.socket.set_onmessage(None);
        self.socket.set_onopen(None);
        self.socket.set_onerror(None);
        self.socket.set_onclose(None);
        // Now safe to send the Normal-Closure frame; any onclose event
        // fired by the browser has nowhere to dispatch.
        let _ = self.socket.close();
    }
}
