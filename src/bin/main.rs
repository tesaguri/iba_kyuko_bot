#![recursion_limit = "1024"]

extern crate chrono;
#[macro_use]
extern crate clap;
extern crate egg_mode;
extern crate either;
extern crate env_logger;
#[macro_use]
extern crate error_chain;
extern crate futures;
extern crate hyper;
extern crate iba_kyuko_bot;
#[macro_use]
extern crate log;
extern crate rand;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json as json;
extern crate serde_yaml as yaml;
extern crate twitter_stream;

mod config;
mod daemon_main;
mod message;
mod util;

mod errors {
    error_chain! {}
}

use config::{Tweeted, UserMap};
use errors::*;
use egg_mode::{Token, tweet};
use util::SyncFile;

fn main() {
    use std::io::{self, Write};
    use std::process;

    if let Err(e) = run() {
        writeln!(&mut io::stderr(), "fatal error: {}", e).unwrap();
        for e in e.iter().skip(1) {
            writeln!(&mut io::stderr(), "caused by: {}", e).unwrap();
        }
        process::exit(1);
    }
}

fn run() -> Result<()> {
    use clap::Arg;

    env_logger::init().chain_err(|| "failed to initialize env_logger")?;

    // A trick to make the compiler rebuild this file when `Cargo.toml` is changed, described in clap's documentation.
    include_str!("../../Cargo.toml");

    let matches = app_from_crate!()
        .arg(Arg::with_name("WORKING_DIR")
            .help("Sets the working directory to store settings and caches in")
            .required(true)
            .index(1))
        .arg(Arg::with_name("remove")
            .short("r")
            .long("remove")
            .value_name("STATUS_ID")
            .multiple(true)
            .help("Removes the specified Tweet")
            .takes_value(true))
        .arg(Arg::with_name("clear")
            .long("clear")
            .help("Removes all the Tweets"))
        .arg(Arg::with_name("clear-users")
            .long("clear-users")
            .help("Clears every following information of all the users"))
        .get_matches();

    let working_dir = matches.value_of("WORKING_DIR").unwrap();
    let (tweeted, mut users, settings, archive) = config::load(working_dir)?;

    if matches.is_present("clear-users") {
        clear_users(&mut users)
    } else if matches.is_present("clear") {
        clear(tweeted, &settings.token())
    } else if let Some(ids) = matches.values_of("remove") {
        remove(ids, tweeted, &settings.token())
    } else {
        daemon_main::run(tweeted, users, settings, archive)
    }
}

fn remove<'a, I: 'a + Iterator<Item=&'a str>>(ids: I, mut tweeted: SyncFile<Tweeted>, token: &Token) -> Result<()> {
    for id in ids {
        info!("removing Tweet ID {}", id);

        for tweets in tweeted.values_mut() {
            tweets.remove(id);
        }

        let id = match id.parse() {
            Ok(id) => id,
            Err(_) => clap::Error::with_description(
                &format!("invalid status id: {}", id),
                clap::ErrorKind::InvalidValue
            ).exit(),
        };

        tweet::delete(id, token).chain_err(|| format!("failed to remove a Tweet (ID: {})", id))?;
        tweeted.commit()?;
    }

    Ok(())
}

fn clear(mut tweeted: SyncFile<Tweeted>, token: &Token) -> Result<()> {
    // TODO: explore better way to handle ownerships.
    loop {
        if let Some((dept, id)) = tweeted.iter()
            .filter_map(|(dept, tweets)| {
                tweets.keys().map(|id| (dept.clone(), id.clone())).next()
            }).next()
        {
            info!("removing Tweet ID {}", id);

            let is_empty = {
                let tweets = tweeted.get_mut(dept.as_str()).unwrap();
                tweets.remove(id.as_str());
                tweets.is_empty()
            };

            if is_empty {
                tweeted.remove(dept.as_str());
            }

            let id = id.parse().chain_err(|| format!("invalid status id {:?} in {:?}", id, tweeted.file_name()))?;
            tweet::delete(id, token).chain_err(|| format!("failed to remove a Tweet (ID: {})", id))?;

            tweeted.commit()?;
        } else {
            return Ok(());
        }
    }
}

fn clear_users(users: &mut SyncFile<UserMap>) -> Result<()> {
    println!("clearing all the following information");

    for user in users.values_mut() {
        user.clear();
    }

    users.commit()
}
