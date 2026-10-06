//! Upload worker for the session server.
//!
//! Handles resource upload state, rate limiting, and the async worker that
//! processes `ResourceUploadStart` / `ResourceUploadChunk` / `ResourceUploadComplete`
//! commands from the session loop.

use crate::proto::session::*;
use crate::session::SharedState;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tze_hud_resource::{
    AgentBudget, ResourceError as StoreResourceError, ResourceStored as StoreResourceStored,
    ResourceType as StoreResourceType, UploadId, UploadStartRequest,
};
use tze_hud_scene::types::*;

// ─── Upload state types ───────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub(super) struct ResidentUploadState {
    pub(super) request_sequence: u64,
    pub(super) metadata: ResourceMetadata,
    pub(super) total_size_bytes: u64,
}

/// Sliding-window byte limiter for resident upload payload intake.
///
/// Tracks accepted upload bytes in a 1-second window. The session handler
/// uses this to apply transport backpressure by delaying additional read
/// processing until enough bytes leave the active window.
#[derive(Debug)]
pub(super) struct UploadByteRateLimiter {
    pub(super) limit_bytes_per_second: usize,
    window: VecDeque<(Instant, usize)>,
    bytes_in_window: usize,
}

impl UploadByteRateLimiter {
    pub(super) fn with_limit(limit_bytes_per_second: usize) -> Self {
        Self {
            limit_bytes_per_second,
            window: VecDeque::new(),
            bytes_in_window: 0,
        }
    }

    fn prune(&mut self, now: Instant) {
        while let Some(&(ts, bytes)) = self.window.front() {
            if now.duration_since(ts) >= Duration::from_secs(1) {
                self.window.pop_front();
                self.bytes_in_window = self.bytes_in_window.saturating_sub(bytes);
            } else {
                break;
            }
        }
    }

    pub(super) fn available_bytes(&mut self, now: Instant) -> usize {
        if self.limit_bytes_per_second == 0 {
            return usize::MAX;
        }
        self.prune(now);
        self.limit_bytes_per_second
            .saturating_sub(self.bytes_in_window)
    }

    pub(super) fn reserve_bytes(&mut self, now: Instant, bytes: usize) {
        if self.limit_bytes_per_second == 0 || bytes == 0 {
            return;
        }
        self.window.push_back((now, bytes));
        self.bytes_in_window = self.bytes_in_window.saturating_add(bytes);
    }

    pub(super) fn next_delay(&mut self, now: Instant) -> Duration {
        if self.limit_bytes_per_second == 0 {
            return Duration::ZERO;
        }
        self.prune(now);
        match self.window.front() {
            Some((ts, _)) => Duration::from_secs(1).saturating_sub(now.duration_since(*ts)),
            None => Duration::ZERO,
        }
    }
}

#[derive(Debug)]
pub(super) enum UploadWorkerCommand {
    Start {
        request_sequence: u64,
        capabilities: Vec<String>,
        start: ResourceUploadStart,
    },
    Chunk {
        request_sequence: u64,
        chunk: ResourceUploadChunk,
    },
    Complete {
        request_sequence: u64,
        capabilities: Vec<String>,
        complete: ResourceUploadComplete,
    },
}

#[derive(Debug)]
pub(super) enum UploadWorkerEvent {
    UploadAccepted {
        request_sequence: u64,
        upload_id: [u8; 16],
    },
    Stored {
        request_sequence: u64,
        stored: StoreResourceStored,
        stored_bytes: u64,
        metadata: ResourceMetadata,
        upload_id: Option<[u8; 16]>,
    },
    Error {
        request_sequence: u64,
        upload_id: Option<Vec<u8>>,
        err: StoreResourceError,
    },
}

