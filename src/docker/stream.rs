//! Log streaming and container supervision.
//!
//! One task per container reads its log stream into a shared channel. A
//! supervisor task watches Docker events so that containers which restart (and
//! therefore get a *new* container ID) are reattached, and containers created
//! after startup are picked up.

use crate::docker::client::is_transient_labels;
use crate::docker::labels;
use crate::docker::DockerClient;
use anyhow::Result;
use bollard::query_parameters::{
    EventsOptionsBuilder, ListContainersOptionsBuilder, LogsOptionsBuilder,
};
use bollard::Docker;
use futures::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How long to wait before re-subscribing after the event stream drops.
const EVENT_RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// Safety net: re-list periodically so a container is picked up even if its
/// event was missed entirely.
const RESYNC_INTERVAL: Duration = Duration::from_secs(5);

/// Largest slice of one log frame carried by a single [`SourceEvent::Output`].
///
/// The size of a frame is the daemon's decision, not ours, and bollard hands
/// one back already allocated -- that allocation is not something this code
/// can prevent. What it can prevent is the frame being multiplied: copied
/// whole into an event, queued in a 4096-slot channel, and copied again by a
/// consumer that bounds only what it *retains* (`MAX_PARTIAL` in the fallback
/// assembler, `MAX_RAW_BYTES` in `LogStore`). Splitting here is what makes
/// everything downstream of the frame finite, and lets the frame itself be
/// freed at the end of the iteration.
///
/// How big a frame gets depends on the service, and there are two cases.
/// Without a tty the daemon multiplexes the stream through stdcopy framing
/// and its log copier splits a message at 16 KiB: a 5 MB line containing no
/// newline at all comes back as 306 frames, none over 16384 bytes, on both
/// the json-file and local drivers. Those never reach this bound. With
/// `tty: true` there is no framing at all -- the same 5 MB arrives as one
/// unbroken body -- and bollard's decoder cuts it at newlines instead,
/// yielding a whole line per frame however long the line is. That is the case
/// this bound exists for, and the case where it actually fires. It buffers
/// that unframed stream until it finds a newline, which #38 tracks.
///
/// 64 KiB is four times the daemon's own 16 KiB message split, so the common
/// path keeps costing exactly one copy per frame, and it holds the most the
/// 4096-slot channel can carry to about 256 MiB, where before a single slot
/// was unbounded.
pub(crate) const MAX_CHUNK_BYTES: usize = 64 * 1024;

/// A message from the Docker layer to the UI.
#[derive(Debug)]
pub enum SourceEvent {
    /// Raw output from one service's container.
    Output {
        service: String,
        replica: u32,
        bytes: Vec<u8>,
    },
    /// Container topology or status changed; the UI should re-read services.
    Topology,
}

/// What a Docker event action means for us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventDecision {
    /// Container set may have changed: re-list and attach as needed.
    Resync,
    /// Only status changed: refresh the UI, don't touch attachments.
    NotifyOnly,
    Ignore,
}

/// Classifies a Docker container event.
///
/// `create` and `start` both bring a new container ID into play — compose
/// replaces containers rather than restarting them in place — so both resync.
pub fn event_decision(action: &str) -> EventDecision {
    match action {
        "create" | "start" | "destroy" | "rename" => EventDecision::Resync,
        "die" | "kill" | "stop" | "restart" | "pause" | "unpause" | "health_status" => {
            EventDecision::NotifyOnly
        }
        _ => EventDecision::Ignore,
    }
}

/// Which slice of a container's log history to request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogWindow<'a> {
    /// Unix seconds to resume from, when reattaching.
    pub since: Option<i32>,
    pub tail: &'a str,
}

/// Chooses the log window for an attach.
///
/// A fresh attach takes the configured tail. A reattach resumes from where the
/// previous stream stopped and asks for everything since, so recovering from a
/// dropped connection neither replays the tail nor leaves a hole.
///
/// `since` has one-second resolution, so a reattach can replay a line the dying
/// task had already delivered, showing it twice. That is deliberate: the only
/// alternative is to resume a second later and risk dropping output, and a
/// duplicated line is far cheaper than a missing one.
pub fn log_window(since: Option<i64>, tail: &str) -> LogWindow<'_> {
    match since {
        // The Engine API models this field as a 32-bit count of seconds, so
        // bollard's builder takes i32. Clamp instead of casting, so a skewed
        // clock can't wrap a future timestamp into the distant past.
        Some(t) => LogWindow {
            since: Some(t.clamp(0, i32::MAX as i64) as i32),
            tail: "all",
        },
        None => LogWindow { since: None, tail },
    }
}

