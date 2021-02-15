use std::{
    collections::{hash_map::Entry, HashMap, VecDeque},
    future::Future,
    hash::{Hash, Hasher},
    pin::Pin,
    time::{Duration, Instant},
};

use futures::{
    task::{Context, Poll},
    FutureExt,
};
use never::Never;
use tokio::sync::{
    mpsc,
    oneshot::{channel, Receiver, Sender},
};
use vecrem::VecExt;

use crate::{
    adaptors::throttle::chan_send::{ChanSend, SendTy},
    requests::{HasPayload, Output, Request, Requester},
    types::*,
};

// Throttling is quite complicated. This comment describes the algorithm of the
// current implementation.
//
// ### Request
//
// When a throttling request is sent, it sends a tuple of `ChatId` and
// `Sender<()>` to the worker. Then the request waits for a notification from
// the worker. When notification is received, it sends the underlying request.
//
// ### Worker
//
// The worker does the most important job -- it ensures that the limits are
// never exceeded.
//
// The worker stores a history of requests sent in the last minute (and to which
// chats they were sent) and a queue of pending updates.
//
// The worker does the following algorithm loop:
//
// 1. If the queue is empty, wait for the first message in incoming channel (and
// add it to the queue).
//
// 2. Read all present messages from an incoming channel and transfer them to
// the queue.
//
// 3. Record the current time.
//
// 4. Clear the history from records whose time < (current time - minute).
//
// 5. Count all requests which were sent last second, `allowed =
// limit.messages_per_sec_overall - count`.
//
// 6. If `allowed == 0` wait a bit and `continue` to the next iteration.
//
// 7. Count how many requests were sent to which chats (i.e.: create
// `Map<ChatId, Count>`). (Note: the same map, but for last minute also exists,
// but it's updated, instead of recreation.)
//
// 8. While `allowed >= 0` search for requests which chat haven't exceed the
// limits (i.e.: map[chat] < limit), if one is found, decrease `allowed`, notify
// the request that it can be now executed, increase counts, add record to the
// history.

const MINUTE: Duration = Duration::from_secs(60);
const SECOND: Duration = Duration::from_secs(1);

// Delay between worker iterations.
//
// For now it's `second/4`, but that number is chosen pretty randomly, we may
// want to change this.
const DELAY: Duration = Duration::from_millis(250);

/// Telegram request limits.
///
/// This struct is used in [`Throttle`].
///
/// Note that you may ask telegram [@BotSupport] to increase limits for your
/// particular bot if it has a lot of users (but they may or may not do that).
///
/// [@BotSupport]: https://t.me/botsupport
#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct Limits {
    /// Allowed messages in one chat per second.
    pub messages_per_sec_chat: u32,

    /// Allowed messages in one chat per minute.
    pub messages_per_min_chat: u32,

    /// Allowed messages per second.
    pub messages_per_sec_overall: u32,
}

/// Defaults are taken from [telegram documentation][tgdoc].
///
/// [tgdoc]: https://core.telegram.org/bots/faq#my-bot-is-hitting-limits-how-do-i-avoid-this
impl Default for Limits {
    fn default() -> Self {
        Self {
            messages_per_sec_chat: 1,
            messages_per_sec_overall: 30,
            messages_per_min_chat: 20,
        }
    }
}

