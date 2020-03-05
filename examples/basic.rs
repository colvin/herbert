//! A simplistic example using two actors based on the same function.

// logging
#[macro_use]
extern crate log;
use chrono::Utc;
use log::LevelFilter;

#[macro_use]
extern crate herbert;
use herbert::prelude::*;

fn main() {
    setup_logging(LevelFilter::Info);

    let router = Router::run("foo");

    spawn_actor!(router, "one", worker).unwrap();
    spawn_actor!(router, "two", worker).unwrap();

    if router.has("one").unwrap() {
        info!("confirmed we have actor one");
        if let Ok(one) = router.get("one") {
            info!("sending actor one a direct message");
            one.send(Box::new(One {
                id: "direct".to_owned(),
            }))
            .unwrap();
        }
    } else {
        panic!("no actor one!");
    }

    info!("sending actors a battery of messages");
    let input = vec!["foo", "bar", "baz"];
    for (i, v) in (0..10).zip(input.into_iter().cycle()) {
        if i % 2 == 0 {
            // Send using the macro
            send_actor!(router, "one", One::new(v)).unwrap();
        } else {
            // Send without using the macro
            router.send("two", Box::new(Two::new(v))).unwrap();
        }
    }

    std::thread::sleep(std::time::Duration::from_millis(100));

    router.stop("one").unwrap();

    std::thread::sleep(std::time::Duration::from_millis(300));

    router.shutdown();
}

struct One {
    id: String,
}

impl One {
    fn new(id: &str) -> Self {
        Self { id: id.to_owned() }
    }
}

struct Two {
    id: String,
}

impl Two {
    fn new(id: &str) -> Self {
        Self { id: id.to_owned() }
    }
}

fn worker(ctx: ActorContext) {
    loop {
        select! {
            recv(ctx.req) -> msg => {
                match msg {
                    Ok(v) => {
                        match ctx.id.as_str() {
                            "one" => {
                                info!("{}: {}", ctx.id, v.downcast_ref::<One>().unwrap().id);
                            }
                            "two" => {
                                info!("{}: {}", ctx.id, v.downcast_ref::<Two>().unwrap().id);
                            }
                            _ => unreachable!(),
                        }
                    }
                    Err(e) => {
                        error!("{}: error receiving on message channel, aborting: {}", ctx.id, e);
                        break;
                    }
                }
            }
            recv(ctx.ctl) -> msg => {
                match msg {
                    Ok(ActorCtl::Stop) => break,
                    Err(e) => {
                        error!("{}: error receiving on control channel, aborting: {}", ctx.id, e);
                        break;
                    }
                }
            }
        }
    }
    ctx.report_stopped().unwrap();
}

fn setup_logging(lvl: LevelFilter) {
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
