//! An example where an actor responds to a request over a channel `Sender` provided by the
//! requestor.

#[macro_use]
extern crate herbert;
use herbert::prelude::*;

fn main() {
    let router = Router::run("example");
    spawn_actor!(router, "pow", pow).unwrap();

    println!("sending pow: 12");
    let (msg, rx) = CustomMessage::new(12);
    send_actor!(router, "pow", msg).unwrap();

    let result = rx.recv().unwrap();
    println!("result: {}", result);

    router.shutdown();
}

/// A custom message type that includes a value to be sent to the actor and a channel `Sender` that
/// the actor will use to communicate back the response.
struct CustomMessage {
    value: i64,
    resp: Sender<i64>,
}

impl CustomMessage {
    /// Constructs a `CustomMessage`, returning both the `CustomMessage` and the channel `Receiver`
    /// to which the response will be sent.
    fn new(val: i64) -> (Self, Receiver<i64>) {
        let (sender, receiver) = unbounded();
        (
            Self {
                value: val,
                resp: sender,
            },
            receiver,
        )
    }
}

/// Here we have a minimalist example of an `ActorFn`. It makes many assumptions that your code may
/// not want to make, such as the fact that all `ActorCtl` messages indicate a stop event, that
/// incorrect normal message types should be silently ignored, and that send/receive errors should
/// panic.
fn pow(ctx: ActorContext) {
    loop {
        select! {
            recv(ctx.req) -> msg => {
                if let Some(message) = msg.unwrap().downcast_ref::<CustomMessage>() {
                    message.resp.send(message.value * message.value).unwrap();
                }
            }
            recv(ctx.ctl) -> _ => break,
        }
    }
    ctx.report_stopped().unwrap();
}
