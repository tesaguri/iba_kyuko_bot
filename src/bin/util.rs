use chrono::{DateTime, UTC};
use cron::schedule::{CronSchedule, CronScheduleIterator};
use errors::*;
use futures::{Poll, Stream};
use serde::{Serialize, Deserialize};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::iter::Peekable;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use yaml;

pub struct Schedule<'a> {
    upcoming: MergedIterator<CronScheduleIterator<'a>>,
    next: Option<DateTime<UTC>>,
    waiting: Arc<AtomicBool>,
}

pub struct SyncFile<T> {
    data: T,
    file: File,
    path: PathBuf,
    backup_path: PathBuf,
}

struct MergedIterator<I> where I: Iterator {
    iters: Vec<Peekable<I>>,
}

impl<'a> Schedule<'a> {
    pub fn new(crons: &'a [CronSchedule]) -> Self {
        Schedule {
            upcoming: MergedIterator::new(crons.iter().map(CronSchedule::upcoming)),
            next: None,
            waiting: Arc::new(AtomicBool::new(false)),
        }
    }

    fn next(&mut self, now: DateTime<UTC>) -> Option<DateTime<UTC>> {
        self.upcoming.by_ref().filter(|tm| tm > &now).next()
    }

    fn set_timer(&mut self, dur: Duration) {
        use futures::task;
        use std::thread;

        let task = task::park();
        let waiting = self.waiting.clone();
        waiting.store(true, Ordering::Relaxed);

        thread::spawn(move || {
            thread::sleep(dur);
            task.unpark();
            waiting.store(false, Ordering::Release);
        });
    }
}

impl<'a> Stream for Schedule<'a> {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<()>, Error> {
        use futures::Async::*;

        let now = UTC::now();

        let next = if let Some(next) = self.next {
            next
        } else if let Some(next) = self.next(now) {
            self.next = Some(next);
            next
        } else {
            return Ok(Ready(None));
        };

        if let Ok(dur) = next.signed_duration_since(now).to_std() {
            if !self.waiting.load(Ordering::Acquire) {
                self.set_timer(dur);
            }
            Ok(NotReady)
        } else {
            if let Some(next) = self.next(now) {
                self.next = Some(next);
                self.set_timer(next.signed_duration_since(now).to_std().unwrap());
            } else {
                self.next = None;
            }
            Ok(Ready(Some(())))
        }
    }
}

impl<T> SyncFile<T> {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> where T: Default + Serialize + Deserialize {
        let path = path.as_ref();

        debug!("SyncFile::new: opening {:?}", path);

        let backup_path = if let Some(n) = path.file_name() {
            let mut name = OsString::from(".");
            name.push(n);
            name.push(".bck");

            let mut path = path.to_owned();
            path.set_file_name(name);
            path
        } else {
            return Err("expected a file name".into());
        };

        let exists = backup_path.exists();
        if exists {
            info!("the last session has aborted unexpectedly; recovering the file");
            if path.exists() {
                fs::remove_file(path).chain_err(|| "failed to remove a corrupt file")?;
            }
            fs::rename(&backup_path, path).chain_err(|| "failed to recover a corrupt file")?;
        }

        let exists = exists || path.exists();
        let file = OpenOptions::new().read(true).write(true).create(true).open(path)
            .chain_err(|| "unable to open the file")?;
        let data = if exists {
            yaml::from_reader(&file).chain_err(|| "failed to load the file")?
        } else {
            T::default()
        };

        let ret = SyncFile {
            data: data,
            file: file,
            path: path.to_owned(),
            backup_path: backup_path,
        };

        if !exists {
            ret.commit().chain_err(|| "failed to initialize the file")?;
        }

        Ok(ret)
    }