/// Automatic request limits respecting mechanism.
///
/// Telegram has strict [limits], which, if exceeded will sooner or later cause
/// `RequestError::RetryAfter(_)` errors. These errors can cause users of your
/// bot to never receive responds from the bot or receive them in wrong order.
///
/// This bot wrapper automatically checks for limits, suspending requests until
/// they could be sent without exceeding limits (request order in chats is not
/// changed).
///
/// It's recommended to use this wrapper before other wrappers (i.e.:
/// `SomeWrapper<Throttle<Bot>>`) because if done otherwise inner wrappers may
/// cause `Throttle` to miscalculate limits usage.
///
/// [limits]: https://core.telegram.org/bots/faq#my-bot-is-hitting-limits-how-do-i-avoid-this
///
/// ## Examples
///
/// ```no_run (throttle fails to spawn task without tokio runtime)
/// use teloxide_core::{adaptors::throttle::Limits, requests::RequesterExt, Bot};
///
/// # #[allow(deprecated)]
/// let bot = Bot::new("TOKEN").throttle(Limits::default());
///
/// /* send many requests here */
/// ```
///
/// ## Note about send-by-@channelusername
///
/// Telegram have limits on sending messages to _the same chat_. To check them
/// we store `chat_id`s of several last requests. _However_ there is no good way
/// to tell if given `ChatId::Id(x)` corresponds to the same chat as
/// `ChatId::ChannelUsername(u)`.
///
/// Our current approach is to just give up and check `chat_id_a == chat_id_b`.
/// This may give incorrect results.
///
/// As such, we encourage not to use `ChatId::ChannelUsername(u)` with this bot
/// wrapper.
pub struct Throttle<B> {
    bot: B,
    // Sender<Never> is used to pass the signal to unlock by closing the channel.
    queue: mpsc::Sender<(ChatIdHash, Sender<Never>)>,
}

type RequestsSent = u32;

// I wish there was special data structure for history which removed the
// need in 2 hashmaps
// (waffle)
#[derive(Default)]
struct RequestsSentToChats {
    per_min: HashMap<ChatIdHash, RequestsSent>,
    per_sec: HashMap<ChatIdHash, RequestsSent>,
}

async fn worker(limits: Limits, mut rx: mpsc::Receiver<(ChatIdHash, Sender<Never>)>) {
    // FIXME(waffle): Make an research about data structures for this queue.
    //                Currently this is O(n) removing (n = number of elements
    //                stayed), amortized O(1) push (vec+vecrem).
    let mut queue: Vec<(ChatIdHash, Sender<Never>)> =
        Vec::with_capacity(limits.messages_per_sec_overall as usize);

    let mut history: VecDeque<(ChatIdHash, Instant)> = VecDeque::new();
    let mut requests_sent = RequestsSentToChats::default();

    let mut rx_is_closed = false;

    while !rx_is_closed || !queue.is_empty() {
        read_from_rx(&mut rx, &mut queue, &mut rx_is_closed).await;

        // _Maybe_ we need to use `spawn_blocking` here, because there is
        // decent amount of blocking work. However _for now_ I've decided not
        // to use it here.
        //
        // Reasons (not to use `spawn_blocking`):
        //
        // 1. The work seems not very CPU-bound, it's not heavy computations,
        //    it's more like light computations.
        //
        // 2. `spawn_blocking` is not zero-cost — it spawns a new system thread
        //    + do so other work. This may actually be *worse* then current
        //    "just do everything in this async fn" approach.
        //
        // 3. With `rt-threaded` feature, tokio uses [`num_cpus()`] threads
        //    which should be enough to work fine with one a-bit-blocking task.
        //    Crucially current behaviour will be problem mostly with
        //    single-threaded runtimes (and in case you're using one, you
        //    probably don't want to spawn unnecessary threads anyway).
        //
        // I think if we'll ever change this behaviour, we need to make it
        // _configurable_.
        //
        // See also [discussion (ru)].
        //
        // NOTE: If you are reading this because you have any problems because
        // of this worker, open an [issue on github]
        //
        // [`num_cpus()`]: https://vee.gg/JGwq2
        // [discussion (ru)]: https://t.me/rust_async/27891
        // [issue on github]: https://github.com/teloxide/teloxide/issues/new
        //
        // (waffle)

        let now = Instant::now();
        let min_back = now - MINUTE;
        let sec_back = now - SECOND;

        // make history and hchats up-to-date
        while let Some((_, time)) = history.front() {
            // history is sorted, we found first up-to-date thing
            if time >= &min_back {
                break;
            }

            if let Some((chat, _)) = history.pop_front() {
                let entry = requests_sent.per_min.entry(chat).and_modify(|count| {
                    *count -= 1;
                });

                if let Entry::Occupied(entry) = entry {
                    if *entry.get() == 0 {
                        entry.remove_entry();
                    }
                }
            }
        }

        // as truncates which is ok since in case of truncation it would always be >=
        // limits.overall_s
        let used = history
            .iter()
            .take_while(|(_, time)| time > &sec_back)
            .count() as u32;
        let mut allowed = limits.messages_per_sec_overall.saturating_sub(used);

        if allowed == 0 {
            requests_sent.per_sec.clear();
            tokio::time::sleep(DELAY).await;
            continue;
        }

        for (chat, _) in history.iter().take_while(|(_, time)| time > &sec_back) {
            *requests_sent.per_sec.entry(*chat).or_insert(0) += 1;
        }

        let mut queue_removing = queue.removing();

        while let Some(entry) = queue_removing.next() {
            let chat = &entry.value().0;
            let requests_sent_count = requests_sent.per_sec.get(chat).copied().unwrap_or(0);
            let limits_not_exceeded = requests_sent_count < limits.messages_per_sec_chat
                && requests_sent_count < limits.messages_per_min_chat;

            if limits_not_exceeded {
                *requests_sent.per_sec.entry(*chat).or_insert(0) += 1;
                *requests_sent.per_min.entry(*chat).or_insert(0) += 1;
                history.push_back((*chat, Instant::now()));

                // Close the channel and unlock the associated request.
                drop(entry.remove());

                // We have "sent" one request, so now we can send one less.
                allowed -= 1;
                if allowed == 0 {
                    break;
                }
            }
        }

        // It's easier to just recompute last second stats, instead of keeping
        // track of it alongside with minute stats, so we just throw this away.
        requests_sent.per_sec.clear();
        tokio::time::sleep(DELAY).await;
    }
}

