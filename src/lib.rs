//! Management for actor services and message routing.
//!
//! An actor is a long-lived thread that performs a specific function or set of functions and
//! consumes a channel of requests from other threads using protocols to describe how and on what
//! data those functions should be performed.
//!
//! While the actor model has numerous benefits, exherting control over actors (such as during a
//! shutdown event) and keeping track of all of their channels can be challenging. This module
//! provides facilities that support the use of the actor model by addressing those challenges.
//!
//! To maintain a registry of actors, keep track of their channels, and respond to control events,
//! a special type of actor called a router is launched. A [Router][Router] encapsulates the
//! channels through which clients interact with the router and the actors it maintains. The
//! router's registry maps an ID for each actor to an [Actor][Actor] type similar to `Router` which
//! encapsulates the channels through which actors receive their input.
//!
//! Actors are spawned through a control message to the router. They are provided an
//! [ActorContext][ActorContext] that provides the channels over which the function receives its
//! inputs and control messages, and a channel over which it can communicate its status back to the
//! router.
//!
//! An actor function must:
//! - Use the `select!` macro that is re-exported from `crossbeam_channel` to receive from both the
//! request channel and the control channel.
//! - Convert the `Any + Send` trait objects it receives over the request channel into the correct
//! concrete type.
//! - Respond to [ActorCtl::Stop][ActorCtl] messages it receives over the control channel by
//! promptly stablizing its state, sending the router an [ActorStatus::Stopped][ActorStatus] status
//! message, and exiting.
//!
//! # Simple Example
//! ```no_run
//! #[macro_use]
//! extern crate herbert;
//! use herbert::prelude::*;
//!
//! fn main() {
//!     // Create a router. This is manditory.
//!     let router = Router::run("example");
//!
//!     // Spawn an actor, here named "herbert".
//!     spawn_actor!(router, "herbert", |ctx: ActorContext| {
//!         loop {
//!             select! {
//!                 // Normal message channel.
//!                 recv(ctx.req) -> msg => {
//!                     match msg {
//!                         Ok(any) => {
//!                             // Convert trait object into concrete type.
//!                             if let Some(value) = any.downcast_ref::<String>() {
//!                                 // Do some work.
//!                                 println!("received order: {}", value);
//!                             } else {
//!                                 // Bad value, here we're just ignoring it.
//!                             }
//!                         }
//!                         Err(e) => {
//!                             // Error receiving a normal message, choosing to terminate.
//!                             break;
//!                         }
//!                     }
//!                 }
//!                 // Control message channel.
//!                 recv(ctx.ctl) -> msg => {
//!                     match msg {
//!                         Ok(ActorCtl::Stop) => {
//!                             // We have been requested to stop.
//!                             // Stabilize any state and then break the loop to exit.
//!                             break;
//!                         }
//!                         Err(e) => {
//!                             // Error receiving a control message, choosing to terminate.
//!                             break;
//!                         },
//!                     }
//!                 }
//!             }
//!         }
//!         // Notify the router we are stopped as our last action.
//!         ctx.report_stopped().unwrap();
//!     }).unwrap();
//!
//!     // Send our order to the "herbert" actor thread.
//!     send_actor!(router, "herbert", String::from("2 gorditas")).unwrap();
//!
//!     // ...
//!
//!     // Shut down the router.
//!     router.shutdown();
//! }
//! ```
//!
//! [Router]: struct.Router.html
//! [Actor]: struct.Actor.html
//! [ActorCtl]: enum.ActorCtl.html
//! [ActorContext]: struct.ActorContext.html
//! [ActorStatus]: enum.ActorStatus.html

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::panic::{catch_unwind, UnwindSafe};
use std::thread;

#[macro_use]
extern crate log;

#[macro_use(select)]
extern crate crossbeam_channel;
use crossbeam_channel::{unbounded, Receiver, Sender};

pub mod prelude {
    //! The things you'll need.
    //!
    //! Re-exports several items from
    //! [crossbeam_channel](https://crates.io/crates/crossbeam-channel), including the `unbounded`
    //! channel type, the `Receiver` and `Sender` types, and the `select!` macro.

