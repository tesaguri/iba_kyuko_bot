use config::*;
use errors::*;
use std::borrow::Cow;
use std::fmt::Write;
use twitter_stream::User;
use twitter_stream::tweet::StatusId;
use util::SyncFile;

const WRITE_FAILED: &'static str = "failed to write a message to a String";

macro_rules! respondln {
    ($dst:expr, $lang:expr, $fmt_ja:expr, $fmt_en:expr $(, $arg:expr)*) => {
        if $lang.starts_with("en") {
            write!($dst, concat!($fmt_en, '\n'), $($arg),*)
        } else {
            write!($dst, concat!($fmt_ja, '\n'), $($arg),*)
        }.chain_err(|| WRITE_FAILED)?;
    };
}

pub fn message(via: MessageMethod, text: &str, sender: User, recipient_screen_name: String,
    in_reply_to: Option<StatusId>, users: &mut SyncFile<UserMap>, tweeted: &mut SyncFile<Tweeted>, settings: &Settings)
    -> Result<String>
{
    use admin;
    use std::fmt::Write;
    use std::process;

    info!("message: processing a message from @{} (ID: {})", sender.screen_name, sender.id);

    let mut resp = String::new();
    let lang = &sender.lang;

    macro_rules! sender_info {
        () => (users.entry(sender.id.to_string()).or_insert_with(UserInfo::default));
    }

    for stmt in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let mut tokens = stmt.split(' ').filter(|s| !s.is_empty());

        macro_rules! unknown {
            ($cmd:expr) => (respondln!(resp, lang, "未知のコマンド: `{}`", "Unknown command: `{}`", $cmd));
        }

        match tokens.next() {
            Some("follow") => follow(
                tokens, &mut resp, via, in_reply_to, sender_info!(), lang, &recipient_screen_name, tweeted
            )?,
            Some("unfollow") => unfollow(tokens, &mut resp, in_reply_to, sender_info!(), lang, &recipient_screen_name)?,
            Some("clear") => {
                sender_info!().clear();
                respondln!(
                    resp, lang,
                    "全ての講座の情報のフォローを解除しました。", "You have unfollowed all the lecture information."
                );
            },
            Some("list") => list(&mut resp, sender_info!(), lang, &recipient_screen_name)?,
            Some("rem") => (), // noop
            Some("admin") if settings.admins.contains(&sender.id) => match tokens.next() {
                Some("clear") => admin::clear(tweeted, &settings.token.clone().into())?,
                Some("clear-users") => admin::clear_users(users)?,
                Some("remove") => admin::remove(tokens, tweeted, &settings.token.clone().into())?,
                Some("shutdown") => process::exit(0), // TODO: graceful shutdown
                Some(cmd) => unknown!(cmd),
                None => (),
            },
            Some(cmd) => unknown!(cmd),
            None => (),
        }
    }

    Ok(resp)
}

