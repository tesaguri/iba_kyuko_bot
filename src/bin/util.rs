use errors::*;
use serde::{Serialize, Deserialize};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::iter::Peekable;
#[cfg(feature = "unstable")]
use std::iter::FusedIterator;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use yaml;

pub struct CarryingUpIterator<I, J, K> where I: Iterator, J: Iterator {
    i_orig: Peekable<I>,
    j_orig: Peekable<J>,
    k_orig: K,
    i: Peekable<I>,
    j: Peekable<J>,
    k: K,
}

pub struct MergedIterator<I> where I: Iterator {
    iters: Vec<Peekable<I>>,
}

pub struct SyncFile<T> {
    data: T,
    file: File,
    path: PathBuf,
    backup_path: PathBuf,
}

impl<I, J, K> CarryingUpIterator<I, J, K>
    where I: Iterator + Clone, J: Iterator + Clone, K: Iterator + Clone, I::Item: Clone, J::Item: Clone
{
    pub fn new(i: I, j: J, k: K) -> Option<Self> {
        let (mut i, mut j) = (i.peekable(), j.peekable());

        if let (Some(_), Some(_), Some(_)) = (i.peek(), j.peek(), k.clone().next()) {
            Some(CarryingUpIterator {
                i_orig: i.clone(),
                j_orig: j.clone(),
                k_orig: k.clone(),
                i: i,
                j: j,
                k: k,
            })
        } else {
            None
        }
    }
}

impl<I, J, K> Iterator for CarryingUpIterator<I, J, K>
    where I: Iterator + Clone, J: Iterator + Clone, K: Iterator + Clone, I::Item: Clone, J::Item: Clone
{
    type Item = (I::Item, J::Item, K::Item);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(k) = self.k.next() {
            Some((self.i.peek().unwrap().clone(), self.j.peek().unwrap().clone(), k))
        } else {
            self.k = self.k_orig.clone();
            self.j.next();
            if let Some(j) = self.j.peek().cloned() {
                Some((self.i.peek().unwrap().clone(), j, self.k.next().unwrap()))
            } else {
                self.j = self.j_orig.clone();
                self.i.next();
                if let Some(i) = self.i.peek().cloned() {
                    Some((i, self.j.peek().unwrap().clone(), self.k.next().unwrap()))
                } else {
                    self.i = self.i_orig.clone();
                    Some((self.i.peek().unwrap().clone(), self.j.peek().unwrap().clone(), self.k.next().unwrap()))
                }
            }
        }
    }
}

#[cfg(feature = "unstable")]
impl<I, J, K> FusedIterator for CarryingUpIterator<I, J, K>
    where I: Iterator + Clone, J: Iterator + Clone, K: Iterator + Clone, I::Item: Clone, J::Item: Clone {}

impl<I> MergedIterator<I> where I: Iterator {
    pub fn new<J: IntoIterator<Item=I>>(iters: J) -> Self {
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
    fn carrying_up_iter() {
        let a0 = [1, 2];
        let a1 = ['x', 'y'];
        let a2 = ['A', 'B', 'C'];

        let mut iter = CarryingUpIterator::new(a0.iter().cloned(), a1.iter().cloned(), a2.iter().cloned()).unwrap();

        assert_eq!(Some((1, 'x', 'A')), iter.next());
        assert_eq!(Some((1, 'x', 'B')), iter.next());
        assert_eq!(Some((1, 'x', 'C')), iter.next());

        assert_eq!(Some((1, 'y', 'A')), iter.next());
        assert_eq!(Some((1, 'y', 'B')), iter.next());
        assert_eq!(Some((1, 'y', 'C')), iter.next());

        assert_eq!(Some((2, 'x', 'A')), iter.next());
        assert_eq!(Some((2, 'x', 'B')), iter.next());
        assert_eq!(Some((2, 'x', 'C')), iter.next());

        assert_eq!(Some((2, 'y', 'A')), iter.next());
        assert_eq!(Some((2, 'y', 'B')), iter.next());
        assert_eq!(Some((2, 'y', 'C')), iter.next());

        assert_eq!(Some((1, 'x', 'A')), iter.next());
        assert_eq!(Some((1, 'x', 'B')), iter.next());
    }

    #[test]
    fn merged_iter() {
        let a0 = [1 ,4, 7, 10];
        let a1 = [3 ,4, 5, 6];
        let a2 = [2 ,5, 8, 11];

        let arrays = [a0.iter().cloned(), a1.iter().cloned(), a2.iter().cloned()];

        let merged: Vec<_> = MergedIterator::new(arrays.iter().cloned()).collect();

        assert_eq!([1, 2, 3, 4, 4, 5, 5, 6, 7, 8, 10, 11].as_ref(), merged.as_slice());
    }
}