    pub use crate::{ActorContext, ActorCtl, ActorStatus, Router, RouterCtl, RouterRequest};
    pub use crossbeam_channel::{select, unbounded, Receiver, Sender};
}

/// The type signature of the messages that are consumed by actors.
///
/// In order to support heterogenous message values between actors, a message is expressed as a
/// trait object statisfying the `Any` and `Send` traits. The value is wrapped in a `Box` so the
/// compiler can reason about its size.
///
/// An actor must downcast the trait object back into a concrete type using the `downcast_ref`
/// method of the `Any` trait.
pub type Message = Box<dyn Any + Send>;

/// A custom result type.
pub type Result<T> = std::result::Result<T, Error>;

/// A handle to the actor management thread.
///
/// When a `Router` is created and its thread is spawned, channels are created to allow clients to
/// send messages to the router and to the actors it is managing. Those channels are packaged into
/// the `Router` struct and returned to the client.
pub struct Router {
    pub id: String,
    pub req: Sender<RouterRequest>,
    pub ctl: Sender<RouterCtl>,
    handle: thread::JoinHandle<()>,
}

impl Router {
    /// Spawn the router thread and return a `Router` handle.
    ///
    /// The router thread will remain resident, consuming the channels over which requests to route
    /// messages to actor and control messages to manage the router itself are sent. It will also
    /// consume an internal channel of status messages sent back from actor threads, so it can
    /// perform tasks when events like the stoppage or panic of an actor thread occurs.
    pub fn run(id: &str) -> Self {
        let router_id = id.to_owned();
        let (req_tx, req) = unbounded::<RouterRequest>();
        let (ctl_tx, ctl) = unbounded::<RouterCtl>();
        let (router_req, router_ctl) = (req_tx.clone(), ctl_tx.clone());
        let handle = thread::spawn(move || {
            let mut table: HashMap<String, Actor> = HashMap::new();
            let (stat_tx, stat) = unbounded::<ActorStatus>();

            info!("router::{}: started", router_id);

            loop {
                select! {
                    recv(req) -> msg => {
                        match msg {
                            Ok(request) => {
                                if let Some(actor) = table.get(&request.id) {
                                    actor.req.send(request.val).unwrap();
                                } else {
                                    error!("router::{}: actor '{}' not found", router_id, request.id);
                                }
                            }
                            Err(e) => error!("router::{}: error receiving request: {}", router_id, e)
                        }
                    },
                    recv(ctl) -> msg => {
                        match msg {
                            Ok(RouterCtl::Spawn { id, f, resp }) => {
                                info!("router::{}: spawning actor '{}'", router_id, id);
                                let (actor_req_tx, actor_req_rx) = unbounded();
                                let (actor_ctl_tx, actor_ctl_rx) = unbounded();
                                let ctx = ActorContext::new(
                                    id.clone(),
                                    actor_req_rx,
                                    actor_ctl_rx,
                                    router_req.clone(),
                                    router_ctl.clone(),
                                    stat_tx.clone()
                                );
                                let panic_stat_tx = stat_tx.clone();
                                let actor_id = id.clone();
                                let handle = thread::spawn(move || {
                                    let actor_status = catch_unwind(move || {
                                        f(ctx);
                                    });
                                    if let Err(_) = actor_status {
                                        panic_stat_tx.send(ActorStatus::Paniced(actor_id)).unwrap();
                                    }
                                });
                                table.insert(id.clone(), Actor {
                                    id: id,
                                    req: actor_req_tx,
                                    ctl: actor_ctl_tx,
                                    handle: handle,
                                });
                                resp.send(()).unwrap();
                            },
                            Ok(RouterCtl::Get { id, resp }) => {
                                if let Some(actor) = table.get(&id) {
                                    resp.send(Ok(actor.req.clone())).unwrap();
                                } else {
                                    resp.send(Err(Error::NoSuchActor(id.clone()))).unwrap();
                                }
                            }
                            Ok(RouterCtl::Has { id, resp }) => {
                                resp.send(table.contains_key(&id)).unwrap();
                            }
                            Ok(RouterCtl::StopActor { id, resp }) => {
                                if let Some(actor) = table.remove(&id) {
                                    info!("router::{}: stopping actor '{}'", router_id, actor.id);
                                    actor.ctl.send(ActorCtl::Stop).unwrap();
                                    actor.handle.join().unwrap();
                                    resp.send(()).unwrap();
                                }
                            }
                            Ok(RouterCtl::StopActorAsync(id)) => {
                                if let Some(actor) = table.remove(&id) {
                                    info!("router::{}: stopping actor '{}'", router_id, actor.id);
                                    actor.ctl.send(ActorCtl::Stop).unwrap();
                                }
                            }
                            Ok(RouterCtl::Shutdown) => {
                                info!("router::{}: stopping all actors ...", router_id);
                                for (_, actor) in table.drain() {
                                    info!("router::{}: stopping actor '{}'", router_id, actor.id);
                                    actor.ctl.send(ActorCtl::Stop).unwrap();
                                    actor.handle.join().unwrap();
                                }
                                break;
                            }
                            Err(e) => error!("router::{}: error receiving on ctrl: {}", router_id, e),
                        }
                    },
                    recv(stat) -> msg => {
                        match msg {
                            Ok(ActorStatus::Stopped(id)) => {
                                info!("router::{}: actor '{}' stopped", router_id, id);
                                table.remove(&id);
                            }
                            Ok(ActorStatus::Paniced(id)) => {
                                warn!("router::{}: actor '{}' paniced", router_id, id);
                                table.remove(&id);
                            }
                            Err(e) => error!("router::{}: error receiving actor status: {}", router_id, e),
                        }
                    },
                }
            }
            info!("router::{}: stopped", router_id);
        });
        Self {
            id: id.to_owned(),
            req: req_tx,
            ctl: ctl_tx,
            handle: handle,
        }
    }