async fn read_from_rx(
    rx: &mut mpsc::Receiver<(ChatIdHash, Sender<Never>)>,
    queue: &mut Vec<(ChatIdHash, Sender<Never>)>,
    rx_is_closed: &mut bool,
) {
    if queue.is_empty() {
        match rx.recv().await {
            Some(req) => queue.push(req),
            None => *rx_is_closed = true,
        }
    }

    loop {
        // FIXME(waffle): https://github.com/tokio-rs/tokio/issues/3350
        match rx.recv().now_or_never() {
            Some(Some(req)) => queue.push(req),
            Some(None) => *rx_is_closed = true,
            // There are no items in queue.
            None => break,
        }
    }
}

impl<B> Throttle<B> {
    /// Creates new [`Throttle`] alongside with worker future.
    ///
    /// Note: [`Throttle`] will only send requests if returned worker is
    /// polled/spawned/awaited.
    pub fn new(bot: B, limits: Limits) -> (Self, impl Future<Output = ()>) {
        let (tx, rx) = mpsc::channel(limits.messages_per_sec_overall as usize);

        let worker = worker(limits, rx);
        let this = Self { bot, queue: tx };

        (this, worker)
    }

    /// Creates new [`Throttle`] spawning the worker with `tokio::spawn`
    ///
    /// Note: it's recommended to use [`RequesterExt::throttle`] instead.
    ///
    /// [`RequesterExt::throttle`]: crate::requests::RequesterExt::throttle
    pub fn new_spawn(bot: B, limits: Limits) -> Self
    where
        // Basically, I hate this bound.
        // This is yet another problem caused by [rust-lang/#76882].
        // And I think it *is* a bug.
        //
        // [rust-lang/#76882]: https://github.com/rust-lang/rust/issues/76882
        //
        // Though crucially I can't think of a case with non-static bot.
        // But anyway, it doesn't change the fact that this bound is redundant.
        //
        // (waffle)
        B: 'static,
    {
        let (this, worker) = Self::new(bot, limits);
        tokio::spawn(worker);
        this
    }

    /// Allows to access inner bot
    pub fn inner(&self) -> &B {
        &self.bot
    }

    /// Unwraps inner bot
    pub fn into_inner(self) -> B {
        self.bot
    }
}

macro_rules! f {
    ($m:ident $this:ident ($($arg:ident : $T:ty),*)) => {
        ThrottlingRequest(
            $this.inner().$m($($arg),*),
            $this.queue.clone(),
            |p| (&p.payload_ref().chat_id).into(),
        )
    };
}

macro_rules! fty {
    ($T:ident) => {
        ThrottlingRequest<B::$T>
    };
}

