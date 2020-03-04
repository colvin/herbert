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
//!         ctx.stat.send(ActorStatus::Stopped(ctx.id.clone())).unwrap();
//!     });
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
                                    resp.send(Some(actor.req.clone())).unwrap();
                                } else {
                                    resp.send(None).unwrap();
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

/// A wrapper for sending a message to an actor through a [Router](actor/struct.Router.html).
///
/// This constructs a new [RouterRequest](actor/struct.RouterRequest.html) and sends it to the router.
/// It returns the `Result` value of the send operation on the [Router's](actor/struct.Router.html)
/// requests channel.
#[macro_export]
macro_rules! send_actor {
    ($r:ident, $i:expr, $v:expr) => {{
        $r.req.send($crate::RouterRequest::new($i, Box::new($v)))
    }};
}

/// A wrapper for sending a message to an actor through a [Router](actor/struct.Router.html), from
/// within another actor.
///
/// This constructs a new [RouterRequest](actor/struct.RouterRequest.html) and sends it to the
/// router through the [ActorContext](actor/struct.ActorContext.html).  It returns the `Result`
/// value of the send operation on the [Router's](actor/struct.Router.html) requests channel.
#[macro_export]
macro_rules! actor_send_actor {
    ($c:ident, $i:expr, $v:expr) => {{
        $c.router_req
            .send($crate::RouterRequest::new($i, Box::new($v)))
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
        resp: Sender<Option<Sender<Message>>>,
    },

    /// Determine whether or not the router has an actor.
    Has { id: String, resp: Sender<bool> },

    /// Stop an actor.
    StopActor { id: String, resp: Sender<()> },

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
    pub fn get(id: &str) -> (Self, Receiver<Option<Sender<Message>>>) {
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
}

/// A wrapper for executing a [RouterCtl::Spawn][RouterCtl] request on a [Router][Router].
///
/// Creates a `RouterCtl::Spawn` request, sends it to the router, and recieves the router's
/// response.
///
/// Will panic if the request fails to be sent or the response fails to be received. In the four
/// argument form, `expect` is used to provide a better panic message.
///
/// [RouterCtl]: actor/enum.RouterCtl.html
/// [Router]: actor/struct.Router.html
#[macro_export]
macro_rules! spawn_actor {
    ($r:ident, $i:expr, $f:expr) => {{
        let (spawn, resp) = $crate::RouterCtl::spawn($i, Box::new($f));
        $r.ctl.send(spawn).unwrap();
        resp.recv().unwrap();
    }};
    ($r:ident, $i:expr, $f:expr, $e:expr) => {{
        let (spawn, resp) = $crate::RouterCtl::spawn($i, Box::new($f));
        $r.ctl.send(spawn).expect($e);
        resp.recv().expect($e);
    }};
}

/// A wrapper for executing a [RouterCtl::Spawn][RouterCtl] request on a [Router][Router], from
/// within another actor.
///
/// Creates a `RouterCtl::Spawn` request, sends it to the router through the
/// [ActorContext][ActorContext], and recieves the router's response.
///
/// Will panic if the request fails to be sent or the response fails to be received. In the four
/// argument form, `expect` is used to provide a better panic message.
///
/// [RouterCtl]: actor/enum.RouterCtl.html
/// [Router]: actor/struct.Router.html
/// [ActorContext]: actor/struct.ActorContext.html
#[macro_export]
macro_rules! actor_spawn_actor {
    ($c:ident, $i:expr, $f:expr) => {{
        let (spawn, resp) = $crate::RouterCtl::spawn($i, Box::new($f));
        $c.router_ctl.send(spawn).unwrap();
        resp.recv().unwrap();
    }};
    ($c:ident, $i:expr, $f:expr, $e:expr) => {{
        let (spawn, resp) = $crate::RouterCtl::spawn($i, Box::new($f));
        $c.router_ctl.send(spawn).expect($e);
        resp.recv().expect($e);
    }};
}

/// A wrapper for executing a [RouterCtl::Has][RouterCtl] request on a [Router][Router].
///
/// Creates a `RouterCtl::Has` request, sends it to the router, and recieves the router's response.
/// The value is returned to the caller.
///
/// Will panic if the request fails to be sent or the response fails to be received. In the four
/// argument form, `expect` is used to provide a better panic message.
///
/// [RouterCtl]: actor/enum.RouterCtl.html
/// [Router]: actor/struct.Router.html
#[macro_export]
macro_rules! has_actor {
    ($r:ident, $i:expr) => {{
        let (has, resp) = $crate::RouterCtl::has($i);
        $r.ctl.send(has).unwrap();
        resp.recv().unwrap()
    }};
    ($r:ident, $i:expr, $e:expr) => {{
        let (has, resp) = $crate::RouterCtl::has($i);
        $r.ctl.send(has).expect($e);
        resp.recv().expect($e)
    }};
}

/// A wrapper for executing a [RouterCtl::Has][RouterCtl] request on a [Router][Router], from
/// within another actor.
///
/// Creates a `RouterCtl::Has` request, sends it to the router through the
/// [ActorContext][ActorContext], and recieves the router's response. The value is returned to the
/// caller.
///
/// Will panic if the request fails to be sent or the response fails to be received. In the four
/// argument form, `expect` is used to provide a better panic message.
///
/// [RouterCtl]: actor/enum.RouterCtl.html
/// [Router]: actor/struct.Router.html
/// [ActorContext]: actor/struct.ActorContext.html
#[macro_export]
macro_rules! actor_has_actor {
    ($c:ident, $i:expr) => {{
        let (has, resp) = $crate::RouterCtl::has($i);
        $c.router_ctl.send(has).unwrap();
        resp.recv().unwrap()
    }};
    ($c:ident, $i:expr, $e:expr) => {{
        let (has, resp) = $crate::RouterCtl::has($i);
        $c.router_ctl.send(has).expect($e);
        resp.recv().expect($e)
    }};
}

/// A wrapper for executing a [RouterCtl::Get][RouterCtl] request on a [Router][Router].
///
/// Creates a `RouterCtl::Get` request, sends it to the router, and recieves the router's response.
/// The value is returned to the caller.
///
/// Will panic if the request fails to be sent or the response fails to be received. In the four
/// argument form, `expect` is used to provide a better panic message.
///
/// [RouterCtl]: actor/enum.RouterCtl.html
/// [Router]: actor/struct.Router.html
#[macro_export]
macro_rules! get_actor {
    ($r:ident, $i:expr) => {{
        let (get, resp) = $crate::RouterCtl::get($i);
        $r.ctl.send(get).unwrap();
        resp.recv().unwrap()
    }};
    ($r:ident, $i:expr, $e:expr) => {{
        let (get, resp) = $crate::RouterCtl::get($i);
        $r.ctl.send(get).expect($e);
        resp.recv().expect($e)
    }};
}

/// A wrapper for executing a [RouterCtl::Get][RouterCtl] request on a [Router][Router], from
/// within another actor.
///
/// Creates a `RouterCtl::Get` request, sends it to the router through the
/// [ActorContext][ActorContext], and recieves the router's response. The value is returned to the
/// caller.
///
/// Will panic if the request fails to be sent or the response fails to be received. In the four
/// argument form, `expect` is used to provide a better panic message.
///
/// [RouterCtl]: actor/enum.RouterCtl.html
/// [Router]: actor/struct.Router.html
/// [ActorContext]: actor/struct.ActorContext.html
#[macro_export]
macro_rules! actor_get_actor {
    ($c:ident, $i:expr) => {{
        let (get, resp) = $crate::RouterCtl::get($i);
        $c.router_ctl.send(get).unwrap();
        resp.recv().unwrap()
    }};
    ($c:ident, $i:expr, $e:expr) => {{
        let (get, resp) = $crate::RouterCtl::get($i);
        $c.router_ctl.send(get).expect($e);
        resp.recv().expect($e)
    }};
}

/// A wrapper for executing a [RouterCtl::StopActor][RouterCtl] request on a [Router][Router].
///
/// Creates a `RouterCtl::StopActor` request, sends it to the router, and recieves the router's
/// response.
///
/// Will panic if the request fails to be sent or the response fails to be received. In the four
/// argument form, `expect` is used to provide a better panic message.
///
/// [RouterCtl]: actor/enum.RouterCtl.html
/// [Router]: actor/struct.Router.html
#[macro_export]
macro_rules! stop_actor {
    ($r:ident, $i:expr) => {{
        let (stop, resp) = $crate::RouterCtl::stop_actor($i);
        $r.ctl.send(stop).unwrap();
        resp.recv().unwrap();
    }};
    ($r:ident, $i:expr, $e:expr) => {{
        let (stop, resp) = $crate::RouterCtl::stop_actor($i);
        $r.ctl.send(stop).expect($e);
        resp.recv().expect($e);
    }};
}

/// A wrapper for executing a [RouterCtl::StopActor][RouterCtl] request on a [Router][Router], from
/// within another actor.
///
/// Creates a `RouterCtl::StopActor` request, sends it to the router through the
/// [ActorContext][ActorContext], and recieves the router's response.
///
/// Will panic if the request fails to be sent or the response fails to be received. In the four
/// argument form, `expect` is used to provide a better panic message.
///
/// [RouterCtl]: actor/enum.RouterCtl.html
/// [Router]: actor/struct.Router.html
/// [ActorContext]: actor/struct.ActorContext.html
#[macro_export]
macro_rules! actor_stop_actor {
    ($c:ident, $i:expr) => {{
        let (stop, resp) = $crate::RouterCtl::stop_actor($i);
        $c.router_ctl.send(stop).unwrap();
        resp.recv().unwrap();
    }};
    ($c:ident, $i:expr, $e:expr) => {{
        let (stop, resp) = $crate::RouterCtl::stop_actor($i);
        $c.router_ctl.send(stop).expect($e);
        resp.recv().expect($e);
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

#[cfg(test)]
mod tests {
    use crate::prelude::*;

    #[test]
    fn basic_router() {
        let router = Router::run("test");

        assert!(!has_actor!(router, "foo"));

        spawn_actor!(router, "foo", |ctx: ActorContext| {
            ctx.ctl.recv().unwrap();
            ctx.stat.send(ActorStatus::Stopped(ctx.id)).unwrap();
        });

        assert!(has_actor!(router, "foo"));

        router.shutdown();
    }
}