    /// Spawn an actor.
    pub fn spawn(&self, id: &str, f: Box<ActorFn>) -> Result<()> {
        let (spawn, resp) = RouterCtl::spawn(id, f);
        match self.ctl.send(spawn) {
            Ok(()) => match resp.recv() {
                Ok(()) => Ok(()),
                Err(e) => Err(Error::recv_error(e)),
            },
            Err(e) => Err(Error::send_error(e)),
        }
    }

    /// Determine whether the router has a route to an actor, at the time of inspection.
    pub fn has(&self, id: &str) -> Result<bool> {
        let (has, resp) = RouterCtl::has(id);
        if let Err(e) = self.ctl.send(has) {
            return Err(Error::send_error(e));
        }
        match resp.recv() {
            Ok(b) => Ok(b),
            Err(e) => Err(Error::recv_error(e)),
        }
    }

    /// Retrieve a copy of an actor's request channel `Sender`.
    pub fn get(&self, id: &str) -> Result<Sender<Message>> {
        let (get, resp) = RouterCtl::get(id);
        if let Err(e) = self.ctl.send(get) {
            return Err(Error::send_error(e));
        }
        match resp.recv() {
            Ok(sender) => sender,
            Err(e) => Err(Error::recv_error(e)),
        }
    }

    /// Send a message to an actor.
    pub fn send(&self, id: &str, msg: Message) -> Result<()> {
        match self.req.send(RouterRequest::new(id, msg)) {
            Ok(()) => Ok(()),
            Err(e) => Err(Error::send_error(e)),
        }
    }

    /// Stop an actor.
    pub fn stop(&self, id: &str) -> Result<()> {
        let (stop, resp) = RouterCtl::stop_actor(id);
        if let Err(e) = self.ctl.send(stop) {
            return Err(Error::send_error(e));
        }
        if let Err(e) = resp.recv() {
            return Err(Error::recv_error(e));
        }
        Ok(())
    }

    /// Stop an actor without waiting for it to stop.
    pub fn stop_async(&self, id: &str) -> Result<()> {
        if let Err(e) = self.ctl.send(RouterCtl::stop_actor_async(id)) {
            return Err(Error::send_error(e));
        }
        Ok(())
    }

    /// Stop a list of actors in order.
    pub fn stop_list(&self, ids: Vec<String>) -> Result<()> {
        for id in ids {
            if let Err(e) = self.stop(&id) {
                return Err(e);
            }
        }
        Ok(())
    }

