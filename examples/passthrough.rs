//! An example where an actor spawns another actor, the former leveraging the latter to perform
//! work on its behalf.

#[macro_use]
extern crate log;
use chrono::Utc;

#[macro_use]
extern crate herbert;
use herbert::prelude::*;

fn main() {
    setup_logging(log::LevelFilter::Info);

    let router = Router::run("example");

    spawn_actor!(router, "lazy", lazy).unwrap();

    let mut i = 0;
    while i < 20 {
        info!("incrementing: {}", i);
        let (incr, resp) = IncrRequest::new(i);
        router.send("lazy", Box::new(incr)).unwrap();
        i = resp.recv().unwrap();
        info!("result: {}", i);
    }

    router.stop("lazy").unwrap();

    router.shutdown();
}

#[derive(Debug, Clone)]
struct IncrRequest {
    val: i32,
    resp: Sender<i32>,
}

impl IncrRequest {
    fn new(val: i32) -> (Self, Receiver<i32>) {
        let (tx, rx) = unbounded();
        (Self { val: val, resp: tx }, rx)
    }
}

fn lazy(ctx: ActorContext) {
    info!("{}: started", ctx.id);
    loop {
        select! {
            recv(ctx.req) -> msg => {
                match msg {
                    Ok(any) => {
                        if let Some(incr) = any.downcast_ref::<IncrRequest>() {
                            if !ctx.has("sap").unwrap() {
                                spawn_actor!(ctx, "sap", sap).unwrap();
                            }
                            send_actor!(ctx, "sap", incr.clone()).unwrap();
                        }
                    }
                    Err(e) => {
                        error!("{}: error receiving message: {}", ctx.id, e);
                        break;
                    }
                }
            }
            recv(ctx.ctl) -> msg => {
                match msg {
                    Ok(ActorCtl::Stop) => {
                        info!("{}: stopping sap", ctx.id);
                        // We MUST use stop_async here to avoid deadlock.
                        ctx.stop_async("sap").unwrap();
                        break;
                    }
                    Err(e) => {
                            error!("{}: error receiving control message: {}", ctx.id, e);
                            break;
                    }
                }
            }
        }
    }
    ctx.report_stopped().unwrap();
    info!("{}: stopped", ctx.id);
}

fn sap(ctx: ActorContext) {
    info!("{}: started", ctx.id);
    loop {
        select! {
            recv(ctx.req) -> msg => {
                match msg {
                    Ok(any) => {
                        if let Some(incr) = any.downcast_ref::<IncrRequest>() {
                            incr.resp.send(incr.val + 1).unwrap();
                        }
                    }
                    Err(e) => {
                        error!("{}: error receiving message: {}", ctx.id, e);
                        break;
                    }
                }
            }
            recv(ctx.ctl) -> msg => {
                match msg {
                    Ok(ActorCtl::Stop) => {
                        info!("{}: stopping", ctx.id);
                        break;
                    }
                    Err(e) => {
                        error!("{}: error receiving control message: {}", ctx.id, e);
                        break;
                    }
                }
            }
        }
    }
    ctx.report_stopped().unwrap();
    info!("{}: stopped", ctx.id);
}

pub fn setup_logging(lvl: log::LevelFilter) {
    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{:>5} {} -- {} -- {}",
                record.level(),
                Utc::now().to_rfc3339(),
                record.target(),
                message,
            ))
        })
        .level(lvl)
        .chain(std::io::stderr())
        .apply()
        .unwrap();
}
