use config::{Tweeted, UserMap};
use egg_mode::{Token, tweet};
use errors::*;
use util::SyncFile;

pub fn remove<'a, I: 'a + Iterator<Item=&'a str>>(ids: I, tweeted: &mut SyncFile<Tweeted>, token: &Token)
    -> Result<()>
{
    use clap;

    for id in ids {
        info!("removing Tweet ID {}", id);

        for tweets in tweeted.values_mut() {
            tweets.remove(id);
        }

        let id = match id.parse() {
            Ok(id) => id,
            Err(_) => clap::Error::with_description(
                &format!("invalid status id: {}", id),
                clap::ErrorKind::InvalidValue
            ).exit(),
        };

        tweet::delete(id, token).chain_err(|| format!("failed to remove a Tweet (ID: {})", id))?;
        tweeted.commit()?;
    }

    Ok(())
}

pub fn clear(tweeted: &mut SyncFile<Tweeted>, token: &Token) -> Result<()> {
    // TODO: explore better way to handle ownerships.
    loop {
        if let Some((dept, id)) = tweeted.iter()
            .filter_map(|(dept, tweets)| {
                tweets.keys().map(|id| (dept.clone(), id.clone())).next()
            }).next()
        {
            info!("removing Tweet ID {}", id);

            let is_empty = {
                let tweets = tweeted.get_mut(dept.as_str()).unwrap();
                tweets.remove(id.as_str());
                tweets.is_empty()
            };

            if is_empty {
                tweeted.remove(dept.as_str());
            }

            let id = id.parse().chain_err(|| format!("invalid status id {:?} in {:?}", id, tweeted.file_name()))?;
            tweet::delete(id, token).chain_err(|| format!("failed to remove a Tweet (ID: {})", id))?;

            tweeted.commit()?;
        } else {
            return Ok(());
        }
    }
}

pub fn clear_users(users: &mut SyncFile<UserMap>) -> Result<()> {
    println!("clearing all the following information");

    for user in users.values_mut() {
        user.clear();
    }

    users.commit()
}