fn follow<'a, I: Iterator<Item=&'a str>>(mut tokens: I, resp: &mut String, via: MessageMethod,
    in_reply_to: Option<StatusId>, sender: &mut UserInfo, lang: &str, recipient_screen_name: &str,
    tweeted: &SyncFile<Tweeted>) -> Result<()>
{
    use self::Follow::*;

    fn register_inner(f: Follow, via: MessageMethod, sender: &mut UserInfo, resp: &mut String, lang: &str,
        recipient_screen_name: &str, tweeted: &SyncFile<Tweeted>) -> Result<()>
    {
        use either::{Left, Right};
        use self::FollowError::*;

        match sender.follow(f, via, tweeted) {
            Ok((id, Left((title, Some(lecturer))))) => respondln!(
                resp, lang,
                "題目が「{}」を含み担当教員「{}」を含む講座の情報を通知します（ID: {}）。",
                "You will be notified of information about lectures containing \"{}\"
                    in their title and \"{}\" in their lecturer's name (ID: \"{}\").",
                title, lecturer, id
            ),
            Ok((id, Left((title, None)))) => respondln!(
                resp, lang,
                "題目が「{}」を含む講座の情報を通知します（ID: {}）。",
                "You will be notified of information about lectures containing \"{}\"
                    in their title (ID: \"{}\")",
                title, id
            ),
            Ok((_, Right((tweet_id, k)))) => {
                let datefmt = if lang.starts_with("en") {
                    k.date.format("%a, %b %d-, %Y")
                } else {
                    k.date.format("%Y年%-m月%-d日")
                };
                respondln!(
                    resp, lang,
                    "follow: 講座「{} [{}]」についての{}情報\
                        （https://twitter.com/{}/status/{}）を{}に通知します。",
                    "You will be reminded of the {2} information of the lecture \
                        \"{0}\" by {1} (https://twitter.com/{3}/status/{4}), on {5}.",
                    k.title, k.lecturer, k.kind, recipient_screen_name, tweet_id, datefmt
                );
            },
            Err(AlreadyFollowing(id)) => respondln!(
                resp, lang,
                "既にフォローしている情報です（ID: {}）",
                "You are already following the information (ID: \"{}\")",
                id
            ),
            Err(TweetDoesNotExist(tweet_id)) => respondln!(
                resp, lang,
                "https://twitter.com/{}/status/{}の情報は存在しないか、\
                    または既に掲示が終了しています。",
                "The lecture information of https://twitter.com/{}/status/{} \
                    does not exist or has been withdrawn.",
                recipient_screen_name, tweet_id
            ),
        }

        Ok(())
    }

    macro_rules! register {
        ($f:expr) => (register_inner($f, via, sender, resp, lang, recipient_screen_name, tweeted)?);
    }

    let mut title: Option<&str> = None;

    while let Some(arg) = tokens.next() {
        match arg {
            "by" => {
                if let Some(t) = title.take() {
                    register!(Pattern {
                        title: t.to_owned(),
                        lecturer: tokens.next().map(ToOwned::to_owned)
                    });
                } else {
                    title = Some(arg);
                }
            },
            "tweet" => {
                if let Some(t) = title.take() {
                    register!(Pattern { title: t.to_owned(), lecturer: None });
                }

                if let Some(id) = in_reply_to {
                    register!(TweetId(id));
                }

                for tweet_id in tokens.by_ref() {
                    if let Ok(tweet_id) = tweet_id.parse() {
                        register!(TweetId(tweet_id));
                    }
                }
            },
            t => {
                if let Some(t) = title {
                    register!(Pattern { title: t.to_owned(), lecturer: None });
                }
                title = Some(t);
            },
        }
    }

    if let Some(t) = title {
        register!(Pattern { title: t.to_owned(), lecturer: None });
    }

    Ok(())
}

fn unfollow<'a, I, S>(tokens: I, resp: &mut String, in_reply_to: Option<StatusId>, sender: &mut UserInfo, lang: &str,
    recipient_screen_name: &str) -> Result<()>
    where I: Iterator<Item=S>, S: Into<Cow<'a, str>>
{
    for id in tokens.map(Into::into).chain(in_reply_to.map(|id| id.to_string().into())) {
        match sender.following.remove(id.as_ref()).map(|ent| ent.0) {
            Some(Follow::Pattern { title, lecturer: None }) => respondln!(
                resp, lang,
                "ID {}（{}）の情報のフォローを解除しました。",
                "Unfollowed lecture information of \"{}\": \"{}\".",
                id, title
            ),
            Some(Follow::Pattern { title, lecturer: Some(lecturer) }) => respondln!(
                resp, lang,
                "ID {}（{} [{}]）の情報のフォローを解除しました。",
                "Unfollowed lecture information of \"{}\": \"{}\" by {}.",
                id, title, lecturer
            ),
            Some(Follow::TweetId(tweet_id)) => respondln!(
                resp, lang,
                "ID {}（https://twitter.com/{}/status/{}）の情報のフォローを解除しました。",
                "Unfollowed lecture information of \"{}\"(https://twitter.com/{}/status/{})",
                id, recipient_screen_name, tweet_id
            ),
            None => respondln!(
                resp, lang,
                "ID {}の情報は存在しないか既に削除されています。",
                "The information of the ID \"{}\" does not exist or has been removed.",
                id
            ),
        }
    }

    Ok(())
}

fn list(resp: &mut String, sender: &UserInfo, lang: &str, recipient_screen_name: &str) -> Result<()> {
    if sender.following.is_empty() {
        respondln!(resp, lang,
            "あなたがフォローしている情報はありません", "You are not following any information."
        );
    } else {
        respondln!(resp, lang,
            "あなたは以下の情報をフォローしています。", "You are following the information shown below:"
        );

        for (id, &FollowEntry(ref follow, via)) in &sender.following {
            match *follow {
                Follow::Pattern { ref title, lecturer: None } => respondln!(
                    resp, lang,
                    "・{}（ID：{}；{}）", "* \"{}\" (ID: {}; {})", title, id, via
                ),
                Follow::Pattern { ref title, lecturer: Some(ref lecturer) } => respondln!(
                    resp, lang,
                    "・{}［{}］（ID：{}；{}）", "* \"{}\" by {} (ID: {}; {})", title, lecturer, id, via
                ),
                Follow::TweetId(tweet_id) => respondln!(
                    resp, lang,
                    "・https://twitter.com/{}/status/{}（ID: {}；{}）",
                    "* https://twitter.com/{}/status/{} (ID: {}; {})",
                    recipient_screen_name, tweet_id, id, via
                ),
            }
        }
    }

    Ok(())
}