pub(super) async fn run_upload_worker(
    state: Arc<Mutex<SharedState>>,
    namespace: String,
    mut command_rx: tokio::sync::mpsc::Receiver<UploadWorkerCommand>,
    event_tx: tokio::sync::mpsc::Sender<UploadWorkerEvent>,
    upload_rate_limit_bytes_per_sec: usize,
    render_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
) {
    let store = {
        let st = state.lock().await;
        st.resource_store.clone()
    };
    let mut in_flight_uploads: HashMap<[u8; 16], ResidentUploadState> = HashMap::new();
    let mut upload_rate_limiter =
        UploadByteRateLimiter::with_limit(upload_rate_limit_bytes_per_sec);

    'worker: while let Some(command) = command_rx.recv().await {
        // The session loop drops its event receiver before joining this worker.
        // Discard queued work instead of admitting uploads after disconnect.
        if event_tx.is_closed() {
            break;
        }
        match command {
            UploadWorkerCommand::Start {
                request_sequence,
                capabilities,
                start,
            } => {
                let resource_type = match proto_resource_type_to_store(start.resource_type) {
                    Some(v) => v,
                    None => {
                        let err = StoreResourceError::UnsupportedType(format!(
                            "unknown resource_type enum value {}",
                            start.resource_type
                        ));
                        if event_tx
                            .send(UploadWorkerEvent::Error {
                                request_sequence,
                                upload_id: None,
                                err,
                            })
                            .await
                            .is_err()
                        {
                            break 'worker;
                        }
                        continue;
                    }
                };

                let expected_hash: [u8; 32] = match start.expected_hash.as_slice().try_into() {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        let err = StoreResourceError::HashMismatch {
                            computed: "invalid".to_string(),
                            expected: format!("len={} (expected 32)", start.expected_hash.len()),
                        };
                        if event_tx
                            .send(UploadWorkerEvent::Error {
                                request_sequence,
                                upload_id: None,
                                err,
                            })
                            .await
                            .is_err()
                        {
                            break 'worker;
                        }
                        continue;
                    }
                };

                let metadata = start.metadata.unwrap_or_default();
                if start.inline_data.is_empty() && start.total_size_bytes == 0 {
                    let err = StoreResourceError::SizeExceeded {
                        detail: "total_size_bytes must be > 0 for chunked uploads".to_string(),
                    };
                    if event_tx
                        .send(UploadWorkerEvent::Error {
                            request_sequence,
                            upload_id: None,
                            err,
                        })
                        .await
                        .is_err()
                    {
                        break 'worker;
                    }
                    continue;
                }

                let total_size = match usize::try_from(start.total_size_bytes) {
                    Ok(v) => v,
                    Err(_) => {
                        let err = StoreResourceError::SizeExceeded {
                            detail: format!(
                                "total_size_bytes {} exceeds platform limit {}",
                                start.total_size_bytes,
                                usize::MAX
                            ),
                        };
                        if event_tx
                            .send(UploadWorkerEvent::Error {
                                request_sequence,
                                upload_id: None,
                                err,
                            })
                            .await
                            .is_err()
                        {
                            break 'worker;
                        }
                        continue;
                    }
                };

                let upload_id_bytes = *uuid::Uuid::now_v7().as_bytes();
                let inline_bytes = start.inline_data.len();
                let request = UploadStartRequest {
                    agent_namespace: namespace.clone(),
                    agent_capabilities: capabilities,
                    agent_budget: AgentBudget {
                        texture_bytes_total_limit: 0,
                        texture_bytes_total_used: 0,
                    },
                    upload_id: UploadId::from_bytes(upload_id_bytes),
                    resource_type,
                    expected_hash,
                    total_size,
                    inline_data: start.inline_data,
                    width: metadata.width,
                    height: metadata.height,
                };

                if inline_bytes > 0 {
                    tokio::select! {
                        biased;
                        _ = event_tx.closed() => break 'worker,
                        _ = apply_upload_transport_backpressure(&mut upload_rate_limiter, inline_bytes) => {}
                    }
                }

                match store.handle_upload_start(request).await {
                    Ok(Some(stored)) => {
                        register_uploaded_scene_resource(&state, &stored.resource_id, &render_wake)
                            .await;
                        if event_tx
                            .send(UploadWorkerEvent::Stored {
                                request_sequence,
                                stored,
                                stored_bytes: start.total_size_bytes,
                                metadata,
                                upload_id: None,
                            })
                            .await
                            .is_err()
                        {
                            break 'worker;
                        }
                    }
                    Ok(None) => {
                        in_flight_uploads.insert(
                            upload_id_bytes,
                            ResidentUploadState {
                                request_sequence,
                                metadata,
                                total_size_bytes: start.total_size_bytes,
                            },
                        );
                        if event_tx
                            .send(UploadWorkerEvent::UploadAccepted {
                                request_sequence,
                                upload_id: upload_id_bytes,
                            })
                            .await
                            .is_err()
                        {
                            break 'worker;
                        }
                    }
                    Err(err) => {
                        if event_tx
                            .send(UploadWorkerEvent::Error {
                                request_sequence,
                                upload_id: None,
                                err,
                            })
                            .await
                            .is_err()
                        {
                            break 'worker;
                        }
                    }
                }
            }
            UploadWorkerCommand::Chunk {
                request_sequence,
                chunk,
            } => {
                let upload_id_bytes = match upload_id_bytes_from_slice(&chunk.upload_id) {
                    Some(id) => id,
                    None => {
                        let err = StoreResourceError::InvalidChunk(format!(
                            "upload_id length={} (must be 16)",
                            chunk.upload_id.len()
                        ));
                        if event_tx
                            .send(UploadWorkerEvent::Error {
                                request_sequence,
                                upload_id: Some(chunk.upload_id.clone()),
                                err,
                            })
                            .await
                            .is_err()
                        {
                            break 'worker;
                        }
                        continue;
                    }
                };

                let Some(tracked) = in_flight_uploads.get(&upload_id_bytes).cloned() else {
                    let err = StoreResourceError::InvalidChunk(
                        "upload_id is not in-flight for this session".to_string(),
                    );
                    if event_tx
                        .send(UploadWorkerEvent::Error {
                            request_sequence,
                            upload_id: Some(chunk.upload_id.clone()),
                            err,
                        })
                        .await
                        .is_err()
                    {
                        break 'worker;
                    }
                    continue;
                };

                if !chunk.data.is_empty() {
                    tokio::select! {
                        biased;
                        _ = event_tx.closed() => break 'worker,
                        _ = apply_upload_transport_backpressure(&mut upload_rate_limiter, chunk.data.len()) => {}
                    }
                }

                if let Err(err) = store
                    .handle_upload_chunk(
                        &namespace,
                        UploadId::from_bytes(upload_id_bytes),
                        chunk.chunk_index,
                        chunk.data,
                    )
                    .await
                {
                    in_flight_uploads.remove(&upload_id_bytes);
                    store
                        .abort_upload(&namespace, UploadId::from_bytes(upload_id_bytes))
                        .await;
                    if event_tx
                        .send(UploadWorkerEvent::Error {
                            request_sequence: tracked.request_sequence,
                            upload_id: Some(upload_id_bytes.to_vec()),
                            err,
                        })
                        .await
                        .is_err()
                    {
                        break 'worker;
                    }
                }
            }
            UploadWorkerCommand::Complete {
                request_sequence,
                capabilities,
                complete,
            } => {
                let upload_id_bytes = match upload_id_bytes_from_slice(&complete.upload_id) {
                    Some(id) => id,
                    None => {
                        let err = StoreResourceError::InvalidChunk(format!(
                            "upload_id length={} (must be 16)",
                            complete.upload_id.len()
                        ));
                        if event_tx
                            .send(UploadWorkerEvent::Error {
                                request_sequence,
                                upload_id: Some(complete.upload_id.clone()),
                                err,
                            })
                            .await
                            .is_err()
                        {
                            break 'worker;
                        }
                        continue;
                    }
                };

                let Some(tracked) = in_flight_uploads.get(&upload_id_bytes).cloned() else {
                    let err = StoreResourceError::InvalidChunk(
                        "upload_id is not in-flight for this session".to_string(),
                    );
                    if event_tx
                        .send(UploadWorkerEvent::Error {
                            request_sequence,
                            upload_id: Some(complete.upload_id.clone()),
                            err,
                        })
                        .await
                        .is_err()
                    {
                        break 'worker;
                    }
                    continue;
                };

                match store
                    .handle_upload_complete(
                        &namespace,
                        UploadId::from_bytes(upload_id_bytes),
                        &capabilities,
                        &AgentBudget {
                            texture_bytes_total_limit: 0,
                            texture_bytes_total_used: 0,
                        },
                    )
                    .await
                {
                    Ok(stored) => {
                        in_flight_uploads.remove(&upload_id_bytes);
                        register_uploaded_scene_resource(&state, &stored.resource_id, &render_wake)
                            .await;
                        if event_tx
                            .send(UploadWorkerEvent::Stored {
                                request_sequence: tracked.request_sequence,
                                stored,
                                stored_bytes: tracked.total_size_bytes,
                                metadata: tracked.metadata,
                                upload_id: Some(upload_id_bytes),
                            })
                            .await
                            .is_err()
                        {
                            break 'worker;
                        }
                    }
                    Err(err) => {
                        in_flight_uploads.remove(&upload_id_bytes);
                        if event_tx
                            .send(UploadWorkerEvent::Error {
                                request_sequence: tracked.request_sequence,
                                upload_id: Some(upload_id_bytes.to_vec()),
                                err,
                            })
                            .await
                            .is_err()
                        {
                            break 'worker;
                        }
                    }
                }
            }
        }
    }
    for upload_id in in_flight_uploads.into_keys() {
        store
            .abort_upload(&namespace, UploadId::from_bytes(upload_id))
            .await;
    }
}