macro_rules! fid {
    ($m:ident $this:ident ($($arg:ident : $T:ty),*)) => {
        $this.inner().$m($($arg),*)
    };
}

macro_rules! ftyid {
    ($T:ident) => {
        B::$T
    };
}

impl<B: Requester> Requester for Throttle<B>
where
    B::SendMessage: Send,
    B::ForwardMessage: Send,
    B::SendPhoto: Send,
    B::SendAudio: Send,
    B::SendDocument: Send,
    B::SendVideo: Send,
    B::SendAnimation: Send,
    B::SendVoice: Send,
    B::SendVideoNote: Send,
    B::SendMediaGroup: Send,
    B::SendLocation: Send,
    B::SendVenue: Send,
    B::SendContact: Send,
    B::SendPoll: Send,
    B::SendDice: Send,
    B::SendSticker: Send,
    B::SendInvoice: Send,
{
    type Err = B::Err;

    requester_forward! {
        send_message,
        forward_message, send_photo, send_audio, send_document, send_video,
        send_animation, send_voice, send_video_note, send_media_group, send_location,
        send_venue, send_contact, send_poll, send_dice, send_sticker,  => f, fty
    }

    type SendInvoice = ThrottlingRequest<B::SendInvoice>;

    fn send_invoice<T, D, Pa, P, S, C, Pri>(
        &self,
        chat_id: i32,
        title: T,
        description: D,
        payload: Pa,
        provider_token: P,
        start_parameter: S,
        currency: C,
        prices: Pri,
    ) -> Self::SendInvoice
    where
        T: Into<String>,
        D: Into<String>,
        Pa: Into<String>,
        P: Into<String>,
        S: Into<String>,
        C: Into<String>,
        Pri: IntoIterator<Item = LabeledPrice>,
    {
        ThrottlingRequest(
            self.inner().send_invoice(
                chat_id,
                title,
                description,
                payload,
                provider_token,
                start_parameter,
                currency,
                prices,
            ),
            self.queue.clone(),
            |p| ChatIdHash::Id(p.payload_ref().chat_id as _),
        )
    }

    requester_forward! {
        get_me, get_updates, set_webhook, delete_webhook, get_webhook_info,
        edit_message_live_location, edit_message_live_location_inline,
        stop_message_live_location, stop_message_live_location_inline,
        send_chat_action, get_user_profile_photos, get_file, kick_chat_member,
        unban_chat_member, restrict_chat_member, promote_chat_member,
        set_chat_administrator_custom_title, set_chat_permissions,
        export_chat_invite_link, set_chat_photo, delete_chat_photo, set_chat_title,
        set_chat_description, pin_chat_message, unpin_chat_message, leave_chat,
        get_chat, get_chat_administrators, get_chat_members_count,
        get_chat_member, set_chat_sticker_set, delete_chat_sticker_set,
        answer_callback_query, set_my_commands, get_my_commands, answer_inline_query,
        edit_message_text, edit_message_text_inline, edit_message_caption,
        edit_message_caption_inline, edit_message_media, edit_message_media_inline,
        edit_message_reply_markup, edit_message_reply_markup_inline, stop_poll,
        delete_message, get_sticker_set, upload_sticker_file, create_new_sticker_set,
        add_sticker_to_set, set_sticker_position_in_set, delete_sticker_from_set,
        set_sticker_set_thumb, answer_shipping_query, answer_pre_checkout_query,
        set_passport_data_errors, send_game, set_game_score, set_game_score_inline,
        get_game_high_scores => fid, ftyid
    }
}

download_forward! {
    'w
    B
    Throttle<B>
    { this => this.inner() }
}

/// An ID used in the worker.
///
/// It is used instead of `ChatId` to make copying cheap even in case of
/// usernames. (It is just a hashed username.)
#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
enum ChatIdHash {
    Id(i64),
    ChannelUsernameHash(u64),
}

impl From<&ChatId> for ChatIdHash {
    fn from(value: &ChatId) -> Self {
        match value {
            ChatId::Id(id) => ChatIdHash::Id(*id),
            ChatId::ChannelUsername(username) => {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                username.hash(&mut hasher);
                let hash = hasher.finish();
                ChatIdHash::ChannelUsernameHash(hash)
            }
        }
    }
}

