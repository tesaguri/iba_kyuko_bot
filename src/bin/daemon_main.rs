use chrono::Local;
use config::*;
use egg_mode::direct;
use egg_mode::user::{self, TwitterUser};
use egg_mode::tweet::DraftTweet;
use errors::*;
use hyper::client::Client;
use iba_kyuko_bot::Kyuko;
use schedule::Schedule;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use twitter_stream::messages::{DirectMessage, StreamMessage, Tweet};
use util::{self, SyncFile};

pub fn run(mut tweeted: SyncFile<Tweeted>, mut users: SyncFile<UserMap>, settings: Settings, archive: File)
    -> Result<()>
{
    use egg_mode::{self, service, Response};
    use futures::{Future, Stream};
    use json;
    use twitter_stream::TwitterJsonStream;

    enum TweetOrDm {
        Dm(DirectMessage),
        Tweet(Tweet),
    }

    let (url_len, dm_text_limit) = {
        let conf = service::config(&settings.token())
            .chain_err(|| "failed to fetch Twitter's service config")?
            .response;
        ((conf.short_url_length, conf.short_url_length_https), conf.dm_text_character_limit as usize)
    };

    let tz = Local;
    let schedule = Schedule::new(&settings.schedule, &tz);

    let Response { response: TwitterUser { id, .. }, .. } = egg_mode::verify_tokens(&settings.token())
        .chain_err(|| "failed to retrieve the information of the authenticating user")?;

    let messages = TwitterJsonStream::user(
        &settings.consumer_key, &settings.consumer_secret,
        &settings.access_key, &settings.access_secret
    )
        .chain_err(|| "failed to connect to User Stream")?
        .then(|r| r.chain_err(|| "an error occured while listening on User Stream"))
        .filter_map(|json| match json::from_str(&json) {
            Ok(StreamMessage::Tweet(t)) => if t.in_reply_to_user_id == Some(id) {
                Some(TweetOrDm::Tweet(t))
            } else {
                // XXX: This clause can be removed after RFC 0107 was implemented.
                // cf. https://github.com/rust-lang/rfcs/blob/master/text/0107-pattern-guards-with-bind-by-move.md
                None
            },
            Ok(StreamMessage::DirectMessage(dm)) => if dm.recipient_id == id {
                Some(TweetOrDm::Dm(dm))
            } else {
                None
            },
            _ => None,
        });

    let client = Client::new();

    let future = schedule.merge(messages).for_each(|merged| {
        use futures::stream::MergedItem::*;
        match merged {
            First(()) => update(&mut tweeted, &users, &settings, &archive, &client, url_len),
            Second(msg) => match msg {
                TweetOrDm::Tweet(t) => reply(t, &mut tweeted, &mut users, &settings, url_len),
                TweetOrDm::Dm(dm) => direct_message(dm, &mut tweeted, &mut users, &settings, dm_text_limit),
            },
            Both((), msg) => {
                match msg {
                    TweetOrDm::Tweet(t) => reply(t, &mut tweeted, &mut users, &settings, url_len)?,
                    TweetOrDm::Dm(dm) => direct_message(dm, &mut tweeted, &mut users, &settings, dm_text_limit)?,
                }
                update(&mut tweeted, &users, &settings, &archive, &client, url_len)
            },
        }
    });

    info!("started");

    future.wait()
}

