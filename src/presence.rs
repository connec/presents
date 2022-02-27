use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    fmt,
    hash::{Hash, Hasher},
    sync::{Arc, Weak},
    task::Poll,
};

use futures::{Stream, StreamExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::wrappers::BroadcastStream;
use tracing::trace;

const BROADCAST_BUFFER: usize = 10;

/// The API for the presence internals.
#[derive(Clone, Debug)]
pub struct Presence {
    joins: mpsc::UnboundedSender<Join>,
}

impl Presence {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(handle_joins(tx.clone(), rx));
        Self { joins: tx }
    }

    pub async fn join(
        &self,
        topic: &str,
        tag: &str,
    ) -> Result<(Vec<Arc<str>>, ParticipantEvents), Unavailable> {
        let (tx, rx) = oneshot::channel();
        let join = Join {
            topic: topic.into(),
            tag: tag.into(),
            tx,
        };

        if self.joins.send(join).is_err() {
            // The receiver has closed, meaning we're no longer handling joins – this is an
            // unrecoverable error, but we want to handle it gracefully.
            return Err(Unavailable);
        }

        let mut joined = rx.await.map_err(|_| {
            // the join never completed, so the service is likely unavailable
            Unavailable
        })?;

        Ok((
            std::mem::take(&mut joined.members),
            ParticipantEvents::new(joined),
        ))
    }
}

// A stream that disseminates presence events.
#[must_use]
pub struct ParticipantEvents {
    tag: Arc<str>,
    topic_tx: mpsc::UnboundedSender<ParticipantEvent>,
    events_rx: BroadcastStream<TopicEvent>,
    _alive: Arc<()>,
}

impl Drop for ParticipantEvents {
    fn drop(&mut self) {
        if self
            .topic_tx
            .send(ParticipantEvent::Leave(self.tag.clone()))
            .is_err()
        {
            tracing::warn!(%self.tag, "topic handler is gone");
        }
    }
}

impl ParticipantEvents {
    fn new(joined: Joined) -> Self {
        Self {
            tag: joined.tag,
            topic_tx: joined.topic_tx,
            events_rx: BroadcastStream::new(joined.events_rx),
            _alive: joined.alive,
        }
    }
}

impl Stream for ParticipantEvents {
    type Item = Result<Event, Unavailable>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.events_rx
            .poll_next_unpin(cx)
            .map_ok(|event| match event {
                TopicEvent::Join(tag) => Event::Join(tag),
                TopicEvent::Leave(tag) => Event::Leave(tag),
            })
            .map_err(|_| Unavailable)
    }
}

pub struct Unavailable;

pub enum Event {
    Join(Arc<str>),
    Leave(Arc<str>),
}

#[derive(Clone, Debug)]
enum TopicEvent {
    Join(Arc<str>),
    Leave(Arc<str>),
}

#[derive(Debug)]
enum ParticipantEvent {
    Join(Join),
    Leave(Arc<str>),
}

struct Join {
    topic: Arc<str>,
    tag: Arc<str>,
    tx: oneshot::Sender<Joined>,
}

impl fmt::Debug for Join {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Join")
            .field("topic", &self.topic.as_ref())
            .field("tag", &self.tag.as_ref())
            .finish_non_exhaustive()
    }
}

struct Joined {
    tag: Arc<str>,
    topic_tx: mpsc::UnboundedSender<ParticipantEvent>,
    members: Vec<Arc<str>>,
    events_rx: broadcast::Receiver<TopicEvent>,
    alive: Arc<()>,
}

// All joins are handled by the same task to simplify state maagement.
#[tracing::instrument(skip_all)]
async fn handle_joins(
    retry: mpsc::UnboundedSender<Join>,
    mut joins: mpsc::UnboundedReceiver<Join>,
) {
    trace!("started");

    let mut topics = HashMap::new();

    while let Some(join) = joins.recv().await {
        trace!(?join);

        // Manually hash the topic so that `topics` doesn't store a copy of the strings
        let mut hasher = DefaultHasher::new();
        join.topic.hash(&mut hasher);
        let topic_key = hasher.finish();

        let tx = topics.entry(topic_key).or_insert_with(|| {
            trace!(topic = %join.topic, "is new");
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(handle_topic(join.topic.clone(), tx.clone(), rx));
            tx
        });

        if let Err(mpsc::error::SendError(ParticipantEvent::Join(join))) =
            tx.send(ParticipantEvent::Join(join))
        {
            // the topic handler has gone, remove it from the state and retry the join
            trace!(?join, "retrying due to dropped topic");
            topics.remove(&topic_key);

            #[allow(clippy::ok_expect)] // Err is not Debug
            retry
                .send(join)
                .ok()
                .expect("BUG: joins receiver gone, in loop over joins receiver?");
        }
    }
}

// Each topic is handled in its own task.
#[tracing::instrument(skip(topic_tx, topic_rx))]
async fn handle_topic(
    topic: Arc<str>,
    topic_tx: mpsc::UnboundedSender<ParticipantEvent>,
    mut topic_rx: mpsc::UnboundedReceiver<ParticipantEvent>,
) {
    trace!("started");

    let (events, _) = broadcast::channel(BROADCAST_BUFFER);
    let mut tags = HashMap::new();

    while let Some(event) = topic_rx.recv().await {
        trace!(?event);
        match event {
            ParticipantEvent::Join(join) => {
                let (tag, alive) = if let Some(res) = tags
                    .get_key_value(&join.tag)
                    .and_then(|(k, v): (&Arc<str>, &Weak<()>)| Some((k.clone(), v.upgrade()?)))
                {
                    trace!(tag = %join.tag, "exists");
                    res
                } else {
                    trace!(tag = %join.tag, "new");
                    let alive = Arc::new(());
                    tags.insert(join.tag.clone(), Arc::downgrade(&alive));
                    (join.tag, alive)
                };

                if events.send(TopicEvent::Join(tag.clone())).is_err() {
                    // all other participants have gone, but we're adding a new one, so we don't terminate
                    trace!("no participants to notify");
                }

                let joined = Joined {
                    tag: tag.clone(),
                    topic_tx: topic_tx.clone(),
                    members: tags.keys().cloned().collect(),
                    events_rx: events.subscribe(),
                    alive,
                };

                trace!(tag = %tag, "joined");
                if join.tx.send(joined).is_err() {
                    // the participant already disconnected, this will be cleaned up automatically
                    // sine the participant was dropped
                    trace!(tag = %tag, "already dropped");
                }
            }
            ParticipantEvent::Leave(tag) => {
                if tags.get(&tag).and_then(Weak::upgrade).is_some() {
                    trace!(%tag, "has other connections");
                } else {
                    trace!(%tag, "no more connections");
                    tags.remove(&tag);
                    if events.send(TopicEvent::Leave(tag)).is_err() {
                        // all participants have gone, so the topic no longer needs handling
                        trace!("no participants to notify, terminating");
                        break;
                    }
                }
            }
        }
    }
}