pub struct ThrottlingRequest<R: HasPayload>(
    R,
    mpsc::Sender<(ChatIdHash, Sender<Never>)>,
    fn(&R::Payload) -> ChatIdHash,
);

impl<R: HasPayload> HasPayload for ThrottlingRequest<R> {
    type Payload = R::Payload;

    fn payload_mut(&mut self) -> &mut Self::Payload {
        self.0.payload_mut()
    }

    fn payload_ref(&self) -> &Self::Payload {
        self.0.payload_ref()
    }
}

impl<R> Request for ThrottlingRequest<R>
where
    R: Request + Send,
{
    type Err = R::Err;
    type Send = ThrottlingSend<R>;
    type SendRef = ThrottlingSendRef<R>;

    fn send(self) -> Self::Send {
        let (tx, rx) = channel();
        let id = self.2(self.payload_ref());
        let send = self.1.send_t((id, tx));
        ThrottlingSend(ThrottlingSendInner::Registering {
            request: self.0,
            send,
            wait: rx,
        })
    }

    fn send_ref(&self) -> Self::SendRef {
        let (tx, rx) = channel();
        let send = self.1.clone().send_t((self.2(self.payload_ref()), tx));

        // As we can't move self.0 (request) out, as we do in `send` we are
        // forced to call `send_ref()`. This may have overhead and/or lead to
        // wrong results because `R::send_ref` does the send.
        //
        // However `Request` documentation explicitly notes that `send{,_ref}`
        // should **not** do any kind of work, so it's ok.
        let request = self.0.send_ref();

        ThrottlingSendRef(ThrottlingSendRefInner::Registering {
            request,
            send,
            wait: rx,
        })
    }
}

#[pin_project::pin_project]
pub struct ThrottlingSend<R: Request>(#[pin] ThrottlingSendInner<R>);

#[pin_project::pin_project(project = SendProj, project_replace = SendRepl)]
enum ThrottlingSendInner<R: Request> {
    Registering {
        request: R,
        #[pin]
        send: ChanSend,
        wait: Receiver<Never>,
    },
    Pending {
        request: R,
        #[pin]
        wait: Receiver<Never>,
    },
    Sent {
        #[pin]
        fut: R::Send,
    },
    Done,
}

impl<R: Request> Future for ThrottlingSend<R> {
    type Output = Result<Output<R>, R::Err>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project().0;

        match this.as_mut().project() {
            SendProj::Registering {
                request: _,
                send,
                wait: _,
            } => match send.poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(res) => {
                    if let SendRepl::Registering {
                        request,
                        send: _,
                        wait,
                    } = this.as_mut().project_replace(ThrottlingSendInner::Done)
                    {
                        match res {
                            Ok(()) => this
                                .as_mut()
                                .project_replace(ThrottlingSendInner::Pending { request, wait }),
                            // The worker is unlikely to drop queue before sending all requests,
                            // but just in case it has dropped the queue, we want to just send the
                            // request.
                            Err(_) => this.as_mut().project_replace(ThrottlingSendInner::Sent {
                                fut: request.send(),
                            }),
                        };
                    }

                    self.poll(cx)
                }
            },
            SendProj::Pending { request: _, wait } => match wait.poll(cx) {
                Poll::Pending => Poll::Pending,
                // Worker pass "message" to unlock us by closing the channel,
                // and thus we can safely ignore this result as we know it will
                // always be `Err(_)` (because `Ok(Never)` is uninhibited)
                // and that's what we want.
                Poll::Ready(_) => {
                    if let SendRepl::Pending { request, wait: _ } =
                        this.as_mut().project_replace(ThrottlingSendInner::Done)
                    {
                        this.as_mut().project_replace(ThrottlingSendInner::Sent {
                            fut: request.send(),
                        });
                    }

                    self.poll(cx)
                }
            },
            SendProj::Sent { fut } => {
                let res = futures::ready!(fut.poll(cx));
                this.set(ThrottlingSendInner::Done);
                Poll::Ready(res)
            }
            SendProj::Done => Poll::Pending,
        }
    }
}

