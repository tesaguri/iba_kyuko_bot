#![recursion_limit = "1024"]

extern crate env_logger;
#[macro_use]
extern crate error_chain;
extern crate hyper;
extern crate iba_kyuko;
#[macro_use]
extern crate log;
extern crate serde_yaml;

use hyper::header::UserAgent;
use hyper::Client;
use iba_kyuko::Kyuko;
use log::LogLevel;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::OpenOptions;
use std::io::{self, Read, Write, Seek, SeekFrom};
use std::path::Path;
use std::process;

mod errors {
    error_chain! {
        links {
            Kyuko(::iba_kyuko::errors::Error, ::iba_kyuko::errors::ErrorKind);
        }

        foreign_links {
            Hyper(::hyper::Error);
            Io(::std::io::Error);
            SetLogger(::log::SetLoggerError);
            Yaml(::serde_yaml::Error);
        }
    }
}

use errors::*;

type Yaml = HashMap<String, HashSet<Kyuko>>;

fn main() {
    if let Err(e) = run() {
        writeln!(&mut io::stderr(), "error: {}", e).unwrap();
        for e in e.iter().skip(1) {
            writeln!(&mut io::stderr(), "caused by: {}", e).unwrap();
        }
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let urls = [
        "http://gbbs.admb.ibaraki.ac.jp/index-student2.php?g=3", // 教養
        "http://gbbs.admb.ibaraki.ac.jp/index-student2.php?g=4", // 人文
        "http://gbbs.admb.ibaraki.ac.jp/index-student2.php?g=5", // 教育
        "http://gbbs.admb.ibaraki.ac.jp/index-student2.php?g=6", // 理学
        "http://gbbs.admb.ibaraki.ac.jp/index-student2.php?g=7", // 工学
        "http://gbbs.admb.ibaraki.ac.jp/index-student2.php?g=8", // 農学
    ];
    let ua = format!(
        "IbarakiUniversityKyukoBot/{} (+{})",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_HOMEPAGE")
    );

    env_logger::init()?;

    let mut args = env::args().skip(1);

    let yaml_path = args.next().unwrap_or("kyuko.yml".to_owned());
    let yaml_path = Path::new(&yaml_path);
    let exists = yaml_path.exists();
    let mut yaml_file = OpenOptions::new().read(true).write(true).create(true).open(yaml_path)
        .chain_err(|| "unable to open cache file")?;
    let mut yaml: Yaml = if exists {
        serde_yaml::from_reader(&yaml_file).chain_err(|| "unable to parse cache file")?
    } else {
        Yaml::new()
    };
    
    let mut archive = if let Some(path) = args.next() {
        Some(OpenOptions::new().append(true).create(true).open(path).chain_err(|| "unable to open archive file")?)
    } else {
        None
    };

    let client = Client::new();

    for &url in urls.iter() {
        let mut res = client.get(url).header(UserAgent(ua.clone())).send()?;
        let mut body = String::new();
        res.read_to_string(&mut body).chain_err(|| format!("unable to read HTTP response from {}", url))?;

        let (dept, kyukos) = iba_kyuko::scrape(body).chain_err(|| format!("failed to scrape HTML from {}", url))?;
        {
            let cache = yaml.entry(dept.clone()).or_insert_with(HashSet::new);

            // TODO: post on Twitter.
            if log_enabled!(LogLevel::Info) {
                info!("department: {:?}", dept);
                for k in kyukos.difference(cache) {
                    info!("{:?}", k);
                }
            }

            for f in archive.as_mut() {
                for k in cache.difference(&kyukos) {
                    write_tsv(f, &dept, k)?;
                }
            }
        }
        yaml.insert(dept, kyukos);
    }

    yaml_file.seek(SeekFrom::Start(0))?;
    yaml_file.set_len(0)?;
    serde_yaml::to_writer(&mut yaml_file, &yaml)?;

    Ok(())
}

fn write_tsv<W: Write>(out: &mut W, dept: &str, k: &Kyuko) -> io::Result<()> {
    match k.remarks {
        Some(ref remarks) => writeln!(out, "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            dept, k.kind, k.date, k.periods, k.title, k.lecturer, remarks),
        None              => writeln!(out, "{}\t{}\t{}\t{}\t{}\t{}",
            dept, k.kind, k.date, k.periods, k.title, k.lecturer),
    }
}
