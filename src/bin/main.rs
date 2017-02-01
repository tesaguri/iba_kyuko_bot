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

use egg_mode::{KeyPair, Token};
use hyper::client::{Client, IntoUrl};
use iba_kyuko_bot::Kyuko;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::Deref;
use std::path::Path;
use twitter_stream::messages::{DirectMessage, StreamMessage, User, UserId};
use util::{Interval, SyncFile};

mod errors {
    error_chain! {}
}

use errors::*;

// TODO: separate this to follow the single responsibility principle
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
    short_url_length: i32,
    short_url_length_https: i32,
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

#[derive(Default, Serialize, Deserialize)]
struct UserInfo {
    following: HashMap<
        String, // id
        Following
    >,
    next_id: u64,
    // TODO: rate limit
}

#[derive(Serialize, Deserialize)]
enum Following {
    #[serde(rename = "pattern")]
    Pattern {
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        lecturer: Option<String>,
    },
    #[serde(rename = "tweet_id")]
    TweetId(u64),
}

fn main() {
    use std::process;

    if let Err(e) = run() {
        writeln!(&mut io::stderr(), "error: {}", e).unwrap();
        for e in e.iter().skip(1) {
            writeln!(&mut io::stderr(), "caused by: {}", e).unwrap();
        }
        process::exit(1);
    }
}