/// A container we are (or were) streaming.
#[derive(Debug)]
struct Attachment {
    cancel: CancellationToken,
    /// True once the streaming task has exited, for any reason.
    finished: bool,
    /// Wall-clock seconds at which the stream ended, so a reattach can resume
    /// from there instead of replaying the whole tail.
    ended_at: Option<i64>,
}

/// One container as seen in a list response, reduced to what we act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerDesc {
    pub id: String,
    pub service: String,
    pub replica: u32,
    pub running: bool,
}

/// Decides which containers to attach to and which attachments to drop.
///
/// A container is (re)attached when it is running and has no live task: either
/// we have never attached it, or its task has since exited. A container that is
/// *not* running is attached only once, to pull its history — reattaching would
/// spin, because a finished container's log stream returns EOF immediately.
pub fn plan_attachments(
    attached: &HashMap<String, Attach>,
    seen: &[ContainerDesc],
) -> (Vec<ContainerDesc>, Vec<String>) {
    let to_attach = seen
        .iter()
        .filter(|c| match attached.get(&c.id) {
            None => true,
            Some(state) => c.running && state.finished,
        })
        .cloned()
        .collect();
    let to_drop = attached
        .keys()
        .filter(|id| !seen.iter().any(|c| &c.id == *id))
        .cloned()
        .collect();
    (to_attach, to_drop)
}

/// The subset of attachment state `plan_attachments` needs, so it can be tested
/// without constructing cancellation tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attach {
    pub finished: bool,
}

/// Owns the per-container log tasks and the event subscription.
pub struct LogSupervisor {
    docker: Docker,
    project: String,
    tail: usize,
    tx: mpsc::Sender<SourceEvent>,
    attached: HashMap<String, Attachment>,
    /// Container IDs whose streaming task has exited, with the time it ended.
    done_tx: mpsc::UnboundedSender<(String, i64)>,
    done_rx: mpsc::UnboundedReceiver<(String, i64)>,
}

impl LogSupervisor {
    pub fn new(
        client: &DockerClient,
        project: impl Into<String>,
        tail: usize,
        tx: mpsc::Sender<SourceEvent>,
    ) -> Self {
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        Self {
            docker: client.raw().clone(),
            project: project.into(),
            tail,
            tx,
            attached: HashMap::new(),
            done_tx,
            done_rx,
        }
    }

