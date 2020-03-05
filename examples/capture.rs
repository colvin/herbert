//! An example where an actor loads external configuration by capturing it from its outer scope.

#[macro_use]
extern crate log;
use chrono::Utc;

#[macro_use]
extern crate herbert;
use herbert::prelude::*;
use herbert::ActorFn;

fn main() {
    setup_logging(log::LevelFilter::Info);

    let router = Router::run("example");
    spawn_actor!(router, "squarer", make_actorfn(Op::Square)).unwrap();
    spawn_actor!(router, "cuber", make_actorfn(Op::Cube)).unwrap();
    router.send("squarer", Box::new(10)).unwrap();
    router.send("cuber", Box::new(10)).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    router.shutdown();
}

#[derive(Debug)]
enum Op {
    Square,
    Cube,
}

fn make_actorfn(op: Op) -> Box<ActorFn> {
    Box::new(move |ctx: ActorContext| {
        info!("{}: started with op {:?}", ctx.id, op);
        loop {
            select! {
                recv(ctx.req) -> msg => {
                    match msg {
                        Ok(any) => {
                            if let Some(val) = any.downcast_ref::<i32>() {
                                match op {
                                    Op::Square => info!("{}: {}^2 --> {}", ctx.id, val, val * val),
                                    Op::Cube => info!("{}: {}^3 --> {}", ctx.id, val, val * val * val),
                                }
                            }
                        }
                        Err(e) => panic!(e),
                    }
                }
                recv(ctx.ctl) -> _ => break,
            }
        }
        ctx.report_stopped().unwrap();
        info!("{}: stopped", ctx.id);
    })
}

fn setup_logging(lvl: log::LevelFilter) {
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
