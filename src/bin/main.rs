#![recursion_limit = "1024"]

extern crate chrono;
extern crate egg_mode;
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
extern crate url;

mod util;

use chrono::Datelike;
use egg_mode::Token;
use egg_mode::tweet::DraftTweet;
use futures::{Future, Stream};
use hyper::client::{Client, IntoUrl};
use hyper::header::UserAgent;
use hyper::status::StatusCode;
use iba_kyuko_bot::Kyuko;
use std::collections::HashMap;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::Deref;
use std::path::Path;
use std::process;
use std::time::Duration;
use twitter_stream::TwitterJsonStream;
use twitter_stream::messages::{DirectMessage, StreamMessage, UserId};
use util::{Interval, SyncFile};

mod errors {
    error_chain! {}
}

use errors::*;

struct Env {
    settings: Settings,
    cache: SyncFile<HashMap<
        String, // department
        HashMap<
            String, // tweet id
            Kyuko
        >
    >>,
    archive: File,
    users: SyncFile<HashMap<
        String, // user id
        UserInfo
    >>,
    client: Client,
}

#[derive(Deserialize)]
struct Settings {
    consumer_key: String,
    consumer_secret: String,
    access_key: String,
    access_secret: String,
    #[serde(default)]
    admins: Vec<UserId>,
    #[serde(default = "default_user_agent")]
    user_agent: String,
    urls: Vec<String>,
}

fn default_user_agent() -> String {
    concat!("IbarakiUniversityKyukoBot/", env!("CARGO_PKG_VERSION"), " (+", env!("CARGO_PKG_HOMEPAGE"), ")").to_owned()
}

#[derive(Serialize, Deserialize)]
struct UserInfo {
    followings: Vec<Following>,
    // TODO: rate limit
}

#[derive(Serialize, Deserialize)]
enum Following {
    #[serde(rename = "pattern")]
    Pattern {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        lecturer: Option<String>,
    },
    #[serde(rename = "tweet_id")]
    TweetId(i64),
}

fn main() {
    if let Err(e) = run() {
        writeln!(&mut io::stderr(), "error: {}", e).unwrap();
        for e in e.iter().skip(1) {
            writeln!(&mut io::stderr(), "caused by: {}", e).unwrap();
        }
        process::exit(1);
    }
}

fn run() -> Result<()> {
    env_logger::init().chain_err(|| "failed to initialize the logger")?;

    let working_dir = env::args().nth(1).ok_or("missing a working directory argument")?;
    let mut env = Env::load(working_dir)?;

    let dms = TwitterJsonStream::user(&env.consumer_key, &env.consumer_secret, &env.access_key, &env.access_secret)
        .chain_err(|| "failed to connect to User Stream")?
        .then(|r| r.chain_err(|| "an error occured while listening on User Stream"))
        .filter_map(|json| if let Ok(StreamMessage::DirectMessage(dm)) = json::from_str(&json) {
            Some(dm)
        } else {
            None
        });
    let interval = Interval::new(|| Some(Duration::from_secs(60))); // TODO

    dms.merge(interval).for_each(|merged| {
        use futures::stream::MergedItem::*;
        match merged {
            First(dm) => env.direct_message(dm),
            Second(()) => env.update(),
            Both(dm, ()) => {
                env.direct_message(dm)?;
                env.update()
            },
        }
    }).wait()
}

impl Env {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        use std::fs;

        let path = path.as_ref();
        fs::create_dir_all(path).chain_err(|| format!("unable to create the working directory {:?}", path))?;
        let mut path = path.to_owned();

        path.push("settings.yaml");
        let settings = yaml::from_reader(File::open(&path).chain_err(|| "unable to open settings.yaml")?)
            .chain_err(|| "failed to load the settings.yaml")?;
        path.pop();

        path.push("kyuko.yaml");
        let cache = SyncFile::new(&path)?;
        path.pop();

        path.push("users.yaml");
        let users = SyncFile::new(&path)?;
        path.pop();

        path.push("archive.tsv");
        let archive = OpenOptions::new().append(true).create(true).open(&path)
            .chain_err(|| "unable to open archive.tsv")?;
        path.pop();

