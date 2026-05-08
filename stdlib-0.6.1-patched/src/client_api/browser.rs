use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::{prelude::Closure, JsCast, JsValue};
use web_sys::{ErrorEvent, MessageEvent};

use super::{client_events::ClientRequest, Error, HostResult};

type Connection = web_sys::WebSocket;

pub struct WebApi {
    conn: Connection,
    error_handler: Box<dyn FnMut(Error) + 'static>,
    next_stream_id: u32,
}

impl Drop for WebApi {
    fn drop(&mut self) {
        // Close with normal closure code when dropped
        let _ = self.conn.close_with_code(1000);
    }
}

impl WebApi {
    pub fn start<ErrFn>(
        conn: Connection,
        result_handler: impl FnMut(HostResult) + 'static,
        error_handler: ErrFn,
        onopen_handler: impl FnOnce() + 'static,
    ) -> Self
    where
        ErrFn: FnMut(Error) + Clone + 'static,
    {
        let eh = Rc::new(RefCell::new(error_handler.clone()));
        let result_handler = Rc::new(RefCell::new(result_handler));
        let reassembly = Rc::new(RefCell::new(super::streaming::ReassemblyBuffer::new()));

        let onmessage_callback = Closure::<dyn FnMut(_)>::new(move |e: MessageEvent| {
            // Patched 2026-05-08: receive ArrayBuffer directly. The original
            // 0.6.1 path went `Blob → FileReader.readAsArrayBuffer`, but
            // inside a sandboxed iframe with opaque (null) origin the
            // FileReader callback never fires for cross-origin-fetched
            // blobs in some browsers, so SubscribeResponse / Update results
            // never reach the client. Switching the WS binaryType to
            // "arraybuffer" (see set_binary_type below) gives us the bytes
            // synchronously on the message event.
            web_sys::console::log_1(&"[stdlib-patched] onmessage fired".into());
            let value: JsValue = e.data();
            let array_buffer = match value.dyn_into::<js_sys::ArrayBuffer>() {
                Ok(ab) => ab,
                Err(orig) => {
                    web_sys::console::log_1(&"[stdlib-patched] data is not ArrayBuffer".into());
                    let _ = orig;
                    return;
                }
            };
            let bytes = js_sys::Uint8Array::new(&array_buffer).to_vec();
            web_sys::console::log_1(
                &format!("[stdlib-patched] received {} bytes", bytes.len()).into(),
            );

            let eh_clone = eh.clone();
            let result_handler_clone = result_handler.clone();
            let reassembly_clone = reassembly.clone();

            let inline_dispatch = move || {
                use super::client_events::HostResponse;

                let response: HostResult = match bincode::deserialize(&bytes) {
                    Ok(val) => val,
                    Err(err) => {
                        web_sys::console::log_1(
                            &format!("[stdlib-patched] bincode err: {err}").into(),
                        );
                        eh_clone.borrow_mut()(Error::ConnectionError(serde_json::json!({
                            "error": format!("{err}"),
                            "source": "host response deserialization"
                        })));
                        return;
                    }
                };

                match response {
                    Ok(HostResponse::StreamHeader { .. }) => {
                        // StreamHeader is metadata only — the following StreamChunks
                        // will be reassembled transparently by the ReassemblyBuffer.
                        // Browser incremental streaming is not yet supported.
                        return;
                    }
                    Ok(HostResponse::StreamChunk {
                        stream_id,
                        index,
                        total,
                        data,
                    }) => {
                        match reassembly_clone
                            .borrow_mut()
                            .receive_chunk(stream_id, index, total, data)
                        {
                            Ok(Some(complete)) => {
                                let inner: HostResult = match bincode::deserialize(&complete) {
                                    Ok(val) => val,
                                    Err(err) => {
                                        eh_clone.borrow_mut()(Error::ConnectionError(
                                            serde_json::json!({
                                                "error": format!("{err}"),
                                                "source": "stream reassembly deserialization"
                                            }),
                                        ));
                                        return;
                                    }
                                };
                                result_handler_clone.borrow_mut()(inner);
                            }
                            Ok(None) => return, // more chunks needed
                            Err(e) => {
                                reassembly_clone.borrow_mut().remove_stream(stream_id);
                                eh_clone.borrow_mut()(Error::ConnectionError(serde_json::json!({
                                    "error": format!("{e}"),
                                    "source": "streaming reassembly"
                                })));
                                return;
                            }
                        }
                    }
                    other => {
                        result_handler_clone.borrow_mut()(other);
                    }
                }
            };
            inline_dispatch();
        });
        conn.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));
        onmessage_callback.forget();

        let mut eh = error_handler.clone();
        let onerror_callback = Closure::<dyn FnMut(_)>::new(move |e: ErrorEvent| {
            let error = format!(
                "error: {file}:{lineno}: {msg}",
                file = e.filename(),
                lineno = e.lineno(),
                msg = e.message()
            );
            eh(Error::ConnectionError(serde_json::json!({
                "error": error, "source": "exec error"
            })));
        });
        conn.set_onerror(Some(onerror_callback.as_ref().unchecked_ref()));
        onerror_callback.forget();

        let onopen_handler = Rc::new(RefCell::new(Some(onopen_handler)));
        let onopen_callback = Closure::wrap(Box::new(move || {
            if let Some(handler) = onopen_handler.borrow_mut().take() {
                handler();
            }
        }) as Box<dyn FnMut()>);
        conn.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));
        onopen_callback.forget();

        let mut eh = error_handler.clone();
        let onclose_callback = Closure::<dyn FnMut(_)>::new(move |_: web_sys::CloseEvent| {
            tracing::warn!("WebSocket connection closed");
            eh(Error::ConnectionError(
                serde_json::json!({ "error": "connection closed", "source": "close" }),
            ));
        });
        conn.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
        onclose_callback.forget();

        conn.set_binary_type(web_sys::BinaryType::Arraybuffer);
        WebApi {
            conn,
            error_handler: Box::new(error_handler),
            next_stream_id: 0,
        }
    }

    pub async fn send(&mut self, request: ClientRequest<'static>) -> Result<(), Error> {
        use super::streaming::{chunk_request, CHUNK_THRESHOLD};

        // Check WebSocket ready state before sending.
        // Per WebSocket spec, send() silently discards data when socket is CLOSING/CLOSED.
        let ready_state = self.conn.ready_state();
        if ready_state != web_sys::WebSocket::OPEN {
            let state_name = match ready_state {
                0 => "CONNECTING",
                1 => "OPEN",
                2 => "CLOSING",
                3 => "CLOSED",
                _ => "UNKNOWN",
            };
            let err = serde_json::json!({
                "error": format!("WebSocket is not open (state: {})", state_name),
                "origin": "send precondition check",
                "request": format!("{request:?}"),
            });
            (self.error_handler)(Error::ConnectionError(err.clone()));
            return Err(Error::ConnectionError(err));
        }

        let send = bincode::serialize(&request)?;
        web_sys::console::log_1(
            &format!(
                "[stdlib-patched send] req={request:?} bytes={} threshold={}",
                send.len(),
                CHUNK_THRESHOLD
            )
            .into(),
        );

        if send.len() > CHUNK_THRESHOLD {
            let stream_id = self.next_stream_id;
            self.next_stream_id = self.next_stream_id.wrapping_add(1);
            let chunks = chunk_request(send, stream_id);
            for chunk in &chunks {
                let chunk_bytes =
                    bincode::serialize(chunk).map_err(|e| Error::OtherError(e.into()))?;
                self.conn
                    .send_with_u8_array(&chunk_bytes)
                    .map_err(|err| Self::map_send_error(err, &request, &mut self.error_handler))?;
            }
        } else {
            self.conn
                .send_with_u8_array(&send)
                .map_err(|err| Self::map_send_error(err, &request, &mut self.error_handler))?;
        }
        Ok(())
    }

    fn map_send_error(
        err: JsValue,
        request: &ClientRequest<'_>,
        error_handler: &mut Box<dyn FnMut(Error) + 'static>,
    ) -> Error {
        let err: serde_json::Value = match serde_wasm_bindgen::from_value(err) {
            Ok(e) => e,
            Err(e) => {
                let e = serde_json::json!({
                    "error": format!("{e}"),
                    "origin": "request serialization",
                    "request": format!("{request:?}"),
                });
                error_handler(Error::ConnectionError(e.clone()));
                return Error::ConnectionError(e);
            }
        };
        error_handler(Error::ConnectionError(serde_json::json!({
            "error": err,
            "origin": "request sending",
            "request": format!("{request:?}"),
        })));
        Error::ConnectionError(err)
    }

    pub fn disconnect(self, cause: impl AsRef<str>) {
        let _ = self.conn.close_with_code_and_reason(1000, cause.as_ref());
    }
}