    /// Stop all actors and then stop the router.
    ///
    /// Note that actors are stopped in an arbitrary order.
    pub fn shutdown(self) {
        self.ctl.send(RouterCtl::Shutdown).unwrap();
        self.handle.join().unwrap();
    }
}

/// A request to route a message to an actor.
///
/// The request consists of the ID of an actor previously spawned through the router and an
/// arbitrary value to be sent to the actor, expressed as a boxed `Any + Send` trait object.
pub struct RouterRequest {
    pub id: String,
    pub val: Box<dyn Any + Send>,
}

impl RouterRequest {
    /// Construct a `RouterRequest`.
    pub fn new(id: &str, val: Box<dyn Any + Send>) -> Self {
        RouterRequest {
            id: id.to_owned(),
            val: val,
        }
    }
}

/// A wrapper for sending a message to an actor.
///
/// Sends a message to an actor through a [Router](struct.Router.html), possibly indirectly
/// through an [ActorContext](struct.ActorContext.html).
#[macro_export]
macro_rules! send_actor {
    ($r:ident, $i:expr, $v:expr) => {{
        $r.send($i, Box::new($v))
    }};
}

/// A control message for the router.
///
/// These messages are processed directly by the router itself and control aspects of its
/// operation. Notably, the router is responsible for creating actors and their threads and must
/// receive a `RouterCtl::Spawn` request to do so.
pub enum RouterCtl {
    /// Create a new actor managed by the router.
    Spawn {
        id: String,
        f: Box<ActorFn>,
        resp: Sender<()>,
    },

    /// Retrieve the request channel for an actor from the router.
    Get {
        id: String,
        resp: Sender<Result<Sender<Message>>>,
    },

    /// Determine whether or not the router has an actor.
    Has { id: String, resp: Sender<bool> },

    /// Stop an actor.
    StopActor { id: String, resp: Sender<()> },

    /// Stop an actor without waiting for it to stop.
    StopActorAsync(String),

    /// Stop all actors and then stop the router.
    Shutdown,
}

impl RouterCtl {
    /// Construct a `RouterCtl::Spawn` request.
    ///
    /// Returns the request object as well as a `Receiver` over which the router will send
    /// confirmation back to the requestor that the actor has been spawned.
    ///
    /// The `spawn_actor!` macro provides a more ergonomic wrapper around this method.
    pub fn spawn(id: &str, f: Box<ActorFn>) -> (Self, Receiver<()>) {
        let (tx, rx) = unbounded();
        (
            Self::Spawn {
                id: id.to_owned(),
                f: f,
                resp: tx,
            },
            rx,
        )
    }

    /// Construct a `RouterCtl::Get` request.
    ///
    /// Returns the request object as well as a `Receiver` over which the router will send back to
    /// the caller the `Sender` for the given actor (or `None`, if no such actor exists).
    ///
    /// The `get_actor!` macro provides a more ergonomic wrapper around this method.
    pub fn get(id: &str) -> (Self, Receiver<Result<Sender<Message>>>) {
        let (tx, rx) = unbounded();
        (
            Self::Get {
                id: id.to_owned(),
                resp: tx,
            },
            rx,
        )
    }

    /// Construct a `RouterCtl::Has` request.
    ///
    /// Returns the request object as well as a `Receiver` over which the router will send back a
    /// boolean to the caller that indicates whether or not the given actor is available at the
    /// time of its observation.
    ///
    /// The `has_actor!` macro provides a more ergonomic wrapper around this method.
    pub fn has(id: &str) -> (Self, Receiver<bool>) {
        let (tx, rx) = unbounded();
        (
            Self::Has {
                id: id.to_owned(),
                resp: tx,
            },
            rx,
        )
    }

    /// Construct a `RouterCtl::StopActor` request.
    ///
    /// Returns the request object as well as a `Receiver` over which the router will send
    /// confirmation back to the caller that the actor has stopped.
    pub fn stop_actor(id: &str) -> (Self, Receiver<()>) {
        let (tx, rx) = unbounded();
        (
            Self::StopActor {
                id: id.to_owned(),
                resp: tx,
            },
            rx,
        )
    }