    /// Runs until `cancel` fires.
    pub async fn run(mut self, cancel: CancellationToken) {
        loop {
            if cancel.is_cancelled() {
                break;
            }
            match self.watch_events(&cancel).await {
                Ok(()) => break, // cancelled
                Err(err) => {
                    log_debug(&format!("event stream ended: {err}"));
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(EVENT_RECONNECT_DELAY) => {}
                    }
                }
            }
        }
        for (_, attachment) in self.attached.drain() {
            attachment.cancel.cancel();
        }
    }

    /// Attaches to every project container that needs it, and drops attachments
    /// for containers that no longer exist.
    async fn resync(&mut self) -> Result<()> {
        let mut filters = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec![format!("{}={}", labels::PROJECT, self.project)],
        );
        let options = ListContainersOptionsBuilder::default()
            .all(true)
            .filters(&filters)
            .build();
        let containers = self.docker.list_containers(Some(options)).await?;

        let seen: Vec<ContainerDesc> = containers
            .iter()
            .filter_map(|c| {
                let labels_map = c.labels.as_ref()?;
                if is_transient_labels(labels_map) {
                    return None;
                }
                Some(ContainerDesc {
                    id: c.id.clone()?,
                    service: labels_map.get(labels::SERVICE)?.clone(),
                    replica: labels_map
                        .get(labels::CONTAINER_NUMBER)
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(1),
                    running: matches!(
                        c.state,
                        Some(bollard::models::ContainerSummaryStateEnum::RUNNING)
                    ),
                })
            })
            .collect();

        let view: HashMap<String, Attach> = self
            .attached
            .iter()
            .map(|(id, a)| {
                (
                    id.clone(),
                    Attach {
                        finished: a.finished,
                    },
                )
            })
            .collect();
        let (to_attach, to_drop) = plan_attachments(&view, &seen);

        for id in to_drop {
            if let Some(attachment) = self.attached.remove(&id) {
                attachment.cancel.cancel();
            }
        }
        for desc in to_attach {
            let since = self.attached.get(&desc.id).and_then(|a| a.ended_at);
            self.attach(desc, since);
        }
        Ok(())
    }

    fn attach(&mut self, desc: ContainerDesc, since: Option<i64>) {
        let cancel = CancellationToken::new();
        self.attached.insert(
            desc.id.clone(),
            Attachment {
                cancel: cancel.clone(),
                finished: false,
                ended_at: None,
            },
        );

        let docker = self.docker.clone();
        let tx = self.tx.clone();
        let done = self.done_tx.clone();
        // On a reattach, resume from where the previous stream stopped rather
        // than replaying the tail and duplicating output.
        let tail = self.tail.to_string();
        tokio::spawn(async move {
            let result = stream_container(&docker, &desc, &tail, since, &tx, &cancel).await;
            if let Err(err) = result {
                log_debug(&format!("log stream for {} ended: {err}", desc.service));
            }
            let _ = done.send((desc.id, now_seconds()));
        });
    }

    /// Subscribes to container events for this project. Returns `Ok` only on
    /// cancellation; any stream error is returned so the caller can reconnect.
    async fn watch_events(&mut self, cancel: &CancellationToken) -> Result<()> {
        // Anchor the subscription before listing. The daemon replays events from
        // this instant, so a container starting between the list snapshot and
        // our first poll is still delivered rather than silently missed.
        let since = now_seconds();

        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert("type".to_string(), vec!["container".to_string()]);
        filters.insert(
            "label".to_string(),
            vec![format!("{}={}", labels::PROJECT, self.project)],
        );
        let options = EventsOptionsBuilder::default()
            .since(&since.to_string())
            .filters(&filters)
            .build();
        let mut stream = self.docker.events(Some(options));

        if let Err(err) = self.resync().await {
            log_debug(&format!("initial resync failed: {err}"));
        }
        let _ = self.tx.send(SourceEvent::Topology).await;

        let mut ticker = tokio::time::interval(RESYNC_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // the first tick resolves immediately

        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),

                // A streaming task exited. Clear its liveness so the next resync
                // can reattach if the container is still running.
                Some((id, ended_at)) = self.done_rx.recv() => {
                    if let Some(attachment) = self.attached.get_mut(&id) {
                        attachment.finished = true;
                        attachment.ended_at = Some(ended_at);
                    }
                    let _ = self.tx.send(SourceEvent::Topology).await;
                }

                _ = ticker.tick() => {
                    if let Err(err) = self.resync().await {
                        log_debug(&format!("periodic resync failed: {err}"));
                    }
                }

                next = stream.next() => {
                    let Some(message) = next else {
                        anyhow::bail!("event stream closed by the daemon");
                    };
                    let message = message?;
                    let Some(action) = message.action.as_deref() else { continue };
                    match event_decision(action) {
                        EventDecision::Resync => {
                            if let Err(err) = self.resync().await {
                                log_debug(&format!("resync on '{action}' failed: {err}"));
                            }
                            let _ = self.tx.send(SourceEvent::Topology).await;
                        }
                        EventDecision::NotifyOnly => {
                            let _ = self.tx.send(SourceEvent::Topology).await;
                        }
                        EventDecision::Ignore => {}
                    }
                }
            }
        }
    }
}

/// Reads one container's log stream until it ends or is cancelled.
async fn stream_container(
    docker: &Docker,
    desc: &ContainerDesc,
    tail: &str,
    since: Option<i64>,
    tx: &mpsc::Sender<SourceEvent>,
    cancel: &CancellationToken,
) -> Result<()> {
    let window = log_window(since, tail);
    let mut builder = LogsOptionsBuilder::default()
        .stdout(true)
        .stderr(true)
        .follow(true)
        .tail(window.tail);
    if let Some(since) = window.since {
        builder = builder.since(since);
    }
    let mut stream = docker.logs(&desc.id, Some(builder.build()));

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            next = stream.next() => {
                let Some(chunk) = next else { return Ok(()) };
                // stdout and stderr both land in the same emulator, exactly as
                // they would in a terminal attached to the container.
                if !forward_frame(tx, desc, &chunk?.into_bytes()).await {
                    // Receiver is gone; the UI has shut down.
                    return Ok(());
                }
            }
        }
    }
}

