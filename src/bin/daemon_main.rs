use chrono::Local;
use config::*;
use egg_mode::direct;
use errors::*;
use hyper::client::Client;
use iba_kyuko_bot::Kyuko;
use schedule::Schedule;
use std::collections::HashMap;
use std::fs::File;
use twitter_stream::messages::{DirectMessage, StreamMessage};
use util::SyncFile;

pub fn run(mut tweeted: SyncFile<Tweeted>, mut users: SyncFile<UserMap>, settings: Settings, archive: File)
    -> Result<()>
{
    use egg_mode::service;
    use futures::{Future, Stream};
    use json;
    use twitter_stream::TwitterJsonStream;

    let url_len = {
        let conf = service::config(&settings.token())
            .chain_err(|| "failed to fetch Twitter's service config")?
            .response;
        (conf.short_url_length, conf.short_url_length_https)
    };

    let tz = Local;
    let schedule = Schedule::new(&settings.schedule, &tz);

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

    let client = Client::new();

    let future = schedule.merge(dms).for_each(|merged| {
        use futures::stream::MergedItem::*;
        match merged {
            First(()) => update(&mut tweeted, &users, &settings, &archive, &client, url_len),
            Second(dm) => direct_message(dm, &mut tweeted, &mut users, &settings),
            Both((), dm) => {
                direct_message(dm, &mut tweeted, &mut users, &settings)?;
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
    use egg_mode::tweet::DraftTweet;

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

fn direct_message(dm: DirectMessage, tweeted: &mut SyncFile<Tweeted>, users: &mut SyncFile<UserMap>,
    settings: &Settings) -> Result<()>
{
    if dm.recipient_id != dm.sender_id // Ignore DMs from the authenticated user (i.e. the bot) itself.
    {
        let response = ::message::message(dm.text, dm.sender, users, dm.recipient.screen_name, tweeted, settings)?;

        if !response.is_empty() {
            if let Err(e) = direct::send(dm.sender_id, &response, &settings.token()) {
                warn!("failed to send a direct message {:?}\ncaused by: {:?}", response, e);
            }
        }
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
            {}年{}月{}日（{}）{}講時\
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
            debug_assert!(c.is_some());
            len -= 1;
        }
        ret.pop();
        ret.push('…');
    }

    write!(ret, "\n{}", url).unwrap();

    ret
}