    pub fn stop_actor_async(id: &str) -> Self {
        Self::StopActorAsync(id.to_owned())
    }
}

/// A wrapper for executing a [RouterCtl::Spawn][RouterCtl] request on a [Router][Router].
///
/// Creates a `RouterCtl::Spawn` request, sends it to the router, and recieves the router's
/// response.
///
/// [RouterCtl]: enum.RouterCtl.html
/// [Router]: struct.Router.html
#[macro_export]
macro_rules! spawn_actor {
    ($r:ident, $i:expr, $f:expr) => {{
        $r.spawn($i, Box::new($f))
    }};
}

/// A handle to an actor.
///
/// When the router constructs an actor it will encapsulate that actor's communication channels and
/// its thread handle into an `Actor`. The router maps the ID of the actor to its `Actor` in its
/// registry.
pub struct Actor {
    pub id: String,
    pub req: Sender<Message>,
    pub ctl: Sender<ActorCtl>,
    pub handle: thread::JoinHandle<()>,
}

/// A signature for actor thread functions.
///
/// Each actor thread, when spawned by the router, is passed an `ActorContext` that encapsulates
/// the channels of communication available to the actor. Because the function is sent from the
/// calling thread to the router thread, and then into a new thread for the actor, the function
/// must be `Send`. The router wraps the invocation of the function to catch panics, so it can
/// receive a status message indicating the function has paniced and clean up its resources, and as
/// such must be `UnwindSafe` as well.
pub type ActorFn = dyn FnOnce(ActorContext) + Send + UnwindSafe + 'static;

/// A handle to communication channels provided to an actor function.
///
/// Actor functions spawned through the router are passed an `ActorContext` that provides the
/// `Receviers` over which the actor receives messages and control messages from the router and
/// `Senders` the actor can use to communicate back with the router.
///
/// This is typically only used by the router.
pub struct ActorContext {
    pub id: String,
    pub req: Receiver<Message>,
    pub ctl: Receiver<ActorCtl>,
    pub router_req: Sender<RouterRequest>,
    pub router_ctl: Sender<RouterCtl>,
    pub stat: Sender<ActorStatus>,
}

impl ActorContext {
    /// Construct an `ActorContext`. This is typically used only by the router.
    pub fn new(
        id: String,
        req: Receiver<Message>,
        ctl: Receiver<ActorCtl>,
        router_req: Sender<RouterRequest>,
        router_ctl: Sender<RouterCtl>,
        stat: Sender<ActorStatus>,
    ) -> Self {
        Self {
            id: id.to_owned(),
            req: req,
            ctl: ctl,
            router_req: router_req,
            router_ctl: router_ctl,
            stat: stat,
        }
    }

    /// Spawn a new actor.
    pub fn spawn(&self, id: &str, f: Box<ActorFn>) -> Result<()> {
        let (spawn, resp) = RouterCtl::spawn(id, f);
        match self.router_ctl.send(spawn) {
            Ok(()) => match resp.recv() {
                Ok(()) => Ok(()),
                Err(e) => Err(Error::recv_error(e)),
            },
            Err(e) => Err(Error::send_error(e)),
        }
    }

    /// Determine whether the router has a route to another actor, at the time of inspection.
    pub fn has(&self, id: &str) -> Result<bool> {
        let (has, resp) = RouterCtl::has(id);
        if let Err(e) = self.router_ctl.send(has) {
            return Err(Error::send_error(e));
        }
        match resp.recv() {
            Ok(b) => Ok(b),
            Err(e) => Err(Error::recv_error(e)),
        }
    }

    /// Retrieve a copy of another actor's request channel `Sender`.
    pub fn get(&self, id: &str) -> Result<Sender<Message>> {
        let (get, resp) = RouterCtl::get(id);
        if let Err(e) = self.router_ctl.send(get) {
            return Err(Error::send_error(e));
        }
        match resp.recv() {
            Ok(sender) => sender,
            Err(e) => Err(Error::recv_error(e)),
        }
    }

