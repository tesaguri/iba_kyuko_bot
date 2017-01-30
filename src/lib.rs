#![feature(slice_patterns)]
#![recursion_limit = "1024"]

extern crate chrono;
#[macro_use]
extern crate error_chain;
extern crate hyper;
extern crate kuchiki;
#[macro_use]
extern crate log;
#[macro_use]
extern crate serde_derive;

pub mod errors {
    error_chain! { }
}

pub mod scraper;

use chrono::{Datelike, NaiveDate};
use std::convert::{AsRef, From};
use std::fmt::{self, Display, Formatter};
use std::iter::FromIterator;

pub use scraper::scrape;

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq, Hash)]
pub struct Kyuko<D: Datelike = NaiveDate> {
    pub kind: String,
    pub date: D,
    pub periods: Periods,
    pub title: String,
    pub lecturer: String,
    pub remarks: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq, Hash)]
pub struct Periods(Vec<u8>);

impl Display for Periods {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        let mut iter = self.0.iter();

        if let Some(&n) = iter.next() {
            let mut start = n;
            write!(f, "{}", start)?;
            let mut before = n;
            loop {
                let n = iter.next().cloned();
                if Some(before+1) != n {
                    if start != before {
                        write!(f, "-{}", before)?;
                    }
                    if let Some(n) = n {
                        start = n;
                        write!(f, ",{}", start)?;
                    }
                }
                if let Some(n) = n {
                    before = n;
                } else {
                    return Ok(());
                }
            }
        } else {
            Ok(())
        }
    }
}

impl From<Vec<u8>> for Periods {
    fn from(mut v: Vec<u8>) -> Periods {
        v.sort();
        v.dedup();
        v.shrink_to_fit();
        Periods(v)
    }
}

impl FromIterator<u8> for Periods {
    fn from_iter<T>(iter: T) -> Periods where T: IntoIterator<Item=u8> {
        iter.into_iter().collect::<Vec<u8>>().into()
    }
}

impl AsRef<[u8]> for Periods {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;

    #[test]
    fn periods_display() {
        macro_rules! test_eq {
            ($p:expr, $d:expr) => {{
                let mut s = String::new();
                write!(s, "{}", $p.iter().cloned().collect::<Periods>()).unwrap();
                assert_eq!(&s, $d);
            }};
        }

        test_eq!([], "");
        test_eq!([1], "1");
        test_eq!([1,2], "1-2");
        test_eq!([1,2,3], "1-3");
        test_eq!([1,3], "1,3");
        test_eq!([1,3,5], "1,3,5");
        test_eq!([1,2,3,5], "1-3,5");
        test_eq!([1,3,4,5], "1,3-5");
        test_eq!([1,3,4,6], "1,3-4,6");
        test_eq!([6,4,1,2], "1-2,4,6");
    }
}