/// Sends one frame downstream in pieces no larger than [`MAX_CHUNK_BYTES`],
/// returning `false` once the receiver has gone away.
///
/// Cutting at a fixed offset is safe because both consumers are stateful
/// across writes: the fallback assembler holds a partial line until its
/// newline arrives, and `LogStore` carries a pending `\r` and its emulator's
/// parse state between writes. A cut mid-line, mid-CRLF, mid-escape or
/// mid-character therefore reads the same as no cut at all, so there is
/// nothing to gain by preferring to cut at a newline.
///
/// An empty frame yields no pieces, which is why there is no explicit guard
/// for one: forwarding it would replace a pane's "waiting" placeholder with a
/// blank pane.
async fn forward_frame(tx: &mpsc::Sender<SourceEvent>, desc: &ContainerDesc, frame: &[u8]) -> bool {
    for piece in frame.chunks(MAX_CHUNK_BYTES) {
        let sent = tx
            .send(SourceEvent::Output {
                service: desc.service.clone(),
                replica: desc.replica,
                bytes: piece.to_vec(),
            })
            .await;
        if sent.is_err() {
            return false;
        }
    }
    true
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Diagnostics go to a file rather than stderr: the alternate screen is active,
/// so printing would corrupt the display.
fn log_debug(message: &str) {
    if std::env::var_os("COMPOSEMUX_DEBUG").is_none() {
        return;
    }
    let path = std::env::temp_dir().join("composemux.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(file, "{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(id: &str, running: bool) -> ContainerDesc {
        ContainerDesc {
            id: id.to_string(),
            service: format!("svc-{id}"),
            replica: 1,
            running,
        }
    }

    fn attached(entries: &[(&str, bool)]) -> HashMap<String, Attach> {
        entries
            .iter()
            .map(|(id, finished)| {
                (
                    id.to_string(),
                    Attach {
                        finished: *finished,
                    },
                )
            })
            .collect()
    }

    /// Drains everything queued, checking each event still carries the identity
    /// of the container it came from.
    fn drain(rx: &mut mpsc::Receiver<SourceEvent>) -> Vec<Vec<u8>> {
        let mut pieces = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                SourceEvent::Output {
                    service,
                    replica,
                    bytes,
                } => {
                    assert_eq!(service, "svc-a", "a piece lost its service");
                    assert_eq!(replica, 1, "a piece lost its replica");
                    pieces.push(bytes);
                }
                other => panic!("expected output, got {other:?}"),
            }
        }
        pieces
    }

    /// The bound the daemon does not give us. Nothing upstream limits how much
    /// one frame contains, and one event used to carry all of it: into a
    /// 4096-slot channel, then into a consumer that bounds only what it keeps.
    #[tokio::test]
    async fn an_oversized_frame_is_forwarded_in_bounded_pieces() {
        let (tx, mut rx) = mpsc::channel(64);
        // Non-uniform, so reassembly is checked byte for byte rather than by
        // length alone; the remainder makes the last piece a short one.
        let size = 4 * MAX_CHUNK_BYTES + 7;
        let frame: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

        assert!(forward_frame(&tx, &desc("a", true), &frame).await);

        let pieces = drain(&mut rx);
        for piece in &pieces {
            assert!(
                piece.len() <= MAX_CHUNK_BYTES,
                "forwarded {} bytes in one event, past the {MAX_CHUNK_BYTES}-byte bound",
                piece.len()
            );
        }
        assert_eq!(pieces.len(), 5, "expected the frame to be cut into pieces");
        let back: Vec<u8> = pieces.concat();
        assert!(back == frame, "the pieces do not reassemble to the frame");
    }

    /// Real Docker output arrives well under the bound, so the common path
    /// must be exactly what it was: one frame, one event, one copy.
    #[tokio::test]
    async fn a_frame_at_the_bound_is_forwarded_in_one_piece() {
        let (tx, mut rx) = mpsc::channel(8);
        let frame = vec![b'x'; MAX_CHUNK_BYTES];

        assert!(forward_frame(&tx, &desc("a", true), &frame).await);

        let pieces = drain(&mut rx);
        assert_eq!(pieces.len(), 1, "a frame at the bound should not be split");
        assert_eq!(pieces[0].len(), MAX_CHUNK_BYTES);
    }

    /// The other side of the boundary, where an off-by-one would live.
    #[tokio::test]
    async fn one_byte_past_the_bound_is_split() {
        let (tx, mut rx) = mpsc::channel(8);
        let frame = vec![b'x'; MAX_CHUNK_BYTES + 1];

        assert!(forward_frame(&tx, &desc("a", true), &frame).await);

        let pieces = drain(&mut rx);
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].len(), MAX_CHUNK_BYTES);
        assert_eq!(pieces[1].len(), 1);
    }

    /// An empty frame must stay unsent: `LogStore` treats any write as output,
    /// so forwarding one would replace a pane's "waiting" placeholder with a
    /// blank pane.
    #[tokio::test]
    async fn an_empty_frame_is_not_forwarded() {
        let (tx, mut rx) = mpsc::channel(8);

        assert!(forward_frame(&tx, &desc("a", true), b"").await);

        assert!(drain(&mut rx).is_empty(), "an empty frame was forwarded");
    }

    /// A departed receiver has to be reported, so the reading loop stops
    /// rather than draining the rest of the container's log into a channel
    /// nobody is holding.
    #[tokio::test]
    async fn forwarding_reports_a_departed_receiver() {
        let (tx, rx) = mpsc::channel::<SourceEvent>(8);
        drop(rx);

        assert!(
            !forward_frame(&tx, &desc("a", true), b"anything").await,
            "the caller must be told to stop reading"
        );
    }

    #[test]
    fn container_lifecycle_events_trigger_a_resync() {
        for action in ["create", "start", "destroy", "rename"] {
            assert_eq!(event_decision(action), EventDecision::Resync, "{action}");
        }
    }

    #[test]
    fn status_only_events_refresh_without_reattaching() {
        for action in [
            "die",
            "kill",
            "stop",
            "restart",
            "pause",
            "unpause",
            "health_status",
        ] {
            assert_eq!(
                event_decision(action),
                EventDecision::NotifyOnly,
                "{action}"
            );
        }
    }

    #[test]
    fn unrelated_events_are_ignored() {
        for action in ["exec_create", "attach", "top", "resize", ""] {
            assert_eq!(event_decision(action), EventDecision::Ignore, "{action}");
        }
    }

    #[test]
    fn a_fresh_attach_uses_the_configured_tail() {
        let window = log_window(None, "200");
        assert_eq!(window.since, None);
        assert_eq!(window.tail, "200");
    }

    #[test]
    fn a_reattach_resumes_from_where_the_stream_stopped() {
        // Replaying the tail here would duplicate output the pane already shows.
        let window = log_window(Some(1_700_000_000), "200");
        assert_eq!(window.since, Some(1_700_000_000));
        assert_eq!(window.tail, "all", "everything since the cut, not a tail");
    }

    #[test]
    fn a_resume_timestamp_is_clamped_rather_than_wrapped() {
        // The API takes i32 seconds; a bad clock must not wrap into the past.
        assert_eq!(log_window(Some(i64::MAX), "200").since, Some(i32::MAX));
        assert_eq!(log_window(Some(-5), "200").since, Some(0));
    }

    #[test]
    fn a_new_container_is_attached() {
        let (attach, drop) = plan_attachments(&HashMap::new(), &[desc("a", true)]);
        assert_eq!(attach.len(), 1);
        assert!(drop.is_empty());
    }

    #[test]
    fn a_live_attachment_is_left_alone() {
        let (attach, drop) = plan_attachments(&attached(&[("a", false)]), &[desc("a", true)]);
        assert!(attach.is_empty(), "must not attach twice");
        assert!(drop.is_empty());
    }

    #[test]
    fn a_running_container_whose_task_died_is_reattached() {
        // The blocking bug: a stream that errors leaves the container running
        // with no reader, and it must be picked back up.
        let (attach, _) = plan_attachments(&attached(&[("a", true)]), &[desc("a", true)]);
        assert_eq!(attach.len(), 1, "a dead task must be replaced");
        assert_eq!(attach[0].id, "a");
    }

    #[test]
    fn a_stopped_container_is_not_reattached_after_its_stream_ends() {
        // Reattaching here would spin: a finished container's follow stream
        // returns its history and closes immediately.
        let (attach, _) = plan_attachments(&attached(&[("a", true)]), &[desc("a", false)]);
        assert!(attach.is_empty());
    }

    #[test]
    fn a_stopped_container_is_still_attached_once_for_its_history() {
        let (attach, _) = plan_attachments(&HashMap::new(), &[desc("a", false)]);
        assert_eq!(attach.len(), 1);
    }

    #[test]
    fn a_removed_container_is_dropped() {
        let (attach, drop) = plan_attachments(&attached(&[("a", false)]), &[]);
        assert!(attach.is_empty());
        assert_eq!(drop, vec!["a".to_string()]);
    }

    #[test]
    fn a_replaced_container_swaps_ids() {
        // Compose recreates rather than restarting, so the old ID vanishes and
        // a new one appears in the same pass.
        let (attach, drop) = plan_attachments(&attached(&[("old", false)]), &[desc("new", true)]);
        assert_eq!(attach.len(), 1);
        assert_eq!(attach[0].id, "new");
        assert_eq!(drop, vec!["old".to_string()]);
    }
}