        Ok(Env {
            settings: settings,
            cache: cache,
            users: users,
            archive: archive,
            client: Client::new(),
        })
    }

    pub fn update(&mut self) -> Result<()> {
        for url in &self.settings.urls {
            let (dept, mut kyukos) = {
                let html = self.fetch(url).chain_err(|| format!("failed to fetch {}", url))?;
                iba_kyuko_bot::scrape(html).chain_err(|| format!("failed to scrape {}", url))?
            };

            {
                let cache = self.cache.entry(dept.clone()).or_insert_with(HashMap::new);

                // Remove withdrawn information.
                // TODO: explore more efficient way.
                // cf. Map::retain · Issue #1338 · rust-lang/rfcs https://github.com/rust-lang/rfcs/issues/1338
                let mut new = HashMap::with_capacity(cache.len());
                for (tweet_id, k) in cache.drain() {
                    if kyukos.contains(&k) {
                        new.insert(tweet_id, k);
                    } else {
                        // Write to the archive file:
                        match k.remarks {
                            Some(ref remarks) => writeln!(&self.archive, "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                                tweet_id, dept, k.kind, k.date, k.periods, k.title, k.lecturer, remarks),
                            None              => writeln!(&self.archive, "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                                tweet_id, dept, k.kind, k.date, k.periods, k.title, k.lecturer),
                        }.chain_err(|| "failed to write to the archive file")?;
                    }
                }

                kyukos.retain(|k| !new.values().any(|c| c == k)); // Retain information to post.

                *cache = new;
            }

            for k in kyukos.drain(..) {
                const WDAYS: [char; 7] = ['月', '火', '水', '木', '金', '土', '日'];

                let text = if let Some(ref remarks) = k.remarks {
                    format!("\
                            {}／{}\n\
                            {} [{}]\n\
                            {}年{}月{}日（{}）{}講時\
                            備考：{}\n\
                            {}\
                        ", dept, k.kind, k.title, k.lecturer, k.date.year(), k.date.month(), k.date.day(),
                        WDAYS[k.date.weekday().num_days_from_monday() as usize], k.periods, remarks, url
                    )
                } else {
                    format!("\
                            {}／{}\n\
                            {} [{}]\n\
                            {}年{}月{}日（{}）{}講時\n\
                            {}\
                        ", dept, k.kind, k.title, k.lecturer, k.date.year(), k.date.month(), k.date.day(),
                        WDAYS[k.date.weekday().num_days_from_monday() as usize], k.periods, url
                    )
                };

                // TODO: adjust Tweet length
                let id = DraftTweet::new(&text)
                    .send(&self.consumer(), &self.access())
                    .chain_err(|| format!("failed to post a Tweet: {:?}", k))?
                    .id.to_string();

                info!("successfully tweeted: status_id = {}\n{}", id, text);

                self.cache.get_mut(&dept).unwrap().insert(id, k);
            }
        }

        self.cache.commit()?;

        Ok(())
    }

    pub fn direct_message(&mut self, dm: DirectMessage) -> Result<()> {
        let mut tokens = dm.text.split_whitespace();

        match tokens.next() {
            // TODO
            Some("follow") => {},
            Some("unfollow") => {},
            Some("list") => {},
            Some("ping") => {},
            Some("shutdown") => {},
            Some(cmd) => {},
            None => {},
        }

        unimplemented!();
    }

    fn fetch<U: IntoUrl>(&self, url: U) -> Result<String> {
        let mut res = self.client
            .get(url)
            .header(UserAgent(self.settings.user_agent.clone()))
            .send()
            .chain_err(|| "failed to make an HTTP request")?;

        if StatusCode::Ok != res.status {
            return Err(res.status.to_string().into());
        }

        let mut body = String::new();
        res.read_to_string(&mut body).chain_err(|| format!("failed to read the response body"))?;

        Ok(body)
    }
}

impl Deref for Env {
    type Target = Settings;
    fn deref(&self) -> &Settings { &self.settings }
}

impl Settings {
    fn consumer<'a>(&'a self) -> Token<'a> {
        Token::new(self.consumer_key.as_str(), self.consumer_secret.as_str())
    }

    fn access<'a>(&'a self) -> Token<'a> {
        Token::new(self.access_key.as_str(), self.access_secret.as_str())
    }
}