fn update(tweeted: &mut SyncFile<Tweeted>, users: &SyncFile<UserMap>, settings: &Settings, archive: &File,
    client: &Client, url_len: (i32, i32)) -> Result<()>
{
    fn fetch(url: &str, client: &Client, user_agent: &str, keep_alive: bool) -> Result<String> {
        use hyper::header::{Connection, UserAgent};
        use hyper::status::StatusCode;
        use std::io::Read;

        let mut res = client
            .get(url)
            .header(if keep_alive { Connection::keep_alive() } else { Connection::close() })
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
        use std::io::{self, Write};

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

    info!("started crawling");

    // Eagarly evaluate HTTP connections to prevent disconnection from the server.
    let mut buf = Vec::new();
    for (i, url) in settings.urls.iter().enumerate() {
        info!("fetching {}", url);
        let html = fetch(url, client, &settings.user_agent, i+1 < settings.urls.len())
            .chain_err(|| format!("failed to fetch {}", url))?;
        buf.push(html);
    }

    for (url, html) in settings.urls.iter().zip(buf.drain(..)) {
        let (dept, mut kyukos) = ::iba_kyuko_bot::scrape(html).chain_err(|| format!("failed to scrape {}", url))?;

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
                for (user_id, via) in users.iter().filter_map(|(user_id, u)| {
                    if let Some(&FollowEntry(_, via)) = u.following.values()
                        .find(|&&FollowEntry(ref f, _)| f.matches(&k))
                    {
                        Some((user_id, via))
                    } else {
                        None
                    }
                }) {
                    let user_id: u64 = user_id.parse()
                        .chain_err(|| format!("invalid user ID in {:?}", users.file_name()))?;
                    match via {
                        MessageMethod::Dm => if let Err(e) = direct::send(user_id, &text, &settings.token()) {
                            warn!("failed to send a direct message {:?}\ncaused by: {:?}", text, e);
                        },
                        MessageMethod::Reply => {
                            match user::show(user_id, &settings.token()) {
                                Ok(user) => {
                                    let text = format!("@{} {}", user.screen_name, text);
                                    if let Err(e) = DraftTweet::new(&text).send(&settings.token()) {
                                        warn!("failed to send a reply {:?}\ncaused by: {:?}", text, e);
                                    }
                                },
                                Err(e) => warn!(
                                    "failed to retrieve the user information of {}\ncaused by {:?}", user_id, e
                                ),
                            }
                        }
                    }
                }

                tweeted_kyukos.insert(id.to_string(), k);
            }
        }

        tweeted.commit()?;
    }

    Ok(())
}

fn reply(tweet: Tweet, tweeted: &mut SyncFile<Tweeted>, users: &mut SyncFile<UserMap>,
    settings: &Settings, url_len: (i32, i32)) -> Result<()>
{
    let id = tweet.id;

     // Remove @user_name
    let text = tweet.text.split_at(tweet.text.find(' ').unwrap() + 1).1;

    let mut response = String::from("@");
    response.push_str(&tweet.user.screen_name);

    let body = ::message::message(
        MessageMethod::Reply, text, tweet.user, tweet.in_reply_to_screen_name.unwrap(), tweet.in_reply_to_status_id,
        users, tweeted, settings
    )?;

    if ! body.is_empty() {
        response.push(' ');

        let mut body = escape(body).into_owned();
        util::shorten_tweet(&mut body, 140 - response.len(), url_len);

        response.push_str(&body);

        if let Err(e) = DraftTweet::new(&response).in_reply_to(id).send(&settings.token()) {
            warn!("failed to send a reply {:?}\ncaused by: {:?}", response, e);
        }
    }

    Ok(())
}

fn direct_message(dm: DirectMessage, tweeted: &mut SyncFile<Tweeted>, users: &mut SyncFile<UserMap>,
    settings: &Settings, dm_text_limit: usize) -> Result<()>
{
    let mut response = ::message::message(
        MessageMethod::Dm, &dm.text, dm.sender, dm.recipient.screen_name, None, users, tweeted, settings
    )?;

    if ! response.is_empty() {
        util::shorten(&mut response, dm_text_limit);
        if let Err(e) = direct::send(dm.sender_id, &response, &settings.token()) {
            warn!("failed to send a direct message {:?}\ncaused by: {:?}", response, e);
        }
    }

    Ok(())
}

fn format_tweet(dept: &str, k: &Kyuko, url: &str, url_len: (i32, i32)) -> String {
    use chrono::Datelike;
    use egg_mode::text;
    use std::fmt::Write;

    const WDAYS: [char; 7] = ['月', '火', '水', '木', '金', '土', '日'];

    let mut ret = format!(
        "\
            {}／{}\n\
            {} [{}]\n\
            {}年{}月{}日（{}）{}講時\
        ",
        escape(dept), escape(k.kind.as_str()), escape(k.title.as_str()), escape(k.lecturer.as_str()),
        k.date.year(), k.date.month(), k.date.day(), WDAYS[k.date.weekday().num_days_from_monday() as usize], k.periods
    );

    if let Some(ref r) = k.remarks {
        write!(ret, "{}\n", escape(r.as_str())).unwrap();
    }

    let suffix_len = 1 + text::character_count(url, url_len.0, url_len.1).0;
    util::shorten_tweet(&mut ret, 140 - suffix_len, url_len);
    ret.push('\n');
    ret.push_str(url);

    ret
}

fn escape<'a, S: Into<Cow<'a, str>>>(s: S) -> Cow<'a, str> {
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