#[pin_project::pin_project]
pub struct ThrottlingSendRef<R: Request>(#[pin] ThrottlingSendRefInner<R>);

#[pin_project::pin_project(project = SendRefProj, project_replace = SendRefRepl)]
enum ThrottlingSendRefInner<R: Request> {
    Registering {
        request: R::SendRef,
        #[pin]
        send: ChanSend,
        wait: Receiver<Never>,
    },
    Pending {
        request: R::SendRef,
        #[pin]
        wait: Receiver<Never>,
    },
    Sent {
        #[pin]
        fut: R::SendRef,
    },
    Done,
}

impl<R: Request> Future for ThrottlingSendRef<R> {
    type Output = Result<Output<R>, R::Err>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project().0;

        match this.as_mut().project() {
            SendRefProj::Registering {
                request: _,
                send,
                wait: _,
            } => match send.poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(res) => {
                    if let SendRefRepl::Registering {
                        request,
                        send: _,
                        wait,
                    } = this.as_mut().project_replace(ThrottlingSendRefInner::Done)
                    {
                        match res {
                            Ok(()) => this
                                .as_mut()
                                .project_replace(ThrottlingSendRefInner::Pending { request, wait }),
                            // The worker is unlikely to drop queue before sending all requests,
                            // but just in case it has dropped the queue, we want to just send the
                            // request.
                            Err(_) => this
                                .as_mut()
                                .project_replace(ThrottlingSendRefInner::Sent { fut: request }),
                        };
                    }

                    self.poll(cx)
                }
            },
            SendRefProj::Pending { request: _, wait } => match wait.poll(cx) {
                Poll::Pending => Poll::Pending,
                // Worker pass "message" to unlock us by closing the channel,
                // and thus we can safely ignore this result as we know it will
                // always be `Err(_)` (because `Ok(Never)` is uninhibited)
                // and that's what we want.
                Poll::Ready(_) => {
                    if let SendRefRepl::Pending { request, wait: _ } =
                        this.as_mut().project_replace(ThrottlingSendRefInner::Done)
                    {
                        this.as_mut()
                            .project_replace(ThrottlingSendRefInner::Sent { fut: request });
                    }

                    self.poll(cx)
                }
            },
            SendRefProj::Sent { fut } => {
                let res = futures::ready!(fut.poll(cx));
                this.set(ThrottlingSendRefInner::Done);
                Poll::Ready(res)
            }
            SendRefProj::Done => Poll::Pending,
        }
    }
}

mod chan_send {
    use std::{future::Future, pin::Pin};

    use futures::task::{Context, Poll};
    use never::Never;
    use tokio::sync::{mpsc, mpsc::error::SendError, oneshot::Sender};

    use crate::adaptors::throttle::ChatIdHash;

    pub(super) trait SendTy {
        fn send_t(self, val: (ChatIdHash, Sender<Never>)) -> ChanSend;
    }

    #[pin_project::pin_project]
    pub(super) struct ChanSend(#[pin] Inner);

    #[cfg(not(feature = "nightly"))]
    type Inner =
        Pin<Box<dyn Future<Output = Result<(), SendError<(ChatIdHash, Sender<Never>)>>> + Send>>;
    #[cfg(feature = "nightly")]
    type Inner = impl Future<Output = Result<(), SendError<(ChatIdHash, Sender<Never>)>>>;

    impl SendTy for mpsc::Sender<(ChatIdHash, Sender<Never>)> {
        // `return`s trick IDEA not to show errors
        #[allow(clippy::needless_return)]
        fn send_t(self, val: (ChatIdHash, Sender<Never>)) -> ChanSend {
            #[cfg(feature = "nightly")]
            {
                fn def(
                    sender: mpsc::Sender<(ChatIdHash, Sender<Never>)>,
                    val: (ChatIdHash, Sender<Never>),
                ) -> Inner {
                    async move { sender.send(val).await }
                }
                return ChanSend(def(self, val));
            }
            #[cfg(not(feature = "nightly"))]
            {
                let this = self;
                return ChanSend(Box::pin(async move { this.send(val).await }));
            }
        }
    }

    impl Future for ChanSend {
        type Output = Result<(), SendError<(ChatIdHash, Sender<Never>)>>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.project().0.poll(cx)
        }
    }
}
