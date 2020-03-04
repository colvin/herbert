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

    spawn_actor!(router, "one", worker);
    spawn_actor!(router, "two", worker);

    if has_actor!(router, "one") {
        info!("confirmed we have actor one");
        if let Some(one) = get_actor!(router, "one") {
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
            send_actor!(router, "one", One { id: v.to_owned() }).unwrap();
        } else {
            send_actor!(router, "two", Two { id: v.to_owned() }).unwrap();
        }
    }

    std::thread::sleep(std::time::Duration::from_millis(100));

    stop_actor!(router, "one");

    std::thread::sleep(std::time::Duration::from_millis(300));

    router.shutdown();
}

struct One {
    id: String,
}

struct Two {
    id: String,
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
                            _ => unimplemented!(),
                        }
                    }
                    Err(e) => error!("{}: {}", ctx.id, e),
                }
            }
            recv(ctx.ctl) -> msg => {
                match msg {
                    Ok(ActorCtl::Stop) => {
                        ctx.stat.send(ActorStatus::Stopped(ctx.id.clone())).unwrap();
                        break;
                    }
                    Err(e) => error!("{}: {}", ctx.id, e),
                }
            }
        }
    }
}

pub fn setup_logging(lvl: LevelFilter) {
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
