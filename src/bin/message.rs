use config::{Follow, FollowError, Settings, Tweeted, UserInfo, UserMap};
use errors::*;
use std::fmt::Write;
use twitter_stream::messages::User;
use util::{self, SyncFile};

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

pub fn message(text: String, sender: User, users: &mut SyncFile<UserMap>, recipient_screen_name: String,
    tweeted: &mut SyncFile<Tweeted>, settings: &Settings) -> Result<String>
{
    use admin;
    use std::fmt::Write;

    info!("message: processing a message from @{} (ID: {})", sender.screen_name, sender.id);

    let mut response = String::new();
    let lang = &sender.lang;

    macro_rules! sender_info {
        () => (users.entry(sender.id.to_string()).or_insert_with(UserInfo::default));
    }

    for stmt in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let mut tokens = stmt.split(' ').filter(|s| !s.is_empty());

        macro_rules! unknown {
            ($cmd:expr) => (respondln!(response, lang, "未知のコマンド: `{}`", "Unknown command: `{}`", $cmd));
        }

        match tokens.next() {
            Some("follow") => follow(tokens, &mut response, sender_info!(), lang, &recipient_screen_name, tweeted)?,
            Some("unfollow") => unfollow(tokens, &mut response, sender_info!(), lang, &recipient_screen_name)?,
            Some("clear") => {
                sender_info!().clear();
                respondln!(
                    response, lang,
                    "全ての講座の情報のフォローを解除しました。", "You have unfollowed all the lecture information."
                );
            },
            Some("list") => list(&mut response, sender_info!(), lang, &recipient_screen_name)?,
            Some("rem") => (), // noop
            Some("admin") if settings.admins.contains(&sender.id) => match tokens.next() {
                Some("clear") => admin::clear(tweeted, &settings.token())?,
                Some("clear-users") => admin::clear_users(users)?,
                Some("remove") => admin::remove(tokens, tweeted, &settings.token())?,
                Some("shutdown") => {
                    // TODO: set a flag for the scheduler to halt
                },
                Some(cmd) => unknown!(cmd),
                None => (),
            },
            Some(cmd) => unknown!(cmd),
            None => (),
        }
    }

    Ok(response)
}

fn follow<'a, I: Iterator<Item=&'a str>>(mut tokens: I, response: &mut String,
    sender: &mut UserInfo, lang: &str, recipient_screen_name: &str, tweeted: &SyncFile<Tweeted>) -> Result<()>
{
    use self::Follow::*;

    fn register_inner(f: Follow, sender: &mut UserInfo, response: &mut String, lang: &str,
        recipient_screen_name: &str, tweeted: &SyncFile<Tweeted>) -> Result<()>
    {
        use either::{Left, Right};
        use self::FollowError::*;

        match sender.follow(f, tweeted) {
            Ok((id, Left((title, Some(lecturer))))) => respondln!(
                response, lang,
                "題目が「{}」を含み担当教員「{}」を含む講座の情報を通知します（ID: {}）。",
                "You will be notified of information about lectures containing \"{}\"
                    in their title and \"{}\" in their lecturer's name (ID: \"{}\").",
                title, lecturer, id
            ),
            Ok((id, Left((title, None)))) => respondln!(
                response, lang,
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
                    response, lang,
                    "follow: 講座「{} [{}]」についての{}情報\
                        （https://twitter.com/{}/status/{}）を{}に通知します。",
                    "You will be reminded of the {2} information of the lecture \
                        \"{0}\" by {1} (https://twitter.com/{3}/status/{4}, on {5}.",
                    k.title, k.lecturer, k.kind, recipient_screen_name, tweet_id, datefmt
                );
            },
            Err(AlreadyFollowing(id)) => respondln!(
                response, lang,
                "既にフォローしている情報です（ID: {}）",
                "You are already following the information (ID: \"{}\")",
                id
            ),
            Err(TweetDoesNotExist(tweet_id)) => respondln!(
                response, lang,
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
        ($f:expr) => (register_inner($f, sender, response, lang, recipient_screen_name, tweeted)?);
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

                for tweet in tokens.by_ref() {
                    if let Some(id) = util::ratoi(tweet) {
                        register!(TweetId(id));
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

fn unfollow<'a, I: Iterator<Item=&'a str>>(tokens: I, response: &mut String, sender: &mut UserInfo, lang: &str,
    recipient_screen_name: &str) -> Result<()>
{
    for id in tokens {
        match sender.following.remove(id) {
            Some(Follow::Pattern { title, lecturer: None }) => respondln!(
                response, lang,
                "ID {}（{}）の情報のフォローを解除しました。",
                "Unfollowed lecture information of \"{}\": \"{}\".",
                id, title
            ),
            Some(Follow::Pattern { title, lecturer: Some(lecturer) }) => respondln!(
                response, lang,
                "ID {}（{} [{}]）の情報のフォローを解除しました。",
                "Unfollowed lecture information of \"{}\": \"{}\" by {}.",
                id, title, lecturer
            ),
            Some(Follow::TweetId(tweet_id)) => respondln!(
                response, lang,
                "ID {}（https://twitter.com/{}/status/{}）の情報のフォローを解除しました。",
                "Unfollowed lecture information of \"{}\"(https://twitter.com/{}/status/{})",
                id, recipient_screen_name, tweet_id
            ),
            None => respondln!(
                response, lang,
                "ID {}の情報は存在しないか既に削除されています。",
                "The information of the ID \"{}\" does not exist or has been removed.",
                id
            ),
        }
    }

    Ok(())
}

fn list(response: &mut String, sender: &UserInfo, lang: &str, recipient_screen_name: &str) -> Result<()> {
    if sender.following.is_empty() {
        respondln!(response, lang,
            "あなたがフォローしている情報はありません", "You are not following any information."
        );
    } else {
        respondln!(response, lang,
            "あなたは以下の情報をフォローしています。", "You are following the information shown below:"
        );

        for (id, follow) in &sender.following {
            match *follow {
                Follow::Pattern { ref title, lecturer: None } => respondln!(
                    response, lang,
                    "・{}（ID: {}）", "* \"{}\" (ID: {})", title, id
                ),
                Follow::Pattern { ref title, lecturer: Some(ref lecturer) } => respondln!(
                    response, lang,
                    "・{}［{}］（ID: {}）", "* \"{}\" by {} (ID: {})", title, lecturer, id
                ),
                Follow::TweetId(tweet_id) => respondln!(
                    response, lang,
                    "・https://twitter.com/{}/status/{}（ID: {}）",
                    "* https://twitter.com/{}/status/{} (ID: {})",
                    recipient_screen_name, tweet_id, id
                ),
            }
        }
    }

    Ok(())
}
