use crate::rss_sites::{RssSites, Site};
use enum_from_impler::EnumFromImpler;
use futures::future::join_all;
use lettre::smtp::authentication::Mechanism;
use lettre::smtp::ConnectionReuseParameters;
use lettre::{ClientSecurity, ClientTlsParameters, SmtpClient, Transport};
use lettre_email::EmailBuilder;
use log::*;
use native_tls::Protocol;
use native_tls::TlsConnector;
use rss::Channel;
use std::io;
use std::io::BufReader;
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;
use structopt::StructOpt;
use tokio::time::interval;
use url::Url;
use uuid::Uuid;

#[derive(StructOpt, Debug)]
#[structopt(name = "rss-notifier")]
enum Opt {
    List,
    Run,
    Add {
        #[structopt(short, long)]
        name: Option<String>,
        #[structopt(parse(try_from_str))]
        url: Url,
    },
}

fn main() {
    let opt = Opt::from_args();
    match opt {
        Opt::Run => run().unwrap(),
        Opt::List => (),
        Opt::Add { name, url } => add(name, url).unwrap(),
    }
}

const SITES_PATH: &str = "sites_v2.json";

fn run() -> io::Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "rss_notifier");
    }
    env_logger::builder().format_module_path(false).init();

    let rss_sites = rss_sites::RssSites::new(SITES_PATH).unwrap();
    set_ctrlc_handler(SITES_PATH, &rss_sites);

    tokio::runtime::Builder::new()
        .threaded_scheduler()
        .enable_io()
        .enable_time()
        .build()
        .unwrap()
        .block_on(start_email_task(rss_sites));

    Ok(())
}

#[allow(irrefutable_let_patterns)]
async fn start_email_task(rss_sites: RssSites) {
    println!("Starting ticker");

    let mut ticker = interval(Duration::from_secs(5));
    while let _ = ticker.tick().await {
        info!("Checking sites...");

        let sites = rss_sites.sites.lock().unwrap().clone();

        let check_results = join_all(sites.into_iter().map(check_site)).await;

        for (site, new_items) in check_results {
            println!(
                "Site: {} has {} new items",
                site.name.unwrap_or_default(),
                new_items.len()
            );
        }
    }
}

#[derive(Debug, EnumFromImpler)]
enum CheckSiteError {
    #[impl_from]
    Request(reqwest::Error),
    #[impl_from]
    Io(std::io::Error),
    #[impl_from]
    Rss(rss::Error),
    #[impl_from]
    Chrono(chrono::ParseError),
}

async fn check_site(site: Site) -> (Site, Vec<rss::Item>) {
    match try_check_site(&site).await {
        Ok(v) => (site, v),
        Err(e) => {
            error!(
                "Error checking: {}({}): {:?}",
                site.name.as_deref().unwrap_or_default(),
                site.url,
                e
            );
            (site, Vec::with_capacity(0))
        }
    }
}

async fn try_check_site(site: &Site) -> Result<Vec<rss::Item>, CheckSiteError> {
    let channel = fetch_rss(&site).await?;

    println!("Feed: {}", channel.title());
    let mut new_items = Vec::new();

    for item in channel.items() {
        let pub_date = chrono::DateTime::parse_from_rfc2822(item.pub_date().unwrap_or_default())?;

        if pub_date > site.last_accessed {
            info!("New item: \"{}\"", item.title().unwrap_or_default());
            new_items.push(item.to_owned());
        }
    }

    Ok(new_items)
}

async fn fetch_rss(site: &Site) -> Result<Channel, CheckSiteError> {
    let rss_bytes = reqwest::get(site.url.clone()).await?.bytes().await?;
    Ok(Channel::read_from(BufReader::new(rss_bytes.as_ref()))?)
}

fn add(name: Option<String>, url: Url) -> io::Result<()> {
    let rss_sites = rss_sites::RssSites::new(SITES_PATH).unwrap();
    rss_sites.sites.lock().unwrap().push(rss_sites::Site {
        uuid: Uuid::new_v4(),
        name,
        url,
        last_accessed: chrono::MIN_DATE.and_hms(0, 0, 0),
    });
    rss_sites.save(SITES_PATH)?;
    Ok(())
}

