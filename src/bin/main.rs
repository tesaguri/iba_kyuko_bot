#![cfg_attr(unstable, feature(fused))]
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

mod admin;
mod config;
mod daemon_main;
mod schedule;
mod util;

mod errors {
    error_chain! {}
}

use errors::*;

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
            .help("Removes all the posted Tweets"))
        .arg(Arg::with_name("clear-users")
            .long("clear-users")
            .help("Clears every following information of all the users"))
        .get_matches();

    let working_dir = matches.value_of("WORKING_DIR").unwrap();

    let (mut tweeted, mut users, settings, archive) = config::load(working_dir)?;
    info!("settings: {:?}", settings);

    if matches.is_present("clear-users") {
        admin::clear_users(&mut users)
    } else if matches.is_present("clear") {
        admin::clear(&mut tweeted, &settings.token())
    } else if let Some(ids) = matches.values_of("remove") {
        admin::remove(ids, &mut tweeted, &settings.token())
    } else {
        daemon_main::run(tweeted, users, settings, archive)
    }
}
