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

mod message;
mod util;

use egg_mode::{KeyPair, Token};
use hyper::client::Client;
use iba_kyuko_bot::Kyuko;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use twitter_stream::messages::{DirectMessage, StreamMessage, UserId};
use util::{Interval, SyncFile, WriteTrace};

mod errors {
    error_chain! {}
}

use errors::*;

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
    concat!(env!("CARGO_PKG_NAME"), '/', env!("CARGO_PKG_VERSION"), " (+", env!("CARGO_PKG_HOMEPAGE"), ')').to_owned()
}

#[derive(Default, Serialize, Deserialize)]
pub struct UserInfo {
    following: HashMap<
        String, // id in radix-64
        Follow
    >,
    next_id: u64,
    // TODO: rate limit
}

#[derive(Serialize, Deserialize, PartialEq)]
enum Follow {
    #[serde(rename = "pattern")]
    Pattern {
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        lecturer: Option<String>,
    },
    #[serde(rename = "tweet_id")]
    TweetId(u64),
}

type Tweeted = HashMap<
    String, // source URL
    HashMap<
        String, // tweet id
        Kyuko
    >
>;

type UserMap = HashMap<
    String, // user id
    UserInfo,
>;

fn main() {
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
    use egg_mode::service::{self, Configuration};
    use futures::{Future, Stream};
    use std::env;
    use std::time::Duration;
    use twitter_stream::TwitterJsonStream;

    env_logger::init().chain_err(|| "failed to initialize the logger")?;

    let working_dir = env::args().nth(1).ok_or("missing a working directory argument")?;
    let (mut tweeted, mut users, settings, archive) = load(working_dir)?;
    let url_len = {
        let Configuration {
            short_url_length,
            short_url_length_https,
            ..
        } = service::config(&settings.token())
            .chain_err(|| "failed to fetch Twitter's service config")?
            .response;
        (short_url_length, short_url_length_https)
    };

    let dms = TwitterJsonStream::user(
        &settings.consumer_key, &settings.consumer_secret,
        &settings.access_key, &settings.access_secret
    )
        .chain_err(|| "failed to connect to User Stream")?
        .then(|r| r.chain_err(|| "an error occured while listening on User Stream"))
        .filter_map(|json| if let Ok(StreamMessage::DirectMessage(dm)) = json::from_str(&json) {
            Some(dm)
        } else {
            None
        });
    let interval = Interval::new(|| Some(Duration::from_secs(60))); // TODO: implement scheduler

    let client = Client::new();

    dms.merge(interval).for_each(|merged| {
        use futures::stream::MergedItem::*;
        match merged {
            First(dm) => direct_message(dm, &tweeted, &mut users, &settings),
            Second(()) => update(&mut tweeted, &users, &settings, &archive, &client, url_len),
            Both(dm, ()) => {
                direct_message(dm, &tweeted, &mut users, &settings)?;
                update(&mut tweeted, &users, &settings, &archive, &client, url_len)
            },
        }
    }).wait()
}

/// Load configuration files under the specified directory.
fn load<P: AsRef<Path>>(working_dir: P) -> Result<(SyncFile<Tweeted>, SyncFile<UserMap>, Settings, File)> {
    use std::fs;

    let path = working_dir.as_ref();
    fs::create_dir_all(path).chain_err(|| format!("unable to create the working directory {:?}", path))?;
    let mut path = path.to_owned();

    macro_rules! for_file {
        ($name:expr, $exec:expr) => {{
            path.push($name);
            let ret = $exec(&path);
            path.pop();
            ret
        }};
    }

    let tweeted = for_file!("tweets.yaml", SyncFile::new).chain_err(|| "unable to open tweets.yaml")?;
    let users = for_file!("users.yaml", SyncFile::new).chain_err(|| "unable to open users.yaml")?;
    let settings: Settings = yaml::from_reader(
        for_file!("settings.yaml", File::open).chain_err(|| "unable to open settings.yaml")?
    ).chain_err(|| "failed to load settings.yaml")?;
    let archive = for_file!("archive.tsv", |path| OpenOptions::new().append(true).create(true).open(path))
        .chain_err(|| "unable to open archive.tsv")?;

    Ok((tweeted, users, settings, archive))
}