fn set_ctrlc_handler(path: &'static str, rss_sites: &RssSites) {
    let sites = Arc::clone(&rss_sites.sites);
    ctrlc::set_handler(move || {
        println!("exit");
        let sites = match sites.lock() {
            Ok(v) => v,
            Err(err) => err.into_inner(),
        };
        let file = std::fs::File::create(&path).unwrap();
        serde_json::to_writer_pretty(file, &*sites).unwrap();

        exit(0);
    })
    .unwrap();
}

#[derive(Debug, EnumFromImpler)]
enum SendEmailError {
    #[impl_from]
    Io(std::io::Error),
    #[impl_from]
    TomlDeserialize(toml::de::Error),
}

//TODO return email client errors, not only std::io
fn send_email(subject: impl Into<String>, body: impl Into<String>) -> Result<(), SendEmailError> {
    let cfg = toml::from_slice::<config::Config>(&std::fs::read("cfg.toml")?)?;

    let email = EmailBuilder::new()
        .to(cfg.recipients[0].as_str())
        .from(cfg.credentials.username.clone())
        .subject(subject)
        .html(body)
        .build()
        .unwrap();

    let mut tls_builder = TlsConnector::builder();
    tls_builder.min_protocol_version(Some(Protocol::Tlsv10));

    let tls_parameters =
        ClientTlsParameters::new(cfg.credentials.domain.clone(), tls_builder.build().unwrap());

    pub const SUBMISSION_PORT: u16 = 465;

    let mut mailer = SmtpClient::new(
        (cfg.credentials.domain.as_str(), SUBMISSION_PORT),
        ClientSecurity::Wrapper(tls_parameters),
    )
    .expect("Failed to create transport")
    .authentication_mechanism(Mechanism::Login)
    .credentials(cfg.credentials)
    .connection_reuse(ConnectionReuseParameters::ReuseUnlimited)
    .transport();

    println!("{:?}", mailer.send(email.into()));

    mailer.close();
    Ok(())
}

mod config {
    use lettre::smtp::authentication::Credentials as LettreCredentials;
    use serde::Deserialize;

    #[derive(Deserialize, Debug)]
    pub struct Config {
        pub credentials: Credentials,
        pub recipients: Vec<String>,
    }

    #[derive(Deserialize, Debug)]
    pub struct Credentials {
        pub username: String,
        pub password: String,
        pub domain: String,
    }

    impl Into<LettreCredentials> for Credentials {
        fn into(self) -> LettreCredentials {
            LettreCredentials::new(self.username, self.password)
        }
    }
}

mod rss_sites {
    use std::io;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use chrono::{DateTime, Utc};
    use notify::{RecommendedWatcher, RecursiveMode, Watcher};
    use serde::{Deserialize, Serialize};
    use url::Url;

    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct Site {
        pub uuid: uuid::Uuid,
        pub name: Option<String>,
        pub url: Url,
        pub last_accessed: DateTime<Utc>,
    }

    pub struct RssSites {
        pub sites: Arc<Mutex<Vec<Site>>>,
    }

    impl RssSites {
        pub fn new<P: AsRef<Path>>(path: P) -> notify::Result<RssSites> {
            let rss_sites = RssSites {
                sites: Arc::new(Mutex::new(load_sites_from_file(&path).unwrap())),
            };
            let sites_c = rss_sites.sites.clone();
            let mut watcher: RecommendedWatcher = Watcher::new_immediate(move |res| {
                println!("Current len: {}", sites_c.lock().unwrap().len());
                match res {
                    Ok(event) => println!("ok {:?}", event),
                    Err(err) => println!("err {:?}", err),
                }
            })?;
            watcher.configure(notify::Config::PreciseEvents(true))?;
            watcher.watch(&path, RecursiveMode::NonRecursive)?;

            Ok(rss_sites)
        }

        pub fn save<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
            let f = std::fs::File::create(path)?;
            serde_json::to_writer_pretty(f, &*self.sites.lock().unwrap()).unwrap();
            Ok(())
        }
    }

    fn load_sites_from_file<P: AsRef<Path>>(path: P) -> io::Result<Vec<Site>> {
        let data = match std::fs::read(&path) {
            Ok(v) => v,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let f = std::fs::File::create(path)?;
                let sites = Vec::new();
                serde_json::to_writer_pretty(f, &sites)?;
                return Ok(sites);
            }
            Err(err) => return Err(err),
        };
        Ok(serde_json::from_slice(&data).unwrap())
    }
}
