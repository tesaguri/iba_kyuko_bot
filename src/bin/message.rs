use {Following, Tweeted, UserInfo};
use errors::*;
use std::fmt::Write;
use twitter_stream::messages::{User, UserId};
use util::{self, SyncFile};

const WRITE_FAILED: &'static str = "failed to write a message to a String";

macro_rules! try_opt {
    ($opt:expr) => {
        if let Some(v) = $opt { v }
        else { continue; }
    };
}

macro_rules! respond {
    ($dst:expr, $lang:expr, $fmt_ja:expr, $fmt_en:expr, $($args:tt)*) => {
        if $lang.starts_with("en") {
            write!($dst, $fmt_en, $($args)*)
        } else {
            write!($dst, $fmt_ja, $($args)*)
        }.chain_err(|| WRITE_FAILED)?;
    };
    ($dst:expr, $lang:expr, $fmt_ja:expr, $fmt_en:expr) => (respond!($dst, $lang, $fmt_ja, $fmt_en,));
}

macro_rules! respondln {
    ($dst:expr, $lang:expr, $fmt_ja:expr, $fmt_en:expr, $($args:tt)*) => {
        respond!($dst, $lang, concat!($fmt_ja, '\n'), concat!($fmt_en, '\n'), $($args)*)
    };
    ($dst:expr, $lang:expr, $fmt_ja:expr, $fmt_en:expr) => (respondln!($dst, $lang, $fmt_ja, $fmt_en,));
}

pub fn message(text: String, sender: User, sender_info: &mut UserInfo, recipient_screen_name: String,
    tweeted: &SyncFile<Tweeted>, admins: &[UserId]) -> Result<String>
{
    use std::fmt::Write;

    let mut response = String::new();

    for stmt in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let mut tokens = stmt.split(' ').filter(|s| !s.is_empty());

        match tokens.next() {
            Some("follow") => {
                follow(tokens, &mut response, sender_info, &sender.lang, &recipient_screen_name, tweeted)?;
            },
            Some("unfollow") => unfollow(tokens, &mut response, sender_info, &sender.lang, &recipient_screen_name)?,
            Some("list") => list(&mut response, &sender, sender_info, &recipient_screen_name)?,
            Some("status") => if admins.contains(&sender.id) {
                writeln!(response, "status: OK").chain_err(|| WRITE_FAILED)?;
            },
            Some("shutdown") => if admins.contains(&sender.id) {
                // TODO: set a flag for the scheduler to halt
            },
			Some("rem") => (), // noop
            Some(cmd) => {
                info!("unknown command: {}", cmd);
            },
            None => (),
        }
    }

    Ok(response)
}

fn follow<'a, I: Iterator<Item=&'a str>>(mut tokens: I, response: &mut String,
    sender: &mut UserInfo, lang: &str, recipient_screen_name: &str, tweeted: &SyncFile<Tweeted>) -> Result<()>
{
    use Following::*;

    fn register_inner(f: Following, sender: &mut UserInfo, response: &mut String, lang: &str,
        recipient_screen_name: &str, tweeted: &SyncFile<Tweeted>) -> Result<()>
    {
        macro_rules! insert {
            () => {{
                sender.following.insert(util::radix64(sender.next_id), f);
                sender.next_id += 1;
            }};
        }

        if let Some((id, _)) = sender.following.iter().find(|&(_, fo)| fo == &f) {
            respond!(
                response, lang,
                "既にフォローしている情報です（ID: {}）",
                "You are already following the information (ID: \"{}\")",
                id
            );
            return Ok(());
        }

        match f {
            Pattern { .. } => {
                if let Pattern { ref title, ref lecturer } = f // Limit the borrow to this block.
                {
                    respond!(
                        response, lang,
                        "題目が「{}」を含",
                        "You will be notified of information about lectures containing \"{}\" \
                            in their title",
                        title
                    );
                    if let Some(ref l) = *lecturer {
                        respond!(
                            response, lang,
                            "み、担当教員名が「{}」を含",
                            " and \"{}\" in their lecturer's name",
                            l
                        );
                    }
                    respondln!(response, lang, "む講座についての情報を通知します。", ".");
                }

                insert!();
            },
            TweetId(id) => {
                let id = id.to_string();
                if let Some(k) = tweeted.values().filter_map(|kyukos| kyukos.get(&id)).next() {
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
                        k.title, k.lecturer, k.kind, recipient_screen_name, id, datefmt
                    );

                    insert!();
                } else {
                    respondln!(
                        response, lang,
                        "https://twitter.com/{}/status/{}の情報は存在しないか、\
                            または既に掲示が終了しています。",
                        "The lecture information of https://twitter.com/{}/status/{} \
                            does not exist or has been withdrawn.",
                        recipient_screen_name, id
                    );
                }
            },
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

                for t in tokens.by_ref() {
                    register!(TweetId(try_opt!(util::ratoi(t))));
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

    if let Some(t) = title.take() {
        register!(Pattern { title: t.to_owned(), lecturer: None });
    }

    Ok(())
}

fn unfollow<'a, I: Iterator<Item=&'a str>>(tokens: I, response: &mut String, sender: &mut UserInfo, lang: &str,
    recipient_screen_name: &str) -> Result<()>
{
    for id in tokens {
        match sender.following.remove(id) {
            Some(Following::Pattern { title, lecturer: None }) => respondln!(
                response, lang,
                "ID {}（{}）の情報のフォローを解除しました。",
                "Unfollowed lecture information of \"{}\": \"{}\".",
                id, title
            ),
            Some(Following::Pattern { title, lecturer: Some(lecturer) }) => respondln!(
                response, lang,
                "ID {}（{} [{}]）の情報のフォローを解除しました。",
                "Unfollowed lecture information of \"{}\": \"{}\" by {}.",
                id, title, lecturer
            ),
            Some(Following::TweetId(tweet_id)) => respondln!(
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

fn list(response: &mut String, sender: &User, sender_info: &UserInfo, recipient_screen_name: &str) -> Result<()> {
	respondln!(
		response, sender.lang,
		"あなたは以下の情報をフォローしています。", "You are following the information shown below:"
	);

	for (id, follow) in &sender_info.following {
		match *follow {
			Following::Pattern { ref title, lecturer: None } => respondln!(
				response, sender.lang,
				"・{}（ID: {}）", "* \"{}\" (ID: {})", title, id
			),
			Following::Pattern { ref title, lecturer: Some(ref lecturer) } => respondln!(
				response, sender.lang,
				"・{}［{}］（ID: {}）", "* \"{}\" by {} (ID: {})", title, lecturer, id
			),
			Following::TweetId(tweet_id) => respondln!(
				response, sender.lang,
				"・https://twitter.com/{}/status/{}（ID: {}）",
				"* https://twitter.com/{}/status/{} (ID: {})",
				recipient_screen_name, tweet_id, id
			),
		}
	}

    Ok(())
}