fn update(tweeted: &mut SyncFile<Tweeted>, users: &SyncFile<UserMap>, settings: &Settings, archive: &File,
    client: &Client, url_len: (i32, i32)) -> Result<()>
{
    fn fetch(url: &str, client: &Client, user_agent: &str) -> Result<String> {
        use hyper::header::UserAgent;
        use hyper::status::StatusCode;

        let mut res = client
            .get(url)
            .header(UserAgent(user_agent.to_owned()))
            .send()
            .chain_err(|| "failed to make an HTTP request")?;

        if StatusCode::Ok != res.status {
            return Err(res.status.to_string().into());
        }

        let mut body = String::new();
        res.read_to_string(&mut body).chain_err(|| "failed to read the response body")?;

        Ok(body)
    }

    fn remove_withdrawn(dept: &str, old: &mut HashMap<String, Kyuko>, new: &[Kyuko], archive: &File) -> Result<()> {
        fn write_archive(mut archive: &File, dept: &str, tweet_id: String, k: Kyuko) -> io::Result<()> {
            match k.remarks {
                Some(remarks) => writeln!(archive, "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    tweet_id, dept, k.kind, k.date, k.periods, k.title, k.lecturer, remarks),
                None              => writeln!(archive, "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    tweet_id, dept, k.kind, k.date, k.periods, k.title, k.lecturer),
            }
        }

        // TODO: explore more efficient way.
        // cf. Map::retain · Issue #1338 · rust-lang/rfcs https://github.com/rust-lang/rfcs/issues/1338
        let mut ret = HashMap::with_capacity(old.len());
        for (tweet_id, k) in old.drain() {
            if new.contains(&k) {
                ret.insert(tweet_id, k);
            } else {
                write_archive(archive, dept, tweet_id, k).chain_err(|| "failed to write to the archive file")?;
            }
        }

        *old = ret;

        Ok(())
    }

    use egg_mode::direct;
    use egg_mode::tweet::DraftTweet;

    for url in &settings.urls {
        let (dept, mut kyukos) = {
            let html = fetch(url, client, &settings.user_agent).chain_err(|| format!("failed to fetch {}", url))?;
            iba_kyuko_bot::scrape(html).chain_err(|| format!("failed to scrape {}", url))?
        };

        {
            let mut tweeted_kyukos = tweeted.entry(dept.clone()).or_insert_with(HashMap::new);
            remove_withdrawn(&dept, &mut tweeted_kyukos, &kyukos, archive)?;

            // Retain information to post.
            kyukos.retain(|k| !tweeted_kyukos.values().any(|c| c == k));

            for k in kyukos.drain(..) {
                let text = format_tweet(&dept, &k, url, url_len);

                // Post the information to Twitter:
                let id = DraftTweet::new(&text)
                    .send(&settings.token())
                    .chain_err(|| format!("failed to post a Tweet: {:?}", text))?
                    .id;
                info!("successfully tweeted: status_id = {}\n{}", id, text);

                // Send notifications to users following the information:
                for (user_id, _) in users.iter().filter(|&(_, u)| u.following.values().any(|f| f.matches(&k))) {
                    let user_id: u64 = user_id.parse()
                        .chain_err(|| format!("invalid user ID in {:?}", users.file_name()))?;
                    if let Err(e) = direct::send(user_id, &text, &settings.token()) {
                        warn!("failed to send a direct message {:?}\ncaused by: {:?}", text, e);
                    }
                }

                tweeted_kyukos.insert(id.to_string(), k);
            }
        }

        tweeted.commit()?;
    }

    Ok(())
}

fn direct_message(dm: DirectMessage, tweeted: &SyncFile<Tweeted>, users: &mut SyncFile<UserMap>, settings: &Settings)
    -> Result<()>
{
    let did_change = {
        let sender_info = users.entry(dm.sender_id.to_string()).or_insert_with(UserInfo::default);
        let mut sender_info = WriteTrace::new(sender_info);

        message::message(dm.text, dm.sender, &mut sender_info, dm.recipient.screen_name, tweeted, &settings.admins)?;

        sender_info.was_written()
    };

    if did_change {
        users.commit()?;
    }

    Ok(())
}

fn format_tweet(dept: &str, k: &Kyuko, url: &str, url_len: (i32, i32)) -> String {
    use chrono::Datelike;
    use egg_mode::text::character_count;
    use std::borrow::Cow;
    use std::fmt::Write;

    const WDAYS: [char; 7] = ['月', '火', '水', '木', '金', '土', '日'];

    fn escape(s: &str) -> Cow<str> {
        let mut s: Cow<str> = s.into();

        macro_rules! replace {
            ($c:expr) => {
                if s.contains($c) {
                    s = s.replace($c, concat!($c, ' ')).into();
                }
            }
        }

        replace!('#');
        replace!('@');
        replace!('$');
        replace!('＃');
        replace!('＠');

        s
    }

    let mut ret = format!(
        "\
            {}／{}\n\
            {} [{}]\n\
            {}年{}月{}日（{}）{}講時\n\
        ",
        escape(dept), escape(k.kind.as_str()), escape(k.title.as_str()), escape(k.lecturer.as_str()),
        k.date.year(), k.date.month(), k.date.day(), WDAYS[k.date.weekday().num_days_from_monday() as usize], k.periods
    );

    if let Some(ref r) = k.remarks {
        write!(ret, "{}\n", escape(r.as_str())).unwrap();
    }

    let mut len = character_count(&ret, url_len.0, url_len.1).0 + 1 + character_count(url, url_len.0, url_len.1).0;
    if len > 140 {
        while len > 140 {
            let c = ret.pop();
            #[cfg(debug_assertions)]
            c.unwrap();
            len -= 1;
        }
        ret.pop();
        ret.push('…');
    }

    write!(ret, "\n{}", url).unwrap();

    ret
}

impl Follow {
    fn matches(&self, k: &Kyuko) -> bool {
        if let Follow::Pattern { ref title, ref lecturer } = *self {
            k.title.contains(title) && lecturer.as_ref().map(|l| k.lecturer.contains(l)).unwrap_or(true)
        } else {
            false
        }
    }
}

impl Settings {
    fn token(&self) -> Token {
        Token::Access {
            consumer: KeyPair::new(self.consumer_key.as_str(), self.consumer_secret.as_str()),
            access: KeyPair::new(self.access_key.as_str(), self.access_secret.as_str()),
        }
    }
}