// ─── Upload helper functions ──────────────────────────────────────────────────

fn proto_resource_type_to_store(resource_type: i32) -> Option<StoreResourceType> {
    match resource_type {
        1 => Some(StoreResourceType::ImageRgba8),
        2 => Some(StoreResourceType::ImagePng),
        3 => Some(StoreResourceType::ImageJpeg),
        4 => Some(StoreResourceType::FontTtf),
        5 => Some(StoreResourceType::FontOtf),
        6 => Some(StoreResourceType::ImageSvg),
        _ => None,
    }
}

pub(super) fn upload_id_bytes_from_slice(upload_id: &[u8]) -> Option<[u8; 16]> {
    let arr: [u8; 16] = upload_id.try_into().ok()?;
    Some(arr)
}

pub(super) async fn apply_upload_transport_backpressure(
    limiter: &mut UploadByteRateLimiter,
    mut bytes: usize,
) {
    while bytes > 0 {
        let now = Instant::now();
        let available = limiter.available_bytes(now);
        if available > 0 {
            let consumed = bytes.min(available);
            limiter.reserve_bytes(now, consumed);
            bytes -= consumed;
            continue;
        }

        let delay = limiter.next_delay(now);
        if delay.is_zero() {
            tokio::task::yield_now().await;
        } else {
            tokio::time::sleep(delay).await;
        }
    }
}

pub(super) async fn register_uploaded_scene_resource(
    state: &Arc<Mutex<SharedState>>,
    resource_id: &tze_hud_resource::ResourceId,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) {
    let scene_resource_id = ResourceId::from_bytes(*resource_id.as_bytes());
    let st = state.lock().await;
    let mut scene = st.scene.lock().await;
    let inserted = !scene.is_resource_registered(&scene_resource_id);
    scene.register_resource(scene_resource_id);
    drop(scene);
    drop(st);
    if inserted {
        render_wake.notify();
    }
}
