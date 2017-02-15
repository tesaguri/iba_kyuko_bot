use egg_mode::{KeyPair, Token};
use either::{Either, Left, Right};
use errors::*;
use iba_kyuko_bot::Kyuko;
use schedule::UnitSchedule;
use std::collections::HashMap;
use std::fmt::{self, Formatter};
use std::fs::{File, OpenOptions};
use std::path::Path;
use twitter_stream::messages::UserId;
use util::SyncFile;

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub enum Follow {
    #[serde(rename = "pattern")]
    Pattern {
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        lecturer: Option<String>,
    },
    #[serde(rename = "tweet_id")]
    TweetId(u64),
}

pub enum FollowError {
    AlreadyFollowing(String),
    TweetDoesNotExist(u64),
}

#[derive(Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub schedule: Vec<UnitSchedule>,
    #[serde(default)]
    pub admins: Vec<UserId>,
    pub consumer_key: String,
    pub consumer_secret: String,
    pub access_key: String,
    pub access_secret: String,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    pub urls: Vec<String>,
}

pub fn default_user_agent() -> String {
    concat!(env!("CARGO_PKG_NAME"), '/', env!("CARGO_PKG_VERSION"), " (+", env!("CARGO_PKG_HOMEPAGE"), ')').to_owned()
}

#[derive(Default, Serialize, Deserialize)]
pub struct UserInfo {
    pub following: HashMap<
        String, // id
        Follow
    >,
    pub next_id: u64,
    // TODO: rate limit
}

pub type Tweeted = HashMap<
    String, // source URL
    HashMap<
        String, // tweet id
        Kyuko
    >
>;

pub type UserMap = HashMap<
    String, // user id
    UserInfo,
>;

/// Load configuration files under the specified directory.
pub fn load<P: AsRef<Path>>(working_dir: P) -> Result<(SyncFile<Tweeted>, SyncFile<UserMap>, Settings, File)> {
    use std::fs;

    let path = working_dir.as_ref();
    debug!("load: loading the working directory {:?}", path);

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

    let tweeted = for_file!("tweets.yml", SyncFile::new).chain_err(|| "unable to open tweets.yml")?;
    let users = for_file!("users.yml", SyncFile::new).chain_err(|| "unable to open users.yml")?;
    let settings: Settings = ::yaml::from_reader(
        for_file!("settings.yml", File::open).chain_err(|| "unable to open settings.yml")?
    ).chain_err(|| "failed to load settings.yml")?;
    let archive = for_file!("archive.tsv", |path| OpenOptions::new().append(true).create(true).open(path))
        .chain_err(|| "unable to open archive.tsv")?;

    Ok((tweeted, users, settings, archive))
}

impl Follow {
    pub fn matches(&self, k: &Kyuko) -> bool {
        if let Follow::Pattern { ref title, ref lecturer } = *self {
            k.title.contains(title) && lecturer.as_ref().map_or(true, |l| k.lecturer.contains(l))
        } else {
            false
        }
    }
}

impl Settings {
    pub fn token(&self) -> Token {
        Token::Access {
            consumer: KeyPair::new(self.consumer_key.as_str(), self.consumer_secret.as_str()),
            access: KeyPair::new(self.access_key.as_str(), self.access_secret.as_str()),
        }
    }
}

impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("Settings")
            .field("schedule", &self.schedule)
            .field("admins", &self.admins)
            .field("user_agent", &self.user_agent)
            .field("urls", &self.user_agent)
            .finish()
    }
}

impl UserInfo {
    pub fn clear(&mut self) {
        self.following.clear();
        self.following.shrink_to_fit();
        self.next_id = 0;
    }

    pub fn follow<'a>(&mut self, target: Follow, tweeted: &'a SyncFile<Tweeted>)
    -> ::std::result::Result<(String, Either<(String, Option<String>), (u64, &'a Kyuko)>), FollowError>
    {
        use self::Follow::*;
        use self::FollowError::*;

        if let Some((id, _)) = self.following.iter().find(|&(_, flw)| flw == &target) {
            return Err(AlreadyFollowing(id.to_owned()));
        }

        let ret = match target.clone() {
            Pattern { title, lecturer } => Left((title, lecturer)),
            TweetId(id) => {
                let id_str = id.to_string();
                if let Some(k) = tweeted.values().filter_map(|kyukos| kyukos.get(&id_str)).next() {
                    Right((id, k))
                } else {
                    return Err(TweetDoesNotExist(id));
                }
            },
        };

        let id = self.next_id.to_string();
        self.following.insert(id.clone(), target);
        self.next_id += 1;

        Ok((id, ret))
    }
}