fn run() -> Result<()> {
    use futures::{Future, Stream};
    use std::env;
    use std::time::Duration;
    use twitter_stream::TwitterJsonStream;

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
    let interval = Interval::new(|| Some(Duration::from_secs(60))); // TODO: implement scheduler

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
        use egg_mode::service::{self, Configuration};
        use std::fs;

        let path = path.as_ref();
        fs::create_dir_all(path).chain_err(|| format!("unable to create the working directory {:?}", path))?;
        let mut path = path.to_owned();

        path.push("settings.yaml");
        let settings: Settings = yaml::from_reader(File::open(&path).chain_err(|| "unable to open settings.yaml")?)
            .chain_err(|| "failed to load settings.yaml")?;
        path.pop();

        path.push("kyuko.yaml");
        let cache = SyncFile::new(&path).chain_err(|| "unable to open kyuko.yaml")?;
        path.pop();

        path.push("users.yaml");
        let users = SyncFile::new(&path).chain_err(|| "unable to open users.toml")?;
        path.pop();

        path.push("archive.tsv");
        let archive = OpenOptions::new().append(true).create(true).open(&path)
            .chain_err(|| "unable to open archive.tsv")?;
        path.pop();

        let Configuration {
            short_url_length,
            short_url_length_https,
            ..
        } = service::config(&settings.access())
            .chain_err(|| "failed to fetch Twitter's service config")?
            .response;

        Ok(Env {
            settings: settings,
            cache: cache,
            users: users,
            archive: archive,
            short_url_length: short_url_length,
            short_url_length_https: short_url_length_https,
            client: Client::new(),
        })
    }

    pub fn update(&mut self) -> Result<()> {
        use egg_mode::tweet::DraftTweet;

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

            // Post information on Twitter:
            for k in kyukos.drain(..) {
                // TODO: adjust Tweet length
                let text = format_tweet(&dept, &k, url, self.short_url_length, self.short_url_length_https);
                let id = DraftTweet::new(&text)
                    .send(&self.access())
                    .chain_err(|| format!("failed to post a Tweet: {:?}", text))?
                    .id.to_string();

                info!("successfully tweeted: status_id = {}\n{}", id, text);

                self.cache.get_mut(&dept).unwrap().insert(id, k);
            }
        }

        self.cache.commit()?;

        Ok(())
    }

    pub fn direct_message(&mut self, dm: DirectMessage) -> Result<()> {
        use std::fmt::Write;

        macro_rules! try_opt {
            ($opt:expr) => {
                if let Some(v) = $opt { v }
                else { continue; }
            };
        }

        let DirectMessage {
            ref text,
            sender_id,
            id,
            sender: User { ref lang, .. },
            recipient: User { ref screen_name, .. },
            ..
        } = dm;
        let sender_id_string = sender_id.to_string();
        self.users.entry(sender_id_string.clone()).or_insert_with(UserInfo::default);
        macro_rules! sender { () => (self.users.get_mut(&sender_id_string).unwrap()); }
        let mut response = String::new();

        macro_rules! respond {
            ($fmt_ja:expr, $fmt_en:expr) => {
                if lang.starts_with("en") {
                    write!(response, $fmt_en).unwrap();
                } else {
                    write!(response, $fmt_ja).unwrap();
                }
            };
            ($fmt_ja:expr, $fmt_en:expr, $($arg:tt)*) => {
                if lang.starts_with("en") {
                    write!(response, $fmt_en, $($arg)*).unwrap();
                } else {
                    write!(response, $fmt_ja, $($arg)*).unwrap();
                }
            };
        }

        macro_rules! respondln {
            ($fmt_ja:expr, $fmt_en:expr) => {
                respond!(concat!($fmt_ja, "\n"), concat!($fmt_en, "\n"));
            };
            ($fmt_ja:expr, $fmt_en:expr, $($arg:tt)*) => {
                respond!(concat!($fmt_ja, "\n"), concat!($fmt_en, "\n"), $($arg)*);
            };
        }

        info!("direct message {} from {}", id, sender_id);

        let statements = text.split(';').map(str::trim).filter(|s| !s.is_empty());

        for statement in statements {
            let mut tokens = statement.split(' ').filter(|s| !s.is_empty());
            match tokens.next() {
                // SYNOPSIS:
                // follow [<title> [by <lecturer>]] ... [tweet <tweet_id> ...]
                Some("follow") => {
                    use Following::*;

                    let mut title: Option<&str> = None;

                    macro_rules! follow {
                        ($f:expr) => {{
                            let f = $f;
                            let insert: bool;

                            match f {
                                Pattern { ref title, ref lecturer } => {
                                    respond!(
                                        "題目が「{}」を含",
                                        "You will be notified of information about lectures containing \"{}\" \
                                            in their title",
                                        title
                                    );
                                    if let &Some(ref l) = lecturer {
                                        respond!(
                                            "み、担当教員名が「{}」を含",
                                            " and \"{}\" in their lecturer's name",
                                            l
                                        );
                                    }
                                    respondln!("む講座についての情報を通知します。", ".");

                                    insert = true;
                                },
                                TweetId(id) => {
                                    let mut kyuko = None;

                                    for kyukos in self.cache.values() {
                                        if let Some(k) = kyukos.get(&id.to_string()) {
                                            kyuko = Some(k);
                                            break;
                                        }
                                    }

                                    if let Some(k) = kyuko {
                                        let datefmt = if dm.sender.lang.starts_with("en") {
                                            k.date.format("%a, %b %d-, %Y")
                                        } else {
                                            k.date.format("%Y年%-m月%-d日")
                                        };
                                        respondln!(
                                            "follow: 講座「{} [{}]」についての{}情報\
                                                （https://twitter.com/{}/status/{}）を{}に通知します。",
                                            "You will be reminded of the {2} information of the lecture \
                                                \"{0}\" by {1} (https://twitter.com/{3}/status/{4}, on {5}.",
                                            k.title, k.lecturer, k.kind, screen_name, id, datefmt
                                        );

                                        insert = true;
                                    } else {
                                        respondln!(
                                            "https://twitter.com/{}/status/{}の情報は存在しないか、\
                                                または既に掲示が終了しています。",
                                            "The lecture information of https://twitter.com/{}/status/{} \
                                                does not exist or has been withdrawn.",
                                            screen_name, id
                                        );

                                        insert = false;
                                    }
                                },
                            }

                            if insert {
                                let sender = self.users.get_mut(&sender_id_string).unwrap();
                                sender.following.insert(follow_id(sender.next_id), f);
                                sender.next_id += 1;
                            }
                        }}
                    }

                    while let Some(arg) = tokens.next() {
                        match arg {
                            "by" => {
                                if let Some(t) = title.take() {
                                    follow!(Pattern {
                                        title: t.to_owned(),
                                        lecturer: tokens.next().map(ToOwned::to_owned)
                                    });
                                } else {
                                    title = Some(arg);
                                }
                            },
                            "tweet" => {
                                if let Some(t) = title.take() {
                                    follow!(Pattern { title: t.to_owned(), lecturer: None });
                                }

                                while let Some(t) = tokens.next() {
                                    follow!(TweetId(try_opt!(util::ratoi(t))));
                                }
                            },
                            t => title = Some(t),
                        }
                    }

                    if let Some(t) = title.take() {
                        follow!(Pattern { title: t.to_owned(), lecturer: None });
                    }
                },
                // unfollow <tweet_id> ...
                Some("unfollow") => {
                    while let Some(id) = tokens.next() {
                        match sender!().following.remove(id) {
                            Some(Following::Pattern { title, lecturer: None }) => respondln!(
                                "ID {}（{}）の情報のフォローを解除しました。",
                                "Unfollowed lecture information of \"{}\": \"{}\".",
                                id, title
                            ),
                            Some(Following::Pattern { title, lecturer: Some(lecturer) }) => respondln!(
                                "ID {}（{} [{}]）の情報のフォローを解除しました。",
                                "Unfollowed lecture information of \"{}\": \"{}\" by {}.",
                                id, title, lecturer
                            ),
                            Some(Following::TweetId(tweet_id)) => respondln!(
                                "ID {}（https://twitter.com/{}/status/{}）の情報のフォローを解除しました。",
                                "Unfollowed lecture information of \"{}\"(https://twitter.com/{}/status/{})",
                                id, screen_name, tweet_id
                            ),
                            None => respondln!(
                                "ID {}の情報は存在しないか既に削除されています。",
                                "The information of the ID \"{}\" does not exist or has been removed.",
                                id
                            ),
                        }
                    }
                },
                Some("list") => {
                    respondln!(
                        "あなたは以下の情報をフォローしています。", "You are following the information shown below:"
                    );
                    for (id, follow) in sender!().following.iter() {
                        match *follow {
                            Following::Pattern { ref title, lecturer: None } => respondln!(
                                "・{}（ID: {}）", "* \"{}\" (ID: {})", title, id
                            ),
                            Following::Pattern { ref title, lecturer: Some(ref lecturer) } => respondln!(
                                "・{}［{}］（ID: {}）", "* \"{}\" by {} (ID: {})", title, lecturer, id
                            ),
                            Following::TweetId(tweet_id) => respondln!(
                                "・https://twitter.com/{}/status/{}（ID: {}）",
                                "* https://twitter.com/{}/status/{} (ID: {})",
                                screen_name, tweet_id, id
                            ),
                        }
                    }
                },
                Some("status") => if self.admins.contains(&sender_id) {
                    writeln!(response, "status: OK").unwrap();
                },
                Some("shutdown") => if self.admins.contains(&sender_id) {
                    // TODO
                },
                Some(cmd) => {
                    info!("unknown command: {}", cmd);
                },
                None => (),
            }
        }

        Ok(())
    }

    fn fetch<U: IntoUrl>(&self, url: U) -> Result<String> {
        use hyper::header::UserAgent;
        use hyper::status::StatusCode;

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
    fn access<'a>(&'a self) -> Token<'a> {
        Token::Access {
            consumer: KeyPair::new(self.consumer_key.as_str(), self.consumer_secret.as_str()),
            access: KeyPair::new(self.access_key.as_str(), self.access_secret.as_str()),
        }
    }
}

fn format_tweet(dept: &str, k: &Kyuko, url: &str, url_len: i32, url_len_https: i32) -> String {
    use chrono::Datelike;
    use egg_mode::text::character_count;
    use std::borrow::Cow;
    use std::fmt::Write;

    const WDAYS: [char; 7] = ['月', '火', '水', '木', '金', '土', '日'];

    fn escape<'a, T: Into<Cow<'a, str>>>(s: T) -> Cow<'a, str> {
        let mut s = s.into();

        macro_rules! replace {
            ($c:expr) => {
                if s.contains($c) {
                    s = s.to_mut().replace($c, concat!($c, ' ')).into();
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

    let mut len = character_count(&ret, url_len, url_len_https).0 + 1 + character_count(url, url_len, url_len_https).0;
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

fn follow_id(mut n: u64) -> String {
    let mut ret = Vec::new();

    while n != 0 {
        ret.push(util::BASE64[n as usize % 64]);
        n >>= 6;
    }

    unsafe {
        String::from_utf8_unchecked(ret)
    }
}