    /// Send a message to another actor.
    pub fn send(&self, id: &str, msg: Message) -> Result<()> {
        match self.router_req.send(RouterRequest::new(id, msg)) {
            Ok(()) => Ok(()),
            Err(e) => Err(Error::send_error(e)),
        }
    }

    /// Stop another actor.
    ///
    /// Warning: if attempting to stop another actor while in the process of being stopped, the
    /// router will be blocked awaiting a response. The router will not receive the stop request
    /// until after you have completed stopping and a deadlock will occur.
    pub fn stop(&self, id: &str) -> Result<()> {
        let (stop, resp) = RouterCtl::stop_actor(id);
        if let Err(e) = self.router_ctl.send(stop) {
            return Err(Error::send_error(e));
        }
        match resp.recv() {
            Ok(()) => Ok(()),
            Err(e) => Err(Error::recv_error(e)),
        }
    }

    /// Stop another actor without waiting for it to stop.
    ///
    /// This should be used when stopping an actor from within an actor that is currently in the
    /// process of being stopped.
    pub fn stop_async(&self, id: &str) -> Result<()> {
        if let Err(e) = self.router_ctl.send(RouterCtl::stop_actor_async(id)) {
            return Err(Error::send_error(e));
        }
        Ok(())
    }

    /// Inform the router that we are stopping.
    pub fn report_stopped(&self) -> Result<()> {
        match self.stat.send(ActorStatus::Stopped(self.id.clone())) {
            Ok(_) => Ok(()),
            Err(e) => Err(Error::send_error(e)),
        }
    }
}

/// Messages that exhert control over actors.
///
/// These messages are sent from the router to an actor thread.
pub enum ActorCtl {
    /// The actor thread should stabilize its state, send an
    /// [ActorStatus::Stopped](enum.ActorStatus.html) to the router through its status channel,
    /// and exit.
    Stop,
}

/// Status messages sent from an actor to the supervising router.
///
/// Each variant embeds the ID of the actor to which the message pertains.
pub enum ActorStatus {
    /// The actor thread paniced. The panic is typically caught by the wrapper function the router
    /// uses around the actor thread, but actor functions may also catch their own panics and
    /// communicate them back to the router.
    Paniced(String),

    /// The actor thread has stopped or is in the process of stopping. This is typically send from
    /// the actor thread to the router immediately before it exits.
    Stopped(String),
}

/// Errors that can occur.
#[derive(Debug)]
pub enum Error {
    /// An error sending on a channel.
    ///
    /// Converts the underlying `crossbeam_channel::SendError` into a string representation to make
    /// it easier, if less flexible, to deal with.
    SendError(String),

    /// An error receiving on a channel.
    ///
    /// Converts the underlying `crossbeam_channel::RecvError` into a string representation to make
    /// it easier, if less flexible, to deal with.
    RecvError(String),

    /// The specified actor does not exist, or is not known to the router. Embeds the ID of the
    /// actor.
    ///
    /// If an actor thread panics or otherwise chooses to stop, it is removed from the router and
    /// subsequent attempts to send messages to that actor will result in this error.
    NoSuchActor(String),
}

impl Error {
    /// Construct an `Error::SendError` from an underlying `crossbeam_channel::SendError<T>`.
    pub fn send_error<T>(e: crossbeam_channel::SendError<T>) -> Self {
        Self::SendError(e.to_string())
    }

    /// Construct an `Error::RecvError` from an underlying `crossbeam_channel::RecvError`.
    pub fn recv_error(e: crossbeam_channel::RecvError) -> Self {
        Self::RecvError(e.to_string())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::SendError(e) => write!(f, "{}", e),
            Self::RecvError(e) => write!(f, "{}", e),
            Self::NoSuchActor(id) => write!(f, "no such actor: {}", id),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::prelude::*;

    #[test]
    fn basic_router() {
        let router = Router::run("test");

        assert!(!router.has("foo").unwrap());

        spawn_actor!(router, "foo", |ctx: ActorContext| {
            ctx.ctl.recv().unwrap();
            ctx.report_stopped().unwrap();
        })
        .unwrap();

        assert!(router.has("foo").unwrap());

        router.stop("foo").unwrap();

        assert!(!router.has("foo").unwrap());

        router.shutdown();
    }
}