    pub fn commit(&self) -> Result<()> where T: Serialize {
        use std::io::{Seek, SeekFrom, Write};

        debug!("SyncFile::commit: commiting changes to {:?}", self.path);

        let temp = temp_path();

        debug!("creating a backup {:?}", temp);
        fs::copy(&self.path, &temp).chain_err(|| "failed to make a backup")?;
        fs::rename(temp, &self.backup_path).chain_err(|| format!("failed to make a backup to {:?}", self.backup_path))?;

        let mut w = &self.file;

        w.seek(SeekFrom::Start(0)).chain_err(|| "failed to update the file")?;
        w.set_len(0).chain_err(|| "failed to update the file")?;
        yaml::to_writer(&mut w, &self.data).chain_err(|| "failed to update the file")?;
        w.flush().chain_err(|| "failed to update the file")?;

        fs::remove_file(&self.backup_path).chain_err(|| "failed to delete a backup file")
    }

    pub fn file_name(&self) -> &OsStr {
        self.path.file_name().unwrap()
    }
}

impl<T> Deref for SyncFile<T> {
    type Target = T;
    fn deref(&self) -> &T { &self.data }
}

impl<T> DerefMut for SyncFile<T> {
    fn deref_mut(&mut self) -> &mut T { &mut self.data }
}

impl<I> MergedIterator<I> where I: Iterator {
    fn new<J: IntoIterator<Item=I>>(iters: J) -> Self {
        MergedIterator {
            iters: iters.into_iter().map(|i| i.peekable()).collect(),
        }
    }
}

impl<I> Iterator for MergedIterator<I> where I: Iterator, I::Item: Ord + Copy {
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        // TODO: explore more efficient way.
        let mut cloned: Vec<_> = self.iters.iter_mut()
            .map(|i| i.peek().cloned())
            .collect();

        // Remove iterators that don't have next item.
        {
            let mut has_next = cloned.iter().map(Option::is_some);
            self.iters.retain(|_| has_next.next().unwrap());
        }
        cloned.retain(Option::is_some);

        self.iters.iter_mut()
            .rev()
            .min_by_key(|_| cloned.pop())
            .and_then(Iterator::next)
    }
}

/// Returns an integer representation of the rightmost contiguous digits in `s`.
pub fn ratoi(s: &str) -> Option<u64> {
    fn atoi(c: u8) -> Option<u8> {
        if b'0' <= c && c <= b'9' {
            Some(c - b'0')
        } else {
            None
        }
    }

    let mut iter = s.as_bytes().iter().cloned().rev();

    while let Some(b) = iter.next() {
        if let Some(n) = atoi(b) {
            let mut ret = n as u64;
            let mut exp = 1;

            for b in iter {
                if let Some(n) = atoi(b) {
                    exp *= 10;
                    ret += exp * n as u64;
                } else {
                    break;
                }
            }

            return Some(ret);
        }
    }

    None
}

fn temp_path() -> PathBuf {
    use rand::{self, Rng};
    use std::env;

    const RADIX64: &'static [u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+-";

    let mut rng = rand::thread_rng();

    loop {
        let mut rand = rng.next_u64();

        let mut name = b".".to_vec();
        for _ in 0..10 {
            name.push(RADIX64[(rand % 64) as usize]);
            rand >>= 6;
        }
        name.extend_from_slice(b".tmp");
        let name = unsafe { String::from_utf8_unchecked(name) };

        let mut path = env::temp_dir();
        path.push(name);

        if !path.exists() {
            return path;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_iter() {
        let mut a0 = vec![1,4,7,10];
        let mut a1 = vec![3,4,5,6];
        let mut a2 = vec![2,5,8,11];
        let mut arrays = vec![
            a0.drain(..),
            a1.drain(..),
            a2.drain(..),
        ];
        let merged: Vec<_> = MergedIterator::new(arrays.drain(..)).collect();
        assert_eq!([1, 2, 3, 4, 4, 5, 5, 6, 7, 8, 10, 11].as_ref(), merged.as_slice());
    }

    #[test]
    fn rdigits_test() {
        assert_eq!(Some(1234567890), ratoi("1234567890"));
        assert_eq!(Some(145344012), ratoi("https://twitter.com/Twitter/status/145344012"));
        assert_eq!(Some(815348177809408001), ratoi("https://twitter.com/Twitter/status/815348177809408001/"));
        assert_eq!(Some(600324682190053376), ratoi("https://twitter.com/POTUS44/status/600324682190053376"));
        assert_eq!(None, ratoi("https://twitter.com/Twitter/"));
    }
}
