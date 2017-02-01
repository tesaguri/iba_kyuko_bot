use errors::*;
use futures::{Poll, Stream};
use serde::{Serialize, Deserialize};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use yaml;

pub const BASE64: &'static [u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub struct Interval<F> {
    scheduler: F,
    next: Instant,
    parked: bool,
}

pub struct SyncFile<T> {
    data: T,
    file: File,
    path: PathBuf,
    backup_path: PathBuf,
}

impl<F> Interval<F> {
    pub fn new(scheduler: F) -> Self {
        Interval {
            scheduler: scheduler,
            next: Instant::now(),
            parked: false,
        }
    }

    fn park(&mut self, now: Instant) {
        use futures::task;
        use std::thread;

        let wait = self.next - now;
        let task = task::park();
        thread::spawn(move || {
            thread::sleep(wait);
            task.unpark();
        });
    }
}

impl<F> Stream for Interval<F> where F: Fn() -> Option<Duration> {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<()>, Error> {
        use futures::Async::*;

        let now = Instant::now();
        if now < self.next {
            if !self.parked {
                self.park(now);
                self.parked = true;
            }
            Ok(NotReady)
        } else {
            if let Some(dur) = (self.scheduler)() {
                self.next = now + dur;
                self.park(now);
                Ok(Ready(Some(())))
            } else {
                Ok(Ready(None))
            }
        }
    }
}

impl<T> SyncFile<T> {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> where T: Default + Deserialize {
        let path = path.as_ref();

        let backup_path = if let Some(n) = path.file_name() {
            let mut name = OsString::from(".");
            name.push(n);
            name.push(".bck");

            let mut path = path.to_owned();
            path.set_file_name(name);
            path
        } else {
            return Err("expecting a file name".into());
        };

        let exists = backup_path.exists();
        if exists {
            if path.exists() {
                fs::remove_file(path).chain_err(|| "failed to remove a corrupt file")?;
            }
            fs::rename(&backup_path, path).chain_err(|| "failed to recover a corrupt file")?;
        }

        let exists = exists || path.exists();
        let file = OpenOptions::new().read(true).write(true).create(true).open(path)
            .chain_err(|| format!("unable to open the file"))?;
        let data = if exists {
            yaml::from_reader(&file).chain_err(|| format!("failed to load the file"))?
        } else {
            T::default()
        };

        Ok(SyncFile {
            data: data,
            file: file,
            path: path.to_owned(),
            backup_path: backup_path,
        })
    }

    pub fn commit(&mut self) -> Result<()> where T: Serialize {
        use std::io::SeekFrom;

        let temp = temp_path();
        fs::copy(&self.path, &temp).chain_err(|| "failed to make a backup")?;

        fs::rename(temp, &self.backup_path).chain_err(|| "failed to make a backup")?;

        self.file.seek(SeekFrom::Start(0)).chain_err(|| "failed to update the file")?;
        self.file.set_len(0).chain_err(|| "failed to update the file")?;
        yaml::to_writer(&mut self.file, &self.data).chain_err(|| "failed to update the file")?;
        self.file.flush().chain_err(|| "failed to update the file")?;

        fs::remove_file(&self.backup_path).chain_err(|| "failed to delete a backup file")
    }
}

impl<T> Deref for SyncFile<T> {
    type Target = T;
    fn deref(&self) -> &T { &self.data }
}

impl<T> DerefMut for SyncFile<T> {
    fn deref_mut(&mut self) -> &mut T { &mut self.data }
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

    let mut rng = rand::thread_rng();

    loop {
        let mut rand = rng.next_u64();

        let mut name = b".".to_vec();
        for _ in 0..10 {
            name.push(BASE64[(rand % 64) as usize]);
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
    fn rdigits_test() {
        assert_eq!(Some(1234567890), ratoi("1234567890"));
        assert_eq!(Some(145344012), ratoi("https://twitter.com/Twitter/status/145344012"));
        assert_eq!(Some(815348177809408001), ratoi("https://twitter.com/Twitter/status/815348177809408001/"));
        assert_eq!(Some(600324682190053376), ratoi("https://twitter.com/POTUS44/status/600324682190053376"));
        assert_eq!(None, ratoi("https://twitter.com/Twitter/"));
    }
}
